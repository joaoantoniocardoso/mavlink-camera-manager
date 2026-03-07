//! Standalone QR latency probe process.
//!
//! Connects to an RTSP stream via GStreamer, decodes each frame through
//! `qrtimestampsink`, and writes one CSV line per sample to stdout:
//!
//!     timestamp_ms,latency_ms
//!
//! Usage:
//!     qr_probe <rtsp_url> <duration_secs>

use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gst::prelude::*;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 3 {
        eprintln!("Usage: {} <rtsp_url> <duration_secs>", args[0]);
        std::process::exit(1);
    }
    let rtsp_url = &args[1];
    let duration_secs: u64 = args[2].parse().expect("duration must be an integer");

    gst::init().expect("GStreamer init");
    gstqrtimestamp::plugin_register_static().expect("qrtimestamp plugin register");

    let pipeline_str = format!(
        "rtspsrc location={rtsp_url} latency=0 protocols=tcp \
         ! rtph264depay ! avdec_h264 ! videoconvert \
         ! qrtimestampsink name=sink sync=false"
    );
    let pipeline = gst::parse::launch(&pipeline_str)
        .expect("failed to parse pipeline")
        .downcast::<gst::Pipeline>()
        .unwrap();

    let samples: Arc<Mutex<Vec<(i64, i64)>>> = Arc::new(Mutex::new(Vec::new()));
    let started = Instant::now();

    let sink = pipeline.by_name("sink").expect("sink element not found");
    let samples_clone = samples.clone();
    sink.connect("on-render", false, move |args| {
        let latency_ms = args[2].get::<i64>().unwrap_or(0);
        let ts = started.elapsed().as_millis() as i64;
        samples_clone.lock().unwrap().push((ts, latency_ms));
        None
    });

    let bus = pipeline.bus().unwrap();
    bus.add_watch(move |_, msg| {
        if let gst::MessageView::Error(err) = msg.view() {
            eprintln!("Pipeline error: {} ({:?})", err.error(), err.debug());
            return gst::glib::ControlFlow::Break;
        }
        gst::glib::ControlFlow::Continue
    })
    .unwrap();

    pipeline
        .set_state(gst::State::Playing)
        .expect("failed to set pipeline to Playing");

    std::thread::sleep(Duration::from_secs(duration_secs));

    let _ = pipeline.set_state(gst::State::Null);

    let out = std::io::stdout();
    let mut out = out.lock();
    for (ts, lat) in samples.lock().unwrap().iter() {
        writeln!(out, "{ts},{lat}").ok();
    }
}
