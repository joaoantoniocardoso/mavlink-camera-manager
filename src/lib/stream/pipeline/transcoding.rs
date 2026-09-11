use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use gst::prelude::*;
use tracing::warn;

use crate::{
    controls::gst_element_controls::set_property_from_api,
    stream::{
        gst::utils::try_set_property,
        types::{ManualTranscodingConfig, PropertyValue},
    },
    video::types::VideoEncodeType,
};

use super::{PIPELINE_FILTER_NAME, PIPELINE_RTP_TEE_NAME, PIPELINE_VIDEO_TEE_NAME};

pub trait TranscodingPipeline {
    fn launch_description(
        &self,
        device_path: &str,
        pipeline_id: &Arc<uuid::Uuid>,
    ) -> Result<String>;
}

pub struct ManualH264TranscodingPipeline {
    pub source_encode: VideoEncodeType,
    pub width: u32,
    pub height: u32,
    pub manual_config: ManualTranscodingConfig,
}

impl TranscodingPipeline for ManualH264TranscodingPipeline {
    fn launch_description(
        &self,
        device_path: &str,
        pipeline_id: &Arc<uuid::Uuid>,
    ) -> Result<String> {
        let raw_format = raw_caps_format(&self.source_encode)?;
        let encoder = &self.manual_config.encoder;
        let filter_name = format!("{PIPELINE_FILTER_NAME}-{pipeline_id}");
        let video_tee_name = format!("{PIPELINE_VIDEO_TEE_NAME}-{pipeline_id}");
        let rtp_tee_name = format!("{PIPELINE_RTP_TEE_NAME}-{pipeline_id}");
        let threads = std::thread::available_parallelism()
            .map(|count| (count.get() / 2).max(1))
            .unwrap_or(1);

        // parse::launch, not Element::link_many: linking libcamerasrc at NULL
        // deadlocks CameraManager on Pi 5 after format probes in the same process.
        Ok(format!(
            "libcamerasrc name=source camera-name={device_path} \
             ! video/x-raw,format={raw_format},width={width},height={height} \
             ! queue leaky=downstream max-size-buffers=2 max-size-time=0 max-size-bytes=0 \
             ! videoconvert \
             ! {encoder} name=encoder speed-preset=ultrafast tune=zerolatency key-int-max=60 bitrate=4000 bframes=0 threads={threads} \
             ! h264parse config-interval=-1 \
             ! capsfilter name={filter_name} caps=video/x-h264,stream-format=avc,alignment=au,width={width},height={height} \
             ! tee name={video_tee_name} allow-not-linked=true \
             ! rtph264pay aggregate-mode=zero-latency config-interval=-1 pt=96 \
             ! tee name={rtp_tee_name} allow-not-linked=true",
            width = self.width,
            height = self.height,
        ))
    }
}

impl ManualH264TranscodingPipeline {
    pub fn apply_runtime_properties(&self, pipeline: &gst::Pipeline) -> Result<()> {
        let encoder = pipeline
            .by_name("encoder")
            .context("Manual transcoding pipeline is missing the encoder element")?;
        for (property_name, property_value) in &self.manual_config.encoder_properties {
            apply_property_value(&encoder, property_name, property_value);
        }
        Ok(())
    }
}

fn raw_caps_format(source_encode: &VideoEncodeType) -> Result<&'static str> {
    match source_encode {
        VideoEncodeType::Nv12 => Ok("NV12"),
        VideoEncodeType::Yuyv => Ok("YUY2"),
        VideoEncodeType::Rgb => Ok("RGB"),
        unsupported => Err(anyhow!(
            "Raw format {unsupported:?} is not supported for manual transcoding"
        )),
    }
}

fn apply_property_value(element: &gst::Element, name: &str, value: &PropertyValue) {
    match value {
        PropertyValue::Bool(boolean) => try_set_property(element, name, boolean),
        PropertyValue::Integer(integer) => {
            if let Err(error) = set_property_from_api(element, name, *integer) {
                warn!("Failed to set encoder property {name} from integer {integer}: {error}");
            }
        }
        PropertyValue::Number(number) => try_set_property(element, name, number),
        PropertyValue::String(string) => try_set_property(element, name, string.as_str()),
    }
}
