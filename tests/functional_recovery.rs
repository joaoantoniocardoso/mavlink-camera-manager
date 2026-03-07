mod common;

use std::time::Duration;

use common::api::McmClient;
use common::mcm::McmProcess;
use common::monitor::ProcMonitor;
use common::types::CaptureConfiguration;

const TIMEOUT: Duration = Duration::from_secs(30);

#[tokio::test]
#[serial_test::serial]
async fn test_restart_streams() {
    let mcm = McmProcess::start().await.unwrap();
    let client = McmClient::new(&mcm.rest_url());

    let post = McmClient::build_fake_h264_rtsp("restart_a", 640, 480, 30, "restart_a");
    client.create_stream(&post).await.unwrap();
    client.wait_for_streams_running(1, TIMEOUT).await.unwrap();

    client.restart_streams().await.unwrap();
    let streams = client.wait_for_streams_running(1, TIMEOUT).await.unwrap();
    assert_eq!(streams.len(), 1);
    assert!(streams[0].running);
}

#[tokio::test]
#[serial_test::serial]
async fn test_reset_settings() {
    let mcm = McmProcess::start().await.unwrap();
    let client = McmClient::new(&mcm.rest_url());

    let post = McmClient::build_fake_h264_rtsp("reset_stream", 640, 480, 30, "reset_stream");
    client.create_stream(&post).await.unwrap();
    client.wait_for_streams_running(1, TIMEOUT).await.unwrap();

    client.reset_settings().await.unwrap();
    let streams = client.list_streams().await.unwrap();
    assert!(streams.is_empty());
}

#[tokio::test]
#[serial_test::serial]
async fn test_block_source() {
    let mcm = McmProcess::start().await.unwrap();
    let client = McmClient::new(&mcm.rest_url());

    let post = McmClient::build_fake_h264_rtsp("block_stream", 640, 480, 30, "block_stream");
    client.create_stream(&post).await.unwrap();
    client.wait_for_streams_running(1, TIMEOUT).await.unwrap();

    client.block_source("ball").await.unwrap();
    let streams = client.list_streams().await.unwrap();
    assert!(streams.is_empty());

    let blocked = client.blocked_sources().await.unwrap();
    assert!(blocked.contains(&"ball".to_string()));
}

#[tokio::test]
#[serial_test::serial]
async fn test_unblock_source() {
    let mcm = McmProcess::start().await.unwrap();
    let client = McmClient::new(&mcm.rest_url());

    client.block_source("ball").await.unwrap();
    let blocked = client.blocked_sources().await.unwrap();
    assert!(blocked.contains(&"ball".to_string()));

    client.unblock_source("ball").await.unwrap();
    let blocked = client.blocked_sources().await.unwrap();
    assert!(blocked.is_empty());

    let sources = client.sources().await.unwrap();
    assert!(sources.iter().any(|s| s.source == "ball"));
}

#[tokio::test]
#[serial_test::serial]
async fn test_rapid_create_delete_no_leak() {
    let mcm = McmProcess::start().await.unwrap();
    let client = McmClient::new(&mcm.rest_url());

    let mut monitor = ProcMonitor::start(mcm.pid(), Duration::from_millis(200));

    for i in 0..10 {
        let name = format!("leak_test_{i}");
        let path = format!("leak_test_{i}");
        let post = McmClient::build_fake_h264_rtsp(&name, 640, 480, 30, &path);
        client.create_stream(&post).await.unwrap();
        client.wait_for_streams_running(1, TIMEOUT).await.unwrap();
        client.delete_stream(&name).await.unwrap();
        // Wait for MCM to finish pipeline teardown before recreating
        // (same pipeline_id is reused since all use the "ball" source)
        tokio::time::sleep(Duration::from_secs(2)).await;
    }

    let samples = monitor.stop_and_collect();
    assert!(
        samples.len() >= 2,
        "need at least 2 samples, got {}",
        samples.len()
    );
    let first_rss = samples.first().unwrap().rss_mb;
    let last_rss = samples.last().unwrap().rss_mb;
    let growth = last_rss - first_rss;
    eprintln!(
        "RSS monitoring: first={:.1} MB, last={:.1} MB, growth={:.1} MB",
        first_rss, last_rss, growth
    );
    // Each create/delete cycle involves x264enc init, RTSP server mount/unmount,
    // and GStreamer pipeline teardown. Some memory overhead is expected.
    // Threshold detects catastrophic leaks, not marginal growth.
    assert!(
        growth < 200.0,
        "RSS growth {:.1} MB exceeds 200 MB threshold (first: {:.1}, last: {:.1})",
        growth,
        first_rss,
        last_rss
    );
}

#[tokio::test]
#[serial_test::serial]
async fn test_restart_preserves_stream_config() {
    let mcm = McmProcess::start().await.unwrap();
    let client = McmClient::new(&mcm.rest_url());

    let width = 640u32;
    let height = 480u32;
    let fps = 15u32;
    let post =
        McmClient::build_fake_h264_rtsp("config_preserve", width, height, fps, "config_preserve");
    client.create_stream(&post).await.unwrap();
    client.wait_for_streams_running(1, TIMEOUT).await.unwrap();

    client.restart_streams().await.unwrap();
    let streams = client.wait_for_streams_running(1, TIMEOUT).await.unwrap();
    assert_eq!(streams.len(), 1);

    let stream = &streams[0].video_and_stream.stream_information;
    let config = match &stream.configuration {
        CaptureConfiguration::Video(v) => v,
        _ => panic!("expected Video configuration"),
    };
    assert_eq!(config.width, width);
    assert_eq!(config.height, height);
    assert_eq!(config.frame_interval.numerator, 1);
    assert_eq!(config.frame_interval.denominator, fps);
}
