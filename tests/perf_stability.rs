mod common;

use std::time::Duration;

use common::api::McmClient;
use common::gst_helpers::{self, SubprocessQrProbe};
use common::mcm::McmProcess;
use common::monitor::{self, ProcMonitor};

const TIMEOUT: Duration = Duration::from_secs(60);
const RTSP_READY_TIMEOUT: Duration = Duration::from_secs(10);

macro_rules! require_qr {
    () => {
        if !gst_helpers::has_qr_plugin() {
            eprintln!("SKIPPED: qrtimestampsrc plugin not available");
            return;
        }
    };
}

fn gst_plugin_path() -> String {
    gst_helpers::qrtimestamp_gst_plugin_path()
}

/// Long-running stability: one QR stream with latency monitoring for 10 minutes.
/// Asserts RSS doesn't grow continuously and thread count stays stable.
#[tokio::test]
#[serial_test::serial]
async fn test_long_run_10min() {
    require_qr!();

    let mcm = McmProcess::start().await.unwrap();
    let client = McmClient::new(&mcm.rest_url());

    let post = McmClient::build_qr_h264_rtsp("long_a", 320, 30, "long_a");
    client.create_stream(&post).await.unwrap();
    client.wait_for_streams_running(1, TIMEOUT).await.unwrap();
    mcm.wait_rtsp_ready(RTSP_READY_TIMEOUT).await.unwrap();

    let mut monitor = ProcMonitor::start(mcm.pid(), Duration::from_secs(1));
    let probe = SubprocessQrProbe::spawn(
        &mcm.rtsp_url("long_a"),
        Duration::from_secs(600),
        &gst_plugin_path(),
    )
    .unwrap();

    let samples = probe.collect().unwrap();
    let res = monitor.stop_and_collect();

    let slope = monitor::rss_trend_slope(&res);
    assert!(slope < 0.05, "RSS slope {slope:.4} MB/s exceeds 0.05 MB/s");

    assert!(
        monitor::thread_count_stable(&res, 5),
        "thread count not stable"
    );

    assert!(!samples.is_empty(), "probe received no frames");

    let stats = gst_helpers::compute_stats(&samples, 30.0);
    eprintln!("  probe: {stats}");
    if let (Some(first), Some(last)) = (res.first(), res.last()) {
        eprintln!(
            "  RSS: {:.1} -> {:.1} MB, threads: {} -> {}",
            first.rss_mb, last.rss_mb, first.threads, last.threads
        );
    }
}

/// Reconnect stability: repeatedly connect/disconnect an RTSP probe to verify
/// MCM doesn't leak resources across client reconnections.
#[tokio::test]
#[serial_test::serial]
async fn test_reconnect_stability() {
    require_qr!();

    let mcm = McmProcess::start().await.unwrap();
    let client = McmClient::new(&mcm.rest_url());

    let post = McmClient::build_qr_h264_rtsp("reconnect", 320, 30, "reconnect");
    client.create_stream(&post).await.unwrap();
    client.wait_for_streams_running(1, TIMEOUT).await.unwrap();
    mcm.wait_rtsp_ready(RTSP_READY_TIMEOUT).await.unwrap();

    let mut monitor = ProcMonitor::start(mcm.pid(), Duration::from_secs(1));
    let url = mcm.rtsp_url("reconnect");
    let pp = gst_plugin_path();
    let mut last_stats = None;

    for _ in 0..10 {
        let probe = SubprocessQrProbe::spawn(&url, Duration::from_secs(50), &pp).unwrap();
        let samples = probe.collect().unwrap();
        last_stats = Some(gst_helpers::compute_stats(&samples, 30.0));
        tokio::time::sleep(Duration::from_secs(10)).await;
    }

    let res = monitor.stop_and_collect();

    if let Some(stats) = last_stats {
        eprintln!("  last cycle: {stats}");
    }
    if let (Some(first), Some(last)) = (res.first(), res.last()) {
        eprintln!(
            "  threads: {} -> {}, RSS: {:.1} -> {:.1} MB",
            first.threads, last.threads, first.rss_mb, last.rss_mb
        );
    }

    // Check that threads aren't growing monotonically (would indicate a leak).
    // GStreamer RTSP sessions create/destroy transient threads, so absolute
    // stability isn't expected. We split samples into quarters and verify the
    // last quarter's max isn't much higher than the first quarter's max.
    if res.len() >= 8 {
        let q1_end = res.len() / 4;
        let q4_start = 3 * res.len() / 4;
        let q1_max = res[..q1_end].iter().map(|s| s.threads).max().unwrap_or(0);
        let q4_max = res[q4_start..].iter().map(|s| s.threads).max().unwrap_or(0);
        assert!(
            q4_max <= q1_max + 20,
            "possible thread leak: Q1 max={q1_max}, Q4 max={q4_max}"
        );
    }
}
