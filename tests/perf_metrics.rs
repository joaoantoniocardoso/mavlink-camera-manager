mod common;

use std::time::Duration;

use common::api::McmClient;
use common::gst_helpers::{self, SubprocessQrProbe};
use common::mcm::McmProcess;
use common::monitor::ProcMonitor;

const TIMEOUT: Duration = Duration::from_secs(60);
const RTSP_READY_TIMEOUT: Duration = Duration::from_secs(10);
const MEASURE_DURATION: Duration = Duration::from_secs(60);

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

#[tokio::test]
#[serial_test::serial]
async fn test_baseline_1_stream() {
    require_qr!();

    let mcm = McmProcess::start().await.unwrap();
    let client = McmClient::new(&mcm.rest_url());

    let post = McmClient::build_qr_h264_rtsp("baseline", 320, 30, "baseline");
    client.create_stream(&post).await.unwrap();
    client.wait_for_streams_running(1, TIMEOUT).await.unwrap();
    mcm.wait_rtsp_ready(RTSP_READY_TIMEOUT).await.unwrap();

    let mut monitor = ProcMonitor::start(mcm.pid(), Duration::from_secs(1));
    let probe = SubprocessQrProbe::spawn(
        &mcm.rtsp_url("baseline"),
        MEASURE_DURATION,
        &gst_plugin_path(),
    )
    .unwrap();

    let samples = probe.collect().unwrap();
    let stats = gst_helpers::compute_stats(&samples, 30.0);
    eprintln!("  {stats}");
    assert!(stats.count > 0, "probe received no frames");

    let res = monitor.stop_and_collect();
    if let (Some(first), Some(last)) = (res.first(), res.last()) {
        eprintln!(
            "  RSS: {:.1} -> {:.1} MB, threads: {} -> {}",
            first.rss_mb, last.rss_mb, first.threads, last.threads
        );
    }
}

#[tokio::test]
#[serial_test::serial]
async fn test_2_rtsp_clients() {
    require_qr!();

    let mcm = McmProcess::start().await.unwrap();
    let client = McmClient::new(&mcm.rest_url());

    let post = McmClient::build_qr_h264_rtsp("dual", 320, 30, "dual");
    client.create_stream(&post).await.unwrap();
    client.wait_for_streams_running(1, TIMEOUT).await.unwrap();
    mcm.wait_rtsp_ready(RTSP_READY_TIMEOUT).await.unwrap();

    let url = mcm.rtsp_url("dual");
    let pp = gst_plugin_path();
    let probe1 = SubprocessQrProbe::spawn(&url, MEASURE_DURATION, &pp).unwrap();
    tokio::time::sleep(Duration::from_secs(3)).await;
    let probe2 = SubprocessQrProbe::spawn(&url, Duration::from_secs(57), &pp).unwrap();

    let samples1 = probe1.collect().unwrap();
    let samples2 = probe2.collect().unwrap();
    let stats1 = gst_helpers::compute_stats(&samples1, 30.0);
    let stats2 = gst_helpers::compute_stats(&samples2, 30.0);
    eprintln!("  probe1: {stats1}");
    eprintln!("  probe2: {stats2}");
    assert!(stats1.count > 0, "probe1 received no frames");
    assert!(stats2.count > 0, "probe2 received no frames");
}

#[tokio::test]
#[serial_test::serial]
async fn test_3_rtsp_clients() {
    require_qr!();

    let mcm = McmProcess::start().await.unwrap();
    let client = McmClient::new(&mcm.rest_url());

    let post = McmClient::build_qr_h264_rtsp("triple", 320, 30, "triple");
    client.create_stream(&post).await.unwrap();
    client.wait_for_streams_running(1, TIMEOUT).await.unwrap();
    mcm.wait_rtsp_ready(RTSP_READY_TIMEOUT).await.unwrap();

    let url = mcm.rtsp_url("triple");
    let pp = gst_plugin_path();
    let probe1 = SubprocessQrProbe::spawn(&url, MEASURE_DURATION, &pp).unwrap();
    tokio::time::sleep(Duration::from_secs(3)).await;
    let probe2 = SubprocessQrProbe::spawn(&url, Duration::from_secs(57), &pp).unwrap();
    tokio::time::sleep(Duration::from_secs(2)).await;
    let probe3 = SubprocessQrProbe::spawn(&url, Duration::from_secs(55), &pp).unwrap();

    let stats1 = gst_helpers::compute_stats(&probe1.collect().unwrap(), 30.0);
    let stats2 = gst_helpers::compute_stats(&probe2.collect().unwrap(), 30.0);
    let stats3 = gst_helpers::compute_stats(&probe3.collect().unwrap(), 30.0);
    eprintln!("  probe1: {stats1}");
    eprintln!("  probe2: {stats2}");
    eprintln!("  probe3: {stats3}");
    assert!(stats1.count > 0, "probe1 received no frames");
    assert!(stats2.count > 0, "probe2 received no frames");
    assert!(stats3.count > 0, "probe3 received no frames");
}

#[tokio::test]
#[serial_test::serial]
async fn test_high_resolution() {
    require_qr!();

    let mcm = McmProcess::start().await.unwrap();
    let client = McmClient::new(&mcm.rest_url());

    let post = McmClient::build_qr_h264_rtsp("highres", 720, 30, "highres");
    client.create_stream(&post).await.unwrap();
    client.wait_for_streams_running(1, TIMEOUT).await.unwrap();
    mcm.wait_rtsp_ready(RTSP_READY_TIMEOUT).await.unwrap();

    let mut monitor = ProcMonitor::start(mcm.pid(), Duration::from_secs(1));
    let probe = SubprocessQrProbe::spawn(
        &mcm.rtsp_url("highres"),
        MEASURE_DURATION,
        &gst_plugin_path(),
    )
    .unwrap();

    let samples = probe.collect().unwrap();
    let stats = gst_helpers::compute_stats(&samples, 30.0);
    eprintln!("  {stats}");
    assert!(stats.count > 0, "probe received no frames");

    let res = monitor.stop_and_collect();
    if let (Some(first), Some(last)) = (res.first(), res.last()) {
        eprintln!(
            "  RSS: {:.1} -> {:.1} MB, threads: {} -> {}",
            first.rss_mb, last.rss_mb, first.threads, last.threads
        );
    }
}
