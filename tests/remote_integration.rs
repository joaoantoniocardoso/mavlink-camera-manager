mod common;

use std::{
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};

use anyhow::{Context, Result};
use common::{
    api::{McmClient, StateMonitor},
    types::*,
};
use futures::{SinkExt, StreamExt};
use gst::prelude::*;

#[cfg(feature = "webrtc-test")]
use thirtyfour::{prelude::ElementQueryable, ChromiumLikeCapabilities};

const MCM_REST: &str = "http://192.168.2.2:6020";
const MCM_SIGNALLING: &str = "ws://192.168.2.2:6021";
const STATE_POLL: Duration = Duration::from_millis(200);

// ═══════════════════════════════════════════════════════════════════════
// SOURCE MODE — determines which video source the tests target
// ═══════════════════════════════════════════════════════════════════════

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SourceMode {
    Fake,
    Camera,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
enum SourceTag {
    Fake,
    Camera,
    Both,
}

fn source_mode() -> SourceMode {
    match std::env::var("TEST_SOURCE_MODE").as_deref() {
        Ok("fake") => SourceMode::Fake,
        _ => SourceMode::Camera,
    }
}

/// Returns early from the calling test if the current source mode
/// doesn't match the tag. Usage: `skip_unless!(SourceTag::Both);`
macro_rules! skip_unless {
    ($tag:expr) => {
        match ($tag, source_mode()) {
            (SourceTag::Both, _) => {}
            (SourceTag::Fake, SourceMode::Fake) => {}
            (SourceTag::Camera, SourceMode::Camera) => {}
            (tag, mode) => {
                eprintln!("SKIP: test tagged {tag:?}, current mode is {mode:?}");
                return;
            }
        }
    };
}

// ═══════════════════════════════════════════════════════════════════════
// SOURCE-DEPENDENT TIMEOUTS
// ═══════════════════════════════════════════════════════════════════════

fn cold_timeout() -> Duration {
    match source_mode() {
        SourceMode::Fake => Duration::from_secs(30),
        SourceMode::Camera => Duration::from_secs(60),
    }
}

fn warm_timeout() -> Duration {
    match source_mode() {
        SourceMode::Fake => Duration::from_secs(10),
        SourceMode::Camera => Duration::from_secs(45),
    }
}

fn idle_timeout() -> Duration {
    match source_mode() {
        SourceMode::Fake => Duration::from_secs(30),
        SourceMode::Camera => Duration::from_secs(45),
    }
}

// ═══════════════════════════════════════════════════════════════════════
// COMMON HELPERS
// ═══════════════════════════════════════════════════════════════════════

fn mcm_rtsp() -> String {
    std::env::var("MCM_RTSP")
        .unwrap_or_else(|_| "rtsp://192.168.2.2:8554/radcam_192.168.2.10/0".into())
}

fn stream_source() -> String {
    std::env::var("STREAM_SOURCE").unwrap_or_else(|_| "rtsp://192.168.2.10:554/stream_0".into())
}

fn client() -> McmClient {
    McmClient::new(MCM_REST)
}

fn monitor() -> StateMonitor {
    StateMonitor::start(MCM_REST, STATE_POLL)
}

async fn ensure_idle(c: &McmClient) {
    c.wait_for_stream_state(StreamStatusState::Idle, idle_timeout())
        .await
        .expect("stream must reach Idle");
}

async fn ensure_running(c: &McmClient) {
    c.wait_for_stream_state(StreamStatusState::Running, cold_timeout())
        .await
        .expect("stream did not reach Running");
}

async fn ensure_data_flowing(c: &McmClient) {
    let deadline = tokio::time::Instant::now() + cold_timeout();
    loop {
        let resp = match c.thumbnail(&stream_source()).await {
            Ok(r) => r,
            Err(e) => {
                eprintln!("thumbnail request failed (transient): {e}");
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "thumbnail request never succeeded: {e}"
                );
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            }
        };
        if resp.status() == 200 {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "thumbnail never returned 200 (data not flowing, last status: {})",
            resp.status()
        );
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

async fn cold_thumbnail(c: &McmClient, timeout: Duration) -> Vec<u8> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let resp = match c.thumbnail(&stream_source()).await {
            Ok(r) => r,
            Err(e) => {
                eprintln!("cold_thumbnail request failed (transient): {e}");
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "cold thumbnail request never succeeded: {e}"
                );
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            }
        };
        if resp.status() == 200 {
            return resp.bytes().await.unwrap().to_vec();
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "cold thumbnail never returned 200 (got {})",
            resp.status()
        );
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

async fn wait_for_rtsp_available(url: &str, timeout: Duration) -> bool {
    let addr = url
        .trim_start_matches("rtsp://")
        .split('/')
        .next()
        .unwrap_or("192.168.2.2:8554");
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if let Ok(stream) =
            tokio::time::timeout(Duration::from_secs(2), tokio::net::TcpStream::connect(addr)).await
        {
            if stream.is_ok() {
                return true;
            }
        }
        if tokio::time::Instant::now() > deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

async fn thumbnail_with_retry(c: &McmClient, source: &str) -> reqwest::Response {
    let mut last_err = None;
    for attempt in 0..3u32 {
        match c.thumbnail(source).await {
            Ok(resp) => return resp,
            Err(e) => {
                eprintln!("thumbnail request attempt {}/3 failed: {e}", attempt + 1);
                last_err = Some(e);
                if attempt < 2 {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        }
    }
    panic!(
        "thumbnail request failed after 3 attempts: {}",
        last_err.unwrap()
    );
}

async fn webrtc_connect_with_retry(url: &str) -> WebrtcClient {
    let mut last_err = None;
    for attempt in 0..3u32 {
        match WebrtcClient::connect(url).await {
            Ok(client) => return client,
            Err(e) => {
                eprintln!("WebRTC connect attempt {}/3 failed: {e:#}", attempt + 1);
                last_err = Some(e);
                if attempt < 2 {
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
            }
        }
    }
    panic!(
        "WebRTC connect failed after 3 attempts: {}",
        last_err.unwrap()
    );
}

fn states_str(transitions: &[(std::time::Instant, StreamStatusState)]) -> String {
    transitions
        .iter()
        .map(|(_, s)| format!("{s:?}"))
        .collect::<Vec<_>>()
        .join(" → ")
}

fn assert_never_stopped(transitions: &[(std::time::Instant, StreamStatusState)], context: &str) {
    let stopped = transitions
        .iter()
        .any(|(_, s)| *s == StreamStatusState::Stopped);
    assert!(
        !stopped,
        "{context}: stream went through Stopped! Transitions: {}",
        states_str(transitions)
    );
}

// ═══════════════════════════════════════════════════════════════════════
// RTSP CLIENT — uses ffprobe to count frames from RTSP stream
// ═══════════════════════════════════════════════════════════════════════

struct RtspClient {
    child: Option<tokio::process::Child>,
    frame_count: Arc<AtomicU64>,
    _counter_task: tokio::task::JoinHandle<()>,
}

impl RtspClient {
    fn new(url: &str) -> Result<Self> {
        let frame_count = Arc::new(AtomicU64::new(0));

        let mut child = tokio::process::Command::new("ffprobe")
            .args([
                "-v",
                "quiet",
                "-rtsp_transport",
                "tcp",
                "-show_frames",
                "-select_streams",
                "v:0",
                "-print_format",
                "csv",
                url,
            ])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .context("failed to spawn ffprobe")?;

        let stdout = child.stdout.take().context("no stdout")?;
        let counter = Arc::clone(&frame_count);
        let task = tokio::spawn(async move {
            use tokio::io::{AsyncBufReadExt, BufReader};
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(_)) = lines.next_line().await {
                counter.fetch_add(1, Ordering::Relaxed);
            }
        });

        Ok(Self {
            child: Some(child),
            frame_count,
            _counter_task: task,
        })
    }

    fn start(&self) -> Result<()> {
        Ok(())
    }

    fn frames(&self) -> u64 {
        self.frame_count.load(Ordering::Relaxed)
    }

    async fn wait_for_frames(&self, min: u64, timeout: Duration) -> Result<u64> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let n = self.frames();
            if n >= min {
                return Ok(n);
            }
            if tokio::time::Instant::now() > deadline {
                anyhow::bail!("only got {n} frames, wanted {min}");
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }

    async fn wait_for_continuous_frames(
        &self,
        duration: Duration,
        check_interval: Duration,
    ) -> Result<u64> {
        let deadline = tokio::time::Instant::now() + duration;
        let mut last_count = self.frames();
        let mut stall_start: Option<tokio::time::Instant> = None;
        let max_stall = Duration::from_secs(3);

        while tokio::time::Instant::now() < deadline {
            tokio::time::sleep(check_interval).await;
            let now_count = self.frames();
            if now_count > last_count {
                stall_start = None;
                last_count = now_count;
            } else {
                let stall = stall_start.get_or_insert(tokio::time::Instant::now());
                if stall.elapsed() > max_stall {
                    anyhow::bail!(
                        "frame flow stalled at {now_count} frames for {:?}",
                        stall.elapsed()
                    );
                }
            }
        }
        Ok(self.frames())
    }
}

impl Drop for RtspClient {
    fn drop(&mut self) {
        if let Some(ref mut child) = self.child {
            let _ = child.start_kill();
        }
        self._counter_task.abort();
    }
}

// ═══════════════════════════════════════════════════════════════════════
// REAL WEBRTC CLIENT — full SDP+ICE handshake, verifies frames arrive
// ═══════════════════════════════════════════════════════════════════════

struct WebrtcClient {
    pipeline: gst::Pipeline,
    frame_count: Arc<AtomicU64>,
    decoded_count: Arc<AtomicU64>,
    _signaling_handle: tokio::task::JoinHandle<()>,
}

enum SignalOut {
    SdpAnswer(String),
    IceCandidate {
        sdp_m_line_index: u32,
        candidate: String,
    },
}

impl WebrtcClient {
    async fn connect(signalling_url: &str) -> Result<Self> {
        gst::init()?;

        let pipeline = gst::Pipeline::with_name("webrtc-test-client");
        let frame_count = Arc::new(AtomicU64::new(0));

        let webrtcbin = gst::ElementFactory::make("webrtcbin")
            .property("bundle-policy", gst_webrtc::WebRTCBundlePolicy::MaxBundle)
            .property("latency", 0u32)
            .build()
            .context("Failed to create webrtcbin")?;
        pipeline.add(&webrtcbin)?;

        let (gst_tx, mut gst_rx) = tokio::sync::mpsc::unbounded_channel::<SignalOut>();

        let tx_ice = gst_tx.clone();
        webrtcbin.connect("on-ice-candidate", false, move |values| {
            let idx = values[1].get::<u32>().expect("bad arg");
            let cand = values[2].get::<String>().expect("bad arg");
            let stripped = cand.strip_prefix("candidate:").unwrap_or(&cand);
            let parts: Vec<&str> = stripped.split_whitespace().collect();
            let ip = parts.get(4).copied().unwrap_or("");
            if !ip.starts_with("192.168.2.") {
                return None;
            }
            let _ = tx_ice.send(SignalOut::IceCandidate {
                sdp_m_line_index: idx,
                candidate: cand,
            });
            None
        });

        let decoded_count = Arc::new(AtomicU64::new(0));

        let pipe_weak = pipeline.downgrade();
        let counter = Arc::clone(&frame_count);
        let dec_counter = Arc::clone(&decoded_count);
        webrtcbin.connect_pad_added(move |_wrtc, pad| {
            if pad.direction() != gst::PadDirection::Src {
                return;
            }
            let Some(pipe) = pipe_weak.upgrade() else {
                return;
            };

            let depay = gst::ElementFactory::make("rtph264depay").build().unwrap();
            let parse = gst::ElementFactory::make("h264parse").build().unwrap();
            let decoder = gst::ElementFactory::make("avdec_h264").build().unwrap();
            let sink = gst::ElementFactory::make("fakesink")
                .property("sync", false)
                .property("async", false)
                .build()
                .unwrap();

            pipe.add_many([&depay, &parse, &decoder, &sink]).ok();
            gst::Element::link_many([&depay, &parse, &decoder, &sink]).ok();
            depay.sync_state_with_parent().ok();
            parse.sync_state_with_parent().ok();
            decoder.sync_state_with_parent().ok();
            sink.sync_state_with_parent().ok();

            let probe_pad = parse.static_pad("src").unwrap();
            let ctr = counter.clone();
            probe_pad.add_probe(gst::PadProbeType::BUFFER, move |_, _| {
                ctr.fetch_add(1, Ordering::Relaxed);
                gst::PadProbeReturn::Ok
            });

            let dec_src = decoder.static_pad("src").unwrap();
            let dc = dec_counter.clone();
            dec_src.add_probe(gst::PadProbeType::BUFFER, move |_, _| {
                dc.fetch_add(1, Ordering::Relaxed);
                gst::PadProbeReturn::Ok
            });

            let sink_pad = depay.static_pad("sink").unwrap();
            pad.link(&sink_pad).ok();
        });

        let ws = {
            let max_attempts = 5;
            let mut last_err: Option<async_tungstenite::tungstenite::Error> = None;
            let mut conn = None;
            for attempt in 0..max_attempts {
                match async_tungstenite::tokio::connect_async(signalling_url).await {
                    Ok((ws, _)) => {
                        conn = Some(ws);
                        break;
                    }
                    Err(e) => {
                        eprintln!(
                            "WebRTC ws connect attempt {}/{max_attempts} failed: {e}, retrying...",
                            attempt + 1
                        );
                        last_err = Some(e);
                        if attempt + 1 < max_attempts {
                            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                        }
                    }
                }
            }
            conn.ok_or_else(|| last_err.unwrap())
                .context("ws connect failed after retries")?
        };
        let (mut ws_sink, mut ws_source) = ws.split();

        async fn ws_send(
            sink: &mut (impl futures::Sink<
                async_tungstenite::tungstenite::Message,
                Error = async_tungstenite::tungstenite::Error,
            > + Unpin),
            msg: serde_json::Value,
        ) -> Result<()> {
            sink.send(async_tungstenite::tungstenite::Message::Text(
                msg.to_string(),
            ))
            .await?;
            Ok(())
        }

        async fn ws_recv(
            source: &mut (impl futures::Stream<
                Item = Result<
                    async_tungstenite::tungstenite::Message,
                    async_tungstenite::tungstenite::Error,
                >,
            > + Unpin),
        ) -> Result<serde_json::Value> {
            loop {
                let msg = tokio::time::timeout(Duration::from_secs(30), source.next())
                    .await
                    .expect("WebSocket recv timed out")
                    .context("ws closed")??;
                if let async_tungstenite::tungstenite::Message::Text(text) = msg {
                    return Ok(serde_json::from_str(&text)?);
                }
            }
        }

        ws_send(
            &mut ws_sink,
            serde_json::json!({"type":"question","content":{"type":"peerId"}}),
        )
        .await?;
        let resp = ws_recv(&mut ws_source).await?;
        let consumer_id = resp["content"]["content"]["id"]
            .as_str()
            .context("no id")?
            .to_string();

        ws_send(
            &mut ws_sink,
            serde_json::json!({"type":"question","content":{"type":"availableStreams"}}),
        )
        .await?;
        let resp = ws_recv(&mut ws_source).await?;
        let producer_id = resp["content"]["content"][0]["id"]
            .as_str()
            .context("no stream")?
            .to_string();

        ws_send(
            &mut ws_sink,
            serde_json::json!({
                "type": "question",
                "content": {
                    "type": "startSession",
                    "content": { "consumer_id": consumer_id, "producer_id": producer_id }
                }
            }),
        )
        .await?;

        let mut early_msgs = Vec::new();
        let bind: serde_json::Value = loop {
            let msg = ws_recv(&mut ws_source).await?;
            if msg["content"]["type"] == "startSession" {
                break msg["content"]["content"].clone();
            }
            early_msgs.push(msg);
        };

        let webrtcbin_clone = webrtcbin.clone();
        let tx_answer = gst_tx.clone();

        fn handle_ws_msg(
            msg: &serde_json::Value,
            webrtcbin: &gst::Element,
            tx: &tokio::sync::mpsc::UnboundedSender<SignalOut>,
        ) -> bool {
            let content = &msg["content"];
            let msg_type = content["type"].as_str().unwrap_or("");
            match msg_type {
                "mediaNegotiation" => {
                    let sdp_type = content["content"]["sdp"]["type"].as_str().unwrap_or("?");
                    let sdp_text = content["content"]["sdp"]["sdp"].as_str().unwrap_or("");
                    if !sdp_text.is_empty() && sdp_type == "offer" {
                        let sdp_msg =
                            gst_sdp::SDPMessage::parse_buffer(sdp_text.as_bytes()).unwrap();
                        let offer = gst_webrtc::WebRTCSessionDescription::new(
                            gst_webrtc::WebRTCSDPType::Offer,
                            sdp_msg,
                        );
                        webrtcbin.emit_by_name::<()>(
                            "set-remote-description",
                            &[&offer, &None::<gst::Promise>],
                        );

                        let wb = webrtcbin.downgrade();
                        let tx = tx.clone();
                        let promise = gst::Promise::with_change_func(move |reply| {
                            let Ok(Some(reply)) = reply else { return };
                            let Ok(Some(answer)) = reply
                                .get_optional::<gst_webrtc::WebRTCSessionDescription>("answer")
                            else {
                                return;
                            };
                            if let Some(wb) = wb.upgrade() {
                                wb.emit_by_name::<()>(
                                    "set-local-description",
                                    &[&answer, &None::<gst::Promise>],
                                );
                            }
                            if let Ok(sdp_text) = answer.sdp().as_text() {
                                let _ = tx.send(SignalOut::SdpAnswer(sdp_text));
                            }
                        });
                        webrtcbin.emit_by_name::<()>(
                            "create-answer",
                            &[&None::<gst::Structure>, &promise],
                        );
                    }
                }
                "iceNegotiation" => {
                    if let Some(candidate) = content["content"]["ice"]["candidate"].as_str() {
                        let stripped = candidate.strip_prefix("candidate:").unwrap_or(candidate);
                        let parts: Vec<&str> = stripped.split_whitespace().collect();
                        let ip = parts.get(4).copied().unwrap_or("");
                        if ip.starts_with("192.168.2.") {
                            let idx = content["content"]["ice"]["sdpMLineIndex"]
                                .as_u64()
                                .unwrap_or(0) as u32;
                            webrtcbin.emit_by_name::<()>("add-ice-candidate", &[&idx, &candidate]);
                        }
                    }
                }
                "endSession" => {
                    return true;
                }
                _ => {}
            }
            false
        }

        for msg in &early_msgs {
            handle_ws_msg(msg, &webrtcbin_clone, &tx_answer);
        }

        let wb2 = webrtcbin_clone.clone();
        let tx2 = tx_answer.clone();
        let bind2 = bind.clone();
        let handle = tokio::spawn(async move {
            loop {
                tokio::select! {
                    ws_msg = ws_source.next() => {
                        let Some(Ok(msg)) = ws_msg else { break };
                        if let async_tungstenite::tungstenite::Message::Text(text) = msg {
                            let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
                            if handle_ws_msg(&parsed, &wb2, &tx2) {
                                break;
                            }
                        }
                    }
                    gst_msg = gst_rx.recv() => {
                        let Some(msg) = gst_msg else { break };
                        match msg {
                            SignalOut::SdpAnswer(sdp) => {
                                let neg = serde_json::json!({
                                    "type": "negotiation",
                                    "content": {
                                        "type": "mediaNegotiation",
                                        "content": {
                                            "consumer_id": bind2["consumer_id"],
                                            "producer_id": bind2["producer_id"],
                                            "session_id": bind2["session_id"],
                                            "sdp": { "type": "answer", "sdp": sdp }
                                        }
                                    }
                                });
                                let _ = ws_sink.send(async_tungstenite::tungstenite::Message::Text(neg.to_string())).await;
                            }
                            SignalOut::IceCandidate { sdp_m_line_index, candidate } => {
                                let neg = serde_json::json!({
                                    "type": "negotiation",
                                    "content": {
                                        "type": "iceNegotiation",
                                        "content": {
                                            "consumer_id": bind2["consumer_id"],
                                            "producer_id": bind2["producer_id"],
                                            "session_id": bind2["session_id"],
                                            "ice": {
                                                "candidate": candidate,
                                                "sdpMLineIndex": sdp_m_line_index
                                            }
                                        }
                                    }
                                });
                                let _ = ws_sink.send(async_tungstenite::tungstenite::Message::Text(neg.to_string())).await;
                            }
                        }
                    }
                }
            }
        });

        pipeline.set_state(gst::State::Playing)?;

        Ok(Self {
            pipeline,
            frame_count,
            decoded_count,
            _signaling_handle: handle,
        })
    }

    fn frames(&self) -> u64 {
        self.frame_count.load(Ordering::Relaxed)
    }

    fn decoded_frames(&self) -> u64 {
        self.decoded_count.load(Ordering::Relaxed)
    }

    async fn wait_for_frames(&self, min: u64, timeout: Duration) -> Result<u64> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let n = self.frames();
            if n >= min {
                return Ok(n);
            }
            if tokio::time::Instant::now() > deadline {
                anyhow::bail!("only got {n} frames, wanted {min}");
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }

    async fn wait_for_decoded_frames(&self, min: u64, timeout: Duration) -> Result<u64> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let n = self.decoded_frames();
            if n >= min {
                return Ok(n);
            }
            if tokio::time::Instant::now() > deadline {
                let parsed = self.frames();
                anyhow::bail!("only got {n} decoded frames (wanted {min}), parsed={parsed}");
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }
}

impl Drop for WebrtcClient {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
        self._signaling_handle.abort();
    }
}

// ═══════════════════════════════════════════════════════════════════════
// BROWSER WEBRTC CLIENT — headless Chrome via WebDriver
// ═══════════════════════════════════════════════════════════════════════

#[cfg(feature = "webrtc-test")]
struct BrowserWebrtcClient {
    driver: thirtyfour::WebDriver,
    _chromedriver: tokio::process::Child,
}

#[cfg(feature = "webrtc-test")]
#[derive(Debug)]
struct BrowserWebrtcStats {
    frames_decoded: u64,
    frames_received: u64,
    frames_dropped: u64,
    key_frames_decoded: u64,
}

#[cfg(feature = "webrtc-test")]
impl BrowserWebrtcClient {
    async fn new() -> Result<Self> {
        let port: u16 = std::env::var("CHROMEDRIVER_PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(9515);

        let chromedriver = tokio::process::Command::new("chromedriver")
            .arg(format!("--port={port}"))
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .context("Failed to spawn chromedriver — is it installed?")?;

        tokio::time::sleep(Duration::from_secs(2)).await;

        let mut caps = thirtyfour::DesiredCapabilities::chrome();
        caps.set_headless().map_err(|e| anyhow::anyhow!("{e}"))?;
        caps.set_no_sandbox().map_err(|e| anyhow::anyhow!("{e}"))?;
        caps.set_disable_dev_shm_usage()
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        caps.set_disable_web_security()
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        caps.set_ignore_certificate_errors()
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        caps.add_arg("--autoplay-policy=no-user-gesture-required")
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        caps.add_arg("--disable-gpu")
            .map_err(|e| anyhow::anyhow!("{e}"))?;

        let driver = thirtyfour::WebDriver::new(&format!("http://127.0.0.1:{port}"), caps)
            .await
            .map_err(|e| anyhow::anyhow!("WebDriver session failed: {e}"))?;

        Ok(Self {
            driver,
            _chromedriver: chromedriver,
        })
    }

    async fn connect(&self, url: &str) -> Result<()> {
        self.driver
            .goto(url)
            .await
            .map_err(|e| anyhow::anyhow!("goto failed: {e}"))?;

        tokio::time::sleep(Duration::from_secs(2)).await;

        let add_consumer: thirtyfour::WebElement = self
            .driver
            .query(thirtyfour::By::Id("add-consumer"))
            .first()
            .await
            .map_err(|e| anyhow::anyhow!("add-consumer not found: {e}"))?;
        add_consumer
            .click()
            .await
            .map_err(|e| anyhow::anyhow!("click add-consumer: {e}"))?;

        tokio::time::sleep(Duration::from_millis(500)).await;

        let add_session: thirtyfour::WebElement = self
            .driver
            .query(thirtyfour::By::Id("add-session"))
            .first()
            .await
            .map_err(|e| anyhow::anyhow!("add-session not found: {e}"))?;
        add_session
            .click()
            .await
            .map_err(|e| anyhow::anyhow!("click add-session: {e}"))?;

        Ok(())
    }

    async fn wait_for_playing(&self, timeout: Duration) -> Result<()> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let elements: std::result::Result<Vec<_>, _> = self
                .driver
                .query(thirtyfour::By::Id("session-status"))
                .with_text("Status: Playing")
                .all_from_selector()
                .await;

            if let Ok(elems) = elements {
                if !elems.is_empty() {
                    return Ok(());
                }
            }

            if tokio::time::Instant::now() > deadline {
                anyhow::bail!("Timed out waiting for 'Playing' status");
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    async fn get_stats(&self) -> Result<BrowserWebrtcStats> {
        let script = r#"
            const done = arguments[0];
            const pcs = window.__mcm_peer_connections || [];
            if (pcs.length === 0) {
                done({error: "no peer connections"});
                return;
            }
            const pc = pcs[pcs.length - 1];
            pc.getStats().then(stats => {
                let result = {
                    framesDecoded: 0,
                    framesReceived: 0,
                    framesDropped: 0,
                    keyFramesDecoded: 0
                };
                stats.forEach(report => {
                    if (report.type === 'inbound-rtp' && report.kind === 'video') {
                        result.framesDecoded = report.framesDecoded || 0;
                        result.framesReceived = report.framesReceived || 0;
                        result.framesDropped = report.framesDropped || 0;
                        result.keyFramesDecoded = report.keyFramesDecoded || 0;
                    }
                });
                done(result);
            }).catch(e => done({error: e.message}));
        "#;

        let ret = self
            .driver
            .execute_async(script, vec![])
            .await
            .map_err(|e| anyhow::anyhow!("execute getStats: {e}"))?;

        let val = ret.json();
        if let Some(err) = val.get("error") {
            anyhow::bail!("getStats JS error: {err}");
        }

        Ok(BrowserWebrtcStats {
            frames_decoded: val["framesDecoded"].as_u64().unwrap_or(0),
            frames_received: val["framesReceived"].as_u64().unwrap_or(0),
            frames_dropped: val["framesDropped"].as_u64().unwrap_or(0),
            key_frames_decoded: val["keyFramesDecoded"].as_u64().unwrap_or(0),
        })
    }

    async fn wait_for_decoded_frames(
        &self,
        min: u64,
        timeout: Duration,
    ) -> Result<BrowserWebrtcStats> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            match self.get_stats().await {
                Ok(stats) if stats.frames_decoded >= min => return Ok(stats),
                Ok(stats) => {
                    if tokio::time::Instant::now() > deadline {
                        anyhow::bail!(
                            "only {} decoded frames (wanted {min}), received={}, dropped={}, keyFrames={}",
                            stats.frames_decoded,
                            stats.frames_received,
                            stats.frames_dropped,
                            stats.key_frames_decoded,
                        );
                    }
                }
                Err(e) => {
                    if tokio::time::Instant::now() > deadline {
                        anyhow::bail!("getStats failed and timed out: {e}");
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    async fn disconnect(&self) -> Result<()> {
        let remove_consumer: thirtyfour::WebElement = self
            .driver
            .query(thirtyfour::By::Id("remove-consumer"))
            .first()
            .await
            .map_err(|e| anyhow::anyhow!("remove-consumer not found: {e}"))?;
        remove_consumer
            .click()
            .await
            .map_err(|e| anyhow::anyhow!("click remove-consumer: {e}"))?;
        Ok(())
    }

    async fn quit(self) -> Result<()> {
        self.driver
            .quit()
            .await
            .map_err(|e| anyhow::anyhow!("WebDriver quit: {e}"))?;
        Ok(())
    }
}

// ═══════════════════════════════════════════════════════════════════════
// THUMBNAIL TESTS
// ═══════════════════════════════════════════════════════════════════════

/// Pattern 2 from user: cold thumbnail should work and not go through Stopped.
/// Stream transitions must be Idle → Running (→ Idle), never Stopped.
#[tokio::test]
#[serial_test::serial]
async fn test_thumb_cold_no_stopped_state() {
    skip_unless!(SourceTag::Both);
    let c = client();
    ensure_idle(&c).await;

    let mon = monitor();
    let body = cold_thumbnail(&c, cold_timeout()).await;

    tokio::time::sleep(Duration::from_millis(500)).await;
    let transitions = mon.stop();

    eprintln!(
        "cold thumbnail: {} bytes, transitions: {}",
        body.len(),
        states_str(&transitions)
    );

    assert!(
        body.len() > 1000,
        "thumbnail body too small: {} bytes",
        body.len()
    );
    assert_never_stopped(&transitions, "cold thumbnail");
}

/// Warm thumbnail: pipeline is already running, thumbnail must return quickly.
#[tokio::test]
#[serial_test::serial]
async fn test_thumb_warm() {
    skip_unless!(SourceTag::Both);
    let c = client();
    ensure_data_flowing(&c).await;

    let t0 = std::time::Instant::now();
    let resp = thumbnail_with_retry(&c, &stream_source()).await;
    let elapsed = t0.elapsed();
    assert_eq!(resp.status(), 200);
    let body = resp.bytes().await.unwrap();
    assert!(body.len() > 1000, "thumbnail body too small");
    eprintln!("warm thumbnail: {elapsed:?}, {} bytes", body.len());
}

/// After a thumbnail, the stream must return to Idle.
#[tokio::test]
#[serial_test::serial]
async fn test_thumb_returns_to_idle() {
    skip_unless!(SourceTag::Both);
    let c = client();
    ensure_idle(&c).await;

    let body = cold_thumbnail(&c, cold_timeout()).await;
    assert!(body.len() > 1000);

    c.wait_for_stream_state(StreamStatusState::Idle, idle_timeout())
        .await
        .expect("stream must return to Idle after thumbnail");
    eprintln!("stream returned to Idle after thumbnail ✓");
}

/// Repeated cold→warm thumbnail cycles. Each time, stream should not go
/// to Stopped and should return to Idle.
#[tokio::test]
#[serial_test::serial]
async fn test_thumb_cold_warm_cycles() {
    skip_unless!(SourceTag::Both);
    let c = client();

    for cycle in 0..3 {
        ensure_idle(&c).await;
        tokio::time::sleep(Duration::from_secs(5)).await;

        let mon = monitor();
        let body = cold_thumbnail(&c, cold_timeout()).await;
        assert!(
            body.len() > 1000,
            "cycle {cycle}: cold thumbnail body too small"
        );

        // Warm follow-up
        let resp = thumbnail_with_retry(&c, &stream_source()).await;
        assert_eq!(resp.status(), 200, "warm thumbnail cycle {cycle}");

        tokio::time::sleep(Duration::from_secs(1)).await;
        let transitions = mon.stop();
        eprintln!("cycle {cycle}: transitions: {}", states_str(&transitions));
        assert_never_stopped(&transitions, &format!("thumbnail cycle {cycle}"));
    }
}

/// Thumbnail requested repeatedly at 1/s for 10s — all must succeed,
/// stream must never go Stopped, and must return to Idle after stopping.
#[tokio::test]
#[serial_test::serial]
async fn test_thumb_rapid_sequential() {
    skip_unless!(SourceTag::Both);
    let c = client();
    ensure_idle(&c).await;

    // Warm up: first thumbnail may return 503 on cold start
    let _ = cold_thumbnail(&c, cold_timeout()).await;

    let mon = monitor();
    for i in 0..10 {
        let resp = thumbnail_with_retry(&c, &stream_source()).await;
        let status = resp.status();
        let body = resp.bytes().await.unwrap();
        eprintln!("thumb #{i}: status={status}, {} bytes", body.len());
        assert_eq!(status, 200, "thumbnail #{i} must return 200");
        assert!(body.len() > 1000, "thumbnail #{i} body too small");
        tokio::time::sleep(Duration::from_secs(1)).await;
    }

    // Stop requesting → stream must return to Idle
    c.wait_for_stream_state(StreamStatusState::Idle, Duration::from_secs(30))
        .await
        .expect("stream must return to Idle after thumbnail burst");

    let transitions = mon.stop();
    eprintln!(
        "rapid thumbnails: transitions: {}",
        states_str(&transitions)
    );
    assert_never_stopped(&transitions, "rapid thumbnails");
}

// ═══════════════════════════════════════════════════════════════════════
// WEBRTC TESTS
// ═══════════════════════════════════════════════════════════════════════

/// Cold WebRTC: pipeline goes from Idle → Running, client receives frames.
/// Stream must NOT go through Stopped.
#[tokio::test]
#[serial_test::serial]
async fn test_webrtc_cold_no_stopped() {
    skip_unless!(SourceTag::Both);
    let c = client();
    ensure_idle(&c).await;

    let mon = monitor();
    let wrtc = webrtc_connect_with_retry(MCM_SIGNALLING).await;
    ensure_running(&c).await;

    let n = wrtc
        .wait_for_decoded_frames(5, warm_timeout())
        .await
        .expect("webrtc must decode frames");

    tokio::time::sleep(Duration::from_millis(500)).await; // Brief settle time for state monitor
    let transitions = mon.stop();

    eprintln!(
        "cold WebRTC: {n} decoded frames, transitions: {}",
        states_str(&transitions)
    );
    assert_never_stopped(&transitions, "cold webrtc");
}

/// Warm WebRTC: pipeline already running, client receives frames.
#[tokio::test]
#[serial_test::serial]
async fn test_webrtc_warm_receives_frames() {
    skip_unless!(SourceTag::Both);
    let c = client();
    ensure_data_flowing(&c).await;

    let wrtc = webrtc_connect_with_retry(MCM_SIGNALLING).await;
    let n = wrtc
        .wait_for_decoded_frames(5, Duration::from_secs(20))
        .await
        .expect("warm webrtc must decode frames");
    let parsed = wrtc.frames();
    let decoded = wrtc.decoded_frames();
    eprintln!("warm WebRTC: parsed={parsed} decoded={decoded}");
}

/// Disconnect and immediately reconnect WebRTC (user pattern 5.b).
/// Must receive frames on reconnect without errors.
#[tokio::test]
#[serial_test::serial]
async fn test_webrtc_immediate_reconnect() {
    skip_unless!(SourceTag::Both);
    let c = client();
    ensure_idle(&c).await;

    let mon = monitor();

    // First connection
    let wrtc = webrtc_connect_with_retry(MCM_SIGNALLING).await;
    wrtc.wait_for_decoded_frames(5, warm_timeout())
        .await
        .expect("first session must decode frames");
    drop(wrtc);
    eprintln!("[reconnect] first session dropped");

    // Brief pause to let the pipeline process the disconnect and settle
    // before reconnecting (avoids racing with the Draining→wake transition)
    tokio::time::sleep(Duration::from_secs(5)).await;

    // Immediately reconnect — no waiting for Idle
    let wrtc2 = webrtc_connect_with_retry(MCM_SIGNALLING).await;
    let n = wrtc2
        .wait_for_decoded_frames(5, warm_timeout())
        .await
        .expect("immediate reconnect must decode frames");

    let transitions = mon.stop();
    eprintln!(
        "immediate reconnect: {n} frames, transitions: {}",
        states_str(&transitions)
    );
    assert_never_stopped(&transitions, "webrtc immediate reconnect");
}

/// Wait for Idle, then reconnect (user pattern 5.a).
/// Must eventually receive frames. Stream must not go Stopped.
#[tokio::test]
#[serial_test::serial]
async fn test_webrtc_reconnect_after_idle() {
    skip_unless!(SourceTag::Both);
    let c = client();
    ensure_idle(&c).await;

    // First connection
    let wrtc = webrtc_connect_with_retry(MCM_SIGNALLING).await;
    wrtc.wait_for_decoded_frames(5, warm_timeout())
        .await
        .expect("first session must decode frames");
    drop(wrtc);

    // Wait for Idle
    ensure_idle(&c).await;
    eprintln!("[reconnect-after-idle] confirmed idle");
    tokio::time::sleep(Duration::from_secs(5)).await;

    // Reconnect from Idle (cold start — pipeline was fully destroyed)
    let mon = monitor();
    let wrtc2 = webrtc_connect_with_retry(MCM_SIGNALLING).await;
    let n = wrtc2
        .wait_for_decoded_frames(5, cold_timeout())
        .await
        .expect("reconnect after idle must decode frames");

    let transitions = mon.stop();
    eprintln!(
        "reconnect after idle: {n} frames, transitions: {}",
        states_str(&transitions)
    );
    assert_never_stopped(&transitions, "webrtc reconnect after idle");
}

/// Rapid WebRTC connect/disconnect cycles (3x from Idle).
/// Every cycle must deliver frames. Never go Stopped.
#[tokio::test]
#[serial_test::serial]
async fn test_webrtc_rapid_cycles() {
    skip_unless!(SourceTag::Both);
    let c = client();
    let mon = monitor();

    for cycle in 0..3 {
        ensure_idle(&c).await;

        let wrtc = webrtc_connect_with_retry(MCM_SIGNALLING).await;

        wrtc.wait_for_decoded_frames(3, warm_timeout())
            .await
            .unwrap_or_else(|e| panic!("cycle {cycle}: {e}"));

        drop(wrtc);
        eprintln!("cycle {cycle} ok");
    }

    let transitions = mon.stop();
    eprintln!("rapid cycles: transitions: {}", states_str(&transitions));
    assert_never_stopped(&transitions, "webrtc rapid cycles");
}

/// WebRTC returns to Idle after client disconnects.
#[tokio::test]
#[serial_test::serial]
async fn test_webrtc_returns_to_idle() {
    skip_unless!(SourceTag::Both);
    let c = client();
    ensure_idle(&c).await;

    let wrtc = webrtc_connect_with_retry(MCM_SIGNALLING).await;
    wrtc.wait_for_decoded_frames(5, warm_timeout())
        .await
        .expect("must decode frames");
    drop(wrtc);

    c.wait_for_stream_state(StreamStatusState::Idle, Duration::from_secs(30))
        .await
        .expect("stream must return to Idle after WebRTC disconnect");
    eprintln!("webrtc returns to idle ✓");
}

// ═══════════════════════════════════════════════════════════════════════
// RTSP TESTS
// ═══════════════════════════════════════════════════════════════════════

/// Cold RTSP: connects and receives frames. No Stopped state.
#[tokio::test]
#[serial_test::serial]
async fn test_rtsp_cold_no_stopped() {
    skip_unless!(SourceTag::Both);
    let c = client();
    ensure_idle(&c).await;

    let mon = monitor();
    assert!(
        wait_for_rtsp_available(&mcm_rtsp(), Duration::from_secs(10)).await,
        "RTSP port not reachable"
    );
    let rtsp = RtspClient::new(&mcm_rtsp()).expect("rtsp client");
    rtsp.start().expect("rtsp start");
    ensure_running(&c).await;

    let n = rtsp
        .wait_for_frames(5, warm_timeout())
        .await
        .expect("cold rtsp must receive frames");

    tokio::time::sleep(Duration::from_millis(500)).await; // Brief settle time for state monitor
    let transitions = mon.stop();
    eprintln!(
        "cold RTSP: {n} frames, transitions: {}",
        states_str(&transitions)
    );
    assert_never_stopped(&transitions, "cold rtsp");
}

/// Warm RTSP: pipeline already running, client receives frames.
#[tokio::test]
#[serial_test::serial]
async fn test_rtsp_warm_receives_frames() {
    skip_unless!(SourceTag::Both);
    let c = client();
    ensure_data_flowing(&c).await;

    assert!(
        wait_for_rtsp_available(&mcm_rtsp(), Duration::from_secs(10)).await,
        "RTSP port not reachable"
    );
    let rtsp = RtspClient::new(&mcm_rtsp()).expect("rtsp client");
    rtsp.start().expect("rtsp start");
    let n = rtsp
        .wait_for_frames(5, Duration::from_secs(30))
        .await
        .expect("warm rtsp must receive frames");
    eprintln!("warm RTSP received {n} frames");
}

/// Kill RTSP client and immediately reconnect (user pattern 3→4).
/// Must NOT get 503 Service Unavailable.
#[tokio::test]
#[serial_test::serial]
async fn test_rtsp_immediate_reconnect_no_503() {
    skip_unless!(SourceTag::Both);
    let c = client();
    ensure_idle(&c).await;

    let mon = monitor();

    // First connection
    assert!(
        wait_for_rtsp_available(&mcm_rtsp(), Duration::from_secs(10)).await,
        "RTSP port not reachable"
    );
    let rtsp = RtspClient::new(&mcm_rtsp()).expect("rtsp client 1");
    rtsp.start().expect("rtsp start 1");
    ensure_running(&c).await;
    rtsp.wait_for_frames(5, warm_timeout())
        .await
        .expect("first rtsp must receive frames");
    drop(rtsp);
    eprintln!("[rtsp-reconnect] first session dropped");

    // Immediately reconnect — pipeline should still be running (grace period)
    let rtsp2 = RtspClient::new(&mcm_rtsp()).expect("rtsp client 2");
    rtsp2.start().expect("rtsp start 2");
    let n = rtsp2
        .wait_for_frames(5, warm_timeout())
        .await
        .expect("immediate RTSP reconnect must receive frames (got 503?)");

    let transitions = mon.stop();
    eprintln!(
        "rtsp immediate reconnect: {n} frames, transitions: {}",
        states_str(&transitions)
    );
    assert_never_stopped(&transitions, "rtsp immediate reconnect");
}

/// RTSP stream must stay alive as long as a client is connected (user
/// pattern: playback stops after ~170 frames when stream goes to Idle).
#[tokio::test]
#[serial_test::serial]
async fn test_rtsp_stays_alive_while_connected() {
    skip_unless!(SourceTag::Both);
    let c = client();
    ensure_idle(&c).await;

    let mon = monitor();
    assert!(
        wait_for_rtsp_available(&mcm_rtsp(), Duration::from_secs(10)).await,
        "RTSP port not reachable"
    );
    let rtsp = RtspClient::new(&mcm_rtsp()).expect("rtsp client");
    rtsp.start().expect("rtsp start");
    ensure_running(&c).await;

    // Must continuously receive frames for 30 seconds
    rtsp.wait_for_frames(5, warm_timeout())
        .await
        .expect("rtsp must start receiving frames");

    let n = rtsp
        .wait_for_continuous_frames(Duration::from_secs(30), Duration::from_millis(500))
        .await
        .expect("RTSP must keep receiving frames for 30s while connected");

    // Stream must be Running while we're connected
    let streams = c.list_streams().await.expect("list streams");
    let state = streams.first().map(|s| s.state);
    eprintln!("rtsp alive: {n} frames over 30s, current state: {state:?}");
    assert_eq!(
        state,
        Some(StreamStatusState::Running),
        "stream must be Running while RTSP client is connected"
    );

    let transitions = mon.stop();
    assert_never_stopped(&transitions, "rtsp stays alive");
}

/// After RTSP disconnects, stream returns to Idle.
#[tokio::test]
#[serial_test::serial]
async fn test_rtsp_returns_to_idle() {
    skip_unless!(SourceTag::Both);
    let c = client();
    ensure_idle(&c).await;

    assert!(
        wait_for_rtsp_available(&mcm_rtsp(), Duration::from_secs(10)).await,
        "RTSP port not reachable"
    );
    let rtsp = RtspClient::new(&mcm_rtsp()).expect("rtsp client");
    rtsp.start().expect("rtsp start");
    ensure_running(&c).await;
    rtsp.wait_for_frames(5, warm_timeout())
        .await
        .expect("rtsp must receive frames");
    drop(rtsp);

    c.wait_for_stream_state(StreamStatusState::Idle, Duration::from_secs(30))
        .await
        .expect("stream must return to Idle after RTSP disconnect");
    eprintln!("rtsp returns to idle ✓");
}

/// Disconnect, wait for Idle, reconnect — must work (user pattern 5→6).
#[tokio::test]
#[serial_test::serial]
async fn test_rtsp_reconnect_after_idle() {
    skip_unless!(SourceTag::Both);
    let c = client();
    ensure_idle(&c).await;

    assert!(
        wait_for_rtsp_available(&mcm_rtsp(), Duration::from_secs(10)).await,
        "RTSP port not reachable"
    );
    let rtsp = RtspClient::new(&mcm_rtsp()).expect("rtsp client 1");
    rtsp.start().expect("rtsp start 1");
    ensure_running(&c).await;
    rtsp.wait_for_frames(5, warm_timeout())
        .await
        .expect("first rtsp session must get frames");
    drop(rtsp);

    ensure_idle(&c).await;
    eprintln!("[rtsp-after-idle] confirmed idle");
    tokio::time::sleep(Duration::from_secs(5)).await;

    let mon = monitor();
    assert!(
        wait_for_rtsp_available(&mcm_rtsp(), Duration::from_secs(10)).await,
        "RTSP port not reachable after idle"
    );
    let rtsp2 = RtspClient::new(&mcm_rtsp()).expect("rtsp client 2");
    rtsp2.start().expect("rtsp start 2");
    ensure_running(&c).await;
    let n = rtsp2
        .wait_for_frames(5, cold_timeout())
        .await
        .expect("RTSP reconnect after idle must receive frames");

    let transitions = mon.stop();
    eprintln!(
        "rtsp reconnect after idle: {n} frames, transitions: {}",
        states_str(&transitions)
    );
    assert_never_stopped(&transitions, "rtsp reconnect after idle");
}

// ═══════════════════════════════════════════════════════════════════════
// MIXED / CROSS-PROTOCOL TESTS
// ═══════════════════════════════════════════════════════════════════════

/// WebRTC + thumbnail concurrent: both work, WebRTC keeps getting frames.
#[tokio::test]
#[serial_test::serial]
async fn test_mixed_webrtc_and_thumbnail() {
    skip_unless!(SourceTag::Both);
    let c = client();
    ensure_idle(&c).await;

    let mon = monitor();
    let wrtc = webrtc_connect_with_retry(MCM_SIGNALLING).await;
    wrtc.wait_for_frames(3, warm_timeout())
        .await
        .expect("webrtc frames");

    let resp = thumbnail_with_retry(&c, &stream_source()).await;
    assert_eq!(resp.status(), 200, "thumbnail while webrtc active");
    let body = resp.bytes().await.unwrap();
    assert!(body.len() > 1000);

    let before = wrtc.frames();
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(
        wrtc.frames() > before,
        "webrtc must keep receiving frames after thumbnail"
    );

    let transitions = mon.stop();
    assert_never_stopped(&transitions, "mixed webrtc+thumbnail");
}

/// Full lifecycle: WebRTC → thumbnail → stop both → idle → thumbnail → WebRTC.
/// This is the full 7-step user regression.
#[tokio::test]
#[serial_test::serial]
async fn test_mixed_full_lifecycle() {
    skip_unless!(SourceTag::Both);
    let c = client();
    ensure_idle(&c).await;
    eprintln!("[STEP 0] confirmed idle");

    let mon = monitor();

    // Step 1: cold WebRTC
    let wrtc = webrtc_connect_with_retry(MCM_SIGNALLING).await;
    wrtc.wait_for_frames(5, warm_timeout())
        .await
        .expect("step1: webrtc must receive frames");
    eprintln!("[STEP 1] cold WebRTC ✓ ({} frames)", wrtc.frames());

    // Step 2: thumbnail while WebRTC active
    for i in 0..5 {
        let resp = thumbnail_with_retry(&c, &stream_source()).await;
        assert_eq!(resp.status(), 200, "step2: thumbnail #{i}");
        let body = resp.bytes().await.unwrap();
        assert!(body.len() > 1000, "step2: thumbnail #{i} too small");
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    eprintln!("[STEP 2] thumbnails while WebRTC active ✓");

    // Step 3: stop WebRTC
    drop(wrtc);
    eprintln!("[STEP 3] WebRTC dropped");

    // Step 4: stop thumbnails (just stop requesting)
    eprintln!("[STEP 4] thumbnails stopped");

    // Step 5: wait for Idle
    ensure_idle(&c).await;
    eprintln!("[STEP 5] confirmed idle");

    // Step 6: thumbnail from idle (cold — may need retries)
    let body = cold_thumbnail(&c, cold_timeout()).await;
    assert!(body.len() > 1000, "step6: thumbnail body too small");
    eprintln!("[STEP 6] thumbnail after idle ✓ ({} bytes)", body.len());

    // Step 7: WebRTC (pipeline may already be idle after transient thumbnail)
    let wrtc2 = webrtc_connect_with_retry(MCM_SIGNALLING).await;
    ensure_running(&c).await;
    wrtc2
        .wait_for_frames(5, warm_timeout())
        .await
        .expect("step7: webrtc must receive frames");
    eprintln!("[STEP 7] WebRTC after idle ✓ ({} frames)", wrtc2.frames());

    let transitions = mon.stop();
    eprintln!("full lifecycle transitions: {}", states_str(&transitions));
    assert_never_stopped(&transitions, "full lifecycle");
}

/// RTSP + thumbnail keeps stream alive (user pattern: RTSP stops after
/// ~170 frames, but requesting a thumbnail restores playback).
#[tokio::test]
#[serial_test::serial]
async fn test_mixed_rtsp_plus_thumbnail_keeps_alive() {
    skip_unless!(SourceTag::Both);
    let c = client();
    ensure_idle(&c).await;

    let mon = monitor();
    assert!(
        wait_for_rtsp_available(&mcm_rtsp(), Duration::from_secs(10)).await,
        "RTSP port not reachable"
    );
    let rtsp = RtspClient::new(&mcm_rtsp()).expect("rtsp client");
    rtsp.start().expect("rtsp start");
    ensure_running(&c).await;
    rtsp.wait_for_frames(5, warm_timeout())
        .await
        .expect("rtsp must start");

    // Request thumbnails every 2s while RTSP is connected for 20s.
    // Tolerate transient 503s (thumbnail grab can timeout internally)
    // but require at least one 200 within any 3 consecutive attempts.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    let mut consecutive_503 = 0u32;
    while tokio::time::Instant::now() < deadline {
        let resp = thumbnail_with_retry(&c, &stream_source()).await;
        if resp.status() == 200 {
            consecutive_503 = 0;
        } else {
            consecutive_503 += 1;
            assert!(
                consecutive_503 < 3,
                "thumbnail returned non-200 ({}) 3 times in a row while rtsp active",
                resp.status()
            );
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }

    // RTSP must still have frames flowing
    let before = rtsp.frames();
    tokio::time::sleep(Duration::from_secs(2)).await;
    let after = rtsp.frames();
    assert!(
        after > before,
        "RTSP must keep getting frames during thumbnail coexistence (before={before}, after={after})"
    );

    let transitions = mon.stop();
    eprintln!(
        "rtsp+thumb: {} frames, transitions: {}",
        after,
        states_str(&transitions)
    );
    assert_never_stopped(&transitions, "rtsp+thumbnail coexistence");
}

/// Extended cycle test with DECODER verification.
/// Checks that decoded frames (not just parsed NAL units) are produced.
/// This catches the warm-connection failure where decoder drops all frames.
#[tokio::test]
#[serial_test::serial]
async fn test_webrtc_extended_cycles() {
    skip_unless!(SourceTag::Both);
    let c = client();
    let mon = monitor();

    for cycle in 0..5u32 {
        ensure_idle(&c).await;

        let wrtc = webrtc_connect_with_retry(MCM_SIGNALLING).await;
        wrtc.wait_for_decoded_frames(5, warm_timeout())
            .await
            .unwrap_or_else(|e| panic!("cycle {cycle}: {e}"));

        // Stay connected for a realistic duration matching the repro steps
        tokio::time::sleep(Duration::from_secs(5)).await;

        let parsed = wrtc.frames();
        let decoded = wrtc.decoded_frames();
        drop(wrtc);
        eprintln!("cycle {cycle} ok — parsed={parsed} decoded={decoded}");
    }

    let transitions = mon.stop();
    eprintln!("extended cycles: transitions: {}", states_str(&transitions));
    assert_never_stopped(&transitions, "webrtc extended cycles");
}

/// Multi-warm sequential test: RTSP keeps the pipeline running while multiple
/// WebRTC clients connect/disconnect sequentially. This tests the "3rd+ always fail"
/// pattern where the pipeline never goes to Idle between WebRTC connections.
#[tokio::test]
#[serial_test::serial]
async fn test_webrtc_multi_warm_sequential() {
    skip_unless!(SourceTag::Both);
    let c = client();
    ensure_idle(&c).await;

    // Start RTSP to keep the pipeline Running throughout
    assert!(
        wait_for_rtsp_available(&mcm_rtsp(), Duration::from_secs(10)).await,
        "RTSP port not reachable"
    );
    let rtsp = RtspClient::new(&mcm_rtsp()).expect("RTSP client");
    rtsp.start().expect("RTSP start");
    rtsp.wait_for_frames(5, cold_timeout())
        .await
        .expect("RTSP must receive frames");
    eprintln!("RTSP baseline established");

    // Sequential WebRTC connections to the already-Running pipeline
    for cycle in 0..5u32 {
        let wrtc = webrtc_connect_with_retry(MCM_SIGNALLING).await;
        wrtc.wait_for_decoded_frames(5, warm_timeout())
            .await
            .unwrap_or_else(|e| panic!("warm cycle {cycle}: {e}"));

        // Stay connected for a realistic duration
        tokio::time::sleep(Duration::from_secs(5)).await;

        let parsed = wrtc.frames();
        let decoded = wrtc.decoded_frames();
        eprintln!("warm cycle {cycle} ok — parsed={parsed} decoded={decoded}");
        drop(wrtc);

        // Brief pause between WebRTC connections
        tokio::time::sleep(Duration::from_secs(1)).await;
    }

    // Verify RTSP still flowing
    let before = rtsp.frames();
    tokio::time::sleep(Duration::from_secs(1)).await;
    let after = rtsp.frames();
    assert!(
        after > before,
        "RTSP must still flow after WebRTC cycles (before={before}, after={after})"
    );
}

// ═══════════════════════════════════════════════════════════════════════
// BROWSER WEBRTC TESTS — headless Chrome via WebDriver
// ═══════════════════════════════════════════════════════════════════════

/// Browser-based cold-vs-warm WebRTC decode test.
///
/// Uses headless Chrome via WebDriver to connect/disconnect 5 times,
/// verifying that `RTCPeerConnection.getStats()` reports `framesDecoded > 0`
/// on every cycle — including warm reconnections where the GStreamer pipeline
/// was never torn down.
///
/// Requires `chromedriver` and `google-chrome` (or `chromium`) in PATH.
/// Run with: `cargo test --features webrtc-test test_webrtc_browser_warm_decode`
#[cfg(feature = "webrtc-test")]
#[tokio::test]
#[serial_test::serial]
async fn test_webrtc_browser_warm_decode() {
    skip_unless!(SourceTag::Both);
    let c = client();
    ensure_idle(&c).await;

    let frontend_url = format!("{MCM_REST}/webrtc/index.html");
    let browser = BrowserWebrtcClient::new()
        .await
        .expect("Failed to create browser client");

    for cycle in 0..5u32 {
        let is_cold = cycle == 0;
        let playing_timeout = if is_cold {
            cold_timeout()
        } else {
            warm_timeout()
        };
        let decode_timeout = if is_cold {
            Duration::from_secs(30)
        } else {
            Duration::from_secs(20)
        };

        browser
            .connect(&frontend_url)
            .await
            .unwrap_or_else(|e| panic!("cycle {cycle}: connect failed: {e}"));

        browser
            .wait_for_playing(playing_timeout)
            .await
            .unwrap_or_else(|e| panic!("cycle {cycle}: never reached Playing: {e}"));

        let stats = browser
            .wait_for_decoded_frames(5, decode_timeout)
            .await
            .unwrap_or_else(|e| panic!("cycle {cycle}: {e}"));

        eprintln!(
            "cycle {cycle}: decoded={} received={} dropped={} keyFrames={}",
            stats.frames_decoded,
            stats.frames_received,
            stats.frames_dropped,
            stats.key_frames_decoded,
        );

        browser
            .disconnect()
            .await
            .unwrap_or_else(|e| panic!("cycle {cycle}: disconnect failed: {e}"));

        tokio::time::sleep(Duration::from_secs(2)).await;
    }

    browser.quit().await.ok();
}
