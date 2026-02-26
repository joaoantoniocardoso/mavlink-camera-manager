mod protocol;
mod rtsp_client;
mod udp_client;
mod webrtc_client;

use std::{
    collections::HashMap,
    hash::{DefaultHasher, Hasher},
    io::Write,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::{anyhow, Result};
use clap::{Parser, ValueEnum};
use gst::prelude::*;
use serde::Serialize;
use tokio::sync::mpsc;
use uuid::Uuid;

// ── Shared types ────────────────────────────────────────────────────────────

pub struct FrameSample {
    pub content_hash: u64,
    pub relative_pts_ms: i64,
    pub arrival: Instant,
    pub buffer_size: usize,
}

pub type SampleSender = mpsc::UnboundedSender<FrameSample>;

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum Codec {
    H264,
    H265,
}

/// Attach a pad probe that hashes each buffer's content and records the hash
/// together with (relative_pts_ms, wall-clock Instant).  Matching by content
/// hash guarantees we compare the exact same frame across clients, regardless
/// of any PTS re-stamping along the way.
pub fn attach_frame_probe(pad: &gst::Pad, client_name: String, sender: SampleSender) {
    let first_pts: Arc<Mutex<Option<u64>>> = Arc::new(Mutex::new(None));

    pad.add_probe(gst::PadProbeType::BUFFER, move |_, info| {
        let Some(gst::PadProbeData::Buffer(ref buffer)) = info.data else {
            return gst::PadProbeReturn::Ok;
        };

        let arrival = Instant::now();

        let Ok(map) = buffer.map_readable() else {
            return gst::PadProbeReturn::Ok;
        };
        let buffer_size = map.len();
        let mut hasher = DefaultHasher::new();
        hasher.write(map.as_slice());
        let content_hash = hasher.finish();

        let relative_pts_ms = buffer.pts().map_or(-1, |pts| {
            let pts_ns = pts.nseconds();
            let mut first = first_pts.lock().unwrap();
            let base = *first.get_or_insert(pts_ns);
            ((pts_ns - base) / 1_000_000) as i64
        });

        if sender
            .send(FrameSample {
                content_hash,
                relative_pts_ms,
                arrival,
                buffer_size,
            })
            .is_err()
        {
            eprintln!("[{client_name}] Sample channel closed");
        }

        gst::PadProbeReturn::Ok
    });
}

// ── CLI ─────────────────────────────────────────────────────────────────────

#[derive(Parser)]
#[command(
    name = "stream_latency",
    about = "Measure pairwise latency between RTSP / WebRTC / UDP stream transports"
)]
struct Args {
    /// RTSP URL(s) to receive from (repeatable)
    #[arg(long = "rtsp", value_name = "URL")]
    rtsp_urls: Vec<String>,

    /// WebRTC signalling server WebSocket URL
    #[arg(long = "webrtc", value_name = "WS_URL")]
    webrtc_url: Option<String>,

    /// WebRTC producer/stream UUID (auto-detected when only one stream exists)
    #[arg(long = "producer-id", value_name = "UUID")]
    producer_id: Option<Uuid>,

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
}

// ── Correlator / Reporter ───────────────────────────────────────────────────

struct ClientData {
    name: String,
    receiver: mpsc::UnboundedReceiver<FrameSample>,
    /// content_hash -> (relative_pts_ms, arrival wall-clock, buffer_size)
    samples: HashMap<u64, (i64, Instant, usize)>,
    /// Arrival-ordered list of (arrival, buffer_size) for bitrate/jitter stats
    arrivals: Vec<(Instant, usize)>,
}

fn drain_samples(clients: &mut [ClientData]) {
    for client in clients.iter_mut() {
        while let Ok(sample) = client.receiver.try_recv() {
            client.samples.insert(
                sample.content_hash,
                (sample.relative_pts_ms, sample.arrival, sample.buffer_size),
            );
            client.arrivals.push((sample.arrival, sample.buffer_size));
        }
    }
}

fn compute_deltas(
    a: &HashMap<u64, (i64, Instant, usize)>,
    b: &HashMap<u64, (i64, Instant, usize)>,
) -> Vec<i64> {
    let mut deltas = Vec::new();
    for (hash, &(_, a_arrival, _)) in a {
        if let Some(&(_, b_arrival, _)) = b.get(hash) {
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

#[derive(Debug, Clone, Serialize)]
struct StutterStats {
    drop_events: usize,
    estimated_missed_frames: usize,
    stutter_events: usize,
    nominal_fps: f64,
}

fn detect_stutters(inter_arrival_us: &[i64], nominal_fps: f64) -> StutterStats {
    if inter_arrival_us.len() < 2 || nominal_fps <= 0.0 {
        return StutterStats {
            drop_events: 0,
            estimated_missed_frames: 0,
            stutter_events: 0,
            nominal_fps,
        };
    }
    let expected_us = 1_000_000.0 / nominal_fps;
    let drop_threshold = expected_us * 1.8;
    let stutter_threshold = expected_us * 0.3;

    let mut drop_events = 0usize;
    let mut estimated_missed = 0usize;
    let mut stutter_events = 0usize;

    for &ia in inter_arrival_us {
        let ia_f = ia as f64;
        if ia_f > drop_threshold {
            drop_events += 1;
            estimated_missed += ((ia_f / expected_us).round() as usize).saturating_sub(1);
        } else if ia_f < stutter_threshold {
            stutter_events += 1;
        }
    }

    StutterStats {
        drop_events,
        estimated_missed_frames: estimated_missed,
        stutter_events,
        nominal_fps,
    }
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
    drop_events_mean: f64,
    stutter_events_mean: f64,
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
                drop_events: 0,
                estimated_missed_frames: 0,
                stutter_events: 0,
                nominal_fps: 0.0,
            },
        };
    }

    let first = client.arrivals.first().unwrap().0;
    let last = client.arrivals.last().unwrap().0;
    let wall_secs = last.duration_since(first).as_secs_f64();
    let total_bytes: usize = client.arrivals.iter().map(|&(_, sz)| sz).sum();

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

    let mut inter_arrival_us: Vec<i64> = client
        .arrivals
        .windows(2)
        .map(|w| w[1].0.duration_since(w[0].0).as_micros() as i64)
        .collect();
    inter_arrival_us.sort();

    let mean_ia = inter_arrival_us.iter().sum::<i64>() as f64 / inter_arrival_us.len() as f64;
    let variance = inter_arrival_us
        .iter()
        .map(|&d| (d as f64 - mean_ia).powi(2))
        .sum::<f64>()
        / inter_arrival_us.len() as f64;
    let jitter_us = variance.sqrt();

    let stutters = detect_stutters(&inter_arrival_us, fps);

    ClientSummary {
        name: client.name.clone(),
        frames: n,
        fps,
        bitrate_mbps,
        avg_frame_kb: total_bytes as f64 / n as f64 / 1024.0,
        jitter_stddev_us: jitter_us,
        inter_arrival_p50_us: percentile(&inter_arrival_us, 0.50),
        inter_arrival_p95_us: percentile(&inter_arrival_us, 0.95),
        inter_arrival_p99_us: percentile(&inter_arrival_us, 0.99),
        inter_arrival_max_us: *inter_arrival_us.last().unwrap_or(&0),
        stutters,
    }
}

fn compute_pair_summary(
    a: &ClientData,
    b: &ClientData,
) -> PairSummary {
    let deltas = compute_deltas(&a.samples, &b.samples);
    let n = deltas.len();
    let a_total = a.samples.len();

    if n == 0 {
        return PairSummary {
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
        };
    }

    let mean = deltas.iter().sum::<i64>() as f64 / n as f64;
    let var = deltas.iter().map(|&d| (d as f64 - mean).powi(2)).sum::<f64>() / n as f64;

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
                .map(|c| c.stutters.drop_events as f64)
                .collect();
            let stutter_vals: Vec<f64> = runs
                .iter()
                .filter_map(|r| r.clients.iter().find(|c| &c.name == name))
                .map(|c| c.stutters.stutter_events as f64)
                .collect();

            AggregateClientStats {
                name: name.clone(),
                fps_mean: mean_f64(&fps_vals),
                fps_stddev: stddev_f64(&fps_vals),
                jitter_mean_us: mean_f64(&jitter_vals),
                jitter_stddev_us: stddev_f64(&jitter_vals),
                drop_events_mean: mean_f64(&drop_vals),
                stutter_events_mean: mean_f64(&stutter_vals),
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

            AggregatePairStats {
                client_a: a.clone(),
                client_b: b.clone(),
                delta_mean_of_means_us: mean_f64(&mean_vals),
                delta_mean_of_p50_us: mean_f64(&p50_vals),
                delta_mean_of_p95_us: mean_f64(&p95_vals),
                delta_mean_of_p99_us: mean_f64(&p99_vals),
                delta_stddev_of_means_us: stddev_f64(&mean_vals),
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
        "  {}: {} frames, {:.1} fps, {:.2} Mbps, {:.1} KB/frame, jitter(stddev)={}, ia p50={} p95={} p99={} max={} | drops={} missed~{} stutters={}",
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
        cs.stutters.drop_events,
        cs.stutters.estimated_missed_frames,
        cs.stutters.stutter_events,
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
            "  {}: fps={:.1}+/-{:.1}, jitter={:.0}+/-{:.0} us, drops={:.1}, stutters={:.1}",
            c.name,
            c.fps_mean,
            c.fps_stddev,
            c.jitter_mean_us,
            c.jitter_stddev_us,
            c.drop_events_mean,
            c.stutter_events_mean,
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
            ",{}_pts_ms,{}_arrival_us,{}_bytes",
            c.name, c.name, c.name
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
        .flat_map(|c| c.arrivals.first().map(|&(inst, _)| inst))
        .min();
    let Some(base_instant) = base_instant else {
        return Ok(());
    };

    for hash in all_hashes {
        write!(f, "{hash:016x}")?;
        for c in clients {
            if let Some(&(pts, arrival, sz)) = c.samples.get(&hash) {
                let us = arrival.duration_since(base_instant).as_micros();
                write!(f, ",{pts},{us},{sz}")?;
            } else {
                write!(f, ",,,")?;
            }
        }
        writeln!(f)?;
    }

    eprintln!("CSV written to {path}");
    Ok(())
}

// ── Single-run measurement ──────────────────────────────────────────────────

async fn run_single_measurement(
    args: &Args,
    run_index: u32,
) -> Result<(RunSummary, Vec<ClientData>)> {
    let mut client_data: Vec<ClientData> = Vec::new();
    let mut pipelines: Vec<gst::Pipeline> = Vec::new();
    let mut signaling_tasks: Vec<tokio::task::JoinHandle<Result<()>>> = Vec::new();

    for (i, url) in args.rtsp_urls.iter().enumerate() {
        let name = format!("rtsp-{i}");
        let (tx, rx) = mpsc::unbounded_channel();
        let pipeline = rtsp_client::create_rtsp_client(&name, url, tx)?;
        eprintln!("[run {run_index}][{name}] Created for {url}");
        client_data.push(ClientData {
            name,
            receiver: rx,
            samples: HashMap::new(),
            arrivals: Vec::new(),
        });
        pipelines.push(pipeline);
    }

    for (i, endpoint) in args.udp_endpoints.iter().enumerate() {
        let name = format!("udp-{i}");
        let (addr, port_str) = endpoint
            .rsplit_once(':')
            .ok_or_else(|| anyhow!("Invalid UDP endpoint '{endpoint}', expected ADDR:PORT"))?;
        let port: i32 = port_str.parse()?;
        let (tx, rx) = mpsc::unbounded_channel();
        let pipeline = udp_client::create_udp_client(&name, addr, port, args.codec, tx)?;
        eprintln!("[run {run_index}][{name}] Created for {endpoint}");
        client_data.push(ClientData {
            name,
            receiver: rx,
            samples: HashMap::new(),
            arrivals: Vec::new(),
        });
        pipelines.push(pipeline);
    }

    if let Some(ref ws_url) = args.webrtc_url {
        let name = "webrtc-0".to_string();
        let (tx, rx) = mpsc::unbounded_channel();
        let (pipeline, task) =
            webrtc_client::create_webrtc_client(&name, ws_url, args.producer_id, tx).await?;
        eprintln!("[run {run_index}][{name}] Created for {ws_url}");
        client_data.push(ClientData {
            name,
            receiver: rx,
            samples: HashMap::new(),
            arrivals: Vec::new(),
        });
        pipelines.push(pipeline);
        signaling_tasks.push(task);
    }

    for pipeline in &pipelines {
        pipeline.set_state(gst::State::Playing)?;
    }

    let n_clients = client_data.len();
    eprintln!(
        "\n[run {run_index}] {} clients started. Measuring for {}s (warmup: {}s)...\n",
        n_clients, args.duration, args.warmup
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
            }
            continue;
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

// ── Main ────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    gst::init()?;

    let args = Args::parse();

    let n_clients =
        args.rtsp_urls.len() + args.udp_endpoints.len() + usize::from(args.webrtc_url.is_some());
    if n_clients < 2 {
        return Err(anyhow!(
            "At least two clients are required for latency comparison.\n\
             Use --rtsp, --webrtc, and/or --udp to add clients."
        ));
    }

    let total_runs = args.runs;
    let mut all_summaries: Vec<RunSummary> = Vec::new();

    for run_idx in 1..=total_runs {
        eprintln!("\n{}", "=".repeat(60));
        eprintln!("  RUN {run_idx} / {total_runs}");
        eprintln!("{}", "=".repeat(60));

        let (summary, client_data) = run_single_measurement(&args, run_idx).await?;

        print_run_summary(&summary, &format!("Final Report (run {run_idx})"));

        if let Some(ref csv_dir) = args.csv {
            let csv_path = format!("{csv_dir}/run_{run_idx}.csv");
            write_csv(&client_data, &csv_path)?;
        }

        all_summaries.push(summary);

        if run_idx < total_runs {
            eprintln!(
                "\nPausing {}s before next run...",
                args.run_pause
            );
            tokio::time::sleep(Duration::from_secs(args.run_pause)).await;
        }
    }

    if total_runs > 1 {
        let aggregate = aggregate_runs(&all_summaries);
        print_aggregate(&aggregate);

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
    } else if let Some(ref json_path) = args.json {
        let aggregate = aggregate_runs(&all_summaries);
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
