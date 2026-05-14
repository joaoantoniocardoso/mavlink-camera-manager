use std::sync::Arc;

use anyhow::{anyhow, Result};
use gst::prelude::*;
use tracing::*;

use crate::{
    stream::types::CaptureConfiguration,
    video::types::{VideoEncodeType, VideoSourceType},
    video_stream::types::VideoAndStreamInformation,
};

use super::{
    PipelineGstreamerInterface, PipelineState, PIPELINE_FILTER_NAME, PIPELINE_RTP_TEE_NAME,
    PIPELINE_VIDEO_TEE_NAME,
};

#[derive(Debug)]
pub struct V4lPipeline {
    pub state: PipelineState,
}

impl V4lPipeline {
    #[instrument(level = "debug", skip_all)]
    pub fn try_new(
        pipeline_id: &Arc<uuid::Uuid>,
        video_and_stream_information: &VideoAndStreamInformation,
    ) -> Result<gst::Pipeline> {
        let configuration = match &video_and_stream_information
            .stream_information
            .configuration
        {
            CaptureConfiguration::Video(configuration) => configuration,
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
        let interval_numerator = configuration.frame_interval.numerator;
        let interval_denominator = configuration.frame_interval.denominator;
        let filter_name = format!("{PIPELINE_FILTER_NAME}-{pipeline_id}");
        let video_tee_name = format!("{PIPELINE_VIDEO_TEE_NAME}-{pipeline_id}");
        let rtp_tee_name = format!("{PIPELINE_RTP_TEE_NAME}-{pipeline_id}");

        // B1 env-flag-gated queue insertion points (see `debug_env`).
        // Each fragment expands to "queue ... ! " when enabled, "" otherwise.
        let q_at_v4l2src = b1_queue_fragment(
            crate::stream::debug_env::queue_at_v4l2src(),
            "b1_q_at_v4l2src",
        );
        let q_before_pay = b1_queue_fragment(
            crate::stream::debug_env::queue_before_payloader(),
            "b1_q_before_pay",
        );
        let q_after_pay = b1_queue_fragment(
            crate::stream::debug_env::queue_after_payloader(),
            "b1_q_after_pay",
        );

        let description = match &configuration.encode {
            VideoEncodeType::H264 => {
                format!(
                    "v4l2src device={device} do-timestamp=true \
                     ! {q_at_v4l2src}h264parse config-interval=-1 \
                     ! capsfilter name={filter_name} caps=video/x-h264,stream-format=avc,alignment=au,width={width},height={height},framerate={interval_denominator}/{interval_numerator} \
                     ! tee name={video_tee_name} allow-not-linked=true \
                     ! {q_before_pay}rtph264pay aggregate-mode=zero-latency config-interval=-1 pt=96 \
                     ! {q_after_pay}tee name={rtp_tee_name} allow-not-linked=true",
                )
            }
            VideoEncodeType::H265 => {
                format!(
                    "v4l2src device={device} do-timestamp=true \
                     ! {q_at_v4l2src}h265parse \
                     ! capsfilter name={filter_name} caps=video/x-h265,stream-format=byte-stream,alignment=au,width={width},height={height},framerate={interval_denominator}/{interval_numerator} \
                     ! tee name={video_tee_name} allow-not-linked=true \
                     ! {q_before_pay}rtph265pay aggregate-mode=zero-latency config-interval=-1 pt=96 \
                     ! {q_after_pay}tee name={rtp_tee_name} allow-not-linked=true",
                )
            }
            VideoEncodeType::Yuyv => {
                format!(
                    "v4l2src device={device} do-timestamp=true \
                     ! {q_at_v4l2src}videoconvert \
                     ! capsfilter name={filter_name} caps=video/x-raw,format=I420,width={width},height={height},framerate={interval_denominator}/{interval_numerator} \
                     ! tee name={video_tee_name} allow-not-linked=true \
                     ! {q_before_pay}rtpvrawpay pt=96 \
                     ! {q_after_pay}tee name={rtp_tee_name} allow-not-linked=true",
                )
            }
            VideoEncodeType::Mjpg => {
                format!(
                    "v4l2src device={device} do-timestamp=true \
                     ! {q_at_v4l2src}capsfilter name={filter_name} caps=image/jpeg,width={width},height={height},framerate={interval_denominator}/{interval_numerator} \
                     ! tee name={video_tee_name} allow-not-linked=true \
                     ! {q_before_pay}rtpjpegpay pt=96 \
                     ! {q_after_pay}tee name={rtp_tee_name} allow-not-linked=true",
                )
            }
            unsupported => {
                return Err(anyhow!(
                    "Encode {unsupported:?} is not supported for V4L2 Pipeline"
                ))
            }
        };

        debug!("pipeline_description: {description:#?}");

        let pipeline = gst::parse::launch(&description)?;

        let pipeline = pipeline
            .downcast::<gst::Pipeline>()
            .expect("Couldn't downcast pipeline");

        pipeline.set_property("name", format!("pipeline-v4l2-{pipeline_id}"));

        Ok(pipeline)
    }
}

impl PipelineGstreamerInterface for V4lPipeline {
    #[instrument(level = "trace")]
    fn is_running(&self) -> bool {
        self.state.pipeline_runner.is_running()
    }
}

/// Returns "queue ... ! " for inlining in a gst-launch description when
/// `enabled` is true, otherwise the empty string. Sizing is taken from
/// `debug_env::b1_queue_sizing`.
fn b1_queue_fragment(enabled: bool, name: &str) -> String {
    if !enabled {
        return String::new();
    }
    let (max_buffers, max_time_ns, leaky) = crate::stream::debug_env::b1_queue_sizing();
    warn!(
        mcm_inst = "b1_queue_inserted",
        queue = %name,
        max_buffers,
        max_time_ns,
        leaky,
        "B1: inserting inline queue in v4l pipeline (env-flag gated)",
    );
    format!(
        "queue name={name} leaky={leaky} silent=true flush-on-eos=true \
         max-size-buffers={max_buffers} max-size-bytes=0 max-size-time={max_time_ns} ! ",
    )
}
