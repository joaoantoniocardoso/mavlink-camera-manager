mod protocol;
mod rtsp_client;
mod udp_client;
mod webrtc_client;

use std::{
    collections::HashMap,
    hash::{DefaultHasher, Hasher},
    io::{BufWriter, Write},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU32, Ordering},
    },
    time::{Duration, Instant, SystemTime},
};

use anyhow::{Context, Result, anyhow};
use clap::{Parser, ValueEnum};
use gst::prelude::*;
use pcap_file::pcap::{PcapHeader, PcapPacket, PcapReader, PcapWriter};
use serde::Serialize;
use tokio::sync::mpsc;
use uuid::Uuid;

// ── Shared types ────────────────────────────────────────────────────────────

pub struct FrameSample {
    pub content_hash: u64,
    pub relative_pts_ms: i64,
    pub arrival: Instant,
    pub buffer_size: usize,
    pub is_keyframe: bool,
    pub rtp_packets: u32,
    pub rtp_span_us: u64,
    pub vcl_bytes: usize,
    pub filler_bytes: usize,
}

/// Tracks per-frame RTP packet statistics across the depay.sink → parse.src
/// boundary. Both probes run on the same GStreamer streaming thread, so the
/// atomic/mutex usage is for API safety, not contention.
pub struct RtpTracker {
    packet_count: AtomicU32,
    first_packet_time: Mutex<Option<Instant>>,
    last_packet_time: Mutex<Option<Instant>>,
}

impl RtpTracker {
    pub fn new() -> Self {
        Self {
            packet_count: AtomicU32::new(0),
            first_packet_time: Mutex::new(None),
            last_packet_time: Mutex::new(None),
        }
    }

    fn record_packet(&self) {
        let now = Instant::now();
        let count = self.packet_count.fetch_add(1, Ordering::Relaxed);
        if count == 0 {
            *self.first_packet_time.lock().unwrap() = Some(now);
        }
        *self.last_packet_time.lock().unwrap() = Some(now);
    }

    fn take(&self) -> (u32, u64) {
        let count = self.packet_count.swap(0, Ordering::Relaxed);
        let first = self.first_packet_time.lock().unwrap().take();
        let last = self.last_packet_time.lock().unwrap().take();
        let span_us = match (first, last) {
            (Some(f), Some(l)) if l > f => l.duration_since(f).as_micros() as u64,
            _ => 0,
        };
        (count, span_us)
    }
}

/// Attach a pad probe on depay.sink that counts incoming RTP packets.
pub fn attach_rtp_counter(pad: &gst::Pad, tracker: Arc<RtpTracker>) {
    pad.add_probe(
        gst::PadProbeType::BUFFER | gst::PadProbeType::BUFFER_LIST,
        move |_, info| {
            match &info.data {
                Some(gst::PadProbeData::Buffer(_)) => {
                    tracker.record_packet();
                }
                Some(gst::PadProbeData::BufferList(list)) => {
                    for _ in 0..list.len() {
                        tracker.record_packet();
                    }
                }
                _ => {}
            }
            gst::PadProbeReturn::Ok
        },
    );
}

pub type SampleSender = mpsc::UnboundedSender<FrameSample>;

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum Codec {
    H264,
    H265,
}

struct NalInfo {
    content_hash: u64,
    vcl_bytes: usize,
    filler_bytes: usize,
}

/// Hash only VCL NAL units from an H.264/H.265 byte-stream buffer.
/// This produces a stable hash across different pipeline processing chains
/// (SPS/PPS injection, stream-format conversion, etc.) because the actual
/// coded slice data is never modified by parse/pay/depay elements.
///
/// Also classifies per-frame byte composition: VCL (types 1-5) vs filler (type 12).
fn classify_nals(data: &[u8]) -> NalInfo {
    let mut hasher = DefaultHasher::new();
    let mut vcl_bytes = 0usize;
    let mut filler_bytes = 0usize;
    let mut i = 0;
    while i < data.len() {
        let (sc_len, nal_start) = if i + 3 < data.len() && data[i] == 0 && data[i + 1] == 0 {
            if data[i + 2] == 1 {
                (3, i + 3)
            } else if i + 4 <= data.len() && data[i + 2] == 0 && data[i + 3] == 1 {
                (4, i + 4)
            } else {
                i += 1;
                continue;
            }
        } else {
            i += 1;
            continue;
        };

        if nal_start >= data.len() {
            break;
        }

        let mut nal_end = data.len();
        for j in nal_start..data.len().saturating_sub(2) {
            if data[j] == 0
                && data[j + 1] == 0
                && (data[j + 2] == 1
                    || (j + 3 < data.len() && data[j + 2] == 0 && data[j + 3] == 1))
            {
                nal_end = j;
                break;
            }
        }

        let nal_type = data[nal_start] & 0x1F;
        let nal_len = nal_end - nal_start;
        if (1..=5).contains(&nal_type) {
            hasher.write(&data[nal_start..nal_end]);
            vcl_bytes += nal_len;
        } else if nal_type == 12 {
            filler_bytes += nal_len;
        }

        i = if nal_end > nal_start + sc_len {
            nal_end
        } else {
            nal_start + 1
        };
    }

    if vcl_bytes == 0 {
        hasher.write(data);
    }

    NalInfo {
        content_hash: hasher.finish(),
        vcl_bytes,
        filler_bytes,
    }
}

/// Attach a pad probe that hashes each buffer's VCL NAL content and records
/// the hash together with (relative_pts_ms, wall-clock Instant). Matching by
/// VCL content hash works across different processing chains (depay/parse/pay)
/// because the coded slice data passes through unchanged.
pub fn attach_frame_probe(
    pad: &gst::Pad,
    client_name: String,
    sender: SampleSender,
    rtp_tracker: Option<Arc<RtpTracker>>,
) {
    let first_pts: Arc<Mutex<Option<u64>>> = Arc::new(Mutex::new(None));

    pad.add_probe(gst::PadProbeType::BUFFER, move |_, info| {
        let Some(gst::PadProbeData::Buffer(ref buffer)) = info.data else {
            return gst::PadProbeReturn::Ok;
        };

        let arrival = Instant::now();
        let is_keyframe = !buffer.flags().contains(gst::BufferFlags::DELTA_UNIT);

        let Ok(map) = buffer.map_readable() else {
            return gst::PadProbeReturn::Ok;
        };
        let buffer_size = map.len();
        let nal_info = classify_nals(map.as_slice());

        let relative_pts_ms = buffer.pts().map_or(-1, |pts| {
            let pts_ns = pts.nseconds();
            let mut first = first_pts.lock().unwrap();
            let base = *first.get_or_insert(pts_ns);
            ((pts_ns - base) / 1_000_000) as i64
        });

        let (rtp_packets, rtp_span_us) = rtp_tracker.as_ref().map(|t| t.take()).unwrap_or((0, 0));

        if sender
            .send(FrameSample {
                content_hash: nal_info.content_hash,
                relative_pts_ms,
                arrival,
                buffer_size,
                is_keyframe,
                rtp_packets,
                rtp_span_us,
                vcl_bytes: nal_info.vcl_bytes,
                filler_bytes: nal_info.filler_bytes,
            })
            .is_err()
        {
            eprintln!("[{client_name}] Sample channel closed");
        }

        gst::PadProbeReturn::Ok
    });
}

// ── RTP pcap recorder ───────────────────────────────────────────────────────

pub struct PcapRecorder {
    writer: Mutex<PcapWriter<BufWriter<std::fs::File>>>,
    epoch: SystemTime,
}

impl PcapRecorder {
    pub fn new(path: &str) -> Result<Self> {
        if let Some(parent) = std::path::Path::new(path).parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = BufWriter::new(std::fs::File::create(path)?);
        let header = PcapHeader {
            datalink: pcap_file::DataLink::RAW,
            ..PcapHeader::default()
        };
        let writer = PcapWriter::with_header(file, header)
            .map_err(|e| anyhow!("Failed to create pcap writer: {e}"))?;
        eprintln!("Recording RTP to {path}");
        Ok(Self {
            writer: Mutex::new(writer),
            epoch: SystemTime::now(),
        })
    }

    /// Write a single RTP packet wrapped in fabricated IPv4+UDP headers.
    pub fn write_rtp_packet(&self, rtp_data: &[u8]) {
        let udp_len = (8 + rtp_data.len()) as u16;
        let ip_total_len = (20 + 8 + rtp_data.len()) as u16;

        let mut pkt = Vec::with_capacity(20 + 8 + rtp_data.len());

        // IPv4 header (20 bytes)
        pkt.extend_from_slice(&[
            0x45, 0x00, // version+IHL, DSCP
        ]);
        pkt.extend_from_slice(&ip_total_len.to_be_bytes());
        pkt.extend_from_slice(&[
            0x00, 0x00, // identification
            0x40, 0x00, // flags (DF) + fragment offset
            0x40, // TTL=64
            0x11, // protocol=UDP
            0x00, 0x00, // checksum (ignored)
            127, 0, 0, 1, // src IP
            127, 0, 0, 1, // dst IP
        ]);

        // UDP header (8 bytes)
        pkt.extend_from_slice(&0u16.to_be_bytes()); // src port
        pkt.extend_from_slice(&5004u16.to_be_bytes()); // dst port
        pkt.extend_from_slice(&udp_len.to_be_bytes()); // length
        pkt.extend_from_slice(&0u16.to_be_bytes()); // checksum

        // RTP payload
        pkt.extend_from_slice(rtp_data);

        let timestamp = self.epoch.elapsed().unwrap_or_default();
        let pcap_pkt = PcapPacket::new(timestamp, pkt.len() as u32, &pkt);

        if let Ok(mut w) = self.writer.lock() {
            let _ = w.write_packet(&pcap_pkt);
        }
    }
}

/// Attach a pad probe that writes each RTP buffer to a pcap file.
pub fn attach_rtp_recorder(pad: &gst::Pad, recorder: Arc<PcapRecorder>) {
    pad.add_probe(gst::PadProbeType::BUFFER, move |_, info| {
        if let Some(gst::PadProbeData::Buffer(ref buffer)) = info.data {
            if let Ok(map) = buffer.map_readable() {
                recorder.write_rtp_packet(map.as_slice());
            }
        }
        gst::PadProbeReturn::Ok
    });
}

// ── CLI ─────────────────────────────────────────────────────────────────────

#[derive(Parser, Clone)]
#[command(
    name = "stream_latency",
    about = "Measure pairwise latency between RTSP / WebRTC / UDP stream transports"
)]
struct Args {
    /// RTSP URL(s) to receive from (repeatable)
    #[arg(long = "rtsp", value_name = "URL")]
    rtsp_urls: Vec<String>,

    /// WebRTC signalling server WebSocket URL(s) (repeatable)
    #[arg(long = "webrtc", value_name = "WS_URL")]
    webrtc_urls: Vec<String>,

    /// WebRTC producer/stream UUID (auto-detected when only one stream exists)
    #[arg(
        long = "producer-id",
        value_name = "UUID",
        conflicts_with = "stream_name"
    )]
    producer_id: Option<Uuid>,

    /// WebRTC stream name (for example: "RadCam 192.168.2.10/0")
    #[arg(
        long = "stream-name",
        value_name = "NAME",
        conflicts_with = "producer_id"
    )]
    stream_name: Option<String>,

    /// UDP endpoint(s) to listen on as ADDR:PORT (repeatable)
    #[arg(long = "udp", value_name = "ADDR:PORT")]
    udp_endpoints: Vec<String>,

    /// Codec hint for UDP streams
    #[arg(long, default_value = "h264")]
    codec: Codec,

    /// Measurement duration in seconds (per run)
    #[arg(long, default_value = "30")]
    duration: u64,

    /// Periodic report interval in seconds
    #[arg(long, default_value = "10")]
    report_interval: u64,

    /// Warmup period in seconds (discard initial samples)
    #[arg(long, default_value = "2")]
    warmup: u64,

    /// Number of runs to perform (results aggregated across runs)
    #[arg(long, default_value = "1")]
    runs: u32,

    /// Pause between runs in seconds (allows pipeline to stabilize)
    #[arg(long, default_value = "3")]
    run_pause: u64,

    /// CSV output directory (files named run_1.csv, run_2.csv, ...)
    #[arg(long)]
    csv: Option<String>,

    /// JSON summary output path
    #[arg(long)]
    json: Option<String>,

    /// Directory to save RTP recordings as pcap (files named run_<N>_<client-name>.pcap)
    #[arg(long)]
    record: Option<String>,

    /// Retry on connection errors instead of exiting (for long-running tests)
    #[arg(long)]
    resilient: bool,

    /// Delay between retry attempts in seconds (requires --resilient)
    #[arg(long, default_value = "2")]
    retry_delay: u64,

    /// Re-analyze existing CSV files from a results directory (skip live capture)
    #[arg(long, value_name = "DIR")]
    analyze: Option<String>,

    /// Render MKV video(s) with a stats HUD overlay from pcap + CSV data in DIR
    #[arg(long, value_name = "DIR")]
    render: Option<String>,

    /// When used with --render, produce a short clip around a specific event
    /// instead of the full video.  Accepts: "worst", "worst-drop",
    /// "worst-freeze", "mildest", "mildest-drop", "mildest-freeze",
    /// or a time in seconds (e.g. "432.0").
    #[arg(long, value_name = "EVENT", default_value = None)]
    clip_event: Option<String>,

    /// Half-width of the clip window in seconds (default 5 = ±5s = 10s clip)
    #[arg(long, default_value = "5")]
    clip_radius: f64,

    /// Use this client's worst event as the clip center for ALL clients
    /// (e.g. --clip-source webrtc-0).  Without this flag each client clips
    /// around its own worst event independently.
    #[arg(long, value_name = "CLIENT")]
    clip_source: Option<String>,

    /// Simulate a jitter buffer of the given depth (in ms) when rendering.
    /// Frames are presented at max(arrival, rtp_pts + offset + depth) so that
    /// arrival jitter within the buffer window is smoothed out and only delays
    /// exceeding the buffer depth produce visible freezes.
    #[arg(long, value_name = "MS")]
    jitterbuffer: Option<f64>,
}

// ── Correlator / Reporter ───────────────────────────────────────────────────

struct ClientData {
    name: String,
    receiver: mpsc::UnboundedReceiver<FrameSample>,
    /// content_hash → (relative_pts_ms, arrival, buffer_size, is_keyframe, rtp_packets, rtp_span_us, vcl_bytes, filler_bytes)
    samples: HashMap<u64, (i64, Instant, usize, bool, u32, u64, usize, usize)>,
    /// Arrival-ordered: (arrival, buffer_size, is_keyframe, rtp_packets, rtp_span_us, vcl_bytes, filler_bytes)
    arrivals: Vec<(Instant, usize, bool, u32, u64, usize, usize)>,
    /// Wall-clock time when the last frame was received (for per-client starvation detection)
    last_frame_time: Option<Instant>,
}

fn drain_samples(clients: &mut [ClientData]) {
    for client in clients.iter_mut() {
        while let Ok(sample) = client.receiver.try_recv() {
            client.last_frame_time = Some(sample.arrival);
            client.samples.insert(
                sample.content_hash,
                (
                    sample.relative_pts_ms,
                    sample.arrival,
                    sample.buffer_size,
                    sample.is_keyframe,
                    sample.rtp_packets,
                    sample.rtp_span_us,
                    sample.vcl_bytes,
                    sample.filler_bytes,
                ),
            );
            client.arrivals.push((
                sample.arrival,
                sample.buffer_size,
                sample.is_keyframe,
                sample.rtp_packets,
                sample.rtp_span_us,
                sample.vcl_bytes,
                sample.filler_bytes,
            ));
        }
    }
}

/// For each frame in `a` that also appears in `b` (matched by VCL content hash),
/// compute the signed arrival-time delta (positive = b arrived later).
fn compute_deltas(
    a: &HashMap<u64, (i64, Instant, usize, bool, u32, u64, usize, usize)>,
    b: &HashMap<u64, (i64, Instant, usize, bool, u32, u64, usize, usize)>,
) -> Vec<i64> {
    let mut deltas = Vec::new();

    for (hash, &(_, a_arrival, _, _, _, _, _, _)) in a {
        if let Some(&(_, b_arrival, _, _, _, _, _, _)) = b.get(hash) {
            let delta_us = if b_arrival >= a_arrival {
                b_arrival.duration_since(a_arrival).as_micros() as i64
            } else {
                -(a_arrival.duration_since(b_arrival).as_micros() as i64)
            };
            deltas.push(delta_us);
        }
    }

    deltas.sort();
    deltas
}

/// Like `compute_deltas`, but splits into (keyframe_deltas, pframe_deltas)
/// using `a`'s keyframe flag (the RTSP side, closer to camera truth).
fn compute_deltas_by_type(
    a: &HashMap<u64, (i64, Instant, usize, bool, u32, u64, usize, usize)>,
    b: &HashMap<u64, (i64, Instant, usize, bool, u32, u64, usize, usize)>,
) -> (Vec<i64>, Vec<i64>) {
    let mut kf_deltas = Vec::new();
    let mut pf_deltas = Vec::new();

    for (hash, &(_, a_arrival, _, is_kf, _, _, _, _)) in a {
        if let Some(&(_, b_arrival, _, _, _, _, _, _)) = b.get(hash) {
            let delta_us = if b_arrival >= a_arrival {
                b_arrival.duration_since(a_arrival).as_micros() as i64
            } else {
                -(a_arrival.duration_since(b_arrival).as_micros() as i64)
            };
            if is_kf {
                kf_deltas.push(delta_us);
            } else {
                pf_deltas.push(delta_us);
            }
        }
    }

    kf_deltas.sort();
    pf_deltas.sort();
    (kf_deltas, pf_deltas)
}

fn percentile(sorted: &[i64], p: f64) -> i64 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

fn format_us(us: i64) -> String {
    if us.unsigned_abs() >= 1_000_000 {
        format!("{:.1}s", us as f64 / 1_000_000.0)
    } else if us.unsigned_abs() >= 1_000 {
        format!("{:.1}ms", us as f64 / 1_000.0)
    } else {
        format!("{us}us")
    }
}

// ── Stutter / drop detection ────────────────────────────────────────────────
//
// Single-pass disruption-window classifier. Each inter-arrival gap is
// classified into exactly one mutually-exclusive category:
//
//   freeze (> 1.5x expected)  ─┐
//   burst  (< 0.5x expected)  ─┤  grouped into disruption windows
//                               └─► classified as true_drop OR freeze_burst
//   isolated_stutter (< 0.5x, outside any window)
//   normal (everything else)

#[derive(Debug, Clone, Serialize)]
struct StutterStats {
    true_drop_events: usize,
    estimated_missed_frames: usize,
    freeze_burst_events: usize,
    freeze_burst_severity_us: i64,
    freeze_burst_at_keyframe: usize,
    freeze_burst_at_delta: usize,
    isolated_stutter_events: usize,
    disruption_episodes: usize,
    disruption_episode_frames: usize,
    disruption_episode_severity_us: i64,
    nominal_fps: f64,
}

fn detect_stutters(
    inter_arrival_us: &[i64],
    is_keyframe: &[bool],
    nominal_fps: f64,
) -> StutterStats {
    let zero = StutterStats {
        true_drop_events: 0,
        estimated_missed_frames: 0,
        freeze_burst_events: 0,
        freeze_burst_severity_us: 0,
        freeze_burst_at_keyframe: 0,
        freeze_burst_at_delta: 0,
        isolated_stutter_events: 0,
        disruption_episodes: 0,
        disruption_episode_frames: 0,
        disruption_episode_severity_us: 0,
        nominal_fps,
    };

    if inter_arrival_us.len() < 2 || nominal_fps <= 0.0 {
        return zero;
    }

    let expected_us = 1_000_000.0 / nominal_fps;
    let freeze_threshold = expected_us * 1.5;
    let burst_threshold = expected_us * 0.5;

    let mut stats = zero;

    // Accumulator for the current disruption window.
    let mut in_window = false;
    let mut window_total_us = 0.0f64;
    let mut window_gaps = 0usize;
    let mut window_has_burst = false;
    let mut window_severity_us = 0.0f64;
    // Index of the first freeze gap that opened this window (for keyframe lookup).
    let mut window_start_idx = 0usize;

    // Classifies and commits the accumulated disruption window.
    let classify_window = |stats: &mut StutterStats,
                           total_us: f64,
                           gaps: usize,
                           has_burst: bool,
                           severity_us: f64,
                           start_idx: usize,
                           is_keyframe: &[bool]| {
        let expected_frames = (total_us / expected_us).round() as usize;
        let deficit = expected_frames.saturating_sub(gaps);

        stats.disruption_episodes += 1;
        stats.disruption_episode_frames += gaps;
        stats.disruption_episode_severity_us += severity_us as i64;

        if deficit > 0 {
            stats.true_drop_events += 1;
            stats.estimated_missed_frames += deficit;
        } else if has_burst {
            stats.freeze_burst_events += 1;
            stats.freeze_burst_severity_us += severity_us as i64;
            // The frame arriving after the initial freeze is is_keyframe[start_idx + 1].
            if is_keyframe.get(start_idx + 1).copied().unwrap_or(false) {
                stats.freeze_burst_at_keyframe += 1;
            } else {
                stats.freeze_burst_at_delta += 1;
            }
        }
        // else: isolated freeze (long gap, no burst, no loss) — already
        // counted in disruption_episodes.
    };

    for (i, &ia) in inter_arrival_us.iter().enumerate() {
        let ia_f = ia as f64;
        let is_freeze = ia_f > freeze_threshold;
        let is_burst = ia_f < burst_threshold;

        if in_window {
            if is_freeze || is_burst {
                window_gaps += 1;
                window_total_us += ia_f;
                window_has_burst |= is_burst;
                if is_freeze {
                    window_severity_us += ia_f - expected_us;
                }
            } else {
                classify_window(
                    &mut stats,
                    window_total_us,
                    window_gaps,
                    window_has_burst,
                    window_severity_us,
                    window_start_idx,
                    is_keyframe,
                );
                in_window = false;
            }
        } else if is_freeze {
            in_window = true;
            window_start_idx = i;
            window_gaps = 1;
            window_total_us = ia_f;
            window_has_burst = false;
            window_severity_us = ia_f - expected_us;
        } else if is_burst {
            stats.isolated_stutter_events += 1;
        }
    }

    if in_window {
        classify_window(
            &mut stats,
            window_total_us,
            window_gaps,
            window_has_burst,
            window_severity_us,
            window_start_idx,
            is_keyframe,
        );
    }

    stats
}

// ── Per-frame annotation for HUD video rendering ────────────────────────────

#[derive(Debug, Clone)]
enum FrameAnnotation {
    Normal,
    Freeze { severity_us: i64 },
    Burst,
    TrueDrop { deficit: usize },
    IsolatedStutter,
}

/// Like `detect_stutters`, but returns a per-gap classification vector instead
/// of aggregate counts.  `annotations[i]` describes the inter-arrival gap
/// between frame `i` and frame `i+1`.
fn annotate_frames(
    inter_arrival_us: &[i64],
    is_keyframe: &[bool],
    nominal_fps: f64,
) -> Vec<FrameAnnotation> {
    let n = inter_arrival_us.len();
    let mut out = vec![FrameAnnotation::Normal; n];

    if n < 2 || nominal_fps <= 0.0 {
        return out;
    }

    let expected_us = 1_000_000.0 / nominal_fps;
    let freeze_threshold = expected_us * 1.5;
    let burst_threshold = expected_us * 0.5;

    let mut in_window = false;
    let mut window_start = 0usize;
    let mut window_end = 0usize;
    let mut window_total_us = 0.0f64;
    let mut window_gaps = 0usize;
    let mut window_has_burst = false;
    let mut window_severity_us = 0.0f64;

    let classify_window = |out: &mut Vec<FrameAnnotation>,
                           start: usize,
                           end: usize,
                           total_us: f64,
                           gaps: usize,
                           has_burst: bool,
                           severity_us: f64,
                           _is_keyframe: &[bool]| {
        let expected_frames = (total_us / expected_us).round() as usize;
        let deficit = expected_frames.saturating_sub(gaps);

        if deficit > 0 {
            for idx in start..=end {
                out[idx] = FrameAnnotation::TrueDrop { deficit };
            }
        } else if has_burst {
            for idx in start..=end {
                let ia_f = inter_arrival_us[idx] as f64;
                if ia_f > freeze_threshold {
                    out[idx] = FrameAnnotation::Freeze {
                        severity_us: severity_us as i64,
                    };
                } else if ia_f < burst_threshold {
                    out[idx] = FrameAnnotation::Burst;
                }
            }
        } else {
            for idx in start..=end {
                out[idx] = FrameAnnotation::Freeze {
                    severity_us: severity_us as i64,
                };
            }
        }
    };

    for (i, &ia) in inter_arrival_us.iter().enumerate() {
        let ia_f = ia as f64;
        let is_freeze = ia_f > freeze_threshold;
        let is_burst = ia_f < burst_threshold;

        if in_window {
            if is_freeze || is_burst {
                window_end = i;
                window_gaps += 1;
                window_total_us += ia_f;
                window_has_burst |= is_burst;
                if is_freeze {
                    window_severity_us += ia_f - expected_us;
                }
            } else {
                classify_window(
                    &mut out,
                    window_start,
                    window_end,
                    window_total_us,
                    window_gaps,
                    window_has_burst,
                    window_severity_us,
                    is_keyframe,
                );
                in_window = false;
            }
        } else if is_freeze {
            in_window = true;
            window_start = i;
            window_end = i;
            window_gaps = 1;
            window_total_us = ia_f;
            window_has_burst = false;
            window_severity_us = ia_f - expected_us;
        } else if is_burst {
            out[i] = FrameAnnotation::IsolatedStutter;
        }
    }

    if in_window {
        classify_window(
            &mut out,
            window_start,
            window_end,
            window_total_us,
            window_gaps,
            window_has_burst,
            window_severity_us,
            is_keyframe,
        );
    }

    out
}

// ── JSON summary types ──────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
struct RunSummary {
    run_index: u32,
    duration_s: f64,
    clients: Vec<ClientSummary>,
    pairs: Vec<PairSummary>,
}

#[derive(Debug, Serialize)]
struct ClientSummary {
    name: String,
    frames: usize,
    fps: f64,
    bitrate_mbps: f64,
    avg_frame_kb: f64,
    jitter_stddev_us: f64,
    inter_arrival_p50_us: i64,
    inter_arrival_p95_us: i64,
    inter_arrival_p99_us: i64,
    inter_arrival_max_us: i64,
    stutters: StutterStats,
    keyframe_count: usize,
    keyframe_avg_bytes: f64,
    keyframe_avg_rtp_packets: f64,
    keyframe_avg_rtp_span_us: f64,
    pframe_count: usize,
    pframe_avg_bytes: f64,
    pframe_avg_rtp_packets: f64,
    pframe_avg_rtp_span_us: f64,
}

#[derive(Debug, Serialize)]
struct PairSummary {
    client_a: String,
    client_b: String,
    matched_frames: usize,
    total_frames_a: usize,
    match_pct: f64,
    delta_mean_us: f64,
    delta_p50_us: i64,
    delta_p95_us: i64,
    delta_p99_us: i64,
    delta_min_us: i64,
    delta_max_us: i64,
    delta_stddev_us: f64,
    delta_keyframe_mean_us: f64,
    delta_keyframe_p50_us: i64,
    delta_keyframe_p95_us: i64,
    delta_keyframe_count: usize,
    delta_pframe_mean_us: f64,
    delta_pframe_p50_us: i64,
    delta_pframe_p95_us: i64,
    delta_pframe_count: usize,
}

#[derive(Debug, Serialize)]
struct AggregatedSummary {
    runs: Vec<RunSummary>,
    aggregate: AggregateCrossRun,
}

#[derive(Debug, Serialize)]
struct AggregateCrossRun {
    n_runs: u32,
    per_client: Vec<AggregateClientStats>,
    per_pair: Vec<AggregatePairStats>,
}

#[derive(Debug, Serialize)]
struct AggregateClientStats {
    name: String,
    fps_mean: f64,
    fps_stddev: f64,
    jitter_mean_us: f64,
    jitter_stddev_us: f64,
    true_drop_events_mean: f64,
    isolated_stutter_events_mean: f64,
    freeze_burst_events_mean: f64,
    freeze_burst_at_keyframe_mean: f64,
    freeze_burst_at_delta_mean: f64,
    disruption_episodes_mean: f64,
}

#[derive(Debug, Serialize)]
struct AggregatePairStats {
    client_a: String,
    client_b: String,
    delta_mean_of_means_us: f64,
    delta_mean_of_p50_us: f64,
    delta_mean_of_p95_us: f64,
    delta_mean_of_p99_us: f64,
    delta_stddev_of_means_us: f64,
    delta_keyframe_mean_of_means_us: f64,
    delta_keyframe_mean_of_p50_us: f64,
    delta_keyframe_mean_of_p95_us: f64,
    delta_pframe_mean_of_means_us: f64,
    delta_pframe_mean_of_p50_us: f64,
    delta_pframe_mean_of_p95_us: f64,
}

// ── Statistics helpers ──────────────────────────────────────────────────────

fn mean_f64(vals: &[f64]) -> f64 {
    if vals.is_empty() {
        return 0.0;
    }
    vals.iter().sum::<f64>() / vals.len() as f64
}

fn stddev_f64(vals: &[f64]) -> f64 {
    if vals.len() < 2 {
        return 0.0;
    }
    let m = mean_f64(vals);
    let var = vals.iter().map(|v| (v - m).powi(2)).sum::<f64>() / (vals.len() - 1) as f64;
    var.sqrt()
}

// ── Per-client stats computation ────────────────────────────────────────────

fn compute_client_summary(client: &ClientData) -> ClientSummary {
    let n = client.arrivals.len();
    if n < 2 {
        return ClientSummary {
            name: client.name.clone(),
            frames: n,
            fps: 0.0,
            bitrate_mbps: 0.0,
            avg_frame_kb: 0.0,
            jitter_stddev_us: 0.0,
            inter_arrival_p50_us: 0,
            inter_arrival_p95_us: 0,
            inter_arrival_p99_us: 0,
            inter_arrival_max_us: 0,
            stutters: StutterStats {
                true_drop_events: 0,
                estimated_missed_frames: 0,
                freeze_burst_events: 0,
                freeze_burst_severity_us: 0,
                freeze_burst_at_keyframe: 0,
                freeze_burst_at_delta: 0,
                isolated_stutter_events: 0,
                disruption_episodes: 0,
                disruption_episode_frames: 0,
                disruption_episode_severity_us: 0,
                nominal_fps: 0.0,
            },
            keyframe_count: 0,
            keyframe_avg_bytes: 0.0,
            keyframe_avg_rtp_packets: 0.0,
            keyframe_avg_rtp_span_us: 0.0,
            pframe_count: 0,
            pframe_avg_bytes: 0.0,
            pframe_avg_rtp_packets: 0.0,
            pframe_avg_rtp_span_us: 0.0,
        };
    }

    let first = client.arrivals.first().unwrap().0;
    let last = client.arrivals.last().unwrap().0;
    let wall_secs = last.duration_since(first).as_secs_f64();
    let total_bytes: usize = client
        .arrivals
        .iter()
        .map(|&(_, sz, _, _, _, _, _)| sz)
        .sum();

    let bitrate_mbps = if wall_secs > 0.0 {
        (total_bytes as f64 * 8.0) / (wall_secs * 1_000_000.0)
    } else {
        0.0
    };
    let fps = if wall_secs > 0.0 {
        (n - 1) as f64 / wall_secs
    } else {
        0.0
    };

    let inter_arrival_temporal: Vec<i64> = client
        .arrivals
        .windows(2)
        .map(|w| w[1].0.duration_since(w[0].0).as_micros() as i64)
        .collect();
    let keyframe_flags: Vec<bool> = client
        .arrivals
        .iter()
        .map(|&(_, _, kf, _, _, _, _)| kf)
        .collect();

    let mut inter_arrival_sorted = inter_arrival_temporal.clone();
    inter_arrival_sorted.sort();

    let mean_ia =
        inter_arrival_sorted.iter().sum::<i64>() as f64 / inter_arrival_sorted.len() as f64;
    let variance = inter_arrival_sorted
        .iter()
        .map(|&d| (d as f64 - mean_ia).powi(2))
        .sum::<f64>()
        / inter_arrival_sorted.len() as f64;
    let jitter_us = variance.sqrt();

    let stutters = detect_stutters(&inter_arrival_temporal, &keyframe_flags, fps);

    // Per-frame-type breakdown
    let kf_entries: Vec<_> = client
        .arrivals
        .iter()
        .filter(|&&(_, _, kf, _, _, _, _)| kf)
        .collect();
    let pf_entries: Vec<_> = client
        .arrivals
        .iter()
        .filter(|&&(_, _, kf, _, _, _, _)| !kf)
        .collect();

    let avg_or_zero = |items: &[&(Instant, usize, bool, u32, u64, usize, usize)],
                       f: fn(&(Instant, usize, bool, u32, u64, usize, usize)) -> f64|
     -> f64 {
        if items.is_empty() {
            0.0
        } else {
            items.iter().map(|e| f(e)).sum::<f64>() / items.len() as f64
        }
    };

    ClientSummary {
        name: client.name.clone(),
        frames: n,
        fps,
        bitrate_mbps,
        avg_frame_kb: total_bytes as f64 / n as f64 / 1024.0,
        jitter_stddev_us: jitter_us,
        inter_arrival_p50_us: percentile(&inter_arrival_sorted, 0.50),
        inter_arrival_p95_us: percentile(&inter_arrival_sorted, 0.95),
        inter_arrival_p99_us: percentile(&inter_arrival_sorted, 0.99),
        inter_arrival_max_us: *inter_arrival_sorted.last().unwrap_or(&0),
        stutters,
        keyframe_count: kf_entries.len(),
        keyframe_avg_bytes: avg_or_zero(&kf_entries, |e| e.1 as f64),
        keyframe_avg_rtp_packets: avg_or_zero(&kf_entries, |e| e.3 as f64),
        keyframe_avg_rtp_span_us: avg_or_zero(&kf_entries, |e| e.4 as f64),
        pframe_count: pf_entries.len(),
        pframe_avg_bytes: avg_or_zero(&pf_entries, |e| e.1 as f64),
        pframe_avg_rtp_packets: avg_or_zero(&pf_entries, |e| e.3 as f64),
        pframe_avg_rtp_span_us: avg_or_zero(&pf_entries, |e| e.4 as f64),
    }
}

fn compute_pair_summary(a: &ClientData, b: &ClientData) -> PairSummary {
    let deltas = compute_deltas(&a.samples, &b.samples);
    let (kf_deltas, pf_deltas) = compute_deltas_by_type(&a.samples, &b.samples);
    let n = deltas.len();
    let a_total = a.samples.len();

    let zero = PairSummary {
        client_a: a.name.clone(),
        client_b: b.name.clone(),
        matched_frames: 0,
        total_frames_a: a_total,
        match_pct: 0.0,
        delta_mean_us: 0.0,
        delta_p50_us: 0,
        delta_p95_us: 0,
        delta_p99_us: 0,
        delta_min_us: 0,
        delta_max_us: 0,
        delta_stddev_us: 0.0,
        delta_keyframe_mean_us: 0.0,
        delta_keyframe_p50_us: 0,
        delta_keyframe_p95_us: 0,
        delta_keyframe_count: 0,
        delta_pframe_mean_us: 0.0,
        delta_pframe_p50_us: 0,
        delta_pframe_p95_us: 0,
        delta_pframe_count: 0,
    };

    if n == 0 {
        return zero;
    }

    let mean = deltas.iter().sum::<i64>() as f64 / n as f64;
    let var = deltas
        .iter()
        .map(|&d| (d as f64 - mean).powi(2))
        .sum::<f64>()
        / n as f64;

    let kf_mean = if kf_deltas.is_empty() {
        0.0
    } else {
        kf_deltas.iter().sum::<i64>() as f64 / kf_deltas.len() as f64
    };
    let pf_mean = if pf_deltas.is_empty() {
        0.0
    } else {
        pf_deltas.iter().sum::<i64>() as f64 / pf_deltas.len() as f64
    };

    PairSummary {
        client_a: a.name.clone(),
        client_b: b.name.clone(),
        matched_frames: n,
        total_frames_a: a_total,
        match_pct: if a_total > 0 {
            100.0 * n as f64 / a_total as f64
        } else {
            0.0
        },
        delta_mean_us: mean,
        delta_p50_us: percentile(&deltas, 0.50),
        delta_p95_us: percentile(&deltas, 0.95),
        delta_p99_us: percentile(&deltas, 0.99),
        delta_min_us: deltas[0],
        delta_max_us: deltas[n - 1],
        delta_stddev_us: var.sqrt(),
        delta_keyframe_mean_us: kf_mean,
        delta_keyframe_p50_us: percentile(&kf_deltas, 0.50),
        delta_keyframe_p95_us: percentile(&kf_deltas, 0.95),
        delta_keyframe_count: kf_deltas.len(),
        delta_pframe_mean_us: pf_mean,
        delta_pframe_p50_us: percentile(&pf_deltas, 0.50),
        delta_pframe_p95_us: percentile(&pf_deltas, 0.95),
        delta_pframe_count: pf_deltas.len(),
    }
}

fn compute_run_summary(clients: &[ClientData], run_index: u32, duration_s: f64) -> RunSummary {
    let client_summaries: Vec<ClientSummary> = clients.iter().map(compute_client_summary).collect();

    let mut pair_summaries = Vec::new();
    for i in 0..clients.len() {
        for j in (i + 1)..clients.len() {
            pair_summaries.push(compute_pair_summary(&clients[i], &clients[j]));
        }
    }

    RunSummary {
        run_index,
        duration_s,
        clients: client_summaries,
        pairs: pair_summaries,
    }
}

fn aggregate_runs(runs: &[RunSummary]) -> AggregateCrossRun {
    let n = runs.len() as u32;
    if n == 0 {
        return AggregateCrossRun {
            n_runs: 0,
            per_client: vec![],
            per_pair: vec![],
        };
    }

    let client_names: Vec<String> = runs[0].clients.iter().map(|c| c.name.clone()).collect();
    let per_client: Vec<AggregateClientStats> = client_names
        .iter()
        .map(|name| {
            let fps_vals: Vec<f64> = runs
                .iter()
                .filter_map(|r| r.clients.iter().find(|c| &c.name == name))
                .map(|c| c.fps)
                .collect();
            let jitter_vals: Vec<f64> = runs
                .iter()
                .filter_map(|r| r.clients.iter().find(|c| &c.name == name))
                .map(|c| c.jitter_stddev_us)
                .collect();
            let drop_vals: Vec<f64> = runs
                .iter()
                .filter_map(|r| r.clients.iter().find(|c| &c.name == name))
                .map(|c| c.stutters.true_drop_events as f64)
                .collect();
            let stutter_vals: Vec<f64> = runs
                .iter()
                .filter_map(|r| r.clients.iter().find(|c| &c.name == name))
                .map(|c| c.stutters.isolated_stutter_events as f64)
                .collect();
            let fb_vals: Vec<f64> = runs
                .iter()
                .filter_map(|r| r.clients.iter().find(|c| &c.name == name))
                .map(|c| c.stutters.freeze_burst_events as f64)
                .collect();
            let fb_kf_vals: Vec<f64> = runs
                .iter()
                .filter_map(|r| r.clients.iter().find(|c| &c.name == name))
                .map(|c| c.stutters.freeze_burst_at_keyframe as f64)
                .collect();
            let fb_delta_vals: Vec<f64> = runs
                .iter()
                .filter_map(|r| r.clients.iter().find(|c| &c.name == name))
                .map(|c| c.stutters.freeze_burst_at_delta as f64)
                .collect();
            let episode_vals: Vec<f64> = runs
                .iter()
                .filter_map(|r| r.clients.iter().find(|c| &c.name == name))
                .map(|c| c.stutters.disruption_episodes as f64)
                .collect();

            AggregateClientStats {
                name: name.clone(),
                fps_mean: mean_f64(&fps_vals),
                fps_stddev: stddev_f64(&fps_vals),
                jitter_mean_us: mean_f64(&jitter_vals),
                jitter_stddev_us: stddev_f64(&jitter_vals),
                true_drop_events_mean: mean_f64(&drop_vals),
                isolated_stutter_events_mean: mean_f64(&stutter_vals),
                freeze_burst_events_mean: mean_f64(&fb_vals),
                freeze_burst_at_keyframe_mean: mean_f64(&fb_kf_vals),
                freeze_burst_at_delta_mean: mean_f64(&fb_delta_vals),
                disruption_episodes_mean: mean_f64(&episode_vals),
            }
        })
        .collect();

    let pair_keys: Vec<(String, String)> = runs[0]
        .pairs
        .iter()
        .map(|p| (p.client_a.clone(), p.client_b.clone()))
        .collect();
    let per_pair: Vec<AggregatePairStats> = pair_keys
        .iter()
        .map(|(a, b)| {
            let mean_vals: Vec<f64> = runs
                .iter()
                .filter_map(|r| {
                    r.pairs
                        .iter()
                        .find(|p| &p.client_a == a && &p.client_b == b)
                })
                .map(|p| p.delta_mean_us)
                .collect();
            let p50_vals: Vec<f64> = runs
                .iter()
                .filter_map(|r| {
                    r.pairs
                        .iter()
                        .find(|p| &p.client_a == a && &p.client_b == b)
                })
                .map(|p| p.delta_p50_us as f64)
                .collect();
            let p95_vals: Vec<f64> = runs
                .iter()
                .filter_map(|r| {
                    r.pairs
                        .iter()
                        .find(|p| &p.client_a == a && &p.client_b == b)
                })
                .map(|p| p.delta_p95_us as f64)
                .collect();
            let p99_vals: Vec<f64> = runs
                .iter()
                .filter_map(|r| {
                    r.pairs
                        .iter()
                        .find(|p| &p.client_a == a && &p.client_b == b)
                })
                .map(|p| p.delta_p99_us as f64)
                .collect();

            let matched_pairs: Vec<&PairSummary> = runs
                .iter()
                .filter_map(|r| {
                    r.pairs
                        .iter()
                        .find(|p| &p.client_a == a && &p.client_b == b)
                })
                .collect();
            let kf_mean_vals: Vec<f64> = matched_pairs
                .iter()
                .map(|p| p.delta_keyframe_mean_us)
                .collect();
            let kf_p50_vals: Vec<f64> = matched_pairs
                .iter()
                .map(|p| p.delta_keyframe_p50_us as f64)
                .collect();
            let kf_p95_vals: Vec<f64> = matched_pairs
                .iter()
                .map(|p| p.delta_keyframe_p95_us as f64)
                .collect();
            let pf_mean_vals: Vec<f64> = matched_pairs
                .iter()
                .map(|p| p.delta_pframe_mean_us)
                .collect();
            let pf_p50_vals: Vec<f64> = matched_pairs
                .iter()
                .map(|p| p.delta_pframe_p50_us as f64)
                .collect();
            let pf_p95_vals: Vec<f64> = matched_pairs
                .iter()
                .map(|p| p.delta_pframe_p95_us as f64)
                .collect();

            AggregatePairStats {
                client_a: a.clone(),
                client_b: b.clone(),
                delta_mean_of_means_us: mean_f64(&mean_vals),
                delta_mean_of_p50_us: mean_f64(&p50_vals),
                delta_mean_of_p95_us: mean_f64(&p95_vals),
                delta_mean_of_p99_us: mean_f64(&p99_vals),
                delta_stddev_of_means_us: stddev_f64(&mean_vals),
                delta_keyframe_mean_of_means_us: mean_f64(&kf_mean_vals),
                delta_keyframe_mean_of_p50_us: mean_f64(&kf_p50_vals),
                delta_keyframe_mean_of_p95_us: mean_f64(&kf_p95_vals),
                delta_pframe_mean_of_means_us: mean_f64(&pf_mean_vals),
                delta_pframe_mean_of_p50_us: mean_f64(&pf_p50_vals),
                delta_pframe_mean_of_p95_us: mean_f64(&pf_p95_vals),
            }
        })
        .collect();

    AggregateCrossRun {
        n_runs: n,
        per_client,
        per_pair,
    }
}

// ── Printing helpers ────────────────────────────────────────────────────────

fn print_client_stats(cs: &ClientSummary) {
    if cs.frames < 2 {
        println!("  {}: {} frames (insufficient data)", cs.name, cs.frames);
        return;
    }
    println!(
        "  {}: {} frames, {:.1} fps, {:.2} Mbps, {:.1} KB/frame, jitter(stddev)={}, ia p50={} p95={} p99={} max={} | true_drops={} missed~{} freeze_bursts={} (kf={} delta={}) isolated_stutters={} episodes={}",
        cs.name,
        cs.frames,
        cs.fps,
        cs.bitrate_mbps,
        cs.avg_frame_kb,
        format_us(cs.jitter_stddev_us as i64),
        format_us(cs.inter_arrival_p50_us),
        format_us(cs.inter_arrival_p95_us),
        format_us(cs.inter_arrival_p99_us),
        format_us(cs.inter_arrival_max_us),
        cs.stutters.true_drop_events,
        cs.stutters.estimated_missed_frames,
        cs.stutters.freeze_burst_events,
        cs.stutters.freeze_burst_at_keyframe,
        cs.stutters.freeze_burst_at_delta,
        cs.stutters.isolated_stutter_events,
        cs.stutters.disruption_episodes,
    );
}

fn print_pair_stats(ps: &PairSummary) {
    if ps.matched_frames == 0 {
        println!(
            "  {} -> {}: no matched frames ({} samples)",
            ps.client_a, ps.client_b, ps.total_frames_a
        );
        return;
    }
    println!(
        "  {} -> {} ({}/{} matched, {:.0}%):  min={}  avg={}  p50={}  p95={}  p99={}  max={}  stddev={}",
        ps.client_a,
        ps.client_b,
        ps.matched_frames,
        ps.total_frames_a,
        ps.match_pct,
        format_us(ps.delta_min_us),
        format_us(ps.delta_mean_us as i64),
        format_us(ps.delta_p50_us),
        format_us(ps.delta_p95_us),
        format_us(ps.delta_p99_us),
        format_us(ps.delta_max_us),
        format_us(ps.delta_stddev_us as i64),
    );
}

fn print_run_summary(summary: &RunSummary, label: &str) {
    println!(
        "\n=== {label} (run {}, {:.1}s) ===",
        summary.run_index, summary.duration_s
    );
    println!("--- Per-client stats ---");
    for cs in &summary.clients {
        print_client_stats(cs);
    }
    if summary.pairs.is_empty() {
        return;
    }
    println!("--- Pairwise latency ---");
    for ps in &summary.pairs {
        print_pair_stats(ps);
    }
}

fn print_aggregate(agg: &AggregateCrossRun) {
    println!("\n{}", "=".repeat(80));
    println!("=== AGGREGATE across {} runs ===", agg.n_runs);

    println!("--- Per-client (mean +/- stddev across runs) ---");
    for c in &agg.per_client {
        println!(
            "  {}: fps={:.1}+/-{:.1}, jitter={:.0}+/-{:.0} us, true_drops={:.1}, freeze_bursts={:.1} (kf={:.1} delta={:.1}) isolated_stutters={:.1} episodes={:.1}",
            c.name,
            c.fps_mean,
            c.fps_stddev,
            c.jitter_mean_us,
            c.jitter_stddev_us,
            c.true_drop_events_mean,
            c.freeze_burst_events_mean,
            c.freeze_burst_at_keyframe_mean,
            c.freeze_burst_at_delta_mean,
            c.isolated_stutter_events_mean,
            c.disruption_episodes_mean,
        );
    }

    println!("--- Pairwise (mean of per-run stats) ---");
    for p in &agg.per_pair {
        println!(
            "  {} -> {}: mean_of_means={}+/-{}, mean_of_p50={}, mean_of_p95={}, mean_of_p99={}",
            p.client_a,
            p.client_b,
            format_us(p.delta_mean_of_means_us as i64),
            format_us(p.delta_stddev_of_means_us as i64),
            format_us(p.delta_mean_of_p50_us as i64),
            format_us(p.delta_mean_of_p95_us as i64),
            format_us(p.delta_mean_of_p99_us as i64),
        );
    }
}

fn write_csv(clients: &[ClientData], path: &str) -> Result<()> {
    if let Some(parent) = std::path::Path::new(path).parent() {
        std::fs::create_dir_all(parent)?;
    }

    let mut f = std::fs::File::create(path)?;

    write!(f, "content_hash")?;
    for c in clients {
        write!(
            f,
            ",{n}_pts_ms,{n}_arrival_us,{n}_bytes,{n}_is_keyframe,{n}_rtp_packets,{n}_rtp_span_us,{n}_vcl_bytes,{n}_filler_bytes",
            n = c.name
        )?;
    }
    writeln!(f)?;

    let mut all_hashes: Vec<u64> = clients
        .iter()
        .flat_map(|c| c.samples.keys().copied())
        .collect();
    all_hashes.sort();
    all_hashes.dedup();

    let base_instant = clients
        .iter()
        .flat_map(|c| c.arrivals.first().map(|&(inst, _, _, _, _, _, _)| inst))
        .min();
    let Some(base_instant) = base_instant else {
        return Ok(());
    };

    for hash in all_hashes {
        write!(f, "{hash:016x}")?;
        for c in clients {
            if let Some(&(pts, arrival, sz, kf, rtp_pkts, rtp_span, vcl, filler)) =
                c.samples.get(&hash)
            {
                let us = arrival.duration_since(base_instant).as_micros();
                write!(
                    f,
                    ",{pts},{us},{sz},{},{rtp_pkts},{rtp_span},{vcl},{filler}",
                    kf as u8
                )?;
            } else {
                write!(f, ",,,,,,,,")?;
            }
        }
        writeln!(f)?;
    }

    eprintln!("CSV written to {path}");
    Ok(())
}

// ── CSV re-analysis ─────────────────────────────────────────────────────────

struct CsvClientData {
    name: String,
    /// content_hash -> (pts_ms, arrival_us, bytes, is_keyframe, rtp_packets, rtp_span_us, vcl_bytes, filler_bytes)
    samples: HashMap<u64, (i64, i64, usize, bool, u32, u64, usize, usize)>,
    /// Arrival-ordered: (arrival_us, bytes, is_keyframe, rtp_packets, rtp_span_us, vcl_bytes, filler_bytes)
    arrivals: Vec<(i64, usize, bool, u32, u64, usize, usize)>,
}

fn load_csv(path: &str) -> Result<Vec<CsvClientData>> {
    use std::io::BufRead;
    let file = std::fs::File::open(path).with_context(|| format!("Failed to open CSV: {path}"))?;
    let mut lines = std::io::BufReader::new(file).lines();

    let header = lines.next().ok_or_else(|| anyhow!("Empty CSV: {path}"))??;
    let columns: Vec<&str> = header.split(',').collect();

    // Discover clients from <name>_arrival_us columns.
    let mut client_infos: Vec<(String, usize)> = Vec::new(); // (name, arrival_col_idx)
    for (i, col) in columns.iter().enumerate() {
        if let Some(name) = col.strip_suffix("_arrival_us") {
            client_infos.push((name.to_string(), i));
        }
    }

    if client_infos.is_empty() {
        return Err(anyhow!("No client columns found in CSV: {path}"));
    }

    struct ColSet {
        name: String,
        pts_idx: usize,
        arrival_idx: usize,
        bytes_idx: usize,
        kf_idx: Option<usize>,
        rtp_packets_idx: Option<usize>,
        rtp_span_idx: Option<usize>,
        vcl_bytes_idx: Option<usize>,
        filler_bytes_idx: Option<usize>,
    }

    let find_col = |suffix: &str| -> Result<usize> {
        columns
            .iter()
            .position(|c| *c == suffix)
            .ok_or_else(|| anyhow!("Missing column '{suffix}' in {path}"))
    };

    let find_col_opt =
        |suffix: &str| -> Option<usize> { columns.iter().position(|c| *c == suffix) };

    let col_sets: Vec<ColSet> = client_infos
        .iter()
        .map(|(name, arrival_idx)| {
            Ok(ColSet {
                name: name.clone(),
                pts_idx: find_col(&format!("{name}_pts_ms"))?,
                arrival_idx: *arrival_idx,
                bytes_idx: find_col(&format!("{name}_bytes"))?,
                kf_idx: find_col_opt(&format!("{name}_is_keyframe")),
                rtp_packets_idx: find_col_opt(&format!("{name}_rtp_packets")),
                rtp_span_idx: find_col_opt(&format!("{name}_rtp_span_us")),
                vcl_bytes_idx: find_col_opt(&format!("{name}_vcl_bytes")),
                filler_bytes_idx: find_col_opt(&format!("{name}_filler_bytes")),
            })
        })
        .collect::<Result<Vec<_>>>()?;

    let mut clients: Vec<CsvClientData> = col_sets
        .iter()
        .map(|cs| CsvClientData {
            name: cs.name.clone(),
            samples: HashMap::new(),
            arrivals: Vec::new(),
        })
        .collect();

    for line_result in lines {
        let line = line_result?;
        if line.is_empty() {
            continue;
        }
        let fields: Vec<&str> = line.split(',').collect();
        if fields.len() < columns.len() {
            continue;
        }

        let hash = u64::from_str_radix(fields[0], 16)
            .with_context(|| format!("Invalid content_hash: {}", fields[0]))?;

        for (ci, cs) in col_sets.iter().enumerate() {
            let arrival_str = fields[cs.arrival_idx];
            if arrival_str.is_empty() {
                continue;
            }
            let pts_ms: i64 = fields[cs.pts_idx].parse().unwrap_or(0);
            let arrival_us: i64 = arrival_str.parse()?;
            let bytes: usize = fields[cs.bytes_idx].parse().unwrap_or(0);
            let kf: bool = cs.kf_idx.map_or(false, |idx| fields[idx] == "1");
            let rtp_pkts: u32 = cs
                .rtp_packets_idx
                .and_then(|idx| fields.get(idx).and_then(|s| s.parse().ok()))
                .unwrap_or(0);
            let rtp_span: u64 = cs
                .rtp_span_idx
                .and_then(|idx| fields.get(idx).and_then(|s| s.parse().ok()))
                .unwrap_or(0);
            let vcl_bytes: usize = cs
                .vcl_bytes_idx
                .and_then(|idx| fields.get(idx).and_then(|s| s.parse().ok()))
                .unwrap_or(0);
            let filler_bytes: usize = cs
                .filler_bytes_idx
                .and_then(|idx| fields.get(idx).and_then(|s| s.parse().ok()))
                .unwrap_or(0);

            clients[ci].samples.insert(
                hash,
                (
                    pts_ms,
                    arrival_us,
                    bytes,
                    kf,
                    rtp_pkts,
                    rtp_span,
                    vcl_bytes,
                    filler_bytes,
                ),
            );
            clients[ci].arrivals.push((
                arrival_us,
                bytes,
                kf,
                rtp_pkts,
                rtp_span,
                vcl_bytes,
                filler_bytes,
            ));
        }
    }

    // Sort arrivals by timestamp (CSV rows are hash-ordered, not time-ordered).
    for c in &mut clients {
        c.arrivals.sort_by_key(|&(us, _, _, _, _, _, _)| us);
    }

    Ok(clients)
}

fn compute_client_summary_from_csv(client: &CsvClientData) -> ClientSummary {
    let n = client.arrivals.len();
    let zero_stutters = StutterStats {
        true_drop_events: 0,
        estimated_missed_frames: 0,
        freeze_burst_events: 0,
        freeze_burst_severity_us: 0,
        freeze_burst_at_keyframe: 0,
        freeze_burst_at_delta: 0,
        isolated_stutter_events: 0,
        disruption_episodes: 0,
        disruption_episode_frames: 0,
        disruption_episode_severity_us: 0,
        nominal_fps: 0.0,
    };

    if n < 2 {
        return ClientSummary {
            name: client.name.clone(),
            frames: n,
            fps: 0.0,
            bitrate_mbps: 0.0,
            avg_frame_kb: 0.0,
            jitter_stddev_us: 0.0,
            inter_arrival_p50_us: 0,
            inter_arrival_p95_us: 0,
            inter_arrival_p99_us: 0,
            inter_arrival_max_us: 0,
            stutters: zero_stutters,
            keyframe_count: 0,
            keyframe_avg_bytes: 0.0,
            keyframe_avg_rtp_packets: 0.0,
            keyframe_avg_rtp_span_us: 0.0,
            pframe_count: 0,
            pframe_avg_bytes: 0.0,
            pframe_avg_rtp_packets: 0.0,
            pframe_avg_rtp_span_us: 0.0,
        };
    }

    let first_us = client.arrivals.first().unwrap().0;
    let last_us = client.arrivals.last().unwrap().0;
    let wall_us = (last_us - first_us) as f64;
    let wall_secs = wall_us / 1_000_000.0;
    let total_bytes: usize = client
        .arrivals
        .iter()
        .map(|&(_, sz, _, _, _, _, _)| sz)
        .sum();

    let bitrate_mbps = if wall_secs > 0.0 {
        (total_bytes as f64 * 8.0) / (wall_secs * 1_000_000.0)
    } else {
        0.0
    };
    let fps = if wall_secs > 0.0 {
        (n - 1) as f64 / wall_secs
    } else {
        0.0
    };

    let inter_arrival: Vec<i64> = client
        .arrivals
        .windows(2)
        .map(|w| w[1].0 - w[0].0)
        .collect();
    let keyframe_flags: Vec<bool> = client
        .arrivals
        .iter()
        .map(|&(_, _, kf, _, _, _, _)| kf)
        .collect();

    let mut sorted = inter_arrival.clone();
    sorted.sort();

    let mean_ia = sorted.iter().sum::<i64>() as f64 / sorted.len() as f64;
    let variance = sorted
        .iter()
        .map(|&d| (d as f64 - mean_ia).powi(2))
        .sum::<f64>()
        / sorted.len() as f64;

    let stutters = detect_stutters(&inter_arrival, &keyframe_flags, fps);

    let kf_entries: Vec<_> = client
        .arrivals
        .iter()
        .filter(|&&(_, _, kf, _, _, _, _)| kf)
        .collect();
    let pf_entries: Vec<_> = client
        .arrivals
        .iter()
        .filter(|&&(_, _, kf, _, _, _, _)| !kf)
        .collect();
    let csv_avg = |items: &[&(i64, usize, bool, u32, u64, usize, usize)],
                   f: fn(&(i64, usize, bool, u32, u64, usize, usize)) -> f64|
     -> f64 {
        if items.is_empty() {
            0.0
        } else {
            items.iter().map(|e| f(e)).sum::<f64>() / items.len() as f64
        }
    };

    ClientSummary {
        name: client.name.clone(),
        frames: n,
        fps,
        bitrate_mbps,
        avg_frame_kb: total_bytes as f64 / n as f64 / 1024.0,
        jitter_stddev_us: variance.sqrt(),
        inter_arrival_p50_us: percentile(&sorted, 0.50),
        inter_arrival_p95_us: percentile(&sorted, 0.95),
        inter_arrival_p99_us: percentile(&sorted, 0.99),
        inter_arrival_max_us: *sorted.last().unwrap_or(&0),
        stutters,
        keyframe_count: kf_entries.len(),
        keyframe_avg_bytes: csv_avg(&kf_entries, |e| e.1 as f64),
        keyframe_avg_rtp_packets: csv_avg(&kf_entries, |e| e.3 as f64),
        keyframe_avg_rtp_span_us: csv_avg(&kf_entries, |e| e.4 as f64),
        pframe_count: pf_entries.len(),
        pframe_avg_bytes: csv_avg(&pf_entries, |e| e.1 as f64),
        pframe_avg_rtp_packets: csv_avg(&pf_entries, |e| e.3 as f64),
        pframe_avg_rtp_span_us: csv_avg(&pf_entries, |e| e.4 as f64),
    }
}

fn compute_pair_summary_from_csv(a: &CsvClientData, b: &CsvClientData) -> PairSummary {
    let mut deltas = Vec::new();
    let mut kf_deltas = Vec::new();
    let mut pf_deltas = Vec::new();
    for (hash, &(_, a_us, _, is_kf, _, _, _, _)) in &a.samples {
        if let Some(&(_, b_us, _, _, _, _, _, _)) = b.samples.get(hash) {
            let d = b_us - a_us;
            deltas.push(d);
            if is_kf {
                kf_deltas.push(d);
            } else {
                pf_deltas.push(d);
            }
        }
    }
    deltas.sort();
    kf_deltas.sort();
    pf_deltas.sort();

    let n = deltas.len();
    let a_total = a.samples.len();

    let zero = PairSummary {
        client_a: a.name.clone(),
        client_b: b.name.clone(),
        matched_frames: 0,
        total_frames_a: a_total,
        match_pct: 0.0,
        delta_mean_us: 0.0,
        delta_p50_us: 0,
        delta_p95_us: 0,
        delta_p99_us: 0,
        delta_min_us: 0,
        delta_max_us: 0,
        delta_stddev_us: 0.0,
        delta_keyframe_mean_us: 0.0,
        delta_keyframe_p50_us: 0,
        delta_keyframe_p95_us: 0,
        delta_keyframe_count: 0,
        delta_pframe_mean_us: 0.0,
        delta_pframe_p50_us: 0,
        delta_pframe_p95_us: 0,
        delta_pframe_count: 0,
    };

    if n == 0 {
        return zero;
    }

    let mean = deltas.iter().sum::<i64>() as f64 / n as f64;
    let var = deltas
        .iter()
        .map(|&d| (d as f64 - mean).powi(2))
        .sum::<f64>()
        / n as f64;

    let kf_mean = if kf_deltas.is_empty() {
        0.0
    } else {
        kf_deltas.iter().sum::<i64>() as f64 / kf_deltas.len() as f64
    };
    let pf_mean = if pf_deltas.is_empty() {
        0.0
    } else {
        pf_deltas.iter().sum::<i64>() as f64 / pf_deltas.len() as f64
    };

    PairSummary {
        client_a: a.name.clone(),
        client_b: b.name.clone(),
        matched_frames: n,
        total_frames_a: a_total,
        match_pct: if a_total > 0 {
            100.0 * n as f64 / a_total as f64
        } else {
            0.0
        },
        delta_mean_us: mean,
        delta_p50_us: percentile(&deltas, 0.50),
        delta_p95_us: percentile(&deltas, 0.95),
        delta_p99_us: percentile(&deltas, 0.99),
        delta_min_us: deltas[0],
        delta_max_us: deltas[n - 1],
        delta_stddev_us: var.sqrt(),
        delta_keyframe_mean_us: kf_mean,
        delta_keyframe_p50_us: percentile(&kf_deltas, 0.50),
        delta_keyframe_p95_us: percentile(&kf_deltas, 0.95),
        delta_keyframe_count: kf_deltas.len(),
        delta_pframe_mean_us: pf_mean,
        delta_pframe_p50_us: percentile(&pf_deltas, 0.50),
        delta_pframe_p95_us: percentile(&pf_deltas, 0.95),
        delta_pframe_count: pf_deltas.len(),
    }
}

/// Auto-detect video codec by peeking at the first RTP packet's NAL header.
fn detect_codec_from_pcap(pcap_path: &str) -> Result<Codec> {
    let file = std::fs::File::open(pcap_path)
        .with_context(|| format!("Failed to open pcap: {pcap_path}"))?;
    let mut reader =
        PcapReader::new(file).map_err(|e| anyhow!("Failed to parse pcap header: {e}"))?;
    let pkt = reader
        .next_packet()
        .ok_or_else(|| anyhow!("Empty pcap: {pcap_path}"))?
        .map_err(|e| anyhow!("Failed to read pcap packet: {e}"))?;
    // IPv4 (20) + UDP (8) + RTP header (12) = 40 bytes before NAL payload
    if pkt.data.len() < 41 {
        return Err(anyhow!(
            "First pcap packet too short ({} bytes)",
            pkt.data.len()
        ));
    }
    let nal_byte = pkt.data[40];
    let h264_type = nal_byte & 0x1F;
    // H.264 RTP-specific types: 24 (STAP-A), 28 (FU-A) are unambiguous
    if h264_type == 24 || h264_type == 28 {
        return Ok(Codec::H264);
    }
    let h265_type = (nal_byte >> 1) & 0x3F;
    // H.265 RTP-specific types: 48 (AP), 49 (FU)
    if h265_type == 48 || h265_type == 49 {
        return Ok(Codec::H265);
    }
    // For single-NAL packets, H.264 VCL types 1-5 are most common
    if (1..=23).contains(&h264_type) {
        return Ok(Codec::H264);
    }
    Ok(Codec::H264) // default fallback
}

/// Write a trimmed copy of `pcap_path` containing only the packets whose
/// capture timestamp falls within the frame window `[kf_idx, clip_end]` (plus
/// `margin_us` on each side).  Returns the path to the temporary file.
///
/// The caller maps CSV `arrival_us` to pcap packet timestamps via relative
/// offset from the first packet/frame, so small epoch differences between the
/// two clocks are absorbed by the margin.
fn trim_pcap(
    pcap_path: &str,
    frames: &[RenderFrameInfo],
    kf_idx: usize,
    clip_end: usize,
    margin_us: i64,
) -> Result<String> {
    let src =
        std::fs::File::open(pcap_path).with_context(|| format!("trim_pcap: open {pcap_path}"))?;
    let mut reader = PcapReader::new(src).map_err(|e| anyhow!("trim_pcap: parse header: {e}"))?;
    let orig_header = reader.header();

    let first_pkt = reader
        .next_packet()
        .ok_or_else(|| anyhow!("trim_pcap: empty pcap"))?
        .map_err(|e| anyhow!("trim_pcap: read first packet: {e}"))?;
    let pcap_t0_us = first_pkt.timestamp.as_secs() as i64 * 1_000_000
        + first_pkt.timestamp.subsec_micros() as i64;

    let csv_t0 = frames[0].arrival_us;
    let start_us = pcap_t0_us + (frames[kf_idx].arrival_us - csv_t0) - margin_us;
    let end_us = pcap_t0_us + (frames[clip_end].arrival_us - csv_t0) + margin_us;

    let tmp_path = format!("{pcap_path}.trimmed.pcap");
    let dst = BufWriter::new(
        std::fs::File::create(&tmp_path)
            .with_context(|| format!("trim_pcap: create {tmp_path}"))?,
    );
    let mut writer =
        PcapWriter::with_header(dst, orig_header).map_err(|e| anyhow!("trim_pcap: writer: {e}"))?;

    let mut kept = 0u64;
    // The first packet was already consumed; check and write it if in range.
    if pcap_t0_us >= start_us {
        writer
            .write_packet(&first_pkt)
            .map_err(|e| anyhow!("trim_pcap: write: {e}"))?;
        kept += 1;
    }

    while let Some(pkt) = reader.next_packet() {
        let pkt = pkt.map_err(|e| anyhow!("trim_pcap: read: {e}"))?;
        let ts_us =
            pkt.timestamp.as_secs() as i64 * 1_000_000 + pkt.timestamp.subsec_micros() as i64;
        if ts_us > end_us {
            break; // pcap is chronological; no more relevant packets
        }
        if ts_us >= start_us {
            writer
                .write_packet(&pkt)
                .map_err(|e| anyhow!("trim_pcap: write: {e}"))?;
            kept += 1;
        }
    }

    eprintln!(
        "  Trimmed pcap: kept {kept} packets ({:.1} MB) -> {tmp_path}",
        std::fs::metadata(&tmp_path).map(|m| m.len()).unwrap_or(0) as f64 / (1024.0 * 1024.0),
    );
    Ok(tmp_path)
}

/// Replay a pcap file through a GStreamer depay+parse pipeline and collect
/// `content_hash → is_keyframe` for every decoded access unit.
/// Per-frame info extracted from pcap replay: keyframe flag + RTP packet count/span.
struct PcapFrameInfo {
    is_keyframe: bool,
    rtp_packets: u32,
    rtp_span_us: u64,
    vcl_bytes: usize,
    filler_bytes: usize,
}

fn extract_frame_info_from_pcap(pcap_path: &str) -> Result<HashMap<u64, PcapFrameInfo>> {
    gst::init().context("GStreamer init failed")?;
    let codec = detect_codec_from_pcap(pcap_path)?;
    let (depay_factory, parse_factory, caps_str) = match codec {
        Codec::H264 => (
            "rtph264depay",
            "h264parse",
            "application/x-rtp,media=video,encoding-name=H264,clock-rate=90000",
        ),
        Codec::H265 => (
            "rtph265depay",
            "h265parse",
            "application/x-rtp,media=video,encoding-name=H265,clock-rate=90000",
        ),
    };
    let norm_caps = match codec {
        Codec::H264 => "video/x-h264,stream-format=byte-stream,alignment=au",
        Codec::H265 => "video/x-h265,stream-format=byte-stream,alignment=au",
    };

    let pipeline_str = format!(
        concat!(
            "filesrc location={location}",
            " ! pcapparse caps=\"{caps}\"",
            " ! {depay} name=depay",
            " ! {parse} name=parse config-interval=-1",
            " ! {norm}",
            " ! fakesink sync=false async=false",
        ),
        location = pcap_path,
        caps = caps_str,
        depay = depay_factory,
        parse = parse_factory,
        norm = norm_caps,
    );

    let pipeline = gst::parse::launch(&pipeline_str)
        .with_context(|| format!("Failed to build pcap pipeline for {pcap_path}"))?
        .downcast::<gst::Pipeline>()
        .map_err(|_| anyhow!("Element is not a pipeline"))?;

    let depay_elem = pipeline
        .by_name("depay")
        .ok_or_else(|| anyhow!("depay element not found in pcap pipeline"))?;
    let rtp_tracker = Arc::new(RtpTracker::new());
    attach_rtp_counter(
        &depay_elem.static_pad("sink").unwrap(),
        Arc::clone(&rtp_tracker),
    );

    let parse_elem = pipeline
        .by_name("parse")
        .ok_or_else(|| anyhow!("parse element not found in pcap pipeline"))?;
    let probe_pad = parse_elem.static_pad("src").unwrap();

    let (tx, mut rx) = mpsc::unbounded_channel();
    attach_frame_probe(
        &probe_pad,
        format!("pcap:{pcap_path}"),
        tx,
        Some(rtp_tracker),
    );

    pipeline
        .set_state(gst::State::Playing)
        .context("Failed to start pcap pipeline")?;

    let bus = pipeline.bus().unwrap();
    loop {
        let Some(msg) = bus.timed_pop(gst::ClockTime::from_seconds(30)) else {
            break;
        };
        match msg.view() {
            gst::MessageView::Eos(..) => break,
            gst::MessageView::Error(e) => {
                let _ = pipeline.set_state(gst::State::Null);
                return Err(anyhow!(
                    "Pcap pipeline error: {} ({})",
                    e.error(),
                    e.debug().unwrap_or_default()
                ));
            }
            _ => {}
        }
    }

    let _ = pipeline.set_state(gst::State::Null);

    let mut map = HashMap::new();
    while let Ok(sample) = rx.try_recv() {
        map.insert(
            sample.content_hash,
            PcapFrameInfo {
                is_keyframe: sample.is_keyframe,
                rtp_packets: sample.rtp_packets,
                rtp_span_us: sample.rtp_span_us,
                vcl_bytes: sample.vcl_bytes,
                filler_bytes: sample.filler_bytes,
            },
        );
    }
    Ok(map)
}

/// Write a CSV from re-analyzed CsvClientData (with potentially injected keyframe flags).
fn write_enriched_csv(clients: &[CsvClientData], path: &str) -> Result<()> {
    if let Some(parent) = std::path::Path::new(path).parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut f = std::fs::File::create(path)?;

    write!(f, "content_hash")?;
    for c in clients {
        write!(
            f,
            ",{n}_pts_ms,{n}_arrival_us,{n}_bytes,{n}_is_keyframe,{n}_rtp_packets,{n}_rtp_span_us,{n}_vcl_bytes,{n}_filler_bytes",
            n = c.name
        )?;
    }
    writeln!(f)?;

    let mut all_hashes: Vec<u64> = clients
        .iter()
        .flat_map(|c| c.samples.keys().copied())
        .collect();
    all_hashes.sort();
    all_hashes.dedup();

    for hash in all_hashes {
        write!(f, "{hash:016x}")?;
        for c in clients {
            if let Some(&(pts, arrival, sz, kf, rtp_pkts, rtp_span, vcl, filler)) =
                c.samples.get(&hash)
            {
                write!(
                    f,
                    ",{pts},{arrival},{sz},{},{rtp_pkts},{rtp_span},{vcl},{filler}",
                    kf as u8
                )?;
            } else {
                write!(f, ",,,,,,,,")?;
            }
        }
        writeln!(f)?;
    }

    eprintln!("Enriched CSV written to {path}");
    Ok(())
}

fn run_analyze(args: &Args) -> Result<()> {
    let dir = args.analyze.as_deref().unwrap();
    let dir_path = std::path::Path::new(dir);

    if !dir_path.is_dir() {
        return Err(anyhow!("Not a directory: {dir}"));
    }

    // Collect CSV files matching run_*.csv or segment_*.csv, sorted by name.
    let mut csv_files: Vec<std::path::PathBuf> = std::fs::read_dir(dir_path)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
            p.extension().and_then(|e| e.to_str()) == Some("csv")
                && (name.starts_with("run_") || name.starts_with("segment_"))
        })
        .collect();
    csv_files.sort();

    if csv_files.is_empty() {
        return Err(anyhow!(
            "No run_*.csv or segment_*.csv files found in {dir}"
        ));
    }

    eprintln!("Analyzing {} CSV file(s) from {dir}...", csv_files.len());

    let mut all_summaries: Vec<RunSummary> = Vec::new();

    for (idx, csv_path) in csv_files.iter().enumerate() {
        let path_str = csv_path.to_string_lossy();
        let mut clients = load_csv(&path_str)?;

        // Inject keyframe flags and RTP packet counts from pcap files if available.
        let csv_stem = csv_path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
        let pcap_dir = dir_path.join("rtp_pcap");
        let mut any_pcap_injected = false;
        for client in &mut clients {
            let pcap_path = pcap_dir.join(format!("{}_{}.pcap", csv_stem, client.name));
            if !pcap_path.exists() {
                continue;
            }
            let pcap_str = pcap_path.to_string_lossy();
            match extract_frame_info_from_pcap(&pcap_str) {
                Ok(info_map) => {
                    let mut injected = 0usize;
                    for (hash, entry) in client.samples.iter_mut() {
                        if let Some(info) = info_map.get(hash) {
                            entry.3 = info.is_keyframe;
                            entry.4 = info.rtp_packets;
                            entry.5 = info.rtp_span_us;
                            entry.6 = info.vcl_bytes;
                            entry.7 = info.filler_bytes;
                            injected += 1;
                        }
                    }
                    client.arrivals = client
                        .samples
                        .values()
                        .map(
                            |&(_, arrival_us, bytes, kf, rtp_pkts, rtp_span, vcl, filler)| {
                                (arrival_us, bytes, kf, rtp_pkts, rtp_span, vcl, filler)
                            },
                        )
                        .collect();
                    client.arrivals.sort_by_key(|&(us, _, _, _, _, _, _)| us);

                    let kf_count = info_map.values().filter(|v| v.is_keyframe).count();
                    let rtp_count = info_map.values().filter(|v| v.rtp_packets > 0).count();
                    eprintln!(
                        "  [{name}] Injected from pcap: {injected}/{total} frames matched, \
                         {kf_count} keyframes, {rtp_count} with RTP counts",
                        name = client.name,
                        total = client.samples.len(),
                    );
                    any_pcap_injected = true;
                }
                Err(e) => {
                    eprintln!(
                        "  [{name}] Warning: failed to extract frame info from {path}: {e}",
                        name = client.name,
                        path = pcap_str,
                    );
                }
            }
        }

        // Write enriched CSV if any pcap data was injected
        if any_pcap_injected {
            write_enriched_csv(&clients, &path_str)?;
        }

        let client_summaries: Vec<ClientSummary> = clients
            .iter()
            .map(compute_client_summary_from_csv)
            .collect();

        let mut pairs: Vec<PairSummary> = Vec::new();
        for i in 0..clients.len() {
            for j in (i + 1)..clients.len() {
                pairs.push(compute_pair_summary_from_csv(&clients[i], &clients[j]));
            }
        }

        let run_idx = (idx + 1) as u32;
        let duration_s = clients
            .iter()
            .filter_map(|c| {
                let first = c.arrivals.first()?.0;
                let last = c.arrivals.last()?.0;
                Some((last - first) as f64 / 1_000_000.0)
            })
            .fold(0.0f64, f64::max);

        let summary = RunSummary {
            run_index: run_idx,
            duration_s,
            clients: client_summaries,
            pairs,
        };

        print_run_summary(&summary, &format!("Re-analysis ({path_str})"));
        all_summaries.push(summary);
    }

    let aggregate = aggregate_runs(&all_summaries);
    if all_summaries.len() > 1 {
        print_aggregate(&aggregate);
    }

    let json_path = args
        .json
        .clone()
        .unwrap_or_else(|| format!("{dir}/summary.json"));

    let full = AggregatedSummary {
        runs: all_summaries,
        aggregate,
    };
    let json = serde_json::to_string_pretty(&full)?;
    if let Some(parent) = std::path::Path::new(&json_path).parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&json_path, &json)?;
    eprintln!("JSON summary written to {json_path}");

    Ok(())
}

// ── HUD video rendering ─────────────────────────────────────────────────────

/// Per-frame state used by the render probe to format the HUD overlay.
#[derive(Clone)]
struct RenderFrameInfo {
    frame_idx: usize,
    arrival_us: i64,
    rtp_pts_us: i64,
    bytes: usize,
    is_keyframe: bool,
    annotation: FrameAnnotation,
}

/// Build a vector of `RenderFrameInfo` from a client's CSV data, sorted by
/// arrival time and annotated with stutter classifications.
fn build_render_frames(client: &CsvClientData, nominal_fps: f64) -> Vec<RenderFrameInfo> {
    let mut sorted: Vec<(u64, i64, i64, usize, bool)> = client
        .samples
        .iter()
        .map(|(&hash, &(pts, arr, sz, kf, _, _, _, _))| (hash, pts, arr, sz, kf))
        .collect();
    sorted.sort_by_key(|&(_, _, arr, _, _)| arr);

    let inter_arrival: Vec<i64> = sorted.windows(2).map(|w| w[1].2 - w[0].2).collect();
    let kf_flags: Vec<bool> = sorted.iter().map(|s| s.4).collect();
    let annotations = annotate_frames(&inter_arrival, &kf_flags, nominal_fps);

    sorted
        .iter()
        .enumerate()
        .map(|(i, &(_, pts_ms, arr, sz, kf))| RenderFrameInfo {
            frame_idx: i,
            arrival_us: arr,
            rtp_pts_us: pts_ms * 1_000,
            bytes: sz,
            is_keyframe: kf,
            annotation: if i < annotations.len() {
                annotations[i].clone()
            } else {
                FrameAnnotation::Normal
            },
        })
        .collect()
}

fn format_hud(
    header: &str,
    frame: &RenderFrameInfo,
    total_frames: usize,
    first_arrival_us: i64,
    total_duration_s: f64,
    expected_gap_ms: f64,
    prev_arrival_us: Option<i64>,
    running_fps: f64,
    running_drops: usize,
    running_fb: usize,
    running_episodes: usize,
) -> String {
    let time_s = (frame.arrival_us - first_arrival_us) as f64 / 1_000_000.0;

    let gap_ms = prev_arrival_us
        .map(|prev| (frame.arrival_us - prev) as f64 / 1_000.0)
        .unwrap_or(0.0);

    let kf_str = if frame.is_keyframe { "YES" } else { "no" };
    let size_kb = frame.bytes as f64 / 1024.0;

    let event_line = match &frame.annotation {
        FrameAnnotation::Normal => " Status: OK".to_string(),
        FrameAnnotation::Freeze { severity_us } => format!(
            " <b>\u{25b6}\u{25b6}\u{25b6}  FREEZE  (severity {:.1} ms)  \u{25c0}\u{25c0}\u{25c0}</b>",
            *severity_us as f64 / 1_000.0,
        ),
        FrameAnnotation::Burst => {
            " <b>\u{25b6}\u{25b6}\u{25b6}  FREEZE-BURST  (catch-up burst)  \u{25c0}\u{25c0}\u{25c0}</b>"
                .to_string()
        }
        FrameAnnotation::TrueDrop { deficit } => format!(
            " <b>\u{25b6}\u{25b6}\u{25b6}  TRUE DROP  ({deficit} frame{} lost)  \u{25c0}\u{25c0}\u{25c0}</b>",
            if *deficit != 1 { "s" } else { "" },
        ),
        FrameAnnotation::IsolatedStutter => {
            " <b>\u{25b6}\u{25b6}\u{25b6}  ISOLATED STUTTER  (burst gap)  \u{25c0}\u{25c0}\u{25c0}</b>"
                .to_string()
        }
    };

    format!(
        concat!(
            " {header}\n",
            " \u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}",
            "\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}",
            "\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}",
            "\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\n",
            " Frame  {frame_idx} / {total_frames}  \u{2502}  Time  {time:.1}s / {dur:.1}s\n",
            " Keyframe: {kf}       \u{2502}  Frame size: {size:.1} KB\n",
            " \u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}",
            "\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}",
            "\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}",
            "\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\n",
            " Measured FPS: {fps:.1}  \u{2502}  Gap: {gap:.1} ms (expected {exp:.1} ms)\n",
            " \u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}",
            "\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}",
            "\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}",
            "\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\n",
            "{event}\n",
            " \u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}",
            "\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}",
            "\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}",
            "\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\n",
            " True Drops: {drops}  \u{2502}  Freeze-Bursts: {fb}  \u{2502}  Episodes: {ep}",
        ),
        header = header,
        frame_idx = frame.frame_idx + 1,
        total_frames = total_frames,
        time = time_s,
        dur = total_duration_s,
        kf = kf_str,
        size = size_kb,
        fps = running_fps,
        gap = gap_ms,
        exp = expected_gap_ms,
        event = event_line,
        drops = running_drops,
        fb = running_fb,
        ep = running_episodes,
    )
}

/// Find the frame index of the worst (or mildest) event matching `kind`.
///
/// Supported `kind` values:
///   "worst", "worst-drop", "worst-freeze"         → highest severity
///   "mildest", "mildest-drop", "mildest-freeze"   → lowest severity
///
/// Returns `(frame_index, description)` or None.
fn find_worst_event(frames: &[RenderFrameInfo], kind: &str) -> Option<(usize, String)> {
    let pick_min = kind.starts_with("mildest");
    let category = kind
        .strip_prefix("mildest")
        .unwrap_or(kind.strip_prefix("worst").unwrap_or(""));

    let mut best: Option<(usize, i64, String)> = None;

    for (i, f) in frames.iter().enumerate() {
        let (severity, desc) = match &f.annotation {
            FrameAnnotation::TrueDrop { deficit } if category.is_empty() || category == "-drop" => {
                (
                    (*deficit as i64) * 100_000,
                    format!("TRUE DROP ({deficit} lost)"),
                )
            }
            FrameAnnotation::Freeze { severity_us }
                if category.is_empty() || category == "-freeze" =>
            {
                (
                    *severity_us,
                    format!("FREEZE (severity {:.1} ms)", *severity_us as f64 / 1_000.0),
                )
            }
            FrameAnnotation::Burst if category.is_empty() || category == "-freeze" => {
                (50_000, "FREEZE-BURST (catch-up)".to_string())
            }
            _ => continue,
        };
        let dominated = if pick_min {
            best.as_ref().map_or(true, |b| severity < b.1)
        } else {
            best.as_ref().map_or(true, |b| severity > b.1)
        };
        if dominated {
            best = Some((i, severity, desc));
        }
    }

    best.map(|(idx, _, desc)| (idx, desc))
}

fn render_client_video(
    pcap_path: &str,
    output_path: &str,
    header: &str,
    frames: &[RenderFrameInfo],
    codec: Codec,
    clip_range: Option<(usize, usize)>,
    frame_offset: usize,
    jitterbuffer_ms: Option<f64>,
) -> Result<()> {
    let total_frames = frames.len();
    if total_frames < 2 {
        return Err(anyhow!("Too few frames ({total_frames}) to render video"));
    }

    let first_arrival = frames.first().unwrap().arrival_us;
    let last_arrival = frames.last().unwrap().arrival_us;
    let total_duration_s = (last_arrival - first_arrival) as f64 / 1_000_000.0;

    // Arrival-time base for PTS retiming: the first decoded frame's arrival
    // maps to a zero-offset, which is then added to the captured DTS base
    // (from the pcapparse segment) so timestamps remain segment-compatible.
    let pts_base_us = frames[frame_offset].arrival_us;
    let nominal_fps = if total_duration_s > 0.0 {
        (total_frames - 1) as f64 / total_duration_s
    } else {
        30.0
    };
    let expected_gap_ms = 1_000.0 / nominal_fps;

    // Build content_hash → index lookup from the pcap replay.
    // We replay the pcap once to collect hashes in decode order, then map
    // those to our arrival-sorted frame list via a second content_hash lookup.
    let (depay, parse_factory, caps_str) = match codec {
        Codec::H264 => (
            "rtph264depay",
            "h264parse",
            "application/x-rtp,media=video,encoding-name=H264,clock-rate=90000",
        ),
        Codec::H265 => (
            "rtph265depay",
            "h265parse",
            "application/x-rtp,media=video,encoding-name=H265,clock-rate=90000",
        ),
    };
    let norm_caps = match codec {
        Codec::H264 => "video/x-h264,stream-format=byte-stream,alignment=au",
        Codec::H265 => "video/x-h265,stream-format=byte-stream,alignment=au",
    };

    let have_nvdec = gst::ElementFactory::find("nvh264dec").is_some();

    // Always use x264enc for encoding: nvh264enc silently drops textoverlay
    // compositing on keyframes (the GPU encoder reads the buffer before the
    // overlay is blitted).  Software x264enc is fast enough for offline render.
    let encoder = "x264enc tune=zerolatency speed-preset=ultrafast bitrate=5000";

    let (decoder, scale_convert) = if have_nvdec {
        let dec = match codec {
            Codec::H264 => "nvh264dec",
            Codec::H265 => "nvh265dec",
        };
        (
            dec,
            concat!(
                "cudaconvertscale",
                " ! video/x-raw(memory:CUDAMemory),width=1920,height=1080",
                " ! cudadownload ! videoconvert",
            ),
        )
    } else {
        let dec = match codec {
            Codec::H264 => "avdec_h264",
            Codec::H265 => "avdec_h265",
        };
        (
            dec,
            "videoscale ! video/x-raw,width=1920,height=1080 ! videoconvert",
        )
    };

    let jb_element = match jitterbuffer_ms {
        Some(ms) => format!(" ! rtpjitterbuffer latency={}", ms as u32),
        None => String::new(),
    };

    let pipeline_str = format!(
        concat!(
            "filesrc location={location}",
            " ! pcapparse caps=\"{caps}\"",
            "{jb}",
            " ! {depay}",
            " ! {parse} name=parse config-interval=-1",
            " ! {norm}",
            " ! {decoder}",
            " ! {scale_convert}",
            " ! identity name=clipgate",
            " ! textoverlay name=overlay",
            " font-desc=\"monospace bold 6\"",
            " halignment=left valignment=top",
            " shaded-background=true",
            " draw-shadow=false",
            " ! {encoder}",
            " ! matroskamux",
            " ! filesink location={output}",
        ),
        location = pcap_path,
        caps = caps_str,
        jb = jb_element,
        depay = depay,
        parse = parse_factory,
        norm = norm_caps,
        decoder = decoder,
        scale_convert = scale_convert,
        encoder = encoder,
        output = output_path,
    );

    let pipeline = gst::parse::launch(&pipeline_str)
        .with_context(|| format!("Failed to build render pipeline for {pcap_path}"))?
        .downcast::<gst::Pipeline>()
        .map_err(|_| anyhow!("Element is not a pipeline"))?;

    let overlay = pipeline
        .by_name("overlay")
        .ok_or_else(|| anyhow!("overlay element not found"))?;

    let parse_elem = pipeline
        .by_name("parse")
        .ok_or_else(|| anyhow!("parse element not found"))?;
    let probe_pad = parse_elem.static_pad("src").unwrap();

    // parse.src probe: PTS→frame_idx mapping (and arrival-time retiming when
    // no jitterbuffer is used).
    //
    // h264parse may emit more buffers than the decoder outputs (the decoder
    // can silently drop frames), so we record (PTS → frame_idx) in a HashMap.
    // The clipgate probe later looks up the decoded buffer's PTS (which the
    // decoder preserves) to find the correct frame — no counters shared across
    // the decode boundary.
    //
    // When --jitterbuffer is active, the rtpjitterbuffer element has already
    // smoothed the packet delivery timing using RTP timestamps, so we preserve
    // its output PTS rather than overriding with arrival times.
    let use_jb = jitterbuffer_ms.is_some();
    let parse_counter = Arc::new(std::sync::atomic::AtomicUsize::new(frame_offset));
    let parse_counter_probe = parse_counter.clone();
    let frames_retime: Arc<Vec<RenderFrameInfo>> = Arc::new(frames.to_vec());
    let frames_retime_probe = frames_retime.clone();
    let pts_base_retime = pts_base_us;

    let dts_base: Arc<Mutex<Option<u64>>> = Arc::new(Mutex::new(None));
    let dts_base_probe = dts_base.clone();

    let pts_to_idx: Arc<Mutex<HashMap<u64, usize>>> = Arc::new(Mutex::new(HashMap::new()));
    let pts_to_idx_parse = pts_to_idx.clone();

    probe_pad.add_probe(gst::PadProbeType::BUFFER, move |_, info| {
        let idx = parse_counter_probe.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if idx >= frames_retime_probe.len() {
            return gst::PadProbeReturn::Ok;
        }

        if let Some(gst::PadProbeData::Buffer(ref mut buffer)) = info.data {
            if use_jb {
                // JB mode: the rtpjitterbuffer already set proper PTS from
                // RTP timestamps.  Just record the mapping.
                let ts = buffer.pts().map(|t| t.nseconds()).unwrap_or(0);
                pts_to_idx_parse.lock().unwrap().insert(ts, idx);
            } else {
                // Raw mode: override PTS/DTS with arrival time so the video
                // plays back with the exact network delivery timing.
                let mut base_guard = dts_base_probe.lock().unwrap();
                if base_guard.is_none() {
                    *base_guard = Some(
                        buffer
                            .dts()
                            .or(buffer.pts())
                            .map(|t| t.nseconds())
                            .unwrap_or(0),
                    );
                }
                let base_ns = base_guard.unwrap_or(0);
                drop(base_guard);

                let frame = &frames_retime_probe[idx];
                let arrival_ns = frame.arrival_us.saturating_sub(pts_base_retime) as u64 * 1_000;
                let ts = gst::ClockTime::from_nseconds(base_ns + arrival_ns);
                let buf = buffer.make_mut();
                buf.set_pts(ts);
                buf.set_dts(ts);

                pts_to_idx_parse.lock().unwrap().insert(ts.nseconds(), idx);
            }
        }

        gst::PadProbeReturn::Ok
    });

    // Clipgate probe: uses PTS from each decoded buffer to look up the
    // correct frame index (via the pts_to_idx map populated by parse.src).
    // This is immune to decoder frame drops since we never rely on a counter
    // that must stay in sync across the decode boundary.
    {
        let clipgate = pipeline
            .by_name("clipgate")
            .expect("clipgate element not found");
        let clipgate_src = clipgate.static_pad("src").unwrap();

        let frames_arc: Arc<Vec<RenderFrameInfo>> = Arc::new(frames.to_vec());
        let header_owned = header.to_string();
        let total = total_frames;
        let first_arr = first_arrival;
        let dur = total_duration_s;
        let exp_gap = expected_gap_ms;
        let nom_fps = nominal_fps;

        let (init_drops, init_fb, init_ep) =
            frames[..frame_offset]
                .iter()
                .fold((0usize, 0usize, 0usize), |(d, fb, ep), f| {
                    match &f.annotation {
                        FrameAnnotation::TrueDrop { .. } => (d + 1, fb, ep + 1),
                        FrameAnnotation::Freeze { .. } => (d, fb, ep + 1),
                        FrameAnnotation::Burst => (d, fb + 1, ep),
                        _ => (d, fb, ep),
                    }
                });
        let drops = std::sync::atomic::AtomicUsize::new(init_drops);
        let fb = std::sync::atomic::AtomicUsize::new(init_fb);
        let ep = std::sync::atomic::AtomicUsize::new(init_ep);

        let last_idx = std::sync::atomic::AtomicUsize::new(if frame_offset > 0 {
            frame_offset - 1
        } else {
            0
        });
        let pts_to_idx_gate = pts_to_idx.clone();
        let pipeline_weak = pipeline.downgrade();

        clipgate_src.add_probe(gst::PadProbeType::BUFFER, move |_, info| {
            let idx = match info.data {
                Some(gst::PadProbeData::Buffer(ref buffer)) => buffer.pts().and_then(|pts| {
                    pts_to_idx_gate
                        .lock()
                        .unwrap()
                        .get(&pts.nseconds())
                        .copied()
                }),
                _ => None,
            };
            let idx = match idx {
                Some(i) if i < frames_arc.len() => i,
                _ => return gst::PadProbeReturn::Ok,
            };

            let frame = &frames_arc[idx];

            // Update running counters for all frames since last seen idx
            // (covers any frames the decoder skipped).
            let prev = last_idx.swap(idx, std::sync::atomic::Ordering::Relaxed);
            let range_start = if prev < idx { prev + 1 } else { idx };
            for i in range_start..=idx {
                if i >= frames_arc.len() {
                    break;
                }
                match &frames_arc[i].annotation {
                    FrameAnnotation::TrueDrop { .. } => {
                        drops.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        ep.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }
                    FrameAnnotation::Freeze { .. } => {
                        ep.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }
                    FrameAnnotation::Burst => {
                        fb.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }
                    _ => {}
                }
            }

            // Clip: drop lead-in / post-clip frames.
            if let Some((cs, ce)) = clip_range {
                if idx < cs {
                    return gst::PadProbeReturn::Drop;
                }
                if idx > ce {
                    if let Some(pipeline) = pipeline_weak.upgrade() {
                        let _ = pipeline.post_message(gst::message::Eos::new());
                    }
                    return gst::PadProbeReturn::Drop;
                }
            }

            let prev_arrival = if idx > 0 {
                Some(frames_arc[idx - 1].arrival_us)
            } else {
                None
            };

            let running_fps = if idx > 0 {
                let elapsed_us = frame.arrival_us - frames_arc[0].arrival_us;
                if elapsed_us > 0 {
                    idx as f64 / (elapsed_us as f64 / 1_000_000.0)
                } else {
                    nom_fps
                }
            } else {
                nom_fps
            };

            let hud = format_hud(
                &header_owned,
                frame,
                total,
                first_arr,
                dur,
                exp_gap,
                prev_arrival,
                running_fps,
                drops.load(std::sync::atomic::Ordering::Relaxed),
                fb.load(std::sync::atomic::Ordering::Relaxed),
                ep.load(std::sync::atomic::Ordering::Relaxed),
            );
            overlay.set_property("text", &hud);

            gst::PadProbeReturn::Ok
        });
    }

    if let Some(parent) = std::path::Path::new(output_path).parent() {
        std::fs::create_dir_all(parent)?;
    }

    pipeline
        .set_state(gst::State::Playing)
        .context("Failed to start render pipeline")?;

    if let Some((cs, ce)) = clip_range {
        let clip_frames = ce - cs + 1;
        let clip_dur = (frames[ce].arrival_us - frames[cs].arrival_us) as f64 / 1e6;
        eprintln!(
            "  Rendering {output_path} (clip: frames {cs}-{ce}, {clip_frames} frames, {clip_dur:.1}s)..."
        );
    } else {
        eprintln!("  Rendering {output_path} ({total_frames} frames, {total_duration_s:.1}s)...");
    }

    let bus = pipeline.bus().unwrap();
    for msg in bus.iter_timed(gst::ClockTime::NONE) {
        match msg.view() {
            gst::MessageView::Eos(..) => break,
            gst::MessageView::Error(e) => {
                let _ = pipeline.set_state(gst::State::Null);
                return Err(anyhow!(
                    "Render pipeline error: {} ({})",
                    e.error(),
                    e.debug().unwrap_or_default()
                ));
            }
            _ => {}
        }
    }

    let _ = pipeline.set_state(gst::State::Null);
    eprintln!("  Done: {output_path}");
    Ok(())
}

fn run_render(args: &Args) -> Result<()> {
    let dir = args.render.as_deref().unwrap();
    let dir_path = std::path::Path::new(dir);

    if !dir_path.is_dir() {
        return Err(anyhow!("Not a directory: {dir}"));
    }

    gst::init().context("GStreamer init failed")?;

    let mut csv_files: Vec<std::path::PathBuf> = std::fs::read_dir(dir_path)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
            p.extension().and_then(|e| e.to_str()) == Some("csv")
                && (name.starts_with("run_") || name.starts_with("segment_"))
        })
        .collect();
    csv_files.sort();

    if csv_files.is_empty() {
        return Err(anyhow!(
            "No run_*.csv or segment_*.csv files found in {dir}"
        ));
    }

    let pcap_dir = dir_path.join("rtp_pcap");
    if !pcap_dir.is_dir() {
        return Err(anyhow!(
            "No rtp_pcap/ directory found in {dir}. Pcap recordings are required for --render."
        ));
    }

    // Derive trial context from the directory path for the HUD header.
    let dir_components: Vec<&str> = dir.split('/').collect();
    let trial_label = dir_components
        .iter()
        .rev()
        .take(2)
        .rev()
        .copied()
        .collect::<Vec<_>>()
        .join("/");

    let mut rendered = 0usize;

    for csv_path in &csv_files {
        let path_str = csv_path.to_string_lossy();
        let csv_stem = csv_path.file_stem().and_then(|s| s.to_str()).unwrap_or("");

        let mut clients = load_csv(&path_str)?;

        // Inject keyframe data from pcap.
        for client in &mut clients {
            let pcap_path = pcap_dir.join(format!("{}_{}.pcap", csv_stem, client.name));
            if !pcap_path.exists() {
                continue;
            }
            if let Ok(info_map) = extract_frame_info_from_pcap(&pcap_path.to_string_lossy()) {
                for (hash, entry) in client.samples.iter_mut() {
                    if let Some(info) = info_map.get(hash) {
                        entry.3 = info.is_keyframe;
                        entry.4 = info.rtp_packets;
                        entry.5 = info.rtp_span_us;
                        entry.6 = info.vcl_bytes;
                        entry.7 = info.filler_bytes;
                    }
                }
                client.arrivals = client
                    .samples
                    .values()
                    .map(
                        |&(_, arrival_us, bytes, kf, rtp_pkts, rtp_span, vcl, filler)| {
                            (arrival_us, bytes, kf, rtp_pkts, rtp_span, vcl, filler)
                        },
                    )
                    .collect();
                client.arrivals.sort_by_key(|&(us, _, _, _, _, _, _)| us);
            }
        }

        // When --clip-source is set, resolve the clip center from that specific
        // client so all other clients use the same wall-clock window.
        let shared_center_us: Option<i64> = if let (Some(ref clip_spec), Some(ref source_name)) =
            (&args.clip_event, &args.clip_source)
        {
            if let Some(src) = clients.iter().find(|c| c.name == *source_name) {
                let n = src.arrivals.len();
                let wall_secs = if n >= 2 {
                    let first = src.arrivals.first().unwrap().0;
                    let last = src.arrivals.last().unwrap().0;
                    (last - first) as f64 / 1_000_000.0
                } else {
                    0.0
                };
                let fps = if wall_secs > 0.0 {
                    (n - 1) as f64 / wall_secs
                } else {
                    30.0
                };
                let frames = build_render_frames(src, fps);
                let first_us = frames.first().map(|f| f.arrival_us).unwrap_or(0);

                let center_idx = if clip_spec.starts_with("worst")
                    || clip_spec.starts_with("mildest")
                {
                    match find_worst_event(&frames, clip_spec) {
                        Some((idx, desc)) => {
                            let t = (frames[idx].arrival_us - first_us) as f64 / 1e6;
                            eprintln!(
                                "  [{}] Worst event (clip source): {desc} at frame {idx} ({t:.1}s)",
                                src.name,
                            );
                            Some(idx)
                        }
                        None => {
                            eprintln!(
                                "  [{}] No matching event for --clip-event {clip_spec}",
                                src.name,
                            );
                            None
                        }
                    }
                } else {
                    let target_s: f64 = clip_spec.parse().unwrap_or(0.0);
                    let target_us = first_us + (target_s * 1_000_000.0) as i64;
                    Some(
                        frames
                            .iter()
                            .enumerate()
                            .min_by_key(|(_, f)| (f.arrival_us - target_us).abs())
                            .map(|(i, _)| i)
                            .unwrap_or(0),
                    )
                };

                center_idx.map(|i| frames[i].arrival_us)
            } else {
                eprintln!(
                    "  Warning: --clip-source {source_name} not found, falling back to per-client",
                );
                None
            }
        } else {
            None
        };

        for client in &clients {
            let pcap_path = pcap_dir.join(format!("{}_{}.pcap", csv_stem, client.name));
            if !pcap_path.exists() {
                eprintln!("  Skipping {}/{}: no pcap file", csv_stem, client.name);
                continue;
            }

            let codec = match detect_codec_from_pcap(&pcap_path.to_string_lossy()) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("  Skipping {}/{}: {}", csv_stem, client.name, e);
                    continue;
                }
            };

            let n = client.arrivals.len();
            let wall_secs = if n >= 2 {
                let first = client.arrivals.first().unwrap().0;
                let last = client.arrivals.last().unwrap().0;
                (last - first) as f64 / 1_000_000.0
            } else {
                0.0
            };
            let nominal_fps = if wall_secs > 0.0 {
                (n - 1) as f64 / wall_secs
            } else {
                30.0
            };

            let frames = build_render_frames(client, nominal_fps);

            // Resolve clip range if --clip-event was specified.
            let clip_range = if let Some(ref clip_spec) = args.clip_event {
                let first_us = frames.first().map(|f| f.arrival_us).unwrap_or(0);
                let radius_us = (args.clip_radius * 1_000_000.0) as i64;

                // If --clip-source resolved a shared center, use it for all
                // clients; otherwise fall back to per-client worst-event.
                let center_idx = if let Some(center_us) = shared_center_us {
                    let idx = frames
                        .iter()
                        .enumerate()
                        .min_by_key(|(_, f)| (f.arrival_us - center_us).abs())
                        .map(|(i, _)| i)
                        .unwrap_or(0);
                    let t = (frames[idx].arrival_us - first_us) as f64 / 1e6;
                    eprintln!(
                        "  [{name}] Clip center (from {src}): frame {idx} ({t:.1}s)",
                        name = client.name,
                        src = args.clip_source.as_deref().unwrap_or("?"),
                    );
                    idx
                } else if clip_spec.starts_with("worst") || clip_spec.starts_with("mildest") {
                    match find_worst_event(&frames, clip_spec) {
                        Some((idx, desc)) => {
                            let t = (frames[idx].arrival_us - first_us) as f64 / 1e6;
                            eprintln!(
                                "  [{name}] Worst event: {desc} at frame {idx} ({t:.1}s)",
                                name = client.name,
                            );
                            idx
                        }
                        None => {
                            eprintln!(
                                "  [{name}] No matching event for --clip-event {clip_spec}, rendering full video",
                                name = client.name,
                            );
                            0 // fallback: no clip
                        }
                    }
                } else {
                    let target_s: f64 = clip_spec.parse().unwrap_or(0.0);
                    let target_us = first_us + (target_s * 1_000_000.0) as i64;
                    frames
                        .iter()
                        .enumerate()
                        .min_by_key(|(_, f)| (f.arrival_us - target_us).abs())
                        .map(|(i, _)| i)
                        .unwrap_or(0)
                };

                if center_idx > 0 || clip_spec != "worst" {
                    let center_us = frames[center_idx].arrival_us;
                    let start = frames
                        .iter()
                        .position(|f| f.arrival_us >= center_us - radius_us)
                        .unwrap_or(0);
                    let end = frames
                        .iter()
                        .rposition(|f| f.arrival_us <= center_us + radius_us)
                        .unwrap_or(frames.len() - 1);
                    Some((start, end))
                } else {
                    None
                }
            } else {
                None
            };

            let codec_str = match codec {
                Codec::H264 => "H264",
                Codec::H265 => "H265",
            };

            // When clipping, trim the pcap to only the relevant time window
            // (from the last keyframe before clip_start through clip_end) so the
            // pipeline avoids decoding thousands of irrelevant leading frames.
            let (effective_pcap, frame_offset, trimmed_path) =
                if let Some((clip_start, clip_end)) = clip_range {
                    let kf_idx = frames[..=clip_start]
                        .iter()
                        .rposition(|f| f.is_keyframe)
                        .unwrap_or(clip_start);
                    match trim_pcap(
                        &pcap_path.to_string_lossy(),
                        &frames,
                        kf_idx,
                        clip_end,
                        1_000_000,
                    ) {
                        Ok(path) => (path.clone(), kf_idx, Some(path)),
                        Err(e) => {
                            eprintln!("  Warning: pcap trim failed ({e}), using full pcap");
                            (pcap_path.to_string_lossy().to_string(), 0, None)
                        }
                    }
                } else {
                    (pcap_path.to_string_lossy().to_string(), 0, None)
                };

            let clip_suffix = match &args.clip_event {
                Some(spec) => format!("_{}", spec.replace('-', "_")),
                None => String::new(),
            };
            let jb_suffix = match args.jitterbuffer {
                Some(ms) => format!("_jb{ms:.0}ms"),
                None => String::new(),
            };
            let header = format!(
                "{} \u{00b7} {} \u{00b7} {} \u{00b7} {}",
                client.name.to_uppercase(),
                codec_str,
                trial_label,
                csv_stem.replace('_', " ").to_uppercase(),
            );

            let output_path = dir_path.join(format!(
                "{}_{}_events{clip_suffix}{jb_suffix}.mkv",
                csv_stem, client.name
            ));

            match render_client_video(
                &effective_pcap,
                &output_path.to_string_lossy(),
                &header,
                &frames,
                codec,
                clip_range,
                frame_offset,
                args.jitterbuffer,
            ) {
                Ok(()) => rendered += 1,
                Err(e) => {
                    eprintln!("  Error rendering {}/{}: {}", csv_stem, client.name, e);
                }
            }

            if let Some(ref tmp) = trimmed_path {
                let _ = std::fs::remove_file(tmp);
            }
        }
    }

    eprintln!("\nRender complete: {rendered} video(s) produced in {dir}");
    Ok(())
}

// ── Single-run measurement ──────────────────────────────────────────────────

async fn run_single_measurement(
    args: &Args,
    run_index: u32,
    record_prefix: Option<&str>,
) -> Result<(RunSummary, Vec<ClientData>)> {
    let mut client_data: Vec<ClientData> = Vec::new();
    let mut pipelines: Vec<gst::Pipeline> = Vec::new();
    let mut signaling_tasks: Vec<tokio::task::JoinHandle<Result<()>>> = Vec::new();

    for (i, url) in args.rtsp_urls.iter().enumerate() {
        let name = format!("rtsp-{i}");
        let recorder = record_prefix
            .map(|p| PcapRecorder::new(&format!("{p}_{name}.pcap")))
            .transpose()?
            .map(Arc::new);
        let (tx, rx) = mpsc::unbounded_channel();
        let pipeline = rtsp_client::create_rtsp_client(&name, url, args.codec, tx, recorder)?;
        eprintln!("[run {run_index}][{name}] Created for {url}");
        client_data.push(ClientData {
            name,
            receiver: rx,
            samples: HashMap::new(),
            arrivals: Vec::new(),
            last_frame_time: None,
        });
        pipelines.push(pipeline);
    }

    for (i, endpoint) in args.udp_endpoints.iter().enumerate() {
        let name = format!("udp-{i}");
        let recorder = record_prefix
            .map(|p| PcapRecorder::new(&format!("{p}_{name}.pcap")))
            .transpose()?
            .map(Arc::new);
        let (addr, port_str) = endpoint
            .rsplit_once(':')
            .ok_or_else(|| anyhow!("Invalid UDP endpoint '{endpoint}', expected ADDR:PORT"))?;
        let port: i32 = port_str.parse()?;
        let (tx, rx) = mpsc::unbounded_channel();
        let pipeline = udp_client::create_udp_client(&name, addr, port, args.codec, tx, recorder)?;
        eprintln!("[run {run_index}][{name}] Created for {endpoint}");
        client_data.push(ClientData {
            name,
            receiver: rx,
            samples: HashMap::new(),
            arrivals: Vec::new(),
            last_frame_time: None,
        });
        pipelines.push(pipeline);
    }

    for (i, ws_url) in args.webrtc_urls.iter().enumerate() {
        let name = format!("webrtc-{i}");
        let recorder = record_prefix
            .map(|p| PcapRecorder::new(&format!("{p}_{name}.pcap")))
            .transpose()?
            .map(Arc::new);
        let (tx, rx) = mpsc::unbounded_channel();
        let (pipeline, task) = webrtc_client::create_webrtc_client(
            &name,
            ws_url,
            args.producer_id,
            args.stream_name.as_deref(),
            tx,
            recorder,
        )
        .await?;
        eprintln!("[run {run_index}][{name}] Created for {ws_url}");
        client_data.push(ClientData {
            name,
            receiver: rx,
            samples: HashMap::new(),
            arrivals: Vec::new(),
            last_frame_time: None,
        });
        pipelines.push(pipeline);
        signaling_tasks.push(task);
    }

    for pipeline in &pipelines {
        pipeline.set_state(gst::State::Playing)?;
    }

    let n_clients = client_data.len();
    eprintln!(
        "\n[run {run_index}] {n_clients} clients started. Measuring for {}s (warmup: {}s)...\n",
        args.duration, args.warmup
    );

    let start = Instant::now();
    let warmup = Duration::from_secs(args.warmup);
    let duration = Duration::from_secs(args.duration);
    let report_interval = Duration::from_secs(args.report_interval);

    let mut next_report = start + warmup + report_interval;
    loop {
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(100)) => {}
            _ = tokio::signal::ctrl_c() => {
                eprintln!("\nInterrupted.");
                break;
            }
        }

        let elapsed = start.elapsed();
        if elapsed >= duration {
            break;
        }

        drain_samples(&mut client_data);

        if elapsed < warmup {
            for c in &mut client_data {
                c.samples.clear();
                c.arrivals.clear();
                c.last_frame_time = None;
            }
            continue;
        }

        // Per-client starvation detection, aligned with MCM's pipeline stuck
        // recovery timing: MCM tears down a stuck pipeline after
        // max_lost_ticks(30) × frame_period(1/fps) ≈ 1s, then needs ~2-3s to
        // restart. Any existing WebRTC session is destroyed during teardown, so
        // a client that sees no frames for longer than the full cycle is dead.
        //
        // T_initial (never received a frame): generous to cover first ICE/DTLS.
        // T_ongoing (had frames, then stopped): tight to the restart cycle.
        const STARVATION_INITIAL: Duration = Duration::from_secs(10);
        const STARVATION_ONGOING: Duration = Duration::from_secs(5);

        if elapsed > warmup {
            let now = Instant::now();
            let mut starved: Vec<&str> = Vec::new();
            for c in client_data.iter() {
                let threshold = match c.last_frame_time {
                    Some(_) => STARVATION_ONGOING,
                    None => STARVATION_INITIAL,
                };
                let since = match c.last_frame_time {
                    Some(t) => now.duration_since(t),
                    None => elapsed - warmup,
                };
                if since > threshold {
                    starved.push(&c.name);
                }
            }
            if !starved.is_empty() {
                let names = starved.join(", ");
                eprintln!(
                    "[run {run_index}] Starvation on [{names}] after {:.0}s. Aborting measurement.",
                    elapsed.as_secs_f64()
                );
                for pipeline in &pipelines {
                    pipeline.set_state(gst::State::Null).ok();
                }
                for task in signaling_tasks {
                    task.abort();
                }
                return Err(anyhow!(
                    "Starvation detected: client(s) [{names}] received no frames for too long \
                     (elapsed={:.0}s, warmup={:.0}s)",
                    elapsed.as_secs_f64(),
                    warmup.as_secs_f64()
                ));
            }
        }

        if Instant::now() >= next_report {
            let interim = compute_run_summary(&client_data, run_index, elapsed.as_secs_f64());
            print_run_summary(
                &interim,
                &format!("Interim @ {:.0}s", elapsed.as_secs_f64()),
            );
            next_report = Instant::now() + report_interval;
        }
    }

    drain_samples(&mut client_data);

    let actual_duration = start.elapsed().as_secs_f64() - args.warmup as f64;
    let summary = compute_run_summary(&client_data, run_index, actual_duration);

    eprintln!("\n[run {run_index}] Shutting down pipelines...");
    for pipeline in &pipelines {
        pipeline.set_state(gst::State::Null).ok();
    }
    for task in signaling_tasks {
        task.abort();
    }

    Ok((summary, client_data))
}

// ── Resilient retry mode ────────────────────────────────────────────────────

async fn run_resilient(args: &Args) -> Result<()> {
    let wall_start = Instant::now();
    let total_budget = Duration::from_secs(args.duration + args.warmup);
    let retry_delay = Duration::from_secs(args.retry_delay);
    let mut segment = 0u32;
    let mut all_summaries: Vec<RunSummary> = Vec::new();

    loop {
        let elapsed = wall_start.elapsed();
        if elapsed >= total_budget {
            break;
        }

        segment += 1;
        let remaining = total_budget - elapsed;
        let remaining_secs = remaining.as_secs();

        eprintln!(
            "\n[resilient][segment {segment}] Attempting connection (wall elapsed: {:.0}s, remaining: {remaining_secs}s)...",
            elapsed.as_secs_f64()
        );

        let mut seg_args = args.clone();
        seg_args.duration = remaining_secs.saturating_sub(args.warmup);
        if seg_args.duration < 5 {
            eprintln!("[resilient] Less than 5s remaining, stopping.");
            break;
        }

        let record_prefix = args
            .record
            .as_ref()
            .map(|dir| format!("{dir}/segment_{segment:03}"));
        match run_single_measurement(&seg_args, segment, record_prefix.as_deref()).await {
            Ok((summary, client_data)) => {
                print_run_summary(&summary, &format!("Segment {segment} Final Report"));

                if let Some(ref csv_dir) = args.csv {
                    let csv_path = format!("{csv_dir}/segment_{segment:03}.csv");
                    write_csv(&client_data, &csv_path)?;
                }

                all_summaries.push(summary);
                break;
            }
            Err(e) => {
                eprintln!("[resilient][segment {segment}] Error: {e:#}");

                if wall_start.elapsed() + retry_delay >= total_budget {
                    eprintln!("[resilient] No time remaining for retry. Exiting.");
                    break;
                }
                eprintln!("[resilient] Retrying in {}s...", retry_delay.as_secs());
                tokio::time::sleep(retry_delay).await;
            }
        }
    }

    let successful = all_summaries.len();

    if !all_summaries.is_empty() {
        let aggregate = aggregate_runs(&all_summaries);
        if all_summaries.len() > 1 {
            print_aggregate(&aggregate);
        }

        if let Some(ref json_path) = args.json {
            let full = AggregatedSummary {
                runs: all_summaries,
                aggregate,
            };
            let json = serde_json::to_string_pretty(&full)?;
            if let Some(parent) = std::path::Path::new(json_path).parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(json_path, &json)?;
            eprintln!("JSON summary written to {json_path}");
        }
    } else {
        eprintln!("[resilient] No successful segments completed.");
    }

    eprintln!(
        "\n[resilient] Complete. Wall time: {:.1}s, segments attempted: {segment}, successful: {successful}",
        wall_start.elapsed().as_secs_f64(),
    );
    Ok(())
}

// ── Main ────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    if args.render.is_some() {
        return run_render(&args);
    }

    if args.analyze.is_some() {
        return run_analyze(&args);
    }

    gst::init()?;

    let n_clients = args.rtsp_urls.len() + args.udp_endpoints.len() + args.webrtc_urls.len();
    if n_clients < 1 {
        return Err(anyhow!(
            "At least one client is required.\n\
             Use --rtsp, --webrtc, and/or --udp to add clients."
        ));
    }

    if let Some(ref dir) = args.record {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("Failed to create record directory '{dir}'"))?;
    }

    if args.resilient {
        return run_resilient(&args).await;
    }

    let total_runs = args.runs;
    let mut all_summaries: Vec<RunSummary> = Vec::new();

    for run_idx in 1..=total_runs {
        eprintln!("\n{}", "=".repeat(60));
        eprintln!("  RUN {run_idx} / {total_runs}");
        eprintln!("{}", "=".repeat(60));

        let record_prefix = args
            .record
            .as_ref()
            .map(|dir| format!("{dir}/run_{run_idx}"));
        let (summary, client_data) =
            run_single_measurement(&args, run_idx, record_prefix.as_deref()).await?;

        print_run_summary(&summary, &format!("Final Report (run {run_idx})"));

        if let Some(ref csv_dir) = args.csv {
            let csv_path = format!("{csv_dir}/run_{run_idx}.csv");
            write_csv(&client_data, &csv_path)?;
        }

        all_summaries.push(summary);

        if run_idx < total_runs {
            eprintln!("\nPausing {}s before next run...", args.run_pause);
            tokio::time::sleep(Duration::from_secs(args.run_pause)).await;
        }
    }

    let aggregate = aggregate_runs(&all_summaries);
    if total_runs > 1 {
        print_aggregate(&aggregate);
    }

    if let Some(ref json_path) = args.json {
        let full = AggregatedSummary {
            runs: all_summaries,
            aggregate,
        };
        let json = serde_json::to_string_pretty(&full)?;
        if let Some(parent) = std::path::Path::new(json_path).parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(json_path, &json)?;
        eprintln!("JSON summary written to {json_path}");
    }

    eprintln!("\nAll runs complete.");
    Ok(())
}
