mod common;

use std::time::Duration;

use common::api::McmClient;
use common::mcm::McmProcess;
use common::monitor::ProcMonitor;
use common::types::{
    BindAnswer, BindOffer, EndSessionQuestion, SignallingAnswer, SignallingMessage,
    SignallingProtocol, SignallingQuestion,
};
use futures::{SinkExt, StreamExt};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use uuid::Uuid;

const TIMEOUT: Duration = Duration::from_secs(15);

#[tokio::test]
#[serial_test::serial]
async fn test_signalling_connect() {
    let mcm = McmProcess::start().await.unwrap();
    let url = mcm.signalling_url();
    let (_, _) = connect_async(&url).await.unwrap();
}

#[tokio::test]
#[serial_test::serial]
async fn test_peer_id_assigned() {
    let mcm = McmProcess::start().await.unwrap();
    let (ws_stream, _) = connect_async(&mcm.signalling_url()).await.unwrap();
    let (mut ws_sink, mut ws_stream) = ws_stream.split();

    let msg = SignallingProtocol {
        message: SignallingMessage::Question(SignallingQuestion::PeerId),
    };
    let text = serde_json::to_string(&msg).unwrap();
    ws_sink.send(Message::Text(text.into())).await.unwrap();

    let raw = ws_stream.next().await.unwrap().unwrap();
    let text = raw.into_text().unwrap();
    let proto: SignallingProtocol = serde_json::from_str(&text).unwrap();
    match &proto.message {
        SignallingMessage::Answer(SignallingAnswer::PeerId(ans)) => {
            assert!(ans.id != Uuid::nil());
        }
        _ => panic!("expected PeerId answer, got {:?}", proto.message),
    }
}

#[tokio::test]
#[serial_test::serial]
async fn test_available_streams_lists_h264() {
    let mcm = McmProcess::start().await.unwrap();
    let client = McmClient::new(&mcm.rest_url());

    let post = McmClient::build_fake_h264_rtsp("webrtc_h264", 640, 480, 30, "webrtc_h264");
    client.create_stream(&post).await.unwrap();
    client.wait_for_streams_running(1, TIMEOUT).await.unwrap();

    let (ws_stream, _) = connect_async(&mcm.signalling_url()).await.unwrap();
    let (mut ws_sink, mut ws_stream) = ws_stream.split();

    let msg = SignallingProtocol {
        message: SignallingMessage::Question(SignallingQuestion::AvailableStreams),
    };
    let text = serde_json::to_string(&msg).unwrap();
    ws_sink.send(Message::Text(text.into())).await.unwrap();

    let raw = ws_stream.next().await.unwrap().unwrap();
    let text = raw.into_text().unwrap();
    let proto: SignallingProtocol = serde_json::from_str(&text).unwrap();
    match &proto.message {
        SignallingMessage::Answer(SignallingAnswer::AvailableStreams(streams)) => {
            assert!(
                streams.iter().any(|s| {
                    s.encode
                        .as_ref()
                        .map(|e| e.to_uppercase().contains("H264"))
                        .unwrap_or(false)
                }),
                "no H264 stream in {:?}",
                streams
            );
        }
        _ => panic!("expected AvailableStreams answer, got {:?}", proto.message),
    }
}

#[tokio::test]
#[serial_test::serial]
async fn test_start_and_end_session() {
    let mcm = McmProcess::start().await.unwrap();
    let client = McmClient::new(&mcm.rest_url());

    let post = McmClient::build_fake_h264_rtsp("session_stream", 640, 480, 30, "session_stream");
    client.create_stream(&post).await.unwrap();
    client.wait_for_streams_running(1, TIMEOUT).await.unwrap();

    let (ws_stream, _) = connect_async(&mcm.signalling_url()).await.unwrap();
    let (mut ws_sink, mut ws_stream) = ws_stream.split();

    let msg = SignallingProtocol {
        message: SignallingMessage::Question(SignallingQuestion::PeerId),
    };
    let text = serde_json::to_string(&msg).unwrap();
    ws_sink.send(Message::Text(text.into())).await.unwrap();

    let raw = ws_stream.next().await.unwrap().unwrap();
    let text = raw.into_text().unwrap();
    let proto: SignallingProtocol = serde_json::from_str(&text).unwrap();
    let consumer_id = match &proto.message {
        SignallingMessage::Answer(SignallingAnswer::PeerId(ans)) => ans.id,
        _ => panic!("expected PeerId answer"),
    };

    let msg = SignallingProtocol {
        message: SignallingMessage::Question(SignallingQuestion::AvailableStreams),
    };
    let text = serde_json::to_string(&msg).unwrap();
    ws_sink.send(Message::Text(text.into())).await.unwrap();

    let raw = ws_stream.next().await.unwrap().unwrap();
    let text = raw.into_text().unwrap();
    let proto: SignallingProtocol = serde_json::from_str(&text).unwrap();
    let producer_id = match &proto.message {
        SignallingMessage::Answer(SignallingAnswer::AvailableStreams(streams)) => {
            streams.first().unwrap().id
        }
        _ => panic!("expected AvailableStreams answer"),
    };

    let offer = BindOffer {
        consumer_id,
        producer_id,
    };
    let msg = SignallingProtocol {
        message: SignallingMessage::Question(SignallingQuestion::StartSession(offer)),
    };
    let text = serde_json::to_string(&msg).unwrap();
    ws_sink.send(Message::Text(text.into())).await.unwrap();

    let mut bind_answer: Option<BindAnswer> = None;
    while let Some(raw) = ws_stream.next().await {
        let raw = raw.unwrap();
        let text = raw.into_text().unwrap();
        let proto: SignallingProtocol = serde_json::from_str(&text).unwrap();
        match &proto.message {
            SignallingMessage::Answer(SignallingAnswer::StartSession(ans)) => {
                bind_answer = Some(ans.clone());
                break;
            }
            SignallingMessage::Negotiation(_) => continue,
            _ => {}
        }
    }
    let bind = bind_answer.expect("expected StartSession answer");

    let end_question = EndSessionQuestion {
        bind: bind.clone(),
        reason: "test_end".to_string(),
    };
    let msg = SignallingProtocol {
        message: SignallingMessage::Question(SignallingQuestion::EndSession(end_question)),
    };
    let text = serde_json::to_string(&msg).unwrap();
    ws_sink.send(Message::Text(text.into())).await.unwrap();
}

#[tokio::test]
#[serial_test::serial]
async fn test_concurrent_sessions() {
    let mcm = McmProcess::start().await.unwrap();
    let client = McmClient::new(&mcm.rest_url());

    let post =
        McmClient::build_fake_h264_rtsp("concurrent_stream", 640, 480, 30, "concurrent_stream");
    client.create_stream(&post).await.unwrap();
    client.wait_for_streams_running(1, TIMEOUT).await.unwrap();

    let producer_id = {
        let (ws_stream, _) = connect_async(&mcm.signalling_url()).await.unwrap();
        let (mut ws_sink, mut ws_stream) = ws_stream.split();

        let msg = SignallingProtocol {
            message: SignallingMessage::Question(SignallingQuestion::AvailableStreams),
        };
        let text = serde_json::to_string(&msg).unwrap();
        ws_sink.send(Message::Text(text.into())).await.unwrap();

        let raw = ws_stream.next().await.unwrap().unwrap();
        let text = raw.into_text().unwrap();
        let proto: SignallingProtocol = serde_json::from_str(&text).unwrap();
        match &proto.message {
            SignallingMessage::Answer(SignallingAnswer::AvailableStreams(streams)) => {
                streams.first().unwrap().id
            }
            _ => panic!("expected AvailableStreams answer"),
        }
    };

    let url = mcm.signalling_url();
    let mut handles = vec![];
    for _ in 0..3 {
        let url = url.clone();
        let pid = producer_id;
        handles.push(tokio::spawn(async move {
            let (ws_stream, _) = connect_async(&url).await.unwrap();
            let (mut ws_sink, mut ws_stream) = ws_stream.split();

            let msg = SignallingProtocol {
                message: SignallingMessage::Question(SignallingQuestion::PeerId),
            };
            let text = serde_json::to_string(&msg).unwrap();
            ws_sink.send(Message::Text(text.into())).await.unwrap();

            let raw = ws_stream.next().await.unwrap().unwrap();
            let text = raw.into_text().unwrap();
            let proto: SignallingProtocol = serde_json::from_str(&text).unwrap();
            let consumer_id = match &proto.message {
                SignallingMessage::Answer(SignallingAnswer::PeerId(ans)) => ans.id,
                _ => panic!("expected PeerId answer"),
            };

            let offer = BindOffer {
                consumer_id,
                producer_id: pid,
            };
            let msg = SignallingProtocol {
                message: SignallingMessage::Question(SignallingQuestion::StartSession(offer)),
            };
            let text = serde_json::to_string(&msg).unwrap();
            ws_sink.send(Message::Text(text.into())).await.unwrap();

            let mut got_bind = false;
            while let Some(raw) = ws_stream.next().await {
                let raw = raw.unwrap();
                let text = raw.into_text().unwrap();
                let proto: SignallingProtocol = serde_json::from_str(&text).unwrap();
                if matches!(
                    proto.message,
                    SignallingMessage::Answer(SignallingAnswer::StartSession(_))
                ) {
                    got_bind = true;
                    break;
                }
            }
            assert!(got_bind, "expected StartSession answer");
        }));
    }

    for h in handles {
        h.await.unwrap();
    }
}

#[tokio::test]
#[serial_test::serial]
async fn test_disconnect_cleanup() {
    let mcm = McmProcess::start().await.unwrap();
    let client = McmClient::new(&mcm.rest_url());

    let post =
        McmClient::build_fake_h264_rtsp("disconnect_stream", 640, 480, 30, "disconnect_stream");
    client.create_stream(&post).await.unwrap();
    client.wait_for_streams_running(1, TIMEOUT).await.unwrap();

    {
        let (ws_stream, _) = connect_async(&mcm.signalling_url()).await.unwrap();
        let (mut ws_sink, mut ws_stream) = ws_stream.split();

        let msg = SignallingProtocol {
            message: SignallingMessage::Question(SignallingQuestion::PeerId),
        };
        let text = serde_json::to_string(&msg).unwrap();
        ws_sink.send(Message::Text(text.into())).await.unwrap();

        let raw = ws_stream.next().await.unwrap().unwrap();
        let text = raw.into_text().unwrap();
        let proto: SignallingProtocol = serde_json::from_str(&text).unwrap();
        let consumer_id = match &proto.message {
            SignallingMessage::Answer(SignallingAnswer::PeerId(ans)) => ans.id,
            _ => panic!("expected PeerId answer"),
        };

        let msg = SignallingProtocol {
            message: SignallingMessage::Question(SignallingQuestion::AvailableStreams),
        };
        let text = serde_json::to_string(&msg).unwrap();
        ws_sink.send(Message::Text(text.into())).await.unwrap();

        let raw = ws_stream.next().await.unwrap().unwrap();
        let text = raw.into_text().unwrap();
        let proto: SignallingProtocol = serde_json::from_str(&text).unwrap();
        let producer_id = match &proto.message {
            SignallingMessage::Answer(SignallingAnswer::AvailableStreams(streams)) => {
                streams.first().unwrap().id
            }
            _ => panic!("expected AvailableStreams answer"),
        };

        let offer = BindOffer {
            consumer_id,
            producer_id,
        };
        let msg = SignallingProtocol {
            message: SignallingMessage::Question(SignallingQuestion::StartSession(offer)),
        };
        let text = serde_json::to_string(&msg).unwrap();
        ws_sink.send(Message::Text(text.into())).await.unwrap();
    }

    tokio::time::sleep(Duration::from_secs(5)).await;

    let mut monitor = ProcMonitor::start(mcm.pid(), Duration::from_millis(500));
    tokio::time::sleep(Duration::from_secs(1)).await;
    let samples = monitor.stop_and_collect();
    assert!(
        !samples.is_empty(),
        "ProcMonitor should have at least one sample (process still alive)"
    );
}
