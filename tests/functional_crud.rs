mod common;

use std::time::Duration;

use common::api::McmClient;
use common::mcm::McmProcess;

const TIMEOUT: Duration = Duration::from_secs(15);

#[tokio::test]
#[serial_test::serial]
async fn test_info_endpoint() {
    let mcm = McmProcess::start().await.unwrap();
    let client = McmClient::new(&mcm.rest_url());

    let info = client.info().await.unwrap();
    assert!(!info.name.is_empty());
    assert!(!info.version.is_empty());
    assert!(!info.sha.is_empty());
}

#[tokio::test]
#[serial_test::serial]
async fn test_sources_lists_fake() {
    let mcm = McmProcess::start().await.unwrap();
    let client = McmClient::new(&mcm.rest_url());

    let sources = client.sources().await.unwrap();
    assert!(
        sources.iter().any(|s| s.source == "ball"),
        "Fake source (source_string='ball') not found in {:?}",
        sources.iter().map(|s| &s.source).collect::<Vec<_>>()
    );

    if common::gst_helpers::has_qr_plugin() {
        assert!(
            sources.iter().any(|s| s.source == "QRTimeStamp"),
            "QR source (source_string='QRTimeStamp') not found in {:?}",
            sources.iter().map(|s| &s.source).collect::<Vec<_>>()
        );
    }
}

#[tokio::test]
#[serial_test::serial]
async fn test_create_fake_h264_rtsp() {
    let mcm = McmProcess::start().await.unwrap();
    let client = McmClient::new(&mcm.rest_url());

    let post = McmClient::build_fake_h264_rtsp("fake_rtsp", 640, 480, 30, "fake_rtsp");
    client.create_stream(&post).await.unwrap();
    let streams = client.wait_for_streams_running(1, TIMEOUT).await.unwrap();
    assert_eq!(streams.len(), 1);
    assert!(streams[0].running);
}

#[tokio::test]
#[serial_test::serial]
async fn test_create_fake_h264_udp() {
    let mcm = McmProcess::start().await.unwrap();
    let client = McmClient::new(&mcm.rest_url());

    let post = McmClient::build_fake_h264_udp("fake_udp", 640, 480, 30, "127.0.0.1", 5000);
    client.create_stream(&post).await.unwrap();
    let streams = client.wait_for_streams_running(1, TIMEOUT).await.unwrap();
    assert_eq!(streams.len(), 1);
    assert!(streams[0].running);
}

#[tokio::test]
#[serial_test::serial]
async fn test_create_qr_h264_rtsp() {
    if !common::gst_helpers::has_qr_plugin() {
        eprintln!("SKIPPED: qrtimestampsrc plugin not available");
        return;
    }

    let mcm = McmProcess::start().await.unwrap();
    let client = McmClient::new(&mcm.rest_url());

    let post = McmClient::build_qr_h264_rtsp("qr_rtsp", 256, 15, "qr_rtsp");
    client.create_stream(&post).await.unwrap();
    let streams = client.wait_for_streams_running(1, TIMEOUT).await.unwrap();
    assert_eq!(streams.len(), 1);
    assert!(streams[0].running);
}

#[tokio::test]
#[serial_test::serial]
async fn test_create_and_delete_then_recreate() {
    let mcm = McmProcess::start().await.unwrap();
    let client = McmClient::new(&mcm.rest_url());

    let post = McmClient::build_fake_h264_rtsp("cycle", 640, 480, 30, "cycle");
    client.create_stream(&post).await.unwrap();
    client.wait_for_streams_running(1, TIMEOUT).await.unwrap();

    client.delete_stream("cycle").await.unwrap();
    let streams = client.list_streams().await.unwrap();
    assert!(streams.is_empty());

    // Recreate the same stream after deletion
    client.create_stream(&post).await.unwrap();
    let streams = client.wait_for_streams_running(1, TIMEOUT).await.unwrap();
    assert_eq!(streams.len(), 1);
    assert!(streams[0].running);
}

#[tokio::test]
#[serial_test::serial]
async fn test_delete_stream() {
    let mcm = McmProcess::start().await.unwrap();
    let client = McmClient::new(&mcm.rest_url());

    let post = McmClient::build_fake_h264_rtsp("to_delete", 640, 480, 30, "to_delete");
    client.create_stream(&post).await.unwrap();
    client.wait_for_streams_running(1, TIMEOUT).await.unwrap();

    let streams = client.list_streams().await.unwrap();
    assert_eq!(streams.len(), 1);

    client.delete_stream("to_delete").await.unwrap();
    let streams = client.list_streams().await.unwrap();
    assert!(streams.is_empty());
}

#[tokio::test]
#[serial_test::serial]
async fn test_delete_nonexistent() {
    let mcm = McmProcess::start().await.unwrap();
    let client = McmClient::new(&mcm.rest_url());

    let resp = client
        .delete_stream_raw("nonexistent_stream")
        .await
        .unwrap();
    assert!(!resp.status().is_success());
}

#[tokio::test]
#[serial_test::serial]
async fn test_duplicate_name_rejected() {
    let mcm = McmProcess::start().await.unwrap();
    let client = McmClient::new(&mcm.rest_url());

    let post = McmClient::build_fake_h264_rtsp("dup_name", 640, 480, 30, "path1");
    client.create_stream(&post).await.unwrap();
    client.wait_for_streams_running(1, TIMEOUT).await.unwrap();

    let post2 = McmClient::build_fake_h264_rtsp("dup_name", 640, 480, 30, "path2");
    let resp = client.create_stream_raw(&post2).await.unwrap();
    assert!(!resp.status().is_success());
}

#[tokio::test]
#[serial_test::serial]
async fn test_duplicate_endpoint_rejected() {
    let mcm = McmProcess::start().await.unwrap();
    let client = McmClient::new(&mcm.rest_url());

    let post = McmClient::build_fake_h264_rtsp("stream_a", 640, 480, 30, "dup_path");
    client.create_stream(&post).await.unwrap();
    client.wait_for_streams_running(1, TIMEOUT).await.unwrap();

    let post2 = McmClient::build_fake_h264_rtsp("stream_b", 640, 480, 30, "dup_path");
    let resp = client.create_stream_raw(&post2).await.unwrap();
    assert!(!resp.status().is_success());
}

#[tokio::test]
#[serial_test::serial]
async fn test_rtsp_stream_accessible() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let mcm = McmProcess::start().await.unwrap();
    let client = McmClient::new(&mcm.rest_url());

    let path = "accessible_rtsp";
    let post = McmClient::build_fake_h264_rtsp("accessible", 640, 480, 30, path);
    client.create_stream(&post).await.unwrap();
    client.wait_for_streams_running(1, TIMEOUT).await.unwrap();

    // The RTSP server binds asynchronously; retry TCP connect.
    let addr = format!("127.0.0.1:{}", mcm.rtsp_port);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut stream = loop {
        match tokio::net::TcpStream::connect(&addr).await {
            Ok(s) => break s,
            Err(_) if tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
            Err(e) => panic!("Could not connect to RTSP port at {addr} within 10s: {e}"),
        }
    };

    // Send a minimal RTSP OPTIONS request to verify the server responds
    let rtsp_url = mcm.rtsp_url(path);
    let request = format!("OPTIONS {rtsp_url} RTSP/1.0\r\nCSeq: 1\r\n\r\n");
    stream.write_all(request.as_bytes()).await.unwrap();

    let mut buf = vec![0u8; 1024];
    match tokio::time::timeout(Duration::from_secs(5), stream.read(&mut buf)).await {
        Ok(Ok(n)) if n > 0 => {
            let response = String::from_utf8_lossy(&buf[..n]);
            assert!(
                response.contains("RTSP/1.0"),
                "Expected RTSP response, got: {response}"
            );
        }
        _ => panic!("No RTSP response within 5s from {rtsp_url}"),
    }
}
