use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc, Mutex,
};

use anyhow::{anyhow, Context, Result};
use gst::prelude::*;
use tracing::*;

use crate::stream::rtsp::rtsp_scheme::RTSPScheme;

use super::{link_sink_to_tee, unlink_sink_from_tee, SinkInterface};

pub type SharedAppSrc = Arc<Mutex<Option<gst_app::AppSrc>>>;
pub type SharedPtsOffset = Arc<Mutex<Option<gst::ClockTime>>>;

/// Shared handle that controls RTSP data flow via a GStreamer `valve`
/// element and participates in stream-level consumer tracking for lazy
/// pipeline support.
///
/// The valve is behind an `Arc<Mutex<>>` so it can be swapped to a new
/// GStreamer element when the pipeline is recreated during lazy-resume,
/// while the RTSP factory's `media_configure` closure (which holds a
/// clone of this handle) keeps working against the *current* valve.
#[derive(Clone, Debug)]
pub struct RtspFlowHandle {
    valve: Arc<Mutex<gst::Element>>,
    client_count: Arc<AtomicUsize>,
    consumer_count: Arc<AtomicUsize>,
    idle: Arc<AtomicBool>,
}

impl RtspFlowHandle {
    fn new(valve: gst::Element, consumer_count: Arc<AtomicUsize>, idle: Arc<AtomicBool>) -> Self {
        Self {
            valve: Arc::new(Mutex::new(valve)),
            client_count: Arc::new(AtomicUsize::new(0)),
            consumer_count,
            idle,
        }
    }

    /// Called when an RTSP client connects (media_configure).
    /// Opens the valve on the first client and tracks the consumer.
    pub fn on_client_connected(&self) {
        let prev = self.client_count.fetch_add(1, Ordering::Relaxed);
        if prev == 0 {
            self.valve.lock().unwrap().set_property("drop", false);
            debug!("RTSP: first client connected, valve opened");
        }
        self.consumer_count.fetch_add(1, Ordering::Relaxed);
        self.idle.store(false, Ordering::Relaxed);
    }

    /// Called when an RTSP client disconnects (media unprepared).
    /// Closes the valve when the last client leaves and updates
    /// stream-level consumer tracking. The watcher handles the idle
    /// timeout transition after a grace period with zero consumers.
    pub fn on_client_disconnected(&self) {
        let prev = self.client_count.fetch_sub(1, Ordering::Relaxed);
        if prev == 1 {
            self.valve.lock().unwrap().set_property("drop", true);
            debug!("RTSP: last client disconnected, valve closed");
        }
        self.consumer_count.fetch_sub(1, Ordering::Relaxed);
    }

    /// Replace the inner valve element (used during lazy pipeline
    /// recreation).  Sets the new valve's `drop` property to match the
    /// current client state.
    pub fn update_valve(&self, new_valve: gst::Element) {
        let has_clients = self.client_count.load(Ordering::Relaxed) > 0;
        new_valve.set_property("drop", !has_clients);
        *self.valve.lock().unwrap() = new_valve;
    }

    /// Returns a clone of the current valve element (cheap ref-counted
    /// handle).
    pub fn valve(&self) -> gst::Element {
        self.valve.lock().unwrap().clone()
    }
}

#[derive(Debug)]
pub struct RtspSink {
    sink_id: Arc<uuid::Uuid>,
    queue: gst::Element,
    appsink: gst_app::AppSink,
    tee_src_pad: Option<gst::Pad>,
    scheme: RTSPScheme,
    path: String,
    rtp_queue_time_ns: u64,
    rtsp_appsrc: SharedAppSrc,
    pts_offset: SharedPtsOffset,
    flow_handle: RtspFlowHandle,
    /// When true, `remove_sink` will NOT call `stop_pipeline` so the RTSP
    /// factory survives across lazy pipeline recreations.
    preserve_factory: AtomicBool,
}
impl SinkInterface for RtspSink {
    #[instrument(level = "debug", skip(self, pipeline))]
    fn link(
        &mut self,
        pipeline: &gst::Pipeline,
        pipeline_id: &Arc<uuid::Uuid>,
        tee_src_pad: gst::Pad,
    ) -> Result<()> {
        if self.tee_src_pad.is_some() {
            return Err(anyhow!(
                "Tee's src pad from Sink {:?} has already been configured",
                self.get_id()
            ));
        }
        self.tee_src_pad.replace(tee_src_pad);
        let Some(tee_src_pad) = &self.tee_src_pad else {
            unreachable!()
        };

        let valve = self.flow_handle.valve();
        let elements = &[&valve, &self.queue, self.appsink.upcast_ref()];
        link_sink_to_tee(tee_src_pad, pipeline, elements)?;

        Ok(())
    }

    #[instrument(level = "debug", skip(self, pipeline))]
    fn unlink(&self, pipeline: &gst::Pipeline, pipeline_id: &Arc<uuid::Uuid>) -> Result<()> {
        let Some(tee_src_pad) = &self.tee_src_pad else {
            warn!("Tried to unlink Sink from a pipeline without a Tee src pad.");
            return Ok(());
        };

        let valve = self.flow_handle.valve();
        let elements = &[&valve, &self.queue, self.appsink.upcast_ref()];
        unlink_sink_from_tee(tee_src_pad, pipeline, elements)?;

        Ok(())
    }

    #[instrument(level = "debug", skip(self))]
    fn get_id(&self) -> Arc<uuid::Uuid> {
        self.sink_id.clone()
    }

    #[instrument(level = "trace", skip(self))]
    fn get_sdp(&self) -> Result<gst_sdp::SDPMessage> {
        Err(anyhow!(
            "Not available. Reason: RTSP Sink should only be connected from its RTSP endpoint."
        ))
    }

    #[instrument(level = "debug", skip(self))]
    fn start(&self) -> Result<()> {
        Ok(())
    }

    #[instrument(level = "debug", skip(self))]
    fn eos(&self) {}

    fn pipeline(&self) -> Option<&gst::Pipeline> {
        None
    }
}

impl RtspSink {
    /// Create a new RTSP sink.
    ///
    /// If `persistent_appsrc` / `persistent_pts_offset` / `persistent_flow_handle`
    /// are supplied, the sink reuses those Arcs (for lazy-resume scenarios
    /// where the RTSP factory already captured them).  Otherwise fresh
    /// instances are created.
    #[instrument(level = "debug", skip_all)]
    pub fn try_new(
        id: Arc<uuid::Uuid>,
        addresses: Vec<url::Url>,
        rtp_queue_time_ns: u64,
        consumer_count: Arc<AtomicUsize>,
        idle: Arc<AtomicBool>,
        persistent_appsrc: Option<SharedAppSrc>,
        persistent_pts_offset: Option<SharedPtsOffset>,
        persistent_flow_handle: Option<RtspFlowHandle>,
    ) -> Result<Self> {
        let valve = gst::ElementFactory::make("valve")
            .name(format!("RtspValve-{id}"))
            .property("drop", true)
            .build()
            .context("Failed to create valve element for RTSP sink")?;

        let queue = gst::ElementFactory::make("queue")
            .property_from_str("leaky", "downstream")
            .property("silent", true)
            .property("flush-on-eos", true)
            .property("max-size-buffers", 0u32)
            .property("max-size-bytes", 0u32)
            .property("max-size-time", rtp_queue_time_ns)
            .build()?;

        let (path, scheme) = addresses
            .iter()
            .find_map(|address| {
                address
                    .scheme()
                    .starts_with("rtsp")
                    .then_some(RTSPScheme::try_from(address.scheme()).unwrap_or_default())
                    .map(|scheme| (address.path().to_string(), scheme))
            })
            .context(
                "Failed to find RTSP compatible address. Example: \"rtsp://0.0.0.0:8554/test\"",
            )?;

        let rtsp_appsrc: SharedAppSrc =
            persistent_appsrc.unwrap_or_else(|| Arc::new(Mutex::new(None)));
        let pts_offset: SharedPtsOffset =
            persistent_pts_offset.unwrap_or_else(|| Arc::new(Mutex::new(None)));

        let rtsp_appsrc_ref = rtsp_appsrc.clone();
        let pts_offset_ref = pts_offset.clone();
        let appsink = gst_app::AppSink::builder()
            .name(format!("RtspAppSink-{id}"))
            .async_(false)
            .sync(false)
            .max_buffers(1u32)
            .drop(true)
            .enable_last_sample(false)
            .build();
        appsink.set_property_from_str("leaky-type", "downstream");
        appsink.set_property("silent", true);

        let sample_count = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        appsink.set_callbacks(
            gst_app::AppSinkCallbacks::builder()
                .new_sample(move |sink| {
                    let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                    if let Some(ref appsrc) = *rtsp_appsrc_ref.lock().unwrap() {
                        let buffer = sample.buffer().ok_or(gst::FlowError::Error)?;

                        let mut buf = buffer.copy();
                        {
                            let buf_ref = buf.get_mut().unwrap();
                            if let Some(pts) = buffer.pts() {
                                let mut offset = pts_offset_ref.lock().unwrap();
                                let base = *offset.get_or_insert(pts);
                                buf_ref.set_pts(pts.checked_sub(base));
                                buf_ref.set_dts(buffer.dts().and_then(|d| d.checked_sub(base)));
                            }
                        }

                        let caps: gst::Caps = sample.caps().unwrap().to_owned();
                        let rebased = gst::Sample::builder().buffer(&buf).caps(&caps).build();

                        match appsrc.push_sample(&rebased) {
                            Ok(_) => {
                                let n =
                                    sample_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                if n == 0 {
                                    debug!(
                                        "RTSP: first sample pushed to appsrc (pts={:?})",
                                        buf.pts()
                                    );
                                } else if n % 300 == 0 {
                                    trace!("RTSP: pushed {n} samples to appsrc");
                                }
                            }
                            Err(err) => {
                                *pts_offset_ref.lock().unwrap() = None;
                                debug!("RTSP appsrc push_sample failed: {err:?}");
                            }
                        }
                    }
                    Ok(gst::FlowSuccess::Ok)
                })
                .build(),
        );

        let flow_handle = if let Some(persistent) = persistent_flow_handle {
            persistent.update_valve(valve);
            persistent
        } else {
            RtspFlowHandle::new(valve, consumer_count, idle)
        };

        Ok(Self {
            sink_id: id.clone(),
            queue,
            appsink,
            scheme,
            path,
            tee_src_pad: Default::default(),
            rtp_queue_time_ns,
            rtsp_appsrc,
            pts_offset,
            flow_handle,
            preserve_factory: AtomicBool::new(false),
        })
    }

    #[instrument(level = "trace", skip(self))]
    pub fn path(&self) -> String {
        self.path.clone()
    }

    #[instrument(level = "trace", skip(self))]
    pub fn scheme(&self) -> RTSPScheme {
        self.scheme.clone()
    }

    pub fn rtsp_appsrc(&self) -> SharedAppSrc {
        self.rtsp_appsrc.clone()
    }

    pub fn pts_offset(&self) -> SharedPtsOffset {
        self.pts_offset.clone()
    }

    pub fn rtp_queue_time_ns(&self) -> u64 {
        self.rtp_queue_time_ns
    }

    pub fn flow_handle(&self) -> RtspFlowHandle {
        self.flow_handle.clone()
    }

    pub fn set_preserve_factory(&self, val: bool) {
        self.preserve_factory.store(val, Ordering::Relaxed);
    }

    pub fn should_preserve_factory(&self) -> bool {
        self.preserve_factory.load(Ordering::Relaxed)
    }
}
