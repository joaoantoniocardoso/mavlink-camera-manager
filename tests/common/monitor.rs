use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use sysinfo::{Pid, ProcessExt, System, SystemExt};

#[derive(Debug, Clone)]
pub struct ResourceSample {
    pub elapsed: Duration,
    pub rss_mb: f64,
    pub threads: usize,
}

/// Periodically samples the MCM process's RSS and thread count.
pub struct ProcMonitor {
    handle: Option<tokio::task::JoinHandle<()>>,
    samples: Arc<Mutex<Vec<ResourceSample>>>,
    stop: Arc<std::sync::atomic::AtomicBool>,
}

impl ProcMonitor {
    pub fn start(pid: u32, interval: Duration) -> Self {
        let samples: Arc<Mutex<Vec<ResourceSample>>> = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));

        let samples_clone = samples.clone();
        let stop_clone = stop.clone();

        let handle = tokio::spawn(async move {
            let start = Instant::now();
            let mut sys = System::new();
            let sysinfo_pid = Pid::from(pid as usize);

            loop {
                if stop_clone.load(std::sync::atomic::Ordering::Relaxed) {
                    break;
                }

                sys.refresh_processes();

                if let Some(proc_) = sys.process(sysinfo_pid) {
                    let rss_mb = proc_.memory() as f64 / (1024.0 * 1024.0);
                    let threads = read_thread_count(pid);
                    samples_clone.lock().unwrap().push(ResourceSample {
                        elapsed: start.elapsed(),
                        rss_mb,
                        threads,
                    });
                }

                tokio::time::sleep(interval).await;
            }
        });

        Self {
            handle: Some(handle),
            samples,
            stop,
        }
    }

    pub fn take_samples(&self) -> Vec<ResourceSample> {
        let mut guard = self.samples.lock().unwrap();
        std::mem::take(&mut *guard)
    }

    pub fn stop_and_collect(&mut self) -> Vec<ResourceSample> {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            h.abort();
        }
        self.samples.lock().unwrap().clone()
    }
}

impl Drop for ProcMonitor {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            h.abort();
        }
    }
}

fn read_thread_count(pid: u32) -> usize {
    std::fs::read_dir(format!("/proc/{pid}/task"))
        .map(|entries| entries.count())
        .unwrap_or(0)
}

// -- Analysis helpers -------------------------------------------------------

/// Simple linear regression slope of RSS over time.
pub fn rss_trend_slope(samples: &[ResourceSample]) -> f64 {
    if samples.len() < 2 {
        return 0.0;
    }
    let n = samples.len() as f64;
    let xs: Vec<f64> = samples.iter().map(|s| s.elapsed.as_secs_f64()).collect();
    let ys: Vec<f64> = samples.iter().map(|s| s.rss_mb).collect();
    let x_mean = xs.iter().sum::<f64>() / n;
    let y_mean = ys.iter().sum::<f64>() / n;
    let numerator: f64 = xs
        .iter()
        .zip(ys.iter())
        .map(|(x, y)| (x - x_mean) * (y - y_mean))
        .sum();
    let denominator: f64 = xs.iter().map(|x| (x - x_mean).powi(2)).sum();
    if denominator.abs() < f64::EPSILON {
        0.0
    } else {
        numerator / denominator
    }
}

/// Returns true if thread count is stable (end within `tolerance` of start).
pub fn thread_count_stable(samples: &[ResourceSample], tolerance: usize) -> bool {
    if samples.len() < 2 {
        return true;
    }
    let first = samples.first().unwrap().threads;
    let last = samples.last().unwrap().threads;
    last.abs_diff(first) <= tolerance
}
