use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc, Mutex,
};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use gst::prelude::*;

#[derive(Debug, Clone)]
pub struct LatencySample {
    pub timestamp_ms: i64,
    pub latency_ms: i64,
    pub arrived_at: Instant,
}

/// Ensure GStreamer and the qrtimestamp plugin are initialised exactly once.
pub fn init_gst() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        gst::init().expect("GStreamer init");
        gstqrtimestamp::plugin_register_static().expect("qrtimestamp plugin register");
    });
}

/// Returns true if MCM will have access to the `qrtimestampsrc` plugin.
///
/// We check whether the compiled `.so` exists at the path MCM would load it from
/// (via `GST_PLUGIN_PATH`), rather than querying the test process's GStreamer
/// registry (which has the plugin registered statically).
pub fn has_qr_plugin() -> bool {
    let manifest_dir = std::path::PathBuf::from(
        std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".into()),
    );
    let base = manifest_dir
        .parent()
        .unwrap_or(&manifest_dir)
        .join("qrtimestamp-gst")
        .join("target");

    for profile in ["release", "debug"] {
        let dir = base.join(profile);
        if dir.join("libgstqrtimestamp.so").exists() || dir.join("libgstqrtimestamp.dylib").exists()
        {
            return true;
        }
    }
    false
}

/// Spawn a GStreamer RTSP client pipeline and count received buffers.
/// Returns a handle whose `frame_count()` can be polled.
pub fn rtsp_frame_counter(rtsp_url: &str) -> Result<RtspFrameCounter> {
    init_gst();

    let pipeline_str = format!(
        "rtspsrc location={rtsp_url} latency=100 protocols=tcp \
         ! fakesink name=counter sync=false signal-handoffs=true"
    );
    let pipeline = gst::parse::launch(&pipeline_str)
        .context("parsing RTSP frame counter pipeline")?
        .downcast::<gst::Pipeline>()
        .unwrap();

    let count = Arc::new(AtomicU64::new(0));

    let fakesink = pipeline
        .by_name("counter")
        .context("finding fakesink 'counter'")?;

    let count_clone = count.clone();
    fakesink.connect("handoff", false, move |_| {
        count_clone.fetch_add(1, Ordering::Relaxed);
        None
    });

    pipeline
        .set_state(gst::State::Playing)
        .context("setting RTSP counter pipeline to Playing")?;

    Ok(RtspFrameCounter { pipeline, count })
}

pub struct RtspFrameCounter {
    pipeline: gst::Pipeline,
    count: Arc<AtomicU64>,
}

impl RtspFrameCounter {
    pub fn frame_count(&self) -> u64 {
        self.count.load(Ordering::Relaxed)
    }
}

impl Drop for RtspFrameCounter {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

/// Create a QR-based latency measurement pipeline that connects to an RTSP
/// stream produced by MCM's `qrtimestampsrc`, decodes each frame, and
/// feeds it into `qrtimestampsink` whose `on-render` signal reports the
/// end-to-end latency (now − encoded-timestamp).
///
/// Returns a handle that collects `LatencySample`s in the background.
pub fn qr_latency_probe(rtsp_url: &str) -> Result<QrLatencyProbe> {
    init_gst();

    let pipeline_str = format!(
        "rtspsrc location={rtsp_url} latency=0 protocols=tcp \
         ! rtph264depay ! avdec_h264 ! videoconvert \
         ! qrtimestampsink name=sink sync=false"
    );
    let pipeline = gst::parse::launch(&pipeline_str)
        .context("parsing QR latency pipeline")?
        .downcast::<gst::Pipeline>()
        .unwrap();

    let samples: Arc<Mutex<Vec<LatencySample>>> = Arc::new(Mutex::new(Vec::new()));
    let started = Instant::now();

    let sink = pipeline
        .by_name("sink")
        .context("finding qrtimestampsink element")?;

    let samples_clone = samples.clone();
    sink.connect("on-render", false, move |args| {
        let latency_ms = args[2].get::<i64>().unwrap_or(0);
        let now = Instant::now();
        let ts_ms = now.duration_since(started).as_millis() as i64;
        samples_clone.lock().unwrap().push(LatencySample {
            timestamp_ms: ts_ms,
            latency_ms,
            arrived_at: now,
        });
        None
    });

    pipeline
        .set_state(gst::State::Playing)
        .context("setting QR latency pipeline to Playing")?;

    Ok(QrLatencyProbe { pipeline, samples })
}

pub struct QrLatencyProbe {
    pipeline: gst::Pipeline,
    samples: Arc<Mutex<Vec<LatencySample>>>,
}

impl QrLatencyProbe {
    pub fn take_samples(&self) -> Vec<LatencySample> {
        let mut guard = self.samples.lock().unwrap();
        std::mem::take(&mut *guard)
    }

    pub fn sample_count(&self) -> usize {
        self.samples.lock().unwrap().len()
    }
}

impl Drop for QrLatencyProbe {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

// -- Subprocess-based QR probe ----------------------------------------------

/// A QR latency probe that runs in a separate process, avoiding GStreamer
/// in-process conflicts when multiple RTSP clients are needed simultaneously.
///
/// Spawns the `qr_probe` example binary which connects to an RTSP stream,
/// measures latency via `qrtimestampsink`, and outputs CSV to stdout.
pub struct SubprocessQrProbe {
    child: Option<std::process::Child>,
}

impl SubprocessQrProbe {
    /// Spawn a probe that connects to `rtsp_url` and measures for `duration`.
    /// The `gst_plugin_path` is passed as `GST_PLUGIN_PATH` so MCM's qrtimestamp
    /// plugin is available to the subprocess.
    pub fn spawn(rtsp_url: &str, duration: Duration, gst_plugin_path: &str) -> Result<Self> {
        let binary = probe_binary_path();
        let child = std::process::Command::new(&binary)
            .args([rtsp_url, &duration.as_secs().to_string()])
            .env("GST_PLUGIN_PATH", gst_plugin_path)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .with_context(|| format!("spawning qr_probe binary at {}", binary.display()))?;
        Ok(Self { child: Some(child) })
    }

    /// Wait for the probe to finish and parse its output into `LatencySample`s.
    pub fn collect(mut self) -> Result<Vec<LatencySample>> {
        let child = self.child.take().context("probe already collected")?;
        let output = child
            .wait_with_output()
            .context("waiting for qr_probe process")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!(
                "qr_probe exited with {}: {}",
                output.status,
                stderr.lines().take(5).collect::<Vec<_>>().join("\n")
            );
        }

        let started = Instant::now();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut samples = Vec::new();
        for line in stdout.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let parts: Vec<&str> = line.split(',').collect();
            if parts.len() == 2 {
                if let (Ok(ts), Ok(lat)) = (parts[0].parse::<i64>(), parts[1].parse::<i64>()) {
                    samples.push(LatencySample {
                        timestamp_ms: ts,
                        latency_ms: lat,
                        arrived_at: started + Duration::from_millis(ts as u64),
                    });
                }
            }
        }
        Ok(samples)
    }
}

impl Drop for SubprocessQrProbe {
    fn drop(&mut self) {
        if let Some(ref mut child) = self.child {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn probe_binary_path() -> std::path::PathBuf {
    let manifest_dir = std::path::PathBuf::from(
        std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".into()),
    );
    let profile = if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    };
    manifest_dir
        .join("target")
        .join(profile)
        .join("examples")
        .join("qr_probe")
}

/// Convenience: the GST_PLUGIN_PATH that contains the qrtimestamp `.so`.
pub fn qrtimestamp_gst_plugin_path() -> String {
    let manifest_dir = std::path::PathBuf::from(
        std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".into()),
    );
    let base = manifest_dir
        .parent()
        .unwrap_or(&manifest_dir)
        .join("qrtimestamp-gst")
        .join("target");

    for profile in ["release", "debug"] {
        let dir = base.join(profile);
        if dir.join("libgstqrtimestamp.so").exists() || dir.join("libgstqrtimestamp.dylib").exists()
        {
            return dir.to_string_lossy().into_owned();
        }
    }

    base.join("release").to_string_lossy().into_owned()
}

// -- Metrics computation ----------------------------------------------------

pub struct LatencyStats {
    pub count: usize,
    pub fps: f64,
    pub p50_ms: f64,
    pub p95_ms: f64,
    pub p99_ms: f64,
    pub max_ms: f64,
    pub jitter_ms: f64,
    pub drops: usize,
}

impl std::fmt::Display for LatencyStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "frames={} fps={:.1} p50={:.1}ms p95={:.1}ms p99={:.1}ms max={:.1}ms jitter={:.1}ms drops={}",
            self.count, self.fps, self.p50_ms, self.p95_ms, self.p99_ms, self.max_ms, self.jitter_ms, self.drops
        )
    }
}

pub fn compute_stats(samples: &[LatencySample], expected_fps: f64) -> LatencyStats {
    if samples.is_empty() {
        return LatencyStats {
            count: 0,
            fps: 0.0,
            p50_ms: 0.0,
            p95_ms: 0.0,
            p99_ms: 0.0,
            max_ms: 0.0,
            jitter_ms: 0.0,
            drops: 0,
        };
    }

    let mut latencies: Vec<f64> = samples.iter().map(|s| s.latency_ms as f64).collect();
    latencies.sort_by(|a, b| a.partial_cmp(b).unwrap());

    let n = latencies.len();
    let percentile = |p: f64| -> f64 {
        let idx = ((p / 100.0) * (n - 1) as f64).round() as usize;
        latencies[idx.min(n - 1)]
    };

    let duration = if samples.len() >= 2 {
        let first = samples.first().unwrap().arrived_at;
        let last = samples.last().unwrap().arrived_at;
        last.duration_since(first).as_secs_f64()
    } else {
        1.0
    };

    let fps = if duration > 0.0 {
        (n - 1) as f64 / duration
    } else {
        0.0
    };

    let expected_interval = Duration::from_secs_f64(1.0 / expected_fps);
    let drop_threshold = expected_interval.mul_f64(2.0);
    let mut drops = 0usize;
    for pair in samples.windows(2) {
        if pair[1].arrived_at.duration_since(pair[0].arrived_at) > drop_threshold {
            drops += 1;
        }
    }

    let inter_arrivals: Vec<f64> = samples
        .windows(2)
        .map(|w| {
            w[1].arrived_at
                .duration_since(w[0].arrived_at)
                .as_secs_f64()
                * 1000.0
        })
        .collect();
    let mean_ia = inter_arrivals.iter().sum::<f64>() / inter_arrivals.len().max(1) as f64;
    let variance = inter_arrivals
        .iter()
        .map(|v| (v - mean_ia).powi(2))
        .sum::<f64>()
        / inter_arrivals.len().max(1) as f64;
    let jitter_ms = variance.sqrt();

    LatencyStats {
        count: n,
        fps,
        p50_ms: percentile(50.0),
        p95_ms: percentile(95.0),
        p99_ms: percentile(99.0),
        max_ms: *latencies.last().unwrap(),
        jitter_ms,
        drops,
    }
}
