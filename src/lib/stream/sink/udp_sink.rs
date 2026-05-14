use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Context, Result};
use gst::prelude::*;
use tracing::*;

use crate::{
    stream::{
        debug_env::{self, UdpDecoupler},
        gst::utils::excise_proxysrc_queue,
        instrumentation::make_b1_queue,
        pipeline::runner::PipelineRunner,
    },
    video_stream::types::VideoAndStreamInformation,
};

use super::{link_sink_to_tee, unlink_sink_from_tee, SinkInterface};

#[derive(Debug)]
pub struct UdpSink {
    sink_id: Arc<uuid::Uuid>,
    pipeline: gst::Pipeline,
    bridge: Bridge,
    /// `MCM_UDP_DECOUPLER=b1` reinserts the protective queue that
    /// commit `ce11a664` removed between the tee and the proxysink.
    b1_queue: Option<gst::Element>,
    decoupler: UdpDecoupler,
    _udpsink: gst::Element,
    udpsink_sink_pad: gst::Pad,
    tee_src_pad: Option<gst::Pad>,
    addresses: Vec<url::Url>,
    pipeline_runner: PipelineRunner,
}
impl SinkInterface for UdpSink {
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

        let upstream_entry = self.bridge.upstream_entry();
        let elements: Vec<&gst::Element> = if let Some(queue) = self.b1_queue.as_ref() {
            vec![queue, upstream_entry]
        } else {
            vec![upstream_entry]
        };
        link_sink_to_tee(tee_src_pad, pipeline, &elements)?;

        if matches!(self.decoupler, UdpDecoupler::B1 | UdpDecoupler::Legacy) {
            if let Bridge::Proxy {
                proxysrc_queue: Some(queue),
                ..
            } = &self.bridge
            {
                if let Some(src_pad) = queue.static_pad("src") {
                    let queue_weak = queue.downgrade();
                    src_pad.add_probe(
                        gst::PadProbeType::BUFFER | gst::PadProbeType::BUFFER_LIST,
                        move |_pad, _info| {
                            excise_proxysrc_queue(&queue_weak);
                            gst::PadProbeReturn::Remove
                        },
                    );
                }
            }
        }

        Ok(())
    }

    #[instrument(level = "debug", skip(self, pipeline))]
    fn unlink(&self, pipeline: &gst::Pipeline, pipeline_id: &Arc<uuid::Uuid>) -> Result<()> {
        let Some(tee_src_pad) = &self.tee_src_pad else {
            warn!("Tried to unlink Sink from a pipeline without a Tee src pad.");
            return Ok(());
        };

        let upstream_entry = self.bridge.upstream_entry();
        let elements: Vec<&gst::Element> = if let Some(queue) = self.b1_queue.as_ref() {
            vec![queue, upstream_entry]
        } else {
            vec![upstream_entry]
        };
        unlink_sink_from_tee(tee_src_pad, pipeline, &elements)?;

        if let Err(error) = self.pipeline.set_state(::gst::State::Null) {
            warn!("Failed setting sink Pipeline state to Null: {error:?}");
        }

        Ok(())
    }

    #[instrument(level = "debug", skip(self))]
    fn get_id(&self) -> Arc<uuid::Uuid> {
        self.sink_id.clone()
    }

    #[instrument(level = "debug", skip(self))]
    fn get_sdp(&self) -> Result<gst_sdp::SDPMessage> {
        let caps = self
            .udpsink_sink_pad
            .current_caps()
            .context("Failed to get caps from UDP Sink 'sink' pad")?;
        debug!("Got caps: {caps:#?}");

        let mut sdp_media = gst_sdp::SDPMedia::new();
        gst_sdp::SDPMediaRef::set_media_from_caps(&mut sdp_media, &caps)?;

        let url = self.addresses.first().context("Missing address")?.clone();
        sdp_media.add_connection("IN", "IP4", url.host_str().context("Missing host")?, 127, 1);
        sdp_media.set_port_info(url.port().context("Missing port")? as u32, 1);
        sdp_media.set_proto("RTP/AVP");

        let mut sdp = gst_sdp::SDPMessage::new();
        sdp.add_media(sdp_media);
        sdp.set_version("0");
        sdp.set_session_name(&self.sink_id.to_string());
        sdp.set_information("This is a UDP stream");
        sdp.add_attribute(
            "tool",
            Some(&format!(
                "{} - {}",
                env!("CARGO_PKG_NAME"),
                option_env!("VERGEN_GIT_SHA").unwrap_or("?")
            )),
        );
        sdp.add_attribute("type", Some("broadcast"));
        sdp.add_attribute("recvonly", None);

        if let Ok(sdp_str) = sdp.as_text() {
            debug!("Got the SDPMessage: {sdp:#?}\n\n..Which as text is: {sdp_str:?}\n\n");
        };

        Ok(sdp)
    }

    #[instrument(level = "debug", skip(self))]
    fn start(&self) -> Result<()> {
        self.pipeline_runner.start()
    }

    #[instrument(level = "debug", skip(self))]
    fn eos(&self) {
        let pipeline_weak = self.pipeline.downgrade();
        if let Err(error) = std::thread::Builder::new()
            .name("EOS".to_string())
            .spawn(move || {
                let pipeline = pipeline_weak.upgrade().unwrap();
                if let Err(error) = pipeline.post_message(gst::message::Eos::new()) {
                    error!("Failed posting Eos message into Sink bus. Reason: {error:?}");
                }
            })
            .expect("Failed spawning EOS thread")
            .join()
        {
            error!(
                "EOS Thread Panicked with: {:?}",
                error.downcast_ref::<String>()
            );
        }
    }

    fn pipeline(&self) -> Option<&gst::Pipeline> {
        Some(&self.pipeline)
    }
}

impl UdpSink {
    #[instrument(level = "debug", skip_all)]
    pub fn try_new(
        sink_id: Arc<uuid::Uuid>,
        video_and_stream_information: &VideoAndStreamInformation,
    ) -> Result<Self> {
        let decoupler = debug_env::udp_decoupler();
        info!(
            mcm_inst = "udp_decoupler_selected",
            variant = ?decoupler,
            sink_id = %sink_id,
            "UDP sink decoupler variant selected",
        );

        let addresses = video_and_stream_information
            .stream_information
            .endpoints
            .clone();

        let clients = addresses
            .iter()
            .filter_map(|address| {
                if !matches!(address.scheme(), "udp" | "udp265") {
                    return None;
                }
                if let (Some(host), Some(port)) = (address.host(), address.port()) {
                    Some(format!("{host}:{port}"))
                } else {
                    None
                }
            })
            .collect::<Vec<String>>()
            .join(",");
        let description = format!("multiudpsink sync=false clients={clients}");
        let _udpsink =
            gst::parse::launch(&description).context("Failed parsing pipeline description")?;

        let udpsink_sink_pad = _udpsink
            .static_pad("sink")
            .context("Failed to get Sink Pad")?;

        let pipeline = gst::Pipeline::builder()
            .name(format!("pipeline-udp-sink-{sink_id}"))
            .build();

        let bridge = match decoupler {
            UdpDecoupler::Appsink => build_appsink_bridge(&sink_id)?,
            UdpDecoupler::B1 | UdpDecoupler::Proxy | UdpDecoupler::Legacy => build_proxy_bridge()?,
        };

        let sink_side_entry = bridge.sink_side_entry();
        let elements = [sink_side_entry, &_udpsink];
        if let Err(add_err) = pipeline.add_many(elements) {
            return Err(anyhow!(
                "Failed adding UdpSink's elements to Sink Pipeline: {add_err:?}"
            ));
        }

        if let Err(link_err) = gst::Element::link_many(elements) {
            if let Err(remove_err) = pipeline.remove_many(elements) {
                warn!("Failed removing elements from UdpSink Pipeline: {remove_err:?}")
            };
            return Err(anyhow!("Failed linking UdpSink's elements: {link_err:?}"));
        }

        let pipeline_runner =
            PipelineRunner::try_new(&pipeline, &sink_id, true, video_and_stream_information)?;

        let b1_queue = match decoupler {
            UdpDecoupler::B1 => Some(make_b1_queue(&format!("b1_q_udp_{sink_id}"))?),
            UdpDecoupler::Appsink | UdpDecoupler::Proxy | UdpDecoupler::Legacy => None,
        };

        Ok(Self {
            sink_id: sink_id.clone(),
            pipeline,
            bridge,
            b1_queue,
            decoupler,
            _udpsink,
            udpsink_sink_pad,
            addresses,
            tee_src_pad: Default::default(),
            pipeline_runner,
        })
    }
}

/// Bridge between MCM's producer pipeline and the UDP sink's own pipeline.
///
/// Exactly one variant is active per `UdpSink`, chosen by
/// `MCM_UDP_DECOUPLER`. The bridge exposes two pads of interest:
/// - `upstream_entry`: the element to link to the producer's `rtp_tee`.
/// - `sink_side_entry`: the element on the sink pipeline side that feeds
///   `multiudpsink`.
#[derive(Debug)]
enum Bridge {
    Proxy {
        proxysink: gst::Element,
        _proxysrc: gst::Element,
        proxysrc_queue: Option<gst::Element>,
    },
    Appsink {
        appsink: gst_app::AppSink,
        _appsrc: gst_app::AppSrc,
        _last_caps: Arc<Mutex<Option<String>>>,
    },
}

impl Bridge {
    fn upstream_entry(&self) -> &gst::Element {
        match self {
            Bridge::Proxy { proxysink, .. } => proxysink,
            Bridge::Appsink { appsink, .. } => appsink.upcast_ref(),
        }
    }

    fn sink_side_entry(&self) -> &gst::Element {
        match self {
            Bridge::Proxy { _proxysrc, .. } => _proxysrc,
            Bridge::Appsink { _appsrc, .. } => _appsrc.upcast_ref(),
        }
    }
}

fn build_proxy_bridge() -> Result<Bridge> {
    let proxysink = gst::ElementFactory::make("proxysink").build()?;
    let _proxysrc = gst::ElementFactory::make("proxysrc")
        .property("proxysink", &proxysink)
        .build()?;

    let proxysrc_queue = _proxysrc.downcast_ref::<gst::Bin>().and_then(|bin| {
        bin.children()
            .into_iter()
            .find(|element| element.name().starts_with("queue"))
    });
    if let Some(queue) = &proxysrc_queue {
        queue.set_property_from_str("leaky", "downstream");
        queue.set_property("silent", true);
        queue.set_property("flush-on-eos", true);
        queue.set_property("max-size-buffers", 1u32);
        queue.set_property("max-size-bytes", 0u32);
        queue.set_property("max-size-time", 0u64);
    } else {
        warn!("Failed to find queue inside proxysrc");
    }

    Ok(Bridge::Proxy {
        proxysink,
        _proxysrc,
        proxysrc_queue,
    })
}

fn build_appsink_bridge(sink_id: &uuid::Uuid) -> Result<Bridge> {
    let appsrc = gst_app::AppSrc::builder()
        .name(format!("UdpAppSrc-{sink_id}"))
        .is_live(true)
        .format(gst::Format::Time)
        .do_timestamp(false)
        .build();
    appsrc.set_max_bytes(0);
    appsrc.set_property("block", false);

    let appsrc_for_cb = appsrc.clone();
    let last_caps: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let last_caps_for_cb = last_caps.clone();
    let sample_count = Arc::new(std::sync::atomic::AtomicU64::new(0));

    let callbacks = gst_app::AppSinkCallbacks::builder()
        .new_sample(move |sink| {
            let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
            let caps = sample.caps().ok_or(gst::FlowError::Error)?;
            let mut last = last_caps_for_cb.lock().unwrap();
            let caps_str_owned;
            let caps_changed = match last.as_deref() {
                Some(existing) => {
                    caps_str_owned = caps.to_string();
                    existing != caps_str_owned.as_str()
                }
                None => {
                    caps_str_owned = caps.to_string();
                    true
                }
            };
            if caps_changed {
                appsrc_for_cb.set_caps(Some(&caps.to_owned()));
                *last = Some(caps_str_owned);
                debug!("UDP appsink bridge: appsrc caps updated");
            }
            drop(last);

            match appsrc_for_cb.push_sample(&sample) {
                Ok(_) => {
                    let n = sample_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    if n == 0 {
                        debug!("UDP appsink bridge: first sample pushed");
                    }
                }
                Err(error) => {
                    debug!("UDP appsink push_sample failed: {error:?}");
                }
            }
            Ok(gst::FlowSuccess::Ok)
        })
        .build();

    let appsink = gst_app::AppSink::builder()
        .name(format!("UdpAppSink-{sink_id}"))
        .async_(false)
        .sync(false)
        .max_buffers(1u32)
        .drop(true)
        .enable_last_sample(false)
        .qos(false)
        .callbacks(callbacks)
        .build();
    if appsink.find_property("leaky-type").is_some() {
        // (0) is 'none'; (1) is 'upstream'; (2) is 'downstream'.
        appsink.set_property_from_str("leaky-type", "downstream");
    }
    if appsink.find_property("silent").is_some() {
        appsink.set_property("silent", true);
    }

    Ok(Bridge::Appsink {
        appsink,
        _appsrc: appsrc,
        _last_caps: last_caps,
    })
}
