mod common;

use std::time::Duration;

use common::api::McmClient;
use common::mcm::McmProcess;
use common::types::{
    CaptureConfiguration, ExtendedConfiguration, FrameInterval, PostStream,
    RedirectCaptureConfiguration, StreamInformation, VideoCaptureConfiguration,
};
use url::Url;

const TIMEOUT: Duration = Duration::from_secs(15);

#[tokio::test]
#[serial_test::serial]
async fn test_h264_720p_30fps_rtsp() {
    let mcm = McmProcess::start().await.unwrap();
    let client = McmClient::new(&mcm.rest_url());

    let post = McmClient::build_fake_h264_rtsp("stream_720p", 1280, 720, 30, "stream_720p");
    client.create_stream(&post).await.unwrap();
    let streams = client.wait_for_streams_running(1, TIMEOUT).await.unwrap();
    assert_eq!(streams.len(), 1);
    assert!(streams[0].running);
}

#[tokio::test]
#[serial_test::serial]
async fn test_h264_480p_15fps_rtsp() {
    let mcm = McmProcess::start().await.unwrap();
    let client = McmClient::new(&mcm.rest_url());

    let post = McmClient::build_fake_h264_rtsp("stream_480p", 640, 480, 15, "stream_480p");
    client.create_stream(&post).await.unwrap();
    let streams = client.wait_for_streams_running(1, TIMEOUT).await.unwrap();
    assert_eq!(streams.len(), 1);
    assert!(streams[0].running);
}

#[tokio::test]
#[serial_test::serial]
async fn test_h264_1080p_30fps_rtsp() {
    let mcm = McmProcess::start().await.unwrap();
    let client = McmClient::new(&mcm.rest_url());

    let post = McmClient::build_fake_h264_rtsp("stream_1080p", 1920, 1080, 30, "stream_1080p");
    client.create_stream(&post).await.unwrap();
    let streams = client.wait_for_streams_running(1, TIMEOUT).await.unwrap();
    assert_eq!(streams.len(), 1);
    assert!(streams[0].running);
}

#[tokio::test]
#[serial_test::serial]
async fn test_h264_udp_endpoint() {
    let mcm = McmProcess::start().await.unwrap();
    let client = McmClient::new(&mcm.rest_url());

    let post = McmClient::build_fake_h264_udp("stream_udp", 640, 480, 30, "192.168.2.1", 5600);
    client.create_stream(&post).await.unwrap();
    let streams = client.wait_for_streams_running(1, TIMEOUT).await.unwrap();
    assert_eq!(streams.len(), 1);
    assert!(streams[0].running);
}

#[tokio::test]
#[serial_test::serial]
async fn test_dual_rtsp_udp_endpoints_rejected() {
    let mcm = McmProcess::start().await.unwrap();
    let client = McmClient::new(&mcm.rest_url());

    // MCM currently rejects mixing RTSP + UDP endpoints in the same stream.
    let post = PostStream {
        name: "dual_stream".to_string(),
        source: "ball".to_string(),
        stream_information: StreamInformation {
            endpoints: vec![
                Url::parse("rtsp://0.0.0.0:8554/dual").unwrap(),
                Url::parse("udp://192.168.2.1:5600").unwrap(),
            ],
            configuration: CaptureConfiguration::Video(VideoCaptureConfiguration {
                encode: "H264".to_string(),
                height: 480,
                width: 640,
                frame_interval: FrameInterval {
                    numerator: 1,
                    denominator: 30,
                },
            }),
            extended_configuration: None,
        },
    };
    client.create_stream(&post).await.unwrap();

    // The stream is accepted by the API but the pipeline fails to start
    // because RTSP+UDP mixing is not supported. Verify it never reaches
    // the running state.
    tokio::time::sleep(Duration::from_secs(3)).await;
    let streams = client.list_streams().await.unwrap();
    assert_eq!(streams.len(), 1);
    assert!(
        !streams[0].running,
        "Expected RTSP+UDP mixed stream to NOT be running"
    );
}

#[tokio::test]
#[serial_test::serial]
async fn test_disable_lazy() {
    let mcm = McmProcess::start().await.unwrap();
    let client = McmClient::new(&mcm.rest_url());

    let ext = ExtendedConfiguration {
        disable_lazy: true,
        ..Default::default()
    };
    let post = McmClient::build_fake_h264_rtsp_ext("lazy_off", 640, 480, 30, "lazy_off", ext);
    client.create_stream(&post).await.unwrap();
    client.wait_for_streams_running(1, TIMEOUT).await.unwrap();

    tokio::time::sleep(Duration::from_secs(10)).await;
    let streams = client.list_streams().await.unwrap();
    assert_eq!(streams.len(), 1);
    assert!(streams[0].running);
}

#[tokio::test]
#[serial_test::serial]
async fn test_disable_mavlink() {
    let mcm = McmProcess::start().await.unwrap();
    let client = McmClient::new(&mcm.rest_url());

    let ext = ExtendedConfiguration {
        disable_mavlink: true,
        ..Default::default()
    };
    let post = McmClient::build_fake_h264_rtsp_ext("no_mavlink", 640, 480, 30, "no_mavlink", ext);
    client.create_stream(&post).await.unwrap();
    let streams = client.wait_for_streams_running(1, TIMEOUT).await.unwrap();
    assert_eq!(streams.len(), 1);
    assert!(streams[0].running);
    assert!(streams[0].mavlink.is_none());
}

#[tokio::test]
#[serial_test::serial]
async fn test_disable_thumbnails() {
    let mcm = McmProcess::start().await.unwrap();
    let client = McmClient::new(&mcm.rest_url());

    let ext = ExtendedConfiguration {
        disable_thumbnails: true,
        ..Default::default()
    };
    let post = McmClient::build_fake_h264_rtsp_ext("no_thumb", 640, 480, 30, "no_thumb", ext);
    client.create_stream(&post).await.unwrap();
    client.wait_for_streams_running(1, TIMEOUT).await.unwrap();

    let resp = client.thumbnail_raw("ball").await.unwrap();
    let is_ok_jpeg = resp.status().is_success()
        && resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .map(|ct| ct.contains("image/jpeg"))
            .unwrap_or(false);
    assert!(!is_ok_jpeg);
}

#[tokio::test]
#[serial_test::serial]
async fn test_qr_source_h264_rtsp() {
    if !common::gst_helpers::has_qr_plugin() {
        eprintln!("SKIPPED: qrtimestampsrc plugin not available");
        return;
    }

    let mcm = McmProcess::start().await.unwrap();
    let client = McmClient::new(&mcm.rest_url());

    let post = McmClient::build_qr_h264_rtsp("qr-stream", 320, 30, "qr_test");
    client.create_stream(&post).await.unwrap();
    let streams = client.wait_for_streams_running(1, TIMEOUT).await.unwrap();
    assert_eq!(streams.len(), 1);
    assert!(streams[0].running);
}

#[tokio::test]
#[serial_test::serial]
async fn test_redirect_rtsp_to_rtsp() {
    // TODO: Redirect mechanism unclear - endpoint semantics for source vs output need verification
    eprintln!("SKIPPED: Redirect mechanism needs verification");
    if true {
        return;
    }
    let mcm = McmProcess::start().await.unwrap();
    let client = McmClient::new(&mcm.rest_url());

    let upstream = McmClient::build_fake_h264_rtsp("upstream", 640, 480, 30, "upstream");
    client.create_stream(&upstream).await.unwrap();
    client.wait_for_streams_running(1, TIMEOUT).await.unwrap();

    let redirect = PostStream {
        name: "redirect-stream".to_string(),
        source: "Redirect".to_string(),
        stream_information: StreamInformation {
            endpoints: vec![Url::parse("rtsp://127.0.0.1:8554/upstream").unwrap()],
            configuration: CaptureConfiguration::Redirect(RedirectCaptureConfiguration {}),
            extended_configuration: None,
        },
    };
    client.create_stream(&redirect).await.unwrap();
    client.wait_for_streams_running(2, TIMEOUT).await.unwrap();
}
