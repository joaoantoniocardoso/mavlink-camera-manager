//! Debug/diagnostic env-var flags consumed at pipeline construction time.
//!
//! These are read once per pipeline build (cheap), not on the hot path.
//! Documented in the on-vehicle WBS (`tools/onvehicle/README.md`).
//!
//! `MCM_QUEUE_*` flags (B1) gate insertion of additional `queue` elements
//! along the `v4l2 -> webrtc` path. `MCM_DISABLE_*` flags (B2) gate sink
//! construction so a branch is omitted entirely. `MCM_QUEUE_SIZING_LARGE`
//! switches the default queue sizing to a larger envelope for the
//! `e4_q_per_sink_xl` experiment. `MCM_UDP_DECOUPLER` selects between
//! competing single-decoupler configurations on the UDP sink path for the
//! decoupler-optimisation experiment.

use std::env;

fn flag(name: &str) -> bool {
    matches!(
        env::var(name).as_deref(),
        Ok("1" | "true" | "TRUE" | "yes" | "YES")
    )
}

/// Insert a `queue` between `v4l2src` and `h264parse` (B1 upstream damping).
pub fn queue_at_v4l2src() -> bool {
    flag("MCM_QUEUE_AT_V4L2SRC")
}

/// Insert a `queue` between the video tee and `rtph264pay` (B1).
pub fn queue_before_payloader() -> bool {
    flag("MCM_QUEUE_BEFORE_PAYLOADER")
}

/// Insert a `queue` between `rtph264pay` and the RTP tee (B1).
pub fn queue_after_payloader() -> bool {
    flag("MCM_QUEUE_AFTER_PAYLOADER")
}

/// Restore the per-branch `queue` between `tee` and each sink's first
/// element (B1). This is the prime Phase D experiment - it reintroduces
/// the queues that commits `ce11a664`/`67941d70`/`64df455b` removed.
pub fn queue_per_sink_branch() -> bool {
    flag("MCM_QUEUE_PER_SINK_BRANCH")
}

/// Skip construction of the zenoh/MCAP sink (B2).
pub fn disable_mcap() -> bool {
    flag("MCM_DISABLE_MCAP")
}

/// Skip construction of the image/thumbnail sink (B2).
pub fn disable_thumbnail() -> bool {
    flag("MCM_DISABLE_THUMBNAIL")
}

/// Skip construction of UDP sinks (B2).
pub fn disable_udp() -> bool {
    flag("MCM_DISABLE_UDP")
}

/// Reject WebRTC session bringup (B2).
pub fn disable_webrtc() -> bool {
    flag("MCM_DISABLE_WEBRTC")
}

/// Switch B1 queue defaults to the larger `e4_q_per_sink_xl` sizing.
pub fn queue_sizing_large() -> bool {
    flag("MCM_QUEUE_SIZING_LARGE")
}

/// Bypass the early "UDP + RTSP can't coexist" check in
/// `Stream::try_new`. The historical limitation pre-dates the appsink
/// bridge added in `6ec8cb70`; the actual failure mode is unclear post-
/// rework, and the UDP-decoupler experiment needs both endpoints on the
/// same stream so latency can be measured pairwise.
pub fn allow_udp_rtsp_concurrent() -> bool {
    flag("MCM_ALLOW_UDP_RTSP_CONCURRENT")
}

/// (max-size-buffers, max-size-time-ns, leaky-mode-string) for B1 queues.
///
/// Default sizing absorbs ~2 s at 30 fps without dropping; large sizing
/// roughly 4x that envelope.
pub fn b1_queue_sizing() -> (u32, u64, &'static str) {
    if queue_sizing_large() {
        (240, 4_000_000_000, "downstream")
    } else {
        (60, 1_000_000_000, "downstream")
    }
}

/// Single-decoupler variant selection for sinks that bridge from the
/// producer's `rtp_tee` to their own pipeline.
///
/// Each variant puts exactly one decoupler between the tee and the sink.
/// `Proxy` is the shipped default after the Phase-1 decoupler experiment
/// (see `results/decoupler_matrix/phase1_*/REPORT.md`): it ties `B1` on
/// CPU (~32 % below `Appsink`), beats both on impair-condition latency
/// medians, and has the cleanest tails outside of idle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UdpDecoupler {
    /// Replace `proxysink`/`proxysrc` with an `appsink`/`appsrc` callback
    /// bridge. The appsink itself (`max-buffers=1, drop=true,
    /// leaky-type=downstream`) is the decoupler.
    Appsink,
    /// Insert a `make_b1_queue` between the tee and `proxysink`, and excise
    /// the proxysrc internal queue after the first buffer.
    B1,
    /// Shipped default: no `b1` queue; preserve the proxysrc internal
    /// queue (already configured to 1-buffer leaky-downstream) as the only
    /// decoupler.
    Proxy,
    /// Original behaviour before `MCM_QUEUE_PER_SINK_BRANCH=1`: no `b1`
    /// queue, proxysrc internal queue excised after first buffer. Zero
    /// decouplers in steady state; the configuration that breaks under
    /// WebRTC backpressure. Kept only as a measurement baseline.
    Legacy,
}

/// Parse `MCM_UDP_DECOUPLER`. Unset defaults to `Proxy`, the Phase-1
/// experiment winner. The legacy `MCM_QUEUE_PER_SINK_BRANCH=1` flag still
/// implies `B1` when the new var is unset, for backwards compatibility
/// with the existing `repro_lab.sh` good/bad harness.
pub fn udp_decoupler() -> UdpDecoupler {
    match env::var("MCM_UDP_DECOUPLER")
        .ok()
        .map(|value| value.to_ascii_lowercase())
        .as_deref()
    {
        Some("appsink") => UdpDecoupler::Appsink,
        Some("b1") => UdpDecoupler::B1,
        Some("proxy") => UdpDecoupler::Proxy,
        Some("legacy") => UdpDecoupler::Legacy,
        Some(other) => {
            tracing::warn!("Unrecognised MCM_UDP_DECOUPLER={other:?}; falling back to proxy",);
            UdpDecoupler::Proxy
        }
        None if queue_per_sink_branch() => UdpDecoupler::B1,
        None => UdpDecoupler::Proxy,
    }
}

/// Decoupler selection for the zenoh sink. Shares the `MCM_UDP_DECOUPLER`
/// env with the UDP sink because the two sinks have the same bridge
/// topology (`proxysink`/`proxysrc` between tee and the sink's
/// sub-pipeline) and the Phase-1 experiment selects the same winner for
/// both. The `Appsink` value is not meaningful here -- the zenoh sub-
/// pipeline already terminates in an `AppSink` for the zenoh handoff --
/// so it folds to `Proxy`.
pub fn zenoh_decoupler() -> UdpDecoupler {
    match udp_decoupler() {
        UdpDecoupler::Appsink => UdpDecoupler::Proxy,
        other => other,
    }
}

/// Decoupler selection for the WebRTC sink. Shares the `MCM_UDP_DECOUPLER`
/// env with the UDP sink for the same reasons as `zenoh_decoupler`: same
/// `proxysink`/`proxysrc` bridge topology, same experiment winner. The
/// `Appsink` value is not meaningful here -- `webrtcbin` does not take an
/// `appsrc` input naturally -- so it folds to `Proxy`.
pub fn webrtc_decoupler() -> UdpDecoupler {
    match udp_decoupler() {
        UdpDecoupler::Appsink => UdpDecoupler::Proxy,
        other => other,
    }
}
