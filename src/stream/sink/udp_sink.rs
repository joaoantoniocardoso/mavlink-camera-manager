use anyhow::{bail, Context, Result};

use tracing::*;

use gst::prelude::*;

use super::sink::SinkInterface;
use crate::stream::pipeline::pipeline::PIPELINE_TEE_NAME;

#[derive(Debug)]
pub struct UdpSink {
    sink_id: uuid::Uuid,
    queue: gst::Element,
    udpsrc: gst::Element,
    udpsrc_sink_pad: gst::Pad,
    tee_src_pad: Option<gst::Pad>,
}
impl SinkInterface for UdpSink {
    #[instrument(level = "debug")]
    fn link(
        &mut self,
        pipeline: &gst::Pipeline,
        pipeline_id: &uuid::Uuid,
        tee_src_pad: gst::Pad,
    ) -> Result<()> {
        let sink_id = &self.get_id();

        // Set Tee's src pad
        if self.tee_src_pad.is_some() {
            bail!("Tee's src pad from UdpSink {sink_id} has already been configured");
        }
        self.tee_src_pad.replace(tee_src_pad);
        let Some(tee_src_pad) = &self.tee_src_pad else {
            unreachable!()
        };

        // Add the Sink elements to the Pipeline
        let elements = &[&self.queue, &self.udpsrc];
        if let Err(error) = pipeline.add_many(elements) {
            bail!(
                "Failed to add WebRTCBin {sink_id} to Pipeline {pipeline_id}. Reason: {error:#?}"
            );
        }

        // Link the queue's src pad to the Sink's sink pad
        let queue_src_pad = &self
            .queue
            .static_pad("src")
            .expect("No sink pad found on Queue");
        if let Err(error) = queue_src_pad.link(&self.udpsrc_sink_pad) {
            pipeline.remove_many(elements)?;
            bail!(error)
        }

        // Link the new Tee's src pad to the queue's sink pad
        let queue_sink_pad = &self
            .queue
            .static_pad("sink")
            .expect("No src pad found on Queue");
        if let Err(error) = tee_src_pad.link(queue_sink_pad) {
            pipeline.remove_many(elements)?;
            queue_src_pad.unlink(&self.udpsrc_sink_pad)?;
            bail!(error)
        }

        Ok(())
    }

    #[instrument(level = "debug")]
    fn unlink(&self, pipeline: &gst::Pipeline, pipeline_id: &uuid::Uuid) -> Result<()> {
        let sink_id = self.get_id();

        let Some(tee_src_pad) = &self.tee_src_pad else {
            warn!("Tried to unlink sink {sink_id} from pipeline {pipeline_id} without a Tee src pad.");
            return Ok(());
        };

        let queue_sink_pad = self
            .queue
            .static_pad("sink")
            .expect("No sink pad found on Queue");
        if let Err(error) = tee_src_pad.unlink(&queue_sink_pad) {
            bail!("Failed unlinking Sink {sink_id} from Tee's source pad. Reason: {error:?}");
        }
        drop(queue_sink_pad);

        let elements = &[&self.queue, &self.udpsrc];
        if let Err(error) = pipeline.remove_many(elements) {
            bail!(
                "Failed removing UdpSrc element {sink_id} from Pipeline {pipeline_id}. Reason: {error:?}"
            );
        }

        if let Err(error) = self.queue.set_state(gst::State::Null) {
            bail!("Failed to set queue from sink {sink_id} state to NULL. Reason: {error:#?}")
        }

        let tee = pipeline
            .by_name(PIPELINE_TEE_NAME)
            .context(format!("no element named {PIPELINE_TEE_NAME:#?}"))?;
        if let Err(error) = tee.remove_pad(tee_src_pad) {
            bail!("Failed removing Tee's source pad. Reason: {error:?}");
        }

        Ok(())
    }

    #[instrument(level = "debug")]
    fn get_id(&self) -> uuid::Uuid {
        self.sink_id.clone()
    }
}

impl UdpSink {
    #[instrument(level = "debug")]
    pub fn try_new(id: uuid::Uuid, addresses: Vec<url::Url>) -> Result<Self> {
        let queue = gst::ElementFactory::make("queue")
            .property_from_str("leaky", "downstream") // Throw away any data
            .property("flush-on-eos", true)
            .property("max-size-buffers", 0u32) // Disable buffers
            .build()?;

        let addresses = addresses
            .iter()
            .filter_map(|address| {
                if address.scheme() != "udp" {
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
        let description = format!("multiudpsink clients={addresses}");
        let udpsrc =
            gst::parse_launch(&description).context("Failed parsing pipeline description")?;

        let udpsrc_sink_pad = udpsrc
            .sink_pads()
            .first()
            .context("Failed to get Sink Pad")?
            .clone(); // Is it safe to clone it?

        Ok(Self {
            sink_id: id,
            queue,
            udpsrc,
            udpsrc_sink_pad,
            tee_src_pad: Default::default(),
        })
    }
}
