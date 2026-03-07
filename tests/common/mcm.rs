use std::{
    net::TcpListener,
    path::PathBuf,
    process::{Child, Command, Stdio},
    time::Duration,
};

use anyhow::{Context, Result};

pub struct McmProcess {
    child: Child,
    pub rest_port: u16,
    pub signalling_port: u16,
    pub rtsp_port: u16,
    _settings_dir: tempfile::TempDir,
}

impl McmProcess {
    /// Spawn a fresh MCM instance with `--reset` and a temporary settings file.
    /// Blocks until the REST API responds on `/info` (up to 30 s).
    pub async fn start() -> Result<Self> {
        let (rest_port, signalling_port) = allocate_port_pair()?;
        let rtsp_port = 8554; // hardcoded in MCM

        let binary = mcm_binary_path();

        let settings_dir = tempfile::tempdir().context("creating temp settings dir")?;
        let settings_file = settings_dir.path().join("settings.json");

        let qr_plugin_path = qrtimestamp_plugin_path();

        let mut cmd = Command::new(&binary);
        cmd.args([
            "--reset",
            "--verbose",
            "--rest-server",
            &format!("0.0.0.0:{rest_port}"),
            "--signalling-server",
            &format!("ws://0.0.0.0:{signalling_port}"),
            "--settings-file",
            settings_file.to_str().unwrap(),
        ])
        .env("GST_PLUGIN_PATH", &qr_plugin_path)
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());

        let child = cmd
            .spawn()
            .with_context(|| format!("spawning MCM binary at {}", binary.display()))?;

        let mcm = Self {
            child,
            rest_port,
            signalling_port,
            rtsp_port,
            _settings_dir: settings_dir,
        };

        mcm.wait_ready(Duration::from_secs(30)).await?;
        Ok(mcm)
    }

    pub fn rest_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.rest_port)
    }

    pub fn signalling_url(&self) -> String {
        format!("ws://127.0.0.1:{}", self.signalling_port)
    }

    pub fn rtsp_url(&self, path: &str) -> String {
        let path = path.trim_start_matches('/');
        format!("rtsp://127.0.0.1:{}/{path}", self.rtsp_port)
    }

    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Wait until the RTSP server is accepting TCP connections on its port.
    /// The GStreamer RTSP server binds asynchronously after `start_pipeline()`
    /// sets `run = true`, so there is a brief window where the port isn't ready.
    pub async fn wait_rtsp_ready(&self, timeout: Duration) -> Result<()> {
        let addr = format!("127.0.0.1:{}", self.rtsp_port);
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            match tokio::net::TcpStream::connect(&addr).await {
                Ok(_) => return Ok(()),
                Err(_) if tokio::time::Instant::now() < deadline => {
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
                Err(e) => {
                    anyhow::bail!(
                        "RTSP server not ready at {addr} within {}s: {e}",
                        timeout.as_secs()
                    );
                }
            }
        }
    }

    pub fn stop(&mut self) {
        #[cfg(unix)]
        {
            unsafe {
                libc::kill(self.child.id() as i32, libc::SIGTERM);
            }
            let _ = self.child.wait().ok();
        }
        #[cfg(not(unix))]
        {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    async fn wait_ready(&self, timeout: Duration) -> Result<()> {
        let url = format!("{}/info", self.rest_url());
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()?;

        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if tokio::time::Instant::now() > deadline {
                anyhow::bail!(
                    "MCM did not become ready within {}s (GET {url})",
                    timeout.as_secs()
                );
            }
            match client.get(&url).send().await {
                Ok(resp) if resp.status().is_success() => return Ok(()),
                _ => tokio::time::sleep(Duration::from_millis(250)).await,
            }
        }
    }
}

impl Drop for McmProcess {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Allocate two distinct ephemeral ports that are free on all interfaces.
///
/// Both listeners are held open simultaneously so the OS cannot return the
/// same port for both. They are closed just before returning, leaving a
/// small TOCTOU window that is acceptable for test purposes.
fn allocate_port_pair() -> Result<(u16, u16)> {
    let l1 = TcpListener::bind("0.0.0.0:0")?;
    let l2 = TcpListener::bind("0.0.0.0:0")?;
    let p1 = l1.local_addr()?.port();
    let p2 = l2.local_addr()?.port();
    drop(l1);
    drop(l2);
    Ok((p1, p2))
}

fn mcm_binary_path() -> PathBuf {
    let manifest_dir =
        PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".into()));
    let profile = if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    };
    manifest_dir
        .join("target")
        .join(profile)
        .join("mavlink-camera-manager")
}

fn qrtimestamp_plugin_path() -> String {
    let manifest_dir =
        PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".into()));
    let base = manifest_dir
        .parent()
        .unwrap_or(&manifest_dir)
        .join("qrtimestamp-gst")
        .join("target");

    // Prefer whichever profile has the built plugin
    for profile in ["release", "debug"] {
        let dir = base.join(profile);
        if dir.join("libgstqrtimestamp.so").exists() || dir.join("libgstqrtimestamp.dylib").exists()
        {
            return dir.to_string_lossy().into_owned();
        }
    }

    // Fallback: match test profile
    let profile = if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    };
    base.join(profile).to_string_lossy().into_owned()
}
