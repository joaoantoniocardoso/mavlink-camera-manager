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
    pub async fn start() -> Result<Self> {
        let ports = allocate_ports(2)?;
        let rest_port = ports[0];
        let signalling_port = ports[1];
        let rtsp_port = 8554;

        let binary = mcm_binary_path();

        let settings_dir = tempfile::tempdir().context("creating temp settings dir")?;
        let settings_file = settings_dir.path().join("settings.json");

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

pub fn allocate_ports(n: u8) -> Result<Vec<u16>> {
    let listeners: Vec<TcpListener> = (0..n)
        .map(|_| TcpListener::bind("0.0.0.0:0"))
        .collect::<std::io::Result<_>>()?;
    let ports = listeners
        .iter()
        .map(|l| l.local_addr().map(|a| a.port()))
        .collect::<std::io::Result<_>>()?;
    drop(listeners);
    Ok(ports)
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
