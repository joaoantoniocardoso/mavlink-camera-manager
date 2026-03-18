mod common;

use std::time::Duration;

use common::{
    api::{end_webrtc_session, start_webrtc_session, McmClient},
    mcm::McmProcess,
    types::*,
};

const TIMEOUT: Duration = Duration::from_secs(15);

/// The watcher's idle grace period before suspending.
const IDLE_GRACE: Duration = Duration::from_secs(5);

/// Extra slack so the watcher loop (100 ms tick) has time to observe
/// the idle condition and flip the state.
const IDLE_WAIT: Duration = Duration::from_secs(8);

// -- helpers ------------------------------------------------------------

async fn setup_fake_rtsp(name: &str, path: &str) -> (McmProcess, McmClient) {
    let mcm = McmProcess::start().await.unwrap();
    let client = McmClient::new(&mcm.rest_url());

    let post = McmClient::build_fake_h264_rtsp(name, 640, 480, 30, path);
    client.create_stream(&post).await.unwrap();
    client.wait_for_streams_running(1, TIMEOUT).await.unwrap();

    (mcm, client)
}

async fn wait_for_idle(client: &McmClient) {
    tokio::time::sleep(IDLE_WAIT).await;
    client
        .wait_for_stream_state(StreamStatusState::Idle, TIMEOUT)
        .await
        .unwrap();
}

// =======================================================================
// LIFECYCLE STATE TRANSITIONS
// =======================================================================

#[tokio::test]
#[serial_test::serial]
async fn test_stream_becomes_idle_after_grace_period() {
    let (_mcm, client) = setup_fake_rtsp("idle_test", "idle_test").await;

    // Stream is Running right after creation
    let streams = client.list_streams().await.unwrap();
    assert_eq!(streams[0].state, StreamStatusState::Running);

    wait_for_idle(&client).await;
}

#[tokio::test]
#[serial_test::serial]
async fn test_udp_stream_never_goes_idle() {
    let mcm = McmProcess::start().await.unwrap();
    let client = McmClient::new(&mcm.rest_url());

    let post = McmClient::build_fake_h264_udp("udp_no_idle", 640, 480, 30, "127.0.0.1", 5600);
    client.create_stream(&post).await.unwrap();
    client.wait_for_streams_running(1, TIMEOUT).await.unwrap();

    // Wait longer than the idle grace period
    tokio::time::sleep(IDLE_WAIT + Duration::from_secs(5)).await;

    let streams = client.list_streams().await.unwrap();
    assert_eq!(
        streams[0].state,
        StreamStatusState::Running,
        "UDP stream must never go idle"
    );
}

#[tokio::test]
#[serial_test::serial]
async fn test_disable_lazy_prevents_idle() {
    let mcm = McmProcess::start().await.unwrap();
    let client = McmClient::new(&mcm.rest_url());

    let post = McmClient::build_fake_h264_rtsp_ext(
        "no_lazy",
        640,
        480,
        30,
        "no_lazy",
        ExtendedConfiguration {
            disable_lazy: true,
            ..Default::default()
        },
    );
    client.create_stream(&post).await.unwrap();
    client.wait_for_streams_running(1, TIMEOUT).await.unwrap();

    tokio::time::sleep(IDLE_WAIT + Duration::from_secs(5)).await;

    let streams = client.list_streams().await.unwrap();
    assert_eq!(
        streams[0].state,
        StreamStatusState::Running,
        "disable_lazy stream must never go idle"
    );
}

// =======================================================================
// THUMBNAIL CLIENT
// =======================================================================

#[tokio::test]
#[serial_test::serial]
async fn test_thumbnail_warm_returns_200() {
    let (_mcm, client) = setup_fake_rtsp("thumb_warm", "thumb_warm").await;

    let resp = client.thumbnail("ball").await.unwrap();
    assert_eq!(resp.status(), 200, "warm thumbnail should return 200");
}

#[tokio::test]
#[serial_test::serial]
async fn test_thumbnail_cold_start_returns_200() {
    let (_mcm, client) = setup_fake_rtsp("thumb_cold", "thumb_cold").await;

    wait_for_idle(&client).await;

    let resp = client.thumbnail("ball").await.unwrap();
    assert_eq!(resp.status(), 200, "cold-start thumbnail should return 200");

    // Stream should be back to Running after thumbnail wakes it
    let streams = client.list_streams().await.unwrap();
    assert_eq!(streams[0].state, StreamStatusState::Running);
}

#[tokio::test]
#[serial_test::serial]
async fn test_thumbnail_rapid_sequence() {
    let (_mcm, client) = setup_fake_rtsp("thumb_rapid", "thumb_rapid").await;

    wait_for_idle(&client).await;

    // The thumbnail endpoint has a rate limit of 4 req/s per IP.
    // Space requests 350 ms apart to stay safely within the limit.
    for i in 0..5 {
        let resp = client.thumbnail("ball").await.unwrap();
        assert_eq!(
            resp.status(),
            200,
            "thumbnail attempt {i} should return 200"
        );
        tokio::time::sleep(Duration::from_millis(350)).await;
    }
}

#[tokio::test]
#[serial_test::serial]
async fn test_thumbnail_keeps_pipeline_alive_during_cooldown() {
    let (_mcm, client) = setup_fake_rtsp("thumb_cooldown", "thumb_cooldown").await;

    wait_for_idle(&client).await;

    // Request a thumbnail to wake the pipeline
    let resp = client.thumbnail("ball").await.unwrap();
    assert_eq!(resp.status(), 200);

    // Wait less than the thumbnail cooldown (15s) but more than
    // the idle grace period (5s). The pipeline should still be Running
    // because the cooldown prevents re-suspension.
    tokio::time::sleep(IDLE_GRACE + Duration::from_secs(2)).await;

    let streams = client.list_streams().await.unwrap();
    assert_eq!(
        streams[0].state,
        StreamStatusState::Running,
        "thumbnail cooldown should prevent re-suspension"
    );
}

// =======================================================================
// WEBRTC CLIENT
// =======================================================================

#[tokio::test]
#[serial_test::serial]
async fn test_webrtc_cold_session() {
    let (mcm, client) = setup_fake_rtsp("wrtc_cold", "wrtc_cold").await;

    wait_for_idle(&client).await;

    let (bind, mut sink, _stream) = start_webrtc_session(&mcm.signalling_url()).await.unwrap();

    // Stream should be Running now
    let streams = client.list_streams().await.unwrap();
    assert_eq!(
        streams[0].state,
        StreamStatusState::Running,
        "WebRTC session should wake the pipeline"
    );

    end_webrtc_session(&mut sink, &bind).await.unwrap();
}

#[tokio::test]
#[serial_test::serial]
async fn test_webrtc_warm_session() {
    let (mcm, client) = setup_fake_rtsp("wrtc_warm", "wrtc_warm").await;

    let (bind, mut sink, _stream) = start_webrtc_session(&mcm.signalling_url()).await.unwrap();

    let streams = client.list_streams().await.unwrap();
    assert_eq!(streams[0].state, StreamStatusState::Running);

    end_webrtc_session(&mut sink, &bind).await.unwrap();
}

#[tokio::test]
#[serial_test::serial]
async fn test_webrtc_multiple_start_stop() {
    let (mcm, client) = setup_fake_rtsp("wrtc_startstop", "wrtc_startstop").await;

    for _cycle in 0..3 {
        wait_for_idle(&client).await;

        let (bind, mut sink, _stream) = start_webrtc_session(&mcm.signalling_url()).await.unwrap();

        let streams = client.list_streams().await.unwrap();
        assert_eq!(streams[0].state, StreamStatusState::Running);

        end_webrtc_session(&mut sink, &bind).await.unwrap();
    }
}

#[tokio::test]
#[serial_test::serial]
async fn test_webrtc_multiple_clients() {
    let (mcm, client) = setup_fake_rtsp("wrtc_multi", "wrtc_multi").await;

    let mut sessions = Vec::new();
    for _ in 0..3 {
        let (bind, sink, stream) = start_webrtc_session(&mcm.signalling_url()).await.unwrap();
        sessions.push((bind, sink, stream));
    }

    let streams = client.list_streams().await.unwrap();
    assert_eq!(
        streams[0].state,
        StreamStatusState::Running,
        "multiple WebRTC clients should keep pipeline running"
    );

    for (bind, ref mut sink, _) in &mut sessions {
        end_webrtc_session(sink, bind).await.unwrap();
    }
}

#[tokio::test]
#[serial_test::serial]
async fn test_webrtc_disconnect_one_doesnt_affect_others() {
    let (mcm, client) = setup_fake_rtsp("wrtc_indep", "wrtc_indep").await;

    let (bind1, mut sink1, _s1) = start_webrtc_session(&mcm.signalling_url()).await.unwrap();
    let (bind2, mut sink2, _s2) = start_webrtc_session(&mcm.signalling_url()).await.unwrap();

    // Disconnect client 1
    end_webrtc_session(&mut sink1, &bind1).await.unwrap();
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Stream should still be Running because client 2 is active
    let streams = client.list_streams().await.unwrap();
    assert_eq!(
        streams[0].state,
        StreamStatusState::Running,
        "disconnecting one WebRTC client should not affect others"
    );

    end_webrtc_session(&mut sink2, &bind2).await.unwrap();
}

// =======================================================================
// RTSP CLIENT
// =======================================================================

/// Connect an RTSP client using raw TCP and verify the server responds.
/// Returns true if we got a valid RTSP response to OPTIONS.
async fn rtsp_options_ok(rtsp_url: &str) -> bool {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    // Extract host:port from rtsp://host:port/path
    let url: url::Url = rtsp_url.parse().unwrap();
    let host = url.host_str().unwrap_or("127.0.0.1");
    let port = url.port().unwrap_or(8554);
    let addr = format!("{host}:{port}");

    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut stream = loop {
        match tokio::net::TcpStream::connect(&addr).await {
            Ok(s) => break s,
            Err(_) if tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
            Err(_) => return false,
        }
    };

    let request = format!("OPTIONS {rtsp_url} RTSP/1.0\r\nCSeq: 1\r\n\r\n");
    if stream.write_all(request.as_bytes()).await.is_err() {
        return false;
    }

    let mut buf = vec![0u8; 1024];
    match tokio::time::timeout(Duration::from_secs(5), stream.read(&mut buf)).await {
        Ok(Ok(n)) if n > 0 => {
            let resp = String::from_utf8_lossy(&buf[..n]);
            resp.contains("RTSP/1.0")
        }
        _ => false,
    }
}

#[tokio::test]
#[serial_test::serial]
async fn test_rtsp_cold_connection() {
    let (mcm, client) = setup_fake_rtsp("rtsp_cold", "rtsp_cold").await;

    wait_for_idle(&client).await;

    // Connect an RTSP client -- this should wake the pipeline via the RTSP
    // factory's on_client_connected callback.
    assert!(
        rtsp_options_ok(&mcm.rtsp_url("rtsp_cold")).await,
        "RTSP server should accept connections even when pipeline is idle"
    );
}

#[tokio::test]
#[serial_test::serial]
async fn test_rtsp_warm_connection() {
    let (mcm, _client) = setup_fake_rtsp("rtsp_warm", "rtsp_warm").await;

    assert!(
        rtsp_options_ok(&mcm.rtsp_url("rtsp_warm")).await,
        "RTSP server should respond while pipeline is running"
    );
}

// =======================================================================
// MIXED CLIENT SCENARIOS
// =======================================================================

#[tokio::test]
#[serial_test::serial]
async fn test_thumbnail_then_webrtc_cold() {
    let (mcm, client) = setup_fake_rtsp("mixed_tw", "mixed_tw").await;

    wait_for_idle(&client).await;

    // Thumbnail first (wakes the pipeline)
    let resp = client.thumbnail("ball").await.unwrap();
    assert_eq!(resp.status(), 200);

    // Then WebRTC on the same pipeline (should be warm)
    let (bind, mut sink, _s) = start_webrtc_session(&mcm.signalling_url()).await.unwrap();

    let streams = client.list_streams().await.unwrap();
    assert_eq!(streams[0].state, StreamStatusState::Running);

    end_webrtc_session(&mut sink, &bind).await.unwrap();
}

#[tokio::test]
#[serial_test::serial]
async fn test_webrtc_and_thumbnail_concurrent() {
    let (mcm, client) = setup_fake_rtsp("mixed_conc", "mixed_conc").await;

    // Start WebRTC session
    let (bind, mut sink, _s) = start_webrtc_session(&mcm.signalling_url()).await.unwrap();

    // Request thumbnail while WebRTC is active
    let resp = client.thumbnail("ball").await.unwrap();
    assert_eq!(
        resp.status(),
        200,
        "thumbnail should work while WebRTC is active"
    );

    // End WebRTC -- thumbnail cooldown should keep pipeline alive
    end_webrtc_session(&mut sink, &bind).await.unwrap();

    // Pipeline should still be running (thumbnail cooldown)
    tokio::time::sleep(Duration::from_secs(2)).await;
    let streams = client.list_streams().await.unwrap();
    assert_eq!(streams[0].state, StreamStatusState::Running);
}

#[tokio::test]
#[serial_test::serial]
async fn test_webrtc_and_rtsp_independent() {
    let (mcm, client) = setup_fake_rtsp("mixed_indep", "mixed_indep").await;

    // Start both clients
    let (bind, mut sink, _s) = start_webrtc_session(&mcm.signalling_url()).await.unwrap();
    assert!(rtsp_options_ok(&mcm.rtsp_url("mixed_indep")).await);

    // End WebRTC — RTSP should keep the pipeline alive
    end_webrtc_session(&mut sink, &bind).await.unwrap();
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Pipeline should still be running because the RTSP factory keeps
    // clients connected and the consumer_count > 0.
    let streams = client.list_streams().await.unwrap();
    assert_eq!(
        streams[0].state,
        StreamStatusState::Running,
        "RTSP client should keep pipeline running after WebRTC disconnects"
    );
}

// =======================================================================
// LIFECYCLE RECOVERY
// =======================================================================

#[tokio::test]
#[serial_test::serial]
async fn test_idle_running_idle_cycle() {
    let (_mcm, client) = setup_fake_rtsp("idle_cycle", "idle_cycle").await;

    for cycle in 0..3 {
        wait_for_idle(&client).await;

        let resp = client.thumbnail("ball").await.unwrap();
        assert_eq!(resp.status(), 200, "thumbnail should work on cycle {cycle}");

        let streams = client.list_streams().await.unwrap();
        assert_eq!(
            streams[0].state,
            StreamStatusState::Running,
            "stream should be running after thumbnail on cycle {cycle}"
        );
    }
}
