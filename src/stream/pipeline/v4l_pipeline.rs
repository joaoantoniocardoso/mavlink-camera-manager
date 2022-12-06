use crate::{
    stream::types::CaptureConfiguration,
    video::types::{VideoEncodeType, VideoSourceType},
    video_stream::types::VideoAndStreamInformation,
};

use super::pipeline::{
    PipelineGstreamerInterface, PipelineState, PIPELINE_FILTER_NAME, PIPELINE_TEE_NAME,
};

use anyhow::{anyhow, Result};

use tracing::*;

use gst::prelude::*;

#[derive(Debug)]
pub struct V4lPipeline {
    pub state: PipelineState,
}

impl V4lPipeline {
    #[instrument(level = "debug")]
    pub fn try_new(
        video_and_stream_information: &VideoAndStreamInformation,
    ) -> Result<gst::Pipeline> {
        let configuration = match &video_and_stream_information
            .stream_information
            .configuration
        {
            CaptureConfiguration::VIDEO(configuration) => configuration,
            unsupported => return Err(anyhow!("{unsupported:?} is not supported as V4l Pipeline")),
        };

        let video_source = match &video_and_stream_information.video_source {
            VideoSourceType::Local(source) => source,
            unsupported => {
                return Err(anyhow!(
                    "SourceType {unsupported:?} is not supported as V4l Pipeline"
                ))
            }
        };

        let device = video_source.device_path.as_str();
        let width = configuration.width;
        let height = configuration.height;
        let interval_denominator = configuration.frame_interval.denominator;
        let interval_numerator = configuration.frame_interval.numerator;

        debug!("Testing device {device:#?} for compatible H264 profiles...");
        // Here we are pro-actively setting the profile because if we don't set it in the capsfilter
        // before it is set to playing, when the second Sink connects to it, the pipeline will try to
        // renegotiate this capability, freezing the already-playing sinks for a while.
        let Some(profile) = [
            "constrained-baseline",
            "baseline",
            "main",
            "high",
        ]
        .iter()
        .find(|profile| discover_v4l2_h264_profiles(device, profile, width, height, interval_denominator, interval_numerator).is_ok()) else {
            return Err(anyhow!("Device {device:#?} doesn't support any known H264 profile"));
        };
        debug!("Found a compatible H264 profiles for device {device:#?}. Profile: {profile:#?}");

        let description = match &configuration.encode {
            VideoEncodeType::H264 => {
                format!(concat!(
                    "v4l2src device={device} do-timestamp=false",
                    " ! h264parse",
                    " ! capsfilter name={filter_name} caps=video/x-h264,stream-format=avc,alignment=au,profile={profile},width={width},height={height},framerate={interval_denominator}/{interval_numerator}",
                    " ! rtph264pay aggregate-mode=zero-latency config-interval=10 pt=96 timestamp-offset=0",
                    " ! tee name={tee_name} allow-not-linked=true"
                ),
                device = device,
                profile = profile,
                width = width,
                height = height,
                interval_denominator = interval_denominator,
                interval_numerator = interval_numerator,
                filter_name = PIPELINE_FILTER_NAME,
                tee_name = PIPELINE_TEE_NAME
            )
            }
            unsupported => {
                return Err(anyhow!(
                    "Encode {unsupported:?} is not supported for V4l Pipeline"
                ))
            }
        };

        debug!("pipeline_description: {description:#?}");

        let pipeline = gst::parse_launch(&description)?;
        let pipeline = pipeline
            .downcast::<gst::Pipeline>()
            .expect("Couldn't downcast pipeline");

        return Ok(pipeline);
    }
}

impl PipelineGstreamerInterface for V4lPipeline {
    fn is_running(&self) -> bool {
        self.state.pipeline_runner.is_running()
    }
}

#[instrument(level = "debug")]
fn discover_v4l2_h264_profiles(
    device: &str,
    profile: &str,
    width: u32,
    height: u32,
    interval_denominator: u32,
    interval_numerator: u32,
) -> Result<String> {
    let description = format!(
        concat!(
            "v4l2src device={device} do-timestamp=false num-buffers=1",
            " ! h264parse",
            " ! video/x-h264,stream-format=avc,alignment=au,profile={profile},width={width},height={height},framerate={interval_denominator}/{interval_numerator}",
            " ! fakesink",
        ),
        device = device,
        profile = profile,
        width = width,
        height = height,
        interval_denominator = interval_denominator,
        interval_numerator = interval_numerator,
    );

    let pipeline = gst::parse_launch(&description)?;
    let pipeline = pipeline
        .downcast::<gst::Pipeline>()
        .expect("Couldn't downcast pipeline");

    let bus = pipeline.bus().unwrap();
    let pipeline_weak = pipeline.downgrade();
    let runner = move || {
        let Some(pipeline) = pipeline_weak.upgrade() else {
                return Err(anyhow!("Couldn't upgrade pipeline from WeakRef<Pipeline>"))
            };

        pipeline
            .set_state(gst::State::Playing)
            .expect("Unable to set the pipeline to the `Playing` state");
        pipeline.debug_to_dot_file_with_ts(gst::DebugGraphDetails::all(), format!("playing"));
        drop(pipeline);

        loop {
            while let Some(msg) = bus.timed_pop_filtered(
                gst::ClockTime::from_mseconds(100),
                &[gst::MessageType::Eos, gst::MessageType::Error],
            ) {
                match msg.view() {
                    gst::MessageView::Eos(..) => return Ok(profile.to_string()),
                    gst::MessageView::Error(error) => return Err(anyhow!("{error:?}")),
                    _ => (),
                };
            }
        }
    };
    let result = runner();

    if let Err(error) = &result {
        println!("Failed with profile {profile:?}. Reason: {error:?}");
    }

    pipeline
        .set_state(gst::State::Null)
        .expect("Unable to set the pipeline to the `Playing` state");

    result
}
