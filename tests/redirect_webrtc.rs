mod common;

use std::time::Duration;

use common::{
    api::{end_webrtc_session, start_webrtc_session_for_producer, McmClient},
    mcm::{allocate_udp_ports, McmProcess},
    types::*,
};

const TIMEOUT: Duration = Duration::from_secs(60);

// -- helpers ------------------------------------------------------------

/// Spawn an external gst-launch-1.0 process that sends H264 RTP packets
/// to the given host:port. This avoids MCM endpoint conflicts and ensures
/// packets arrive on the correct loopback interface.
fn spawn_h264_udp_sender(host: &str, port: u16) -> std::process::Child {
    std::process::Command::new("gst-launch-1.0")
        .args([
            "videotestsrc",
            "is-live=true",
            "pattern=ball",
            "do-timestamp=true",
            "!",
            "video/x-raw,width=160,height=120,framerate=30/1",
            "!",
            "x264enc",
            "tune=zerolatency",
            "speed-preset=ultrafast",
            "bitrate=5000",
            "!",
            "h264parse",
            "config-interval=-1",
            "!",
            "video/x-h264,stream-format=avc,alignment=au",
            "!",
            "rtph264pay",
            "aggregate-mode=zero-latency",
            "config-interval=-1",
            "pt=96",
            "!",
            "udpsink",
            &format!("host={host}"),
            &format!("port={port}"),
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("failed to start gst-launch-1.0 H264 UDP sender")
}

fn spawn_h265_udp_sender(host: &str, port: u16) -> std::process::Child {
    std::process::Command::new("gst-launch-1.0")
        .args([
            "videotestsrc",
            "is-live=true",
            "pattern=ball",
            "do-timestamp=true",
            "!",
            "video/x-raw,width=160,height=120,framerate=30/1,format=I420",
            "!",
            "x265enc",
            "tune=zerolatency",
            "speed-preset=ultrafast",
            "bitrate=5000",
            "!",
            "h265parse",
            "config-interval=-1",
            "!",
            "video/x-h265,stream-format=byte-stream,alignment=au",
            "!",
            "rtph265pay",
            "aggregate-mode=zero-latency",
            "config-interval=-1",
            "pt=96",
            "!",
            "udpsink",
            &format!("host={host}"),
            &format!("port={port}"),
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("failed to start gst-launch-1.0 H265 UDP sender")
}

/// Start MCM with a lazy redirect receiver on the given port, and an
/// external GStreamer sender providing H264 RTP to that port. The redirect
/// is lazy -- the test functions trigger the wake-up chain themselves.
async fn setup_udp_redirect() -> (McmProcess, McmClient, std::process::Child) {
    let udp_port = allocate_udp_ports(1).unwrap()[0];
    let mut sender = spawn_h264_udp_sender("127.0.0.1", udp_port);

    // Give the sender time to start producing frames
    tokio::time::sleep(Duration::from_secs(2)).await;

    let mcm = McmProcess::start().await.unwrap();
    let client = McmClient::new(&mcm.rest_url());

    let redirect = McmClient::build_redirect_udp("redirect_receiver", "127.0.0.1", udp_port);
    client.create_stream(&redirect).await.unwrap_or_else(|e| {
        sender.kill().ok();
        panic!("failed to create redirect stream: {e}");
    });

    (mcm, client, sender)
}

/// Start MCM with a lazy Fake H264 RTSP stream and a lazy Redirect RTSP
/// receiver pointing at the same RTSP path. The fake sender is allowed to
/// go idle first (confirming its RTSP factory is mounted and preserved),
/// then the redirect is created. Neither stream is kept running -- the
/// test functions trigger the wake-up chain themselves.
async fn setup_fake_rtsp_and_redirect(path: &str) -> (McmProcess, McmClient) {
    let mcm = McmProcess::start().await.unwrap();
    let client = McmClient::new(&mcm.rest_url());

    let fake =
        McmClient::build_fake_h264_rtsp("fake_rtsp_sender", 160, 120, 30, path, mcm.rtsp_port);
    client.create_stream(&fake).await.unwrap();

    client
        .wait_for_stream_idle("fake_rtsp_sender", TIMEOUT)
        .await
        .expect("fake RTSP sender should complete initial lifecycle");

    let redirect =
        McmClient::build_redirect_rtsp("redirect_receiver", "127.0.0.1", mcm.rtsp_port, path);
    client.create_stream(&redirect).await.unwrap();

    client
        .wait_for_stream_idle("redirect_receiver", TIMEOUT)
        .await
        .expect("redirect should complete initial lifecycle");

    (mcm, client)
}

async fn setup_h265_udp_redirect() -> (McmProcess, McmClient, std::process::Child) {
    let udp_port = allocate_udp_ports(1).unwrap()[0];
    let mut sender = spawn_h265_udp_sender("127.0.0.1", udp_port);

    tokio::time::sleep(Duration::from_secs(2)).await;

    let mcm = McmProcess::start().await.unwrap();
    let client = McmClient::new(&mcm.rest_url());

    let redirect = McmClient::build_redirect_udp("redirect_receiver", "127.0.0.1", udp_port);
    client.create_stream(&redirect).await.unwrap_or_else(|e| {
        sender.kill().ok();
        panic!("failed to create redirect stream: {e}");
    });

    (mcm, client, sender)
}

async fn setup_fake_h265_rtsp_and_redirect(path: &str) -> (McmProcess, McmClient) {
    let mcm = McmProcess::start().await.unwrap();
    let client = McmClient::new(&mcm.rest_url());

    let fake =
        McmClient::build_fake_h265_rtsp("fake_rtsp_sender", 160, 120, 30, path, mcm.rtsp_port);
    client.create_stream(&fake).await.unwrap();

    client
        .wait_for_stream_idle("fake_rtsp_sender", TIMEOUT)
        .await
        .expect("fake H265 RTSP sender should complete initial lifecycle");

    let redirect =
        McmClient::build_redirect_rtsp("redirect_receiver", "127.0.0.1", mcm.rtsp_port, path);
    client.create_stream(&redirect).await.unwrap();

    client
        .wait_for_stream_idle("redirect_receiver", TIMEOUT)
        .await
        .expect("redirect should complete initial lifecycle");

    (mcm, client)
}

/// Poll `GET /thumbnail?source=...` until it returns 200 with a non-empty
/// body, or time out.
async fn wait_for_thumbnail(client: &McmClient, source: &str, timeout: Duration) -> Vec<u8> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if let Ok(resp) = client.thumbnail(source).await {
            if resp.status().is_success() {
                let bytes = resp.bytes().await.unwrap_or_default();
                if !bytes.is_empty() {
                    return bytes.to_vec();
                }
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "thumbnail for {source:?} not available after {}s",
            timeout.as_secs()
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

// =======================================================================
// UDP REDIRECT PIPELINE + WebRTC
// =======================================================================

/// The redirect stream must appear in the signalling server's available
/// streams list and a WebRTC session must be creatable against it.
/// The server must send an SDP offer containing an H264 media line.
#[tokio::test]

async fn test_redirect_webrtc_session_and_sdp_offer() {
    let (mcm, _client, mut sender) = setup_udp_redirect().await;

    // Start a WebRTC session targeting the redirect producer.
    // The redirect's encode takes time to resolve (probe + brute-force),
    // so poll until it appears in the signalling server's available list.
    let (bind, available, mut ws_sink, mut ws_stream) =
        start_webrtc_session_for_producer(&mcm.signalling_url(), "redirect_receiver", TIMEOUT)
            .await
            .expect("should start WebRTC session on redirect producer");

    // The redirect stream must be in the available list
    assert!(
        available
            .iter()
            .any(|s| s.name.contains("redirect_receiver")),
        "redirect stream should be in available streams: {available:?}"
    );

    // The bind answer must reference the same producer
    assert_eq!(
        available
            .iter()
            .find(|s| s.name.contains("redirect_receiver"))
            .unwrap()
            .id,
        bind.producer_id,
        "bind producer_id must match the redirect stream"
    );

    // Wait for an SDP offer (Negotiation message) from the server.
    // The server sends the offer asynchronously after session creation.
    use futures::StreamExt;
    let sdp_offer = tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(Ok(msg)) = ws_stream.next().await {
            let text = match msg.into_text() {
                Ok(t) => t,
                Err(_) => continue,
            };
            let proto: Result<SignallingProtocol, _> = serde_json::from_str(&text);
            let Ok(proto) = proto else { continue };
            if let SignallingMessage::Negotiation(ref val) = proto.message {
                return Some(val.clone());
            }
        }
        None
    })
    .await;

    let sdp_json = sdp_offer
        .expect("should receive negotiation message within 10s")
        .expect("ws stream should not close before SDP offer");

    // The negotiation message should contain an SDP offer with H264
    let sdp_text = sdp_json
        .pointer("/content/sdp/sdp")
        .and_then(|v| v.as_str())
        .expect("negotiation message should contain /content/sdp/sdp field");

    assert!(
        sdp_text.contains("H264") || sdp_text.contains("h264"),
        "SDP offer should mention H264, got:\n{sdp_text}"
    );

    // Clean up
    let _ = end_webrtc_session(&mut ws_sink, &bind).await;
    sender.kill().ok();
}

// =======================================================================
// UDP REDIRECT PIPELINE + Thumbnails
// =======================================================================

#[tokio::test]

async fn test_udp_redirect_thumbnail() {
    let (_mcm, client, mut sender) = setup_udp_redirect().await;

    let body = wait_for_thumbnail(&client, "Redirect", TIMEOUT).await;
    assert!(
        body.len() > 100,
        "thumbnail body too small ({} bytes), expected a JPEG image",
        body.len()
    );
    sender.kill().ok();
}

// =======================================================================
// RTSP REDIRECT PIPELINE + WebRTC
// =======================================================================

#[tokio::test]

async fn test_rtsp_redirect_webrtc_session_and_sdp_offer() {
    let (mcm, _client) = setup_fake_rtsp_and_redirect("test_redir").await;

    let (bind, available, mut ws_sink, mut ws_stream) =
        start_webrtc_session_for_producer(&mcm.signalling_url(), "redirect_receiver", TIMEOUT)
            .await
            .expect("should start WebRTC session on RTSP redirect producer");

    assert!(
        available
            .iter()
            .any(|s| s.name.contains("redirect_receiver")),
        "RTSP redirect stream should be in available streams: {available:?}"
    );

    assert_eq!(
        available
            .iter()
            .find(|s| s.name.contains("redirect_receiver"))
            .unwrap()
            .id,
        bind.producer_id,
        "bind producer_id must match the RTSP redirect stream"
    );

    use futures::StreamExt;
    let sdp_offer = tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(Ok(msg)) = ws_stream.next().await {
            let text = match msg.into_text() {
                Ok(t) => t,
                Err(_) => continue,
            };
            let proto: Result<SignallingProtocol, _> = serde_json::from_str(&text);
            let Ok(proto) = proto else { continue };
            if let SignallingMessage::Negotiation(ref val) = proto.message {
                return Some(val.clone());
            }
        }
        None
    })
    .await;

    let sdp_json = sdp_offer
        .expect("should receive negotiation message within 10s")
        .expect("ws stream should not close before SDP offer");

    let sdp_text = sdp_json
        .pointer("/content/sdp/sdp")
        .and_then(|v| v.as_str())
        .expect("negotiation message should contain /content/sdp/sdp field");

    assert!(
        sdp_text.contains("H264") || sdp_text.contains("h264"),
        "SDP offer should mention H264, got:\n{sdp_text}"
    );

    let _ = end_webrtc_session(&mut ws_sink, &bind).await;
}

// =======================================================================
// RTSP REDIRECT PIPELINE + Thumbnails
// =======================================================================

#[tokio::test]

async fn test_rtsp_redirect_thumbnail() {
    let (_mcm, client) = setup_fake_rtsp_and_redirect("test_redir_thumb").await;

    let body = wait_for_thumbnail(&client, "Redirect", TIMEOUT).await;
    assert!(
        body.len() > 100,
        "thumbnail body too small ({} bytes), expected a JPEG image",
        body.len()
    );
}

// =======================================================================
// H265 UDP REDIRECT PIPELINE + WebRTC
// =======================================================================

#[tokio::test]

async fn test_h265_redirect_webrtc_session_and_sdp_offer() {
    let (mcm, _client, mut sender) = setup_h265_udp_redirect().await;

    let (bind, available, mut ws_sink, mut ws_stream) =
        start_webrtc_session_for_producer(&mcm.signalling_url(), "redirect_receiver", TIMEOUT)
            .await
            .expect("should start WebRTC session on H265 redirect producer");

    assert!(
        available
            .iter()
            .any(|s| s.name.contains("redirect_receiver")),
        "H265 redirect stream should be in available streams: {available:?}"
    );

    assert_eq!(
        available
            .iter()
            .find(|s| s.name.contains("redirect_receiver"))
            .unwrap()
            .id,
        bind.producer_id,
        "bind producer_id must match the H265 redirect stream"
    );

    use futures::StreamExt;
    let sdp_offer = tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(Ok(msg)) = ws_stream.next().await {
            let text = match msg.into_text() {
                Ok(t) => t,
                Err(_) => continue,
            };
            let proto: Result<SignallingProtocol, _> = serde_json::from_str(&text);
            let Ok(proto) = proto else { continue };
            if let SignallingMessage::Negotiation(ref val) = proto.message {
                return Some(val.clone());
            }
        }
        None
    })
    .await;

    let sdp_json = sdp_offer
        .expect("should receive negotiation message within 10s")
        .expect("ws stream should not close before SDP offer");

    let sdp_text = sdp_json
        .pointer("/content/sdp/sdp")
        .and_then(|v| v.as_str())
        .expect("negotiation message should contain /content/sdp/sdp field");

    assert!(
        sdp_text.contains("H265") || sdp_text.contains("h265"),
        "SDP offer should mention H265, got:\n{sdp_text}"
    );

    let _ = end_webrtc_session(&mut ws_sink, &bind).await;
    sender.kill().ok();
}

// =======================================================================
// H265 UDP REDIRECT PIPELINE + Thumbnails
// =======================================================================

#[tokio::test]

async fn test_h265_udp_redirect_thumbnail() {
    let (_mcm, client, mut sender) = setup_h265_udp_redirect().await;

    let body = wait_for_thumbnail(&client, "Redirect", TIMEOUT).await;
    assert!(
        body.len() > 100,
        "thumbnail body too small ({} bytes), expected a JPEG image",
        body.len()
    );
    sender.kill().ok();
}

// =======================================================================
// H265 RTSP REDIRECT PIPELINE + WebRTC
// =======================================================================

#[tokio::test]

async fn test_h265_rtsp_redirect_webrtc_session_and_sdp_offer() {
    let (mcm, _client) = setup_fake_h265_rtsp_and_redirect("test_h265_redir").await;

    let (bind, available, mut ws_sink, mut ws_stream) =
        start_webrtc_session_for_producer(&mcm.signalling_url(), "redirect_receiver", TIMEOUT)
            .await
            .expect("should start WebRTC session on H265 RTSP redirect producer");

    assert!(
        available
            .iter()
            .any(|s| s.name.contains("redirect_receiver")),
        "H265 RTSP redirect stream should be in available streams: {available:?}"
    );

    assert_eq!(
        available
            .iter()
            .find(|s| s.name.contains("redirect_receiver"))
            .unwrap()
            .id,
        bind.producer_id,
        "bind producer_id must match the H265 RTSP redirect stream"
    );

    use futures::StreamExt;
    let sdp_offer = tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(Ok(msg)) = ws_stream.next().await {
            let text = match msg.into_text() {
                Ok(t) => t,
                Err(_) => continue,
            };
            let proto: Result<SignallingProtocol, _> = serde_json::from_str(&text);
            let Ok(proto) = proto else { continue };
            if let SignallingMessage::Negotiation(ref val) = proto.message {
                return Some(val.clone());
            }
        }
        None
    })
    .await;

    let sdp_json = sdp_offer
        .expect("should receive negotiation message within 10s")
        .expect("ws stream should not close before SDP offer");

    let sdp_text = sdp_json
        .pointer("/content/sdp/sdp")
        .and_then(|v| v.as_str())
        .expect("negotiation message should contain /content/sdp/sdp field");

    assert!(
        sdp_text.contains("H265") || sdp_text.contains("h265"),
        "SDP offer should mention H265, got:\n{sdp_text}"
    );

    let _ = end_webrtc_session(&mut ws_sink, &bind).await;
}

// =======================================================================
// H265 RTSP REDIRECT PIPELINE + Thumbnails
// =======================================================================

#[tokio::test]

async fn test_h265_rtsp_redirect_thumbnail() {
    let (_mcm, client) = setup_fake_h265_rtsp_and_redirect("test_h265_redir_thumb").await;

    let body = wait_for_thumbnail(&client, "Redirect", TIMEOUT).await;
    assert!(
        body.len() > 100,
        "thumbnail body too small ({} bytes), expected a JPEG image",
        body.len()
    );
}
