use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use anyhow::{anyhow, Context, Error, Result};
use gst::prelude::*;
use gst_video::VideoFrameExt;
use image::FlatSamples;
use tracing::*;
use tokio::sync::mpsc;

use crate::{stream::pipeline::runner::PipelineRunner, video::types::VideoEncodeType};

use super::{link_sink_to_tee, unlink_sink_from_tee, SinkInterface};

type ClonableResult<T> = Result<T, Arc<Error>>;

#[derive(Debug)]
pub struct ZenohSink {
    sink_id: Arc<uuid::Uuid>,
    pipeline: gst::Pipeline,
    queue: gst::Element,
    proxysink: gst::Element,
    _proxysrc: gst::Element,
    _transcoding_elements: Vec<gst::Element>,
    appsink: gst_app::AppSink,
    tee_src_pad: Option<gst::Pad>,
    zenoh_session: Arc<zenoh::Session>,
    pipeline_runner: PipelineRunner,
}

impl SinkInterface for ZenohSink {
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

        let elements = &[&self.queue, &self.proxysink];
        link_sink_to_tee(tee_src_pad, pipeline, elements)?;

        Ok(())
    }

    #[instrument(level = "debug", skip(self, pipeline))]
    fn unlink(&self, pipeline: &gst::Pipeline, pipeline_id: &Arc<uuid::Uuid>) -> Result<()> {
        let Some(tee_src_pad) = &self.tee_src_pad else {
            warn!("Tried to unlink Sink from a pipeline without a Tee src pad.");
            return Ok(());
        };

        let elements = &[&self.queue, &self.proxysink];
        unlink_sink_from_tee(tee_src_pad, pipeline, elements)?;

        if let Err(error) = self.pipeline.set_state(::gst::State::Null) {
            warn!("Failed setting sink Pipeline state to Null: {error:?}");
        }

        Ok(())
    }

    #[instrument(level = "debug", skip(self))]
    fn get_id(&self) -> Arc<uuid::Uuid> {
        self.sink_id.clone()
    }

    #[instrument(level = "trace", skip(self))]
    fn get_sdp(&self) -> Result<gst_sdp::SDPMessage> {
        Err(anyhow!(
            "Not available. Reason: Zenoh Sink doesn't provide endpoints"
        ))
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
}

impl ZenohSink {
    #[instrument(level = "debug")]
    pub async fn try_new(sink_id: Arc<uuid::Uuid>, encoding: VideoEncodeType) -> Result<Self> {
        let queue = gst::ElementFactory::make("queue")
            .property_from_str("leaky", "downstream") // Throw away any data
            .property("silent", true)
            .property("flush-on-eos", true)
            .property("max-size-buffers", 0u32) // Disable buffers
            .build()?;

        // Create a pair of proxies. The proxysink will be used in the source's pipeline,
        // while the proxysrc will be used in this sink's pipeline
        let proxysink = gst::ElementFactory::make("proxysink").build()?;
        let _proxysrc = gst::ElementFactory::make("proxysrc")
            .property("proxysink", &proxysink)
            .build()?;

        // Configure proxysrc's queue, skips if fails
        match _proxysrc.downcast_ref::<gst::Bin>() {
            Some(bin) => {
                let elements = bin.children();
                match elements
                    .iter()
                    .find(|element| element.name().starts_with("queue"))
                {
                    Some(element) => {
                        element.set_property_from_str("leaky", "downstream"); // Throw away any data
                        element.set_property("silent", true);
                        element.set_property("flush-on-eos", true);
                        element.set_property("max-size-buffers", 0u32); // Disable buffers
                    }
                    None => {
                        warn!("Failed to customize proxysrc's queue: Failed to find queue in proxysrc");
                    }
                }
            }
            None => {
                warn!("Failed to customize proxysrc's queue: Failed to downcast element to bin")
            }
        }

        // Create Zenoh session
        let zenoh_session = zenoh::open(zenoh::Config::default())
            .await
            .map_err(|e| anyhow!("Failed to open Zenoh session: {}", e))?;
        let zenoh_session = Arc::new(zenoh_session);

        // Create channel for sending video data
        let (tx, mut rx) = mpsc::channel(100);

        // Spawn a task to handle the video data publishing
        let zenoh_session_clone = zenoh_session.clone();
        let _task = tokio::spawn(async move {
            println!("Spawning ZenohSink task");
            while let Some(data) = rx.recv().await {
                println!("Publishing data");
                if let Err(e) = zenoh_session_clone.put("video/h264", data).await {
                    error!("Error publishing data: {}", e);
                }
            }
        });

        // Depending of the sources' format we need different elements to transform it into a raw format
        let mut _transcoding_elements: Vec<gst::Element> = Default::default();
        match encoding {
            VideoEncodeType::H264 => {
                // For h264, we need to filter-out unwanted non-key frames here, before decoding it.
                let filter = gst::ElementFactory::make("identity")
                    .property("drop-buffer-flags", gst::BufferFlags::DELTA_UNIT)
                    .property("sync", false)
                    .build()?;
                let decoder = gst::ElementFactory::make("avdec_h264")
                    .property_from_str("lowres", "2") // (0) is 'full'; (1) is '1/2-size'; (2) is '1/4-size'
                    .build()?;
                decoder.has_property("discard-corrupted-frames", None).then(|| decoder.set_property("discard-corrupted-frames", true));
                _transcoding_elements.push(filter);
                _transcoding_elements.push(decoder);
            }
            VideoEncodeType::H265 => {
                // For h265, we need to filter-out unwanted non-key frames here, before decoding it.
                let filter = gst::ElementFactory::make("identity")
                .property("drop-buffer-flags", gst::BufferFlags::DELTA_UNIT)
                .property("sync", false)
                .build()?;
                let decoder = gst::ElementFactory::make("avdec_h265")
                    .property_from_str("lowres", "2") // (0) is 'full'; (1) is '1/2-size'; (2) is '1/4-size'
                    .build()?;
                decoder.has_property("discard-corrupted-frames", None).then(|| decoder.set_property("discard-corrupted-frames", true));
                decoder.has_property("std-compliance", None).then(|| decoder.set_property_from_str("std-compliance", "normal"));
                _transcoding_elements.push(filter);
                _transcoding_elements.push(decoder);
            }
            VideoEncodeType::Mjpg => {
                let decoder = gst::ElementFactory::make("jpegdec").build()?;
                decoder.has_property("discard-corrupted-frames", None).then(|| decoder.set_property("discard-corrupted-frames", true));
                _transcoding_elements.push(decoder);
            }
            VideoEncodeType::Rgb => {}
            VideoEncodeType::Yuyv => {}
            _ => return Err(anyhow!("Unsupported video encoding for ZenohSink: {encoding:?}. The supported are: H264, MJPG and YUYV")),
        };

        let videoconvert = gst::ElementFactory::make("videoconvert").build()?;
        _transcoding_elements.push(videoconvert);

        // We want H264 format for Zenoh publishing
        let caps = gst::Caps::builder("video/x-h264")
            .field("stream-format", "byte-stream")
            .field("alignment", "au")
            .build();

        // Create the appsink callbacks
        let tx_clone = tx.clone();
        let appsink_callbacks = gst_app::AppSinkCallbacks::builder()
            .new_sample(move |appsink| {
                let sample = match appsink.pull_sample() {
                    Ok(sample) => sample,
                    Err(e) => {
                        error!("Error pulling sample: {}", e);
                        return Ok(gst::FlowSuccess::Ok);
                    }
                };

                let buffer = match sample.buffer() {
                    Some(buffer) => buffer,
                    None => {
                        error!("No buffer in sample");
                        return Ok(gst::FlowSuccess::Ok);
                    }
                };

                let map = match buffer.map_readable() {
                    Ok(map) => map,
                    Err(e) => {
                        error!("Error mapping buffer: {}", e);
                        return Ok(gst::FlowSuccess::Ok);
                    }
                };

                let data = map.as_slice().to_vec();
                debug!("Publishing H.264 frame with size: {}", data.len());

                // Only send if we have valid data
                if !data.is_empty() && data.len() < 1024 * 1024 { // 1MB limit
                    if let Err(e) = tx_clone.blocking_send(data) {
                        error!("Error sending data through channel: {}", e);
                    }
                } else {
                    warn!("Skipping invalid frame: size={}", data.len());
                }

                Ok(gst::FlowSuccess::Ok)
            })
            .build();

        let appsink = gst_app::AppSink::builder()
            .name(format!("AppSink-{sink_id}"))
            .sync(false)
            .max_buffers(1u32)
            .drop(true)
            .caps(&caps)
            .callbacks(appsink_callbacks)
            .build();

        // Create the pipeline
        let pipeline = gst::Pipeline::builder()
            .name(format!("pipeline-sink-{sink_id}"))
            .build();

        // Add Sink elements to the Sink's Pipeline
        let mut elements = vec![&_proxysrc];
        elements.extend(_transcoding_elements.iter().collect::<Vec<&gst::Element>>());
        elements.push(appsink.upcast_ref());
        let elements = &elements;
        if let Err(add_err) = pipeline.add_many(elements) {
            return Err(anyhow!(
                "Failed adding ZenohSink's elements to Sink Pipeline: {add_err:?}"
            ));
        }

        // Link Sink's elements
        if let Err(link_err) = gst::Element::link_many(elements) {
            if let Err(remove_err) = pipeline.remove_many(elements) {
                warn!("Failed removing elements from ZenohSink Pipeline: {remove_err:?}")
            };
            return Err(anyhow!("Failed linking ZenohSink's elements: {link_err:?}"));
        }

        let pipeline_runner = PipelineRunner::try_new(&pipeline, &sink_id, false)?;

        // Start the pipeline in Playing state
        if let Err(state_err) = pipeline.set_state(gst::State::Playing) {
            return Err(anyhow!(
                "Failed starting ZenohSink's pipeline: {state_err:#?}"
            ));
        }

        Ok(Self {
            sink_id: sink_id.clone(),
            pipeline,
            queue,
            proxysink,
            _proxysrc,
            _transcoding_elements,
            appsink,
            tee_src_pad: Default::default(),
            zenoh_session,
            pipeline_runner,
        })
    }
}
