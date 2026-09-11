use anyhow::{Context, Result};
use gst::prelude::*;
use tracing::*;

pub trait SourcePipeline {
    fn build_source(&self, pipeline: &gst::Pipeline) -> Result<gst::Element>;
}

pub struct LibcameraSourcePipeline {
    pub device_path: String,
}

impl SourcePipeline for LibcameraSourcePipeline {
    fn build_source(&self, pipeline: &gst::Pipeline) -> Result<gst::Element> {
        let source = gst::ElementFactory::make("libcamerasrc")
            .name("source")
            .build()
            .context("Failed to create libcamerasrc")?;

        pipeline.add(&source)?;

        source.set_property("camera-name", self.device_path.as_str());
        debug!("Applied libcamerasrc camera-name={:?}", self.device_path);

        Ok(source)
    }
}
