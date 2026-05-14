//! Pipeline B0 instrumentation - always-on diagnostic probes used for the
//! `pr/integration-test-suite` on-vehicle WBS.
//!
//! The probes are designed so the on-vehicle debug binary emits structured
//! `tracing` events that can be filtered by `field`:
//!
//! - `mcm_inst="v4l2_drops"` - per-second buffer count and PTS-gap-derived
//!   drop count on each `v4l2src` src pad. `v4l2src` reports QoS dropped
//!   as `-1`, so we derive drops from PTS deltas instead.
//! - `mcm_inst="queue_level"` - per-second `current-level-{buffers,time,bytes}`
//!   for every `queue` element discovered in the pipeline.
//! - `mcm_inst="rtp_stats"` - per-second packets/bytes/NACKs for any
//!   `rtpsession` or `nicesink` discovered in the pipeline.
//!
//! All events carry a `pipeline_id` field so multi-pipeline runs stay
//! disambiguated.

use std::{
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};

use gst::prelude::*;
use tracing::{info, instrument, warn};

/// Install the always-on B0 probes on `pipeline`. Spawns one tokio task
/// per metric family; tasks exit cleanly when the pipeline is dropped.
#[instrument(level = "debug", skip(pipeline), fields(pipeline_id = pipeline_id.to_string()))]
pub fn install(pipeline: &gst::Pipeline, pipeline_id: &Arc<uuid::Uuid>) {
    install_v4l2_drop_probes(pipeline, pipeline_id);
    spawn_queue_level_logger(pipeline, pipeline_id);
    spawn_rtp_stats_logger(pipeline, pipeline_id);
}

/// Walk the pipeline once and attach a PTS-gap probe to every `v4l2src`
/// src pad. The probe is idempotent: each probe owns its own state.
fn install_v4l2_drop_probes(pipeline: &gst::Pipeline, pipeline_id: &Arc<uuid::Uuid>) {
    let bin = pipeline.upcast_ref::<gst::Bin>();
    for element in bin.iterate_recurse().into_iter().filter_map(Result::ok) {
        let factory_name = element
            .factory()
            .map(|f| f.name().to_string())
            .unwrap_or_default();
        if factory_name != "v4l2src" {
            continue;
        }
        let Some(src_pad) = element.static_pad("src") else {
            continue;
        };
        attach_v4l2_drop_probe(&element, &src_pad, pipeline_id);
    }
}

fn attach_v4l2_drop_probe(
    element: &gst::Element,
    src_pad: &gst::Pad,
    pipeline_id: &Arc<uuid::Uuid>,
) {
    let element_name = element.name().to_string();
    let pipeline_id_str = pipeline_id.to_string();
    let buffers = Arc::new(AtomicU64::new(0));
    let drops = Arc::new(AtomicU64::new(0));
    let last_pts_ns = Arc::new(AtomicU64::new(u64::MAX));

    let probe_buffers = buffers.clone();
    let probe_drops = drops.clone();
    let probe_last = last_pts_ns.clone();
    src_pad.add_probe(
        gst::PadProbeType::BUFFER | gst::PadProbeType::BUFFER_LIST,
        move |_pad, info| {
            let count = match &info.data {
                Some(gst::PadProbeData::Buffer(buffer)) => {
                    if let Some(pts) = buffer.pts() {
                        record_pts(&probe_drops, &probe_last, pts.nseconds());
                    }
                    1
                }
                Some(gst::PadProbeData::BufferList(list)) => {
                    let count = list.len() as u64;
                    if let Some(last) = list.iter().next_back() {
                        if let Some(pts) = last.pts() {
                            record_pts(&probe_drops, &probe_last, pts.nseconds());
                        }
                    }
                    count
                }
                _ => 0,
            };
            probe_buffers.fetch_add(count, Ordering::Relaxed);
            gst::PadProbeReturn::Ok
        },
    );

    let element_weak = element.downgrade();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(1));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut last_buffers = 0u64;
        let mut last_drops = 0u64;
        loop {
            interval.tick().await;
            if element_weak.upgrade().is_none() {
                break;
            }
            let total_buffers = buffers.load(Ordering::Relaxed);
            let total_drops = drops.load(Ordering::Relaxed);
            let delta_buffers = total_buffers.wrapping_sub(last_buffers);
            let delta_drops = total_drops.wrapping_sub(last_drops);
            last_buffers = total_buffers;
            last_drops = total_drops;
            info!(
                mcm_inst = "v4l2_drops",
                pipeline_id = %pipeline_id_str,
                element = %element_name,
                buffers_1s = delta_buffers,
                drops_1s = delta_drops,
                buffers_total = total_buffers,
                drops_total = total_drops,
                "v4l2src 1s window",
            );
        }
    });
}

/// PTS-gap derived drop estimator.
///
/// We don't know the configured frame interval here, so we use a fixed
/// threshold of ~50 ms: any inter-buffer PTS jump bigger than that
/// indicates at least one missing frame at 30+ fps. The exact frame
/// duration could be wired through, but the 50 ms threshold is good
/// enough for the WBS - what matters is *when* drops happen, not the
/// absolute count.
fn record_pts(drops: &AtomicU64, last_pts_ns: &AtomicU64, pts_ns: u64) {
    const GAP_THRESHOLD_NS: u64 = 50_000_000;
    const ASSUMED_FRAME_NS: u64 = 33_333_333; // ~30 fps
    let prev = last_pts_ns.swap(pts_ns, Ordering::Relaxed);
    if prev == u64::MAX || pts_ns <= prev {
        return;
    }
    let gap = pts_ns - prev;
    if gap > GAP_THRESHOLD_NS {
        let missed = (gap / ASSUMED_FRAME_NS).saturating_sub(1);
        if missed > 0 {
            drops.fetch_add(missed, Ordering::Relaxed);
        }
    }
}

fn spawn_queue_level_logger(pipeline: &gst::Pipeline, pipeline_id: &Arc<uuid::Uuid>) {
    let pipeline_weak = pipeline.downgrade();
    let pipeline_id_str = pipeline_id.to_string();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(1));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            let Some(pipeline) = pipeline_weak.upgrade() else {
                break;
            };
            log_queue_levels(&pipeline, &pipeline_id_str);
        }
    });
}

fn log_queue_levels(pipeline: &gst::Pipeline, pipeline_id_str: &str) {
    let bin = pipeline.upcast_ref::<gst::Bin>();
    for element in bin.iterate_recurse().into_iter().filter_map(Result::ok) {
        let factory_name = element
            .factory()
            .map(|f| f.name().to_string())
            .unwrap_or_default();
        if factory_name != "queue" && factory_name != "queue2" {
            continue;
        }
        let buffers = read_u32(&element, "current-level-buffers");
        let time_ns = read_u64(&element, "current-level-time");
        let bytes = read_u32(&element, "current-level-bytes");
        let max_buffers = read_u32(&element, "max-size-buffers");
        let max_time_ns = read_u64(&element, "max-size-time");
        info!(
            mcm_inst = "queue_level",
            pipeline_id = %pipeline_id_str,
            queue = %element.name(),
            factory = %factory_name,
            buffers,
            time_ns,
            bytes,
            max_buffers,
            max_time_ns,
            "queue 1s sample",
        );
    }
}

fn spawn_rtp_stats_logger(pipeline: &gst::Pipeline, pipeline_id: &Arc<uuid::Uuid>) {
    let pipeline_weak = pipeline.downgrade();
    let pipeline_id_str = pipeline_id.to_string();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(1));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            let Some(pipeline) = pipeline_weak.upgrade() else {
                break;
            };
            log_rtp_stats(&pipeline, &pipeline_id_str);
        }
    });
}

fn log_rtp_stats(pipeline: &gst::Pipeline, pipeline_id_str: &str) {
    let bin = pipeline.upcast_ref::<gst::Bin>();
    for element in bin.iterate_recurse().into_iter().filter_map(Result::ok) {
        let factory_name = element
            .factory()
            .map(|f| f.name().to_string())
            .unwrap_or_default();

        if factory_name == "rtpsession" {
            log_rtpsession_stats(&element, pipeline_id_str);
        } else if factory_name == "nicesink" {
            log_nicesink_stats(&element, pipeline_id_str);
        }
    }
}

fn log_rtpsession_stats(element: &gst::Element, pipeline_id_str: &str) {
    if element.find_property("stats").is_none() {
        return;
    }
    let stats = element.property::<gst::Structure>("stats");
    info!(
        mcm_inst = "rtp_stats",
        pipeline_id = %pipeline_id_str,
        element = %element.name(),
        factory = "rtpsession",
        stats = %stats,
        "rtpsession 1s sample",
    );
}

fn log_nicesink_stats(element: &gst::Element, pipeline_id_str: &str) {
    // nicesink doesn't expose a stats structure; surface what we can.
    let host = read_string(element, "host");
    let port = read_u32(element, "port");
    info!(
        mcm_inst = "rtp_stats",
        pipeline_id = %pipeline_id_str,
        element = %element.name(),
        factory = "nicesink",
        host = %host,
        port,
        "nicesink 1s sample",
    );
}

fn read_u32(element: &gst::Element, name: &str) -> u32 {
    if element.find_property(name).is_some() {
        element.property::<u32>(name)
    } else {
        0
    }
}

fn read_u64(element: &gst::Element, name: &str) -> u64 {
    if element.find_property(name).is_some() {
        element.property::<u64>(name)
    } else {
        0
    }
}

fn read_string(element: &gst::Element, name: &str) -> String {
    if element.find_property(name).is_none() {
        return String::new();
    }
    element.property::<Option<String>>(name).unwrap_or_default()
}

/// Build a B1 queue with the default sizing from `debug_env::b1_queue_sizing`.
///
/// Used by `udp_sink`, `zenoh_sink`, `webrtc_sink` when
/// `MCM_QUEUE_PER_SINK_BRANCH=1`, and inline in `v4l_pipeline.rs` for the
/// upstream / payloader-adjacent points. Centralised here so the on-vehicle
/// runbook can audit sizing in one place.
///
/// We deliberately keep the queue's hard-limits aligned with the originals
/// removed by `ce11a664` / `67941d70` / `64df455b` - leaky=downstream with
/// generous buffer and time caps. The `silent` and `flush-on-eos` flags
/// match `image_sink`'s surviving queue (which is known good).
pub fn make_b1_queue(name: &str) -> anyhow::Result<gst::Element> {
    let (max_buffers, max_time_ns, leaky) = super::debug_env::b1_queue_sizing();
    let queue = gst::ElementFactory::make("queue")
        .name(name)
        .property_from_str("leaky", leaky)
        .property("max-size-buffers", max_buffers)
        .property("max-size-bytes", 0u32)
        .property("max-size-time", max_time_ns)
        .build()?;
    warn!(
        mcm_inst = "b1_queue_inserted",
        queue = %name,
        max_buffers,
        max_time_ns,
        leaky,
        "B1: inserted leaky=downstream queue (env-flag gated)",
    );
    Ok(queue)
}
