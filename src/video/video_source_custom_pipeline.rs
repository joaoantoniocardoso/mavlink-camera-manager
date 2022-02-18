use super::types::*;
use super::video_source::{VideoSource, VideoSourceAvailable};

use paperclip::actix::Apiv2Schema;
use serde::{Deserialize, Serialize};

#[derive(Apiv2Schema, Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum VideoSourceCustomPipelineType {
    Pipeline(String),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct VideoSourceCustomPipeline {
    pub name: String,
    pub source: VideoSourceCustomPipelineType,
}

impl VideoSource for VideoSourceCustomPipeline {
    fn name(&self) -> &String {
        return &self.name;
    }

    fn source_string(&self) -> &str {
        match &self.source {
            VideoSourceCustomPipelineType::Pipeline(string) => &string,
        }
    }

    fn formats(&self) -> Vec<Format> {
        match &self.source {
            VideoSourceCustomPipelineType::Pipeline(_) => {
                vec![]
            }
        }
    }

    fn set_control_by_name(&self, _control_name: &str, _value: i64) -> std::io::Result<()> {
        Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "Custom pipeline sources doesn't have controls.",
        ))
    }

    fn set_control_by_id(&self, _control_id: u64, _value: i64) -> std::io::Result<()> {
        Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "Custom pipeline sources doesn't have controls.",
        ))
    }

    fn control_value_by_name(&self, _control_name: &str) -> std::io::Result<i64> {
        Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "Custom pipeline sources doesn't have controls.",
        ))
    }

    fn control_value_by_id(&self, _control_id: u64) -> std::io::Result<i64> {
        Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "Custom pipeline sources doesn't have controls.",
        ))
    }

    fn controls(&self) -> Vec<Control> {
        vec![]
    }

    fn is_valid(&self) -> bool {
        match &self.source {
            VideoSourceCustomPipelineType::Pipeline(_) => true,
        }
    }
}

impl VideoSourceAvailable for VideoSourceCustomPipeline {
    fn cameras_available() -> Vec<VideoSourceType> {
        vec![VideoSourceType::CustomPipeline(VideoSourceCustomPipeline {
            name: "Pipeline source".into(),
            source: VideoSourceCustomPipelineType::Pipeline("Pipeline".into()),
        })]
    }
}
