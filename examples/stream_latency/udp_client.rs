use std::sync::Arc;

use anyhow::{Context, Result};
use gst::prelude::*;

use super::{
    Codec, PcapRecorder, RtpTracker, SampleSender, attach_frame_probe, attach_rtp_counter,
    attach_rtp_recorder,
};

pub fn create_udp_client(
    name: &str,
    address: &str,
    port: i32,
    codec: Codec,
    sender: SampleSender,
    recorder: Option<Arc<PcapRecorder>>,
) -> Result<gst::Pipeline> {
    let pipeline = gst::Pipeline::with_name(name);

    let (encoding_name, depay_factory, parse_factory, norm_caps) = match codec {
        Codec::H264 => (
            "H264",
            "rtph264depay",
            "h264parse",
            gst::Caps::builder("video/x-h264")
                .field("stream-format", "byte-stream")
                .field("alignment", "au")
                .build(),
        ),
        Codec::H265 => (
            "H265",
            "rtph265depay",
            "h265parse",
            gst::Caps::builder("video/x-h265")
                .field("stream-format", "byte-stream")
                .field("alignment", "au")
                .build(),
        ),
    };

    let rtp_caps = gst::Caps::builder("application/x-rtp")
        .field("media", "video")
        .field("clock-rate", 90000i32)
        .field("encoding-name", encoding_name)
        .build();

    let src = gst::ElementFactory::make("udpsrc")
        .property("address", address)
        .property("port", port)
        .property("caps", &rtp_caps)
        .build()
        .context("Failed to create udpsrc")?;

    let depay = gst::ElementFactory::make(depay_factory)
        .build()
        .context(format!("Failed to create {depay_factory}"))?;

    let parse = gst::ElementFactory::make(parse_factory)
        .build()
        .context(format!("Failed to create {parse_factory}"))?;

    let capsfilter = gst::ElementFactory::make("capsfilter")
        .property("caps", &norm_caps)
        .build()
        .context("Failed to create capsfilter")?;

    let sink = gst::ElementFactory::make("fakesink")
        .property("sync", false)
        .property("async", false)
        .build()
        .context("Failed to create fakesink")?;

    pipeline.add_many([&src, &depay, &parse, &capsfilter, &sink])?;
    gst::Element::link_many([&src, &depay, &parse, &capsfilter, &sink])?;

    let rtp_sink_pad = depay.static_pad("sink").unwrap();
    let rtp_tracker = Arc::new(RtpTracker::new());
    attach_rtp_counter(&rtp_sink_pad, Arc::clone(&rtp_tracker));

    let probe_pad = parse.static_pad("src").unwrap();
    attach_frame_probe(&probe_pad, name.to_string(), sender, Some(rtp_tracker));

    if let Some(rec) = recorder {
        attach_rtp_recorder(&rtp_sink_pad, rec);
    }

    Ok(pipeline)
}
