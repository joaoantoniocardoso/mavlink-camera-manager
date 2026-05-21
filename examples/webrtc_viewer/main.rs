use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::Parser;
use stream_clients::{StreamClient, webrtc_client::WebrtcClient};
use uuid::Uuid;

/// Open a WebRTC stream from an MCM signalling endpoint and display it through
/// a permissive GStreamer decoder (`avdec_h264` / `avdec_h265`) so that
/// partially-decoded frames are rendered with their macroblock artifacts,
/// matching what `gst-launch ... avdec_h264 ! videoconvert ! autovideosink`
/// would do for an RTSP feed.
///
/// This is intended for lab repro of pipeline-side corruption that Chrome's
/// strict WebRTC decoder would otherwise hide behind a freeze.
#[derive(Parser)]
#[command(name = "webrtc_viewer")]
struct Args {
    /// WebRTC signalling URL (e.g. ws://192.168.2.2:6020/ws)
    #[arg(long)]
    webrtc: String,

    /// Optional producer/stream UUID (auto-detected when only one stream exists)
    #[arg(long)]
    producer_id: Option<Uuid>,

    /// GStreamer sink description (gst-launch syntax).
    /// Examples:
    ///   "autovideosink sync=false"
    ///   "fpsdisplaysink sync=false text-overlay=true video-sink=autovideosink"
    ///   "glimagesink sync=false"
    #[arg(
        long,
        default_value = "fpsdisplaysink sync=false text-overlay=true video-sink=autovideosink"
    )]
    sink: String,

    /// Optional ICE candidate IP prefix filter (e.g. "192.168.2." to force LAN)
    #[arg(long)]
    ice_filter: Option<String>,

    /// Stop after this many seconds (default: run until Ctrl+C)
    #[arg(long)]
    duration: Option<u64>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    // SAFETY: single-threaded init before any other thread can read env.
    unsafe {
        std::env::set_var("MCM_WEBRTC_VIDEO_SINK", &args.sink);
    }

    eprintln!(
        "[webrtc-viewer] connecting to {} sink=\"{}\"",
        args.webrtc, args.sink
    );

    let client = WebrtcClient::connect(
        &args.webrtc,
        args.producer_id,
        None,
        args.ice_filter.as_deref(),
    )
    .await
    .context("WebRTC connect failed")?;

    eprintln!("[webrtc-viewer] connected; rendering frames...");

    let started = Instant::now();
    let mut last_parsed = 0u64;
    let mut last_decoded = 0u64;
    let report = tokio::time::sleep(Duration::from_secs(1));
    tokio::pin!(report);
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                eprintln!("[webrtc-viewer] interrupted");
                break;
            }
            _ = &mut report => {
                let parsed = client.frames();
                let decoded = client.decoded_frames();
                let d_parsed = parsed - last_parsed;
                let d_decoded = decoded - last_decoded;
                last_parsed = parsed;
                last_decoded = decoded;
                eprintln!(
                    "[webrtc-viewer] +{d_parsed}f/s parsed, +{d_decoded}f/s decoded (totals parsed={parsed} decoded={decoded})"
                );
                if let Some(d) = args.duration {
                    if started.elapsed().as_secs() >= d {
                        break;
                    }
                }
                report.as_mut().reset(tokio::time::Instant::now() + Duration::from_secs(1));
            }
        }
    }

    Ok(())
}
