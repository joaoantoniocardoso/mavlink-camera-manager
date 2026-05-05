use super::*;

/// gst-launch-1.0 sender that forces the given H.264 profile on the wire
/// via a `video/x-h264,profile=<profile>` caps filter after `x264enc`.
/// This is the shape that triggered the black-screen bug in the JM log:
/// the tee sink pad caps carry the real profile-level-id, and any
/// downstream rewrite must preserve those bytes.
fn spawn_h264_profile_udp_sender(profile: &str, host: &str, port: u16) -> std::process::Child {
    let profile_caps = format!("video/x-h264,profile=(string){profile}");
    let host_arg = format!("host={host}");
    let port_arg = format!("port={port}");

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
            profile_caps.as_str(),
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
            host_arg.as_str(),
            port_arg.as_str(),
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("failed to start gst-launch-1.0 H264 UDP sender")
}

/// Spin up MCM, wire an external H.264 UDP sender through a redirect,
/// open a WebRTC session, read the server-generated SDP offer and run
/// `assert_plid` on the `profile-level-id` observed in `a=fmtp:96`.
///
/// Shared across every H.264 profile variant so each entry point boils
/// down to "what did x264enc emit, what should the wire SDP say".
async fn run_webrtc_offer_h264_profile_check(
    profile: &str,
    producer_name: &str,
    assert_plid: impl Fn(&str, &str),
) {
    let udp_port = allocate_udp_ports(1).unwrap()[0];
    let mut sender = spawn_h264_profile_udp_sender(profile, "127.0.0.1", udp_port);

    // Give the sender time to start producing frames so the tee caps
    // carry the real profile by the time the redirect is created.
    tokio::time::sleep(Duration::from_secs(2)).await;

    let mcm = McmProcess::start().await.unwrap();
    let client = McmClient::new(&mcm.rest_url());

    let redirect = McmClient::build_redirect_udp(producer_name, "127.0.0.1", udp_port);
    client.create_stream(&redirect).await.unwrap_or_else(|e| {
        sender.kill().ok();
        panic!("failed to create redirect stream: {e}");
    });

    let (bind, available, mut ws_sink, mut ws_stream) =
        start_webrtc_session_for_producer(&mcm.signalling_url(), producer_name, TIMEOUT)
            .await
            .unwrap_or_else(|e| {
                sender.kill().ok();
                panic!("should start WebRTC session on {profile}-profile redirect: {e}");
            });

    assert!(
        available.iter().any(|s| s.name.contains(producer_name)),
        "redirect stream should be in available streams: {available:?}"
    );

    assert_eq!(
        available
            .iter()
            .find(|s| s.name.contains(producer_name))
            .unwrap()
            .id,
        bind.producer_id,
        "bind producer_id must match the redirect stream"
    );

    let sdp_text = read_offer_sdp_text(&mut ws_stream).await;

    let fmtp_line = sdp_text
        .lines()
        .find(|l| l.starts_with("a=fmtp:96"))
        .unwrap_or_else(|| panic!("offer must have fmtp for pt 96, SDP:\n{sdp_text}"));

    let plid = fmtp_line
        .split(';')
        .find_map(|kv| kv.trim().strip_prefix("profile-level-id="))
        .unwrap_or_else(|| panic!("fmtp must carry profile-level-id, got: {fmtp_line}"));

    assert_plid(plid, &sdp_text);

    let _ = end_webrtc_session(&mut ws_sink, &bind).await;
    sender.kill().ok();
}

/// End-to-end assertion for the JM black-screen regression on H.264 Main
/// profile: the wire SDP offer must carry a `profile-level-id` that
/// reflects the upstream Main profile (`4d..`), not the Constrained
/// Baseline default that `customize_sent_sdp` currently hardcodes.
///
/// Fails on current master (`profile-level-id=42e0..`) and passes once
/// both fix sites in the analysis plan stop overwriting the profile.
#[tokio::test]
async fn test_webrtc_offer_preserves_h264_main_profile() {
    run_webrtc_offer_h264_profile_check("main", "redirect_h264_main", |plid, sdp| {
        assert!(
            plid.starts_with("4d"),
            "expected Main (4d..) profile-level-id from x264enc profile=main, got {plid:?} in SDP:\n{sdp}",
        );
    })
    .await;
}

/// Same regression expressed against H.264 High profile: x264enc emits a
/// `64..` profile-level-id and the wire SDP offer must carry it through.
///
/// Fails on current master (the rewrite forces `42e0..`) and passes once
/// both fix sites stop overwriting the profile.
#[tokio::test]
async fn test_webrtc_offer_preserves_h264_high_profile() {
    run_webrtc_offer_h264_profile_check("high", "redirect_h264_high", |plid, sdp| {
        assert!(
            plid.starts_with("64"),
            "expected High (64..) profile-level-id from x264enc profile=high, got {plid:?} in SDP:\n{sdp}",
        );
    })
    .await;
}
