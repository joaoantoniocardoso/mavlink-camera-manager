use super::{
    gst::pipeline_runner::{Pipeline, PipelineRunner},
    stream_backend::StreamBackend,
};

#[derive(Debug)]
#[allow(dead_code)]
pub struct VideoStreamCustomPipeline {
    pipeline_runner: PipelineRunner,
}

impl Default for VideoStreamCustomPipeline {
    fn default() -> Self {
        Self {
            pipeline_runner: PipelineRunner::new(Pipeline {
                description: "".into(),
            }),
        }
    }
}

impl StreamBackend for VideoStreamCustomPipeline {
    fn start(&mut self) -> bool {
        self.pipeline_runner.start()
    }

    fn stop(&mut self) -> bool {
        self.pipeline_runner.stop()
    }

    fn restart(&mut self) {
        self.pipeline_runner.restart()
    }

    fn is_running(&self) -> bool {
        self.pipeline_runner.is_running()
    }

    fn set_pipeline_description(&mut self, description: &str) {
        self.pipeline_runner.set_pipeline_description(description);
    }

    fn pipeline(&self) -> String {
        self.pipeline_runner.pipeline()
    }
}
