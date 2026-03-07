mod common;

use std::time::Duration;

use common::api::McmClient;
use common::mcm::McmProcess;
use common::monitor::ProcMonitor;
use common::types::ExtendedConfiguration;

const TIMEOUT: Duration = Duration::from_secs(60);

/// Stress test: create a 1080p stream with disable_lazy so it immediately starts
/// encoding, and verify MCM stays healthy for 30 seconds.
#[tokio::test]
#[serial_test::serial]
async fn test_high_res_stream_stability() {
    let mcm = McmProcess::start().await.unwrap();
    let client = McmClient::new(&mcm.rest_url());
    let mut monitor = ProcMonitor::start(mcm.pid(), Duration::from_secs(2));

    let post = McmClient::build_fake_h264_rtsp_ext(
        "hires",
        1920,
        1080,
        30,
        "hires",
        ExtendedConfiguration {
            disable_lazy: true,
            ..Default::default()
        },
    );
    client.create_stream(&post).await.unwrap();
    client.wait_for_streams_running(1, TIMEOUT).await.unwrap();

    tokio::time::sleep(Duration::from_secs(30)).await;

    let streams = client.list_streams().await.unwrap();
    assert_eq!(streams.len(), 1);
    assert!(
        streams[0].running,
        "stream should still be running after 30s"
    );

    let samples = monitor.stop_and_collect();
    let rss_growth = samples
        .first()
        .zip(samples.last())
        .map(|(f, l)| l.rss_mb - f.rss_mb)
        .unwrap_or(0.0);
    eprintln!(
        "High-res 30s: RSS growth={rss_growth:.1} MB, samples={}",
        samples.len()
    );
}

/// Stress test: verify MCM's REST API stays responsive under a running stream.
#[tokio::test]
#[serial_test::serial]
async fn test_api_responsive_under_load() {
    let mcm = McmProcess::start().await.unwrap();
    let client = McmClient::new(&mcm.rest_url());

    let post = McmClient::build_fake_h264_rtsp("load", 640, 480, 30, "load");
    client.create_stream(&post).await.unwrap();
    client.wait_for_streams_running(1, TIMEOUT).await.unwrap();

    // Hammer the API with 50 rapid requests while the stream is running
    for _ in 0..50 {
        let info = client.info().await.unwrap();
        assert!(!info.name.is_empty());
        let streams = client.list_streams().await.unwrap();
        assert_eq!(streams.len(), 1);
    }
}

/// Stress test: rapid create/delete cycles to detect resource leaks.
#[tokio::test]
#[serial_test::serial]
async fn test_rapid_stream_churn() {
    let mcm = McmProcess::start().await.unwrap();
    let client = McmClient::new(&mcm.rest_url());
    let mut monitor = ProcMonitor::start(mcm.pid(), Duration::from_millis(500));

    for i in 0..10 {
        let name = format!("churn_{i}");
        let post = McmClient::build_fake_h264_rtsp(&name, 640, 480, 30, &name);
        client.create_stream(&post).await.unwrap();
        client.wait_for_streams_running(1, TIMEOUT).await.unwrap();
        client.delete_stream(&name).await.unwrap();
        tokio::time::sleep(Duration::from_secs(2)).await;
    }

    let res = monitor.stop_and_collect();
    let rss_growth = res
        .first()
        .zip(res.last())
        .map(|(f, l)| l.rss_mb - f.rss_mb)
        .unwrap_or(0.0);
    eprintln!("Churn 10 cycles: RSS growth={rss_growth:.1} MB");
    assert!(
        rss_growth < 200.0,
        "RSS grew by {rss_growth:.1} MB (limit 200 MB)"
    );

    let _ = client.list_streams().await.unwrap();
}

/// Stress test: sustained stream with periodic health checks.
#[tokio::test]
#[serial_test::serial]
async fn test_sustained_load() {
    let mcm = McmProcess::start().await.unwrap();
    let client = McmClient::new(&mcm.rest_url());
    let mut monitor = ProcMonitor::start(mcm.pid(), Duration::from_secs(2));

    let post = McmClient::build_fake_h264_rtsp_ext(
        "sustained",
        640,
        480,
        30,
        "sustained",
        ExtendedConfiguration {
            disable_lazy: true,
            ..Default::default()
        },
    );
    client.create_stream(&post).await.unwrap();
    client.wait_for_streams_running(1, TIMEOUT).await.unwrap();

    // Periodically check health over 30 seconds
    for _ in 0..6 {
        tokio::time::sleep(Duration::from_secs(5)).await;
        let streams = client.list_streams().await.unwrap();
        assert_eq!(streams.len(), 1, "stream disappeared during sustained run");
        assert!(
            streams[0].running,
            "stream stopped running during sustained period"
        );
    }

    let samples = monitor.stop_and_collect();
    let rss_growth = samples
        .first()
        .zip(samples.last())
        .map(|(f, l)| l.rss_mb - f.rss_mb)
        .unwrap_or(0.0);
    eprintln!(
        "Sustained 30s: RSS growth={rss_growth:.1} MB, samples={}",
        samples.len()
    );
    assert!(
        rss_growth < 200.0,
        "RSS grew by {rss_growth:.1} MB (limit 200 MB)"
    );
}
