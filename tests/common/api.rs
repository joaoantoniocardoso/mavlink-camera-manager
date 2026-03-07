use std::time::Duration;

use anyhow::{Context, Result};
use url::Url;

use super::types::*;

pub struct McmClient {
    client: reqwest::Client,
    base_url: String,
}

impl McmClient {
    pub fn new(base_url: &str) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .expect("building reqwest client");
        Self {
            client,
            base_url: base_url.trim_end_matches('/').to_string(),
        }
    }

    // -- Info ---------------------------------------------------------------

    pub async fn info(&self) -> Result<Info> {
        let resp = self
            .client
            .get(format!("{}/info", self.base_url))
            .send()
            .await?
            .error_for_status()?;
        resp.json().await.context("deserializing /info")
    }

    // -- Sources ------------------------------------------------------------

    pub async fn sources(&self) -> Result<Vec<ApiVideoSource>> {
        let resp = self
            .client
            .get(format!("{}/v4l", self.base_url))
            .send()
            .await?
            .error_for_status()?;
        resp.json().await.context("deserializing /v4l")
    }

    // -- Streams ------------------------------------------------------------

    pub async fn list_streams(&self) -> Result<Vec<StreamStatus>> {
        let resp = self
            .client
            .get(format!("{}/streams", self.base_url))
            .send()
            .await?
            .error_for_status()?;
        resp.json().await.context("deserializing /streams")
    }

    pub async fn create_stream(&self, post: &PostStream) -> Result<Vec<StreamStatus>> {
        let resp = self
            .client
            .post(format!("{}/streams", self.base_url))
            .json(post)
            .send()
            .await?
            .error_for_status()?;
        resp.json().await.context("deserializing POST /streams")
    }

    pub async fn create_stream_raw(&self, post: &PostStream) -> Result<reqwest::Response> {
        Ok(self
            .client
            .post(format!("{}/streams", self.base_url))
            .json(post)
            .send()
            .await?)
    }

    pub async fn delete_stream(&self, name: &str) -> Result<Vec<StreamStatus>> {
        let resp = self
            .client
            .delete(format!("{}/delete_stream", self.base_url))
            .query(&[("name", name)])
            .send()
            .await?
            .error_for_status()?;
        resp.json()
            .await
            .context("deserializing DELETE /delete_stream")
    }

    pub async fn delete_stream_raw(&self, name: &str) -> Result<reqwest::Response> {
        Ok(self
            .client
            .delete(format!("{}/delete_stream", self.base_url))
            .query(&[("name", name)])
            .send()
            .await?)
    }

    // -- Restart / Reset ----------------------------------------------------

    pub async fn restart_streams(&self) -> Result<()> {
        self.client
            .post(format!("{}/restart_streams", self.base_url))
            .query(&[("all", "true")])
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    pub async fn reset_settings(&self) -> Result<()> {
        self.client
            .post(format!("{}/reset_settings", self.base_url))
            .query(&[("all", "true")])
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    // -- Block / Unblock sources --------------------------------------------

    pub async fn block_source(&self, source_string: &str) -> Result<()> {
        self.client
            .post(format!("{}/block_source", self.base_url))
            .query(&[("source_string", source_string)])
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    pub async fn unblock_source(&self, source_string: &str) -> Result<()> {
        self.client
            .post(format!("{}/unblock_source", self.base_url))
            .query(&[("source_string", source_string)])
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    pub async fn blocked_sources(&self) -> Result<Vec<String>> {
        let resp = self
            .client
            .get(format!("{}/blocked_sources", self.base_url))
            .send()
            .await?
            .error_for_status()?;
        resp.json().await.context("deserializing /blocked_sources")
    }

    pub async fn clear_blocked_sources(&self) -> Result<()> {
        self.client
            .delete(format!("{}/blocked_sources", self.base_url))
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    // -- Thumbnail ----------------------------------------------------------

    pub async fn thumbnail_raw(&self, source: &str) -> Result<reqwest::Response> {
        Ok(self
            .client
            .get(format!("{}/thumbnail", self.base_url))
            .query(&[("source", source)])
            .send()
            .await?)
    }

    // -- Convenience builders -----------------------------------------------

    pub fn build_fake_h264_rtsp(
        name: &str,
        width: u32,
        height: u32,
        fps: u32,
        path: &str,
    ) -> PostStream {
        PostStream {
            name: name.to_string(),
            source: "ball".to_string(),
            stream_information: StreamInformation {
                endpoints: vec![Url::parse(&format!("rtsp://0.0.0.0:8554/{path}")).unwrap()],
                configuration: CaptureConfiguration::Video(VideoCaptureConfiguration {
                    encode: "H264".to_string(),
                    height,
                    width,
                    frame_interval: FrameInterval {
                        numerator: 1,
                        denominator: fps,
                    },
                }),
                extended_configuration: None,
            },
        }
    }

    pub fn build_fake_h264_udp(
        name: &str,
        width: u32,
        height: u32,
        fps: u32,
        host: &str,
        port: u16,
    ) -> PostStream {
        PostStream {
            name: name.to_string(),
            source: "ball".to_string(),
            stream_information: StreamInformation {
                endpoints: vec![Url::parse(&format!("udp://{host}:{port}")).unwrap()],
                configuration: CaptureConfiguration::Video(VideoCaptureConfiguration {
                    encode: "H264".to_string(),
                    height,
                    width,
                    frame_interval: FrameInterval {
                        numerator: 1,
                        denominator: fps,
                    },
                }),
                extended_configuration: None,
            },
        }
    }

    pub fn build_qr_h264_rtsp(name: &str, size: u32, fps: u32, path: &str) -> PostStream {
        PostStream {
            name: name.to_string(),
            source: "QRTimeStamp".to_string(),
            stream_information: StreamInformation {
                endpoints: vec![Url::parse(&format!("rtsp://0.0.0.0:8554/{path}")).unwrap()],
                configuration: CaptureConfiguration::Video(VideoCaptureConfiguration {
                    encode: "H264".to_string(),
                    height: size,
                    width: size,
                    frame_interval: FrameInterval {
                        numerator: 1,
                        denominator: fps,
                    },
                }),
                extended_configuration: None,
            },
        }
    }

    pub fn build_fake_h264_rtsp_ext(
        name: &str,
        width: u32,
        height: u32,
        fps: u32,
        path: &str,
        ext: ExtendedConfiguration,
    ) -> PostStream {
        PostStream {
            name: name.to_string(),
            source: "ball".to_string(),
            stream_information: StreamInformation {
                endpoints: vec![Url::parse(&format!("rtsp://0.0.0.0:8554/{path}")).unwrap()],
                configuration: CaptureConfiguration::Video(VideoCaptureConfiguration {
                    encode: "H264".to_string(),
                    height,
                    width,
                    frame_interval: FrameInterval {
                        numerator: 1,
                        denominator: fps,
                    },
                }),
                extended_configuration: Some(ext),
            },
        }
    }

    /// Poll `/streams` until at least `count` streams report `running: true`,
    /// or until `timeout` elapses.
    pub async fn wait_for_streams_running(
        &self,
        count: usize,
        timeout: Duration,
    ) -> Result<Vec<StreamStatus>> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let streams = self.list_streams().await?;
            let running = streams.iter().filter(|s| s.running).count();
            if running >= count {
                return Ok(streams);
            }
            if tokio::time::Instant::now() > deadline {
                anyhow::bail!(
                    "only {running}/{count} streams running after {}s",
                    timeout.as_secs()
                );
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }
}
