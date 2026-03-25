pub mod gst;
pub mod lifecycle;
pub mod manager;
pub mod pipeline;
pub mod rtsp;
pub mod sink;
pub mod types;
pub mod webrtc;

use std::sync::{Arc, Mutex};

use ::gst::prelude::*;
use anyhow::{anyhow, Context, Result};
use gst::utils::get_capture_configuration_from_stream_uri;
use manager::Manager;
use pipeline::{Pipeline, PipelineGstreamerInterface};
use sink::{create_image_sink, create_rtsp_sink, create_udp_sink, create_zenoh_sink};
use tokio::sync::RwLock;
use tracing::*;
use types::*;
use webrtc::signalling_protocol::PeerId;

use crate::{
    mavlink::mavlink_camera::MavlinkCamera,
    video::{
        types::{FrameInterval, VideoEncodeType, VideoSourceType},
        video_source::cameras_available,
    },
    video_stream::types::VideoAndStreamInformation,
};

use self::lifecycle::{LifecycleState, Phase};

use self::{
    gst::utils::wait_for_element_state,
    rtsp::{rtsp_scheme::RTSPScheme, rtsp_server::RTSPServer},
    sink::SinkInterface,
};

pub struct Stream {
    pub state: Arc<RwLock<Option<StreamState>>>,
    pipeline_id: Arc<PeerId>,
    video_and_stream_information: Arc<RwLock<VideoAndStreamInformation>>,
    error: Arc<RwLock<anyhow::Result<()>>>,
    terminated: Arc<RwLock<bool>>,
    watcher_handle: Option<tokio::task::JoinHandle<()>>,
    pub lifecycle: Arc<LifecycleState>,
    pub notify: Arc<tokio::sync::Notify>,
    /// Tracks the timestamp of the last thumbnail request so the pipeline
    /// stays alive for a cooldown period after the thumbnail is served.
    pub thumbnail_cooldown: Arc<Mutex<Option<std::time::Instant>>>,
    /// Persists across idle/wake cycles so heartbeats continue when the
    /// pipeline is not running.
    pub mavlink_camera: Arc<RwLock<Option<MavlinkCamera>>>,
}

#[derive(Debug)]
pub struct StreamState {
    pub pipeline_id: Arc<PeerId>,
    pub pipeline: Option<Pipeline>,
    pub video_and_stream_information: Arc<RwLock<VideoAndStreamInformation>>,
}

impl Stream {
    #[instrument(level = "debug", skip_all)]
    pub async fn try_new(video_and_stream_information: &VideoAndStreamInformation) -> Result<Self> {
        let video_source_inner = video_and_stream_information.video_source.inner();

        // To be DHCP-friendly, we ignore the address for IP-based sources
        let source_string = match video_source_inner.source_string().parse::<url::Url>() {
            Ok(mut url) => {
                let _ = url.set_host(None);
                let _ = url.set_port(None);
                url.to_string()
            }
            Err(_) => video_source_inner.source_string().to_string(),
        };

        let pipeline_id = Arc::new(Manager::generate_uuid(Some(&format!(
            "{}:{}",
            video_source_inner.name(),
            source_string,
        ))));

        // Replace Redirect with Video
        let video_and_stream_information = {
            let mut video_and_stream_information = video_and_stream_information.clone();

            if matches!(
                video_and_stream_information.video_source,
                VideoSourceType::Redirect(_)
            ) {
                video_and_stream_information
                    .stream_information
                    .configuration = CaptureConfiguration::Video(VideoCaptureConfiguration {
                    encode: VideoEncodeType::Unknown("Redirect stream".to_string()),
                    height: 0,
                    width: 0,
                    frame_interval: FrameInterval {
                        numerator: 0,
                        denominator: 0,
                    },
                });
            }

            Arc::new(RwLock::new(video_and_stream_information))
        };

        let lifecycle = Arc::new(LifecycleState::new());
        let notify = Arc::new(tokio::sync::Notify::new());

        let state = Arc::new(RwLock::new(Some(
            StreamState::try_default(video_and_stream_information.clone(), pipeline_id.clone())
                .await?,
        )));

        let terminated = Arc::new(RwLock::new(false));
        let error = Arc::new(RwLock::new(Ok(())));
        let mavlink_camera = Arc::new(RwLock::new(None));

        debug!("Starting StreamWatcher task...");

        let watcher_handle = Some(tokio::spawn({
            let terminated = terminated.clone();
            let error = error.clone();
            let video_and_stream_information = video_and_stream_information.clone();
            let state = state.clone();
            let pipeline_id = pipeline_id.clone();
            let lifecycle = lifecycle.clone();
            let notify = notify.clone();
            let mavlink_camera = mavlink_camera.clone();

            async move {
                debug!("StreamWatcher task started!");
                match Self::watcher(
                    video_and_stream_information,
                    pipeline_id,
                    error,
                    state,
                    terminated,
                    lifecycle,
                    notify,
                    mavlink_camera,
                )
                .await
                {
                    Ok(_) => debug!("StreamWatcher task eneded with no errors"),
                    Err(error) => warn!("StreamWatcher task ended with error: {error:#?}"),
                };
            }
        }));

        // Start pipeline once at creation to initialize infrastructure (RTSP server, etc.).
        // For lazy streams the initial consumer is immediately removed so the pipeline
        // drains to Idle if no real consumers connect within the grace period.
        // For non-lazy streams (disable_lazy=true) we keep the phantom consumer so
        // the pipeline transitions to Running(1) and stays alive indefinitely.
        let disable_lazy = video_and_stream_information
            .read()
            .await
            .stream_information
            .extended_configuration
            .as_ref()
            .map(|ext| ext.disable_lazy)
            .unwrap_or(false);
        lifecycle.add_consumer(&*notify);
        if !disable_lazy {
            lifecycle.remove_consumer(true);
        }

        Ok(Self {
            pipeline_id,
            video_and_stream_information,
            error,
            state,
            terminated,
            watcher_handle,
            lifecycle,
            notify,
            thumbnail_cooldown: Arc::new(Mutex::new(None)),
            mavlink_camera,
        })
    }

    #[instrument(
        level = "debug",
        skip(state, terminated, lifecycle, notify, mavlink_camera)
    )]
    async fn watcher(
        video_and_stream_information: Arc<RwLock<VideoAndStreamInformation>>,
        pipeline_id: Arc<uuid::Uuid>,
        error_status: Arc<RwLock<anyhow::Result<()>>>,
        state: Arc<RwLock<Option<StreamState>>>,
        terminated: Arc<RwLock<bool>>,
        lifecycle: Arc<LifecycleState>,
        notify: Arc<tokio::sync::Notify>,
        mavlink_camera: Arc<RwLock<Option<MavlinkCamera>>>,
    ) -> Result<()> {
        let report_interval_mult = 2;
        let report_interval_max = 60;
        let mut report_interval = std::time::Duration::from_secs(1);
        let mut last_report_time = std::time::Instant::now();

        let idle_grace_period = std::time::Duration::from_secs(5);
        let mut drain_start: Option<std::time::Instant> = None;
        let mut period = tokio::time::interval(tokio::time::Duration::from_millis(100));

        let mut persistent_rtsp: Option<sink::rtsp_sink::RtspSinkPersistent> = None;

        loop {
            tokio::select! {
                _ = notify.notified() => {}
                _ = period.tick() => {}
            }

            if *terminated.read().await {
                break;
            }

            let (phase, count) = lifecycle.load();

            match phase {
                Phase::Idle => {
                    drain_start = None;
                    if state
                        .read()
                        .await
                        .as_ref()
                        .is_some_and(|s| s.pipeline.is_some())
                    {
                        if let Some(old) = state.write().await.take() {
                            tokio::task::spawn_blocking(move || drop(old));
                        }
                    }
                    continue;
                }

                Phase::Waking => {
                    drain_start = None;
                    debug!(
                        "Waking handler entered: consumers={count}, error_count={}, backoff will be computed on error",
                        lifecycle.error_count()
                    );

                    // Await the old pipeline teardown so GStreamer resources
                    // are fully released before creating the new pipeline.
                    // Bounded to avoid blocking the watcher indefinitely if
                    // rtspsrc hangs during its NULL state change.
                    if let Some(old) = state.write().await.take() {
                        let handle = tokio::task::spawn_blocking(move || drop(old));
                        if tokio::time::timeout(tokio::time::Duration::from_secs(10), handle)
                            .await
                            .is_err()
                        {
                            warn!("Pipeline teardown timed out in Waking handler, proceeding with pipeline creation");
                        }
                    }

                    let video_and_stream_information_cloned =
                        video_and_stream_information.read().await.clone();

                    match video_and_stream_information_cloned.video_source {
                        VideoSourceType::Redirect(_) => {
                            let url = video_and_stream_information_cloned
                                .stream_information
                                .endpoints
                                .first()
                                .context("No URL found")?;

                            let capture_configuration =
                                match get_capture_configuration_from_stream_uri(url).await {
                                    Ok(capture_configuration) => capture_configuration,
                                    Err(error) => {
                                        let error_message = format!(
                                            "Failed getting CaptureConfiguration from endpoint. Error: {error:?}. Trying again soon..."
                                        );
                                        warn!(error_message);
                                        *error_status.write().await = Err(anyhow!(error_message));
                                        let backoff = lifecycle.handle_pipeline_error();
                                        warn!("Waking: CaptureConfiguration error, backoff={backoff:?}, error_count={}", lifecycle.error_count());
                                        tokio::time::sleep(backoff).await;
                                        notify.notify_one();
                                        continue;
                                    }
                                };

                            *error_status.write().await = Ok(());
                            video_and_stream_information
                                .write()
                                .await
                                .stream_information
                                .configuration = capture_configuration;
                        }

                        VideoSourceType::Local(_) => {
                            let mut streams = vec![video_and_stream_information_cloned.clone()];
                            let mut candidates = cameras_available().await;

                            let current_running_streams = manager::streams()
                                .await
                                .unwrap()
                                .iter()
                                .filter_map(|status| {
                                    status
                                        .running
                                        .then_some(status.video_and_stream.video_source.clone())
                                })
                                .collect::<Vec<VideoSourceType>>();
                            candidates
                                .retain(|candidate| !current_running_streams.contains(candidate));

                            let should_report =
                                std::time::Instant::now() - last_report_time >= report_interval;

                            manager::update_devices(&mut streams, &mut candidates, should_report)
                                .await;
                            *video_and_stream_information.write().await =
                                streams.first().unwrap().clone();

                            match crate::video::video_source::get_video_source(
                                video_and_stream_information_cloned
                                    .video_source
                                    .inner()
                                    .source_string(),
                            )
                            .await
                            {
                                Ok(best_candidate) => {
                                    video_and_stream_information.write().await.video_source =
                                        best_candidate;
                                }
                                Err(error) => {
                                    if should_report {
                                        let error_message = format!(
                                            "Failed to recreate the stream {pipeline_id:?}: {error:?}. Is the device connected? Trying again each second until the success or stream is removed. Next report in {report_interval:?} to reduce log size."
                                        );
                                        warn!(error_message);
                                        *error_status.write().await = Err(anyhow!(error_message));
                                        last_report_time = std::time::Instant::now();
                                        report_interval *= report_interval_mult;
                                        if report_interval
                                            > std::time::Duration::from_secs(report_interval_max)
                                        {
                                            report_interval =
                                                std::time::Duration::from_secs(report_interval_max);
                                        }
                                    }
                                    let backoff = lifecycle.handle_pipeline_error();
                                    warn!("Waking: Local device error, backoff={backoff:?}, error_count={}", lifecycle.error_count());
                                    tokio::time::sleep(backoff).await;
                                    notify.notify_one();
                                    continue;
                                }
                            }
                        }

                        VideoSourceType::Gst(_) => (),
                        VideoSourceType::Onvif(_) => (),
                    }

                    let new_state = match StreamState::try_new(
                        video_and_stream_information.clone(),
                        pipeline_id.clone(),
                        lifecycle.clone(),
                        notify.clone(),
                        persistent_rtsp.clone(),
                    )
                    .await
                    {
                        Ok(state) => state,
                        Err(error) => {
                            let error_message = format!(
                                "Failed to recreate the stream {pipeline_id:?}: {error:#?}. Trying again soon..."
                            );
                            warn!(error_message);
                            *error_status.write().await = Err(anyhow!(error_message));
                            let backoff = lifecycle.handle_pipeline_error();
                            warn!("Waking: StreamState::try_new error, backoff={backoff:?}, error_count={}", lifecycle.error_count());
                            tokio::time::sleep(backoff).await;
                            notify.notify_one();
                            continue;
                        }
                    };

                    if persistent_rtsp.is_none() {
                        if let Some(ref pipeline) = new_state.pipeline {
                            for s in pipeline.inner_state_as_ref().sinks.values() {
                                if let sink::Sink::Rtsp(rtsp) = s {
                                    persistent_rtsp = Some(sink::rtsp_sink::RtspSinkPersistent {
                                        appsrc: Some(rtsp.rtsp_appsrc()),
                                        pts_offset: Some(rtsp.pts_offset()),
                                        flow_handle: Some(rtsp.flow_handle()),
                                    });
                                    break;
                                }
                            }
                        }
                    }

                    state.write().await.replace(new_state);

                    // Create MavlinkCamera once on first successful wake;
                    // it persists across idle/wake cycles.
                    if mavlink_camera.read().await.is_none() {
                        let vsi = video_and_stream_information.read().await.clone();
                        let mavlink_enabled = vsi
                            .stream_information
                            .extended_configuration
                            .as_ref()
                            .map(|e| !e.disable_mavlink)
                            .unwrap_or_default();

                        if mavlink_enabled {
                            match MavlinkCamera::try_new(&vsi).await {
                                Ok(cam) => {
                                    mavlink_camera.write().await.replace(cam);
                                }
                                Err(error) => {
                                    warn!("Failed to create MavlinkCamera: {error:?}");
                                }
                            }
                        }
                    }

                    lifecycle.transition_to_running();
                    lifecycle.reset_error_backoff();
                    *error_status.write().await = Ok(());
                    report_interval = std::time::Duration::from_secs(1);
                    debug!("Pipeline {pipeline_id:?} started successfully");
                }

                Phase::Running => {
                    drain_start = None;
                    let is_running = state.read().await.as_ref().is_some_and(|s| {
                        s.pipeline
                            .as_ref()
                            .map(|p| p.is_running())
                            .unwrap_or_default()
                    });
                    if !is_running {
                        warn!("Pipeline {pipeline_id:?} stopped unexpectedly while Running, handling error");
                        // Mark RTSP sinks for preservation before dropping
                        if let Some(ref old_st) = *state.read().await {
                            if let Some(ref pipeline) = old_st.pipeline {
                                for s in pipeline.inner_state_as_ref().sinks.values() {
                                    if let sink::Sink::Rtsp(rtsp) = s {
                                        rtsp.set_preserve_factory(true);
                                    }
                                }
                            }
                        }
                        let backoff = lifecycle.handle_pipeline_error();
                        warn!(
                            "Running: pipeline stopped, backoff={backoff:?}, error_count={}",
                            lifecycle.error_count()
                        );
                        tokio::time::sleep(backoff).await;
                        notify.notify_one();
                    }
                }

                Phase::Draining => {
                    let since = drain_start.get_or_insert(std::time::Instant::now());
                    if since.elapsed() >= idle_grace_period {
                        debug!("Lazy pipeline {pipeline_id:?}: grace period expired, transitioning to Idle");
                        if lifecycle.transition_to_idle() {
                            // Successfully transitioned -- tear down pipeline
                            // Mark RTSP sinks for preservation
                            if let Some(ref old_st) = *state.read().await {
                                if let Some(ref pipeline) = old_st.pipeline {
                                    for s in pipeline.inner_state_as_ref().sinks.values() {
                                        if let sink::Sink::Rtsp(rtsp) = s {
                                            rtsp.set_preserve_factory(true);
                                        }
                                    }
                                }
                            }
                            if let Some(old) = state.write().await.take() {
                                tokio::task::spawn_blocking(move || drop(old));
                            }
                            drain_start = None;
                        } else {
                            // A consumer reconnected -- CAS failed harmlessly
                            drain_start = None;
                        }
                    }
                }
            }
        }

        Ok(())
    }
}

impl Drop for Stream {
    #[instrument(level = "debug", skip(self))]
    fn drop(&mut self) {
        if let Some(handle) = self.watcher_handle.take() {
            let state = self.state.clone();
            let terminated = self.terminated.clone();

            std::thread::Builder::new()
                .name("Stream::Drop".to_string())
                .spawn(move || {
                    let pipeline_id = state
                        .blocking_read()
                        .as_ref()
                        .map(|state| state.pipeline_id.clone().to_string());

                    debug!(pipeline_id, "Dropping Stream...");

                    *terminated.blocking_write() = true;

                    if !handle.is_finished() {
                        handle.abort();

                        // futures::executor::block_on(async move {
                        //     let _ = handle.await;
                        //     debug!(pipeline_id, "PipelineWatcher task aborted");
                        // });
                    } else {
                        debug!(pipeline_id, "PipelineWatcher task nicely finished!");
                    }
                })
                .unwrap()
                .join()
                .unwrap()
        }
    }
}

impl StreamState {
    #[instrument(level = "debug", skip_all)]
    pub async fn try_default(
        video_and_stream_information: Arc<RwLock<VideoAndStreamInformation>>,
        pipeline_id: Arc<uuid::Uuid>,
    ) -> Result<Self> {
        if let Err(error) = validate_endpoints(&video_and_stream_information.read().await.clone()) {
            return Err(anyhow!("Failed validating endpoints. Reason: {error:?}"));
        }

        Ok(StreamState {
            pipeline_id,
            pipeline: None,
            video_and_stream_information,
        })
    }

    #[instrument(level = "debug", skip_all)]
    pub async fn try_new(
        video_and_stream_information: Arc<RwLock<VideoAndStreamInformation>>,
        pipeline_id: Arc<uuid::Uuid>,
        lifecycle: Arc<LifecycleState>,
        notify: Arc<tokio::sync::Notify>,
        persistent_rtsp: Option<sink::rtsp_sink::RtspSinkPersistent>,
    ) -> Result<Self> {
        let mut stream =
            Self::try_default(video_and_stream_information.clone(), pipeline_id.clone()).await?;

        let video_and_stream_information = video_and_stream_information.read().await.clone();

        stream.pipeline = Some(Pipeline::try_new(
            &video_and_stream_information,
            &pipeline_id,
        )?);

        // Do not add any Sink if it's a redirect Pipeline
        if !matches!(
            &video_and_stream_information.video_source,
            VideoSourceType::Redirect(_)
        ) {
            let endpoints = &video_and_stream_information.stream_information.endpoints;

            // Disable concurrent RTSP and UDP sinks creation, as it is failing.
            if endpoints.iter().any(|endpoint| endpoint.scheme() == "udp")
                && endpoints.iter().any(|endpoint| endpoint.scheme() == "rtsp")
            {
                return Err(anyhow!(
                    "UDP endpoints won't work together with RTSP endpoints. You need to choose one. This is a (temporary) software limitation, if this is a feature you need, please, contact us."
                ));
            }

            if endpoints.iter().any(|endpoint| endpoint.scheme() == "udp") {
                let sink_id = Arc::new(Manager::generate_uuid(None));
                match create_udp_sink(sink_id.clone(), &video_and_stream_information) {
                    Ok(sink) => {
                        if let Some(pipeline) = stream.pipeline.as_mut() {
                            if let Err(reason) = pipeline.add_sink(sink).await {
                                return Err(anyhow!(
                                    "Failed to add Sink of type UDP to the Pipeline. Reason: {reason}"
                                ));
                            }
                        } else {
                            return Err(anyhow!("No Pipeline available to add UDP sink"));
                        }
                    }
                    Err(reason) => {
                        return Err(anyhow!(
                            "Failed to create Sink of type UDP. Reason: {reason}"
                        ));
                    }
                }
                // UDP sinks are fire-and-forget: hold a permanent +1 so
                // the stream never enters Draining/Idle.
                lifecycle.add_consumer(&*notify);
            }

            if endpoints
                .iter()
                .any(|endpoint| RTSPScheme::try_from(endpoint.scheme()).is_ok())
            {
                let sink_id = Arc::new(Manager::generate_uuid(None));
                match create_rtsp_sink(
                    sink_id.clone(),
                    &video_and_stream_information,
                    lifecycle.clone(),
                    notify.clone(),
                    persistent_rtsp,
                ) {
                    Ok(sink) => {
                        if let Some(pipeline) = stream.pipeline.as_mut() {
                            if let Err(reason) = pipeline.add_sink(sink).await {
                                return Err(anyhow!(
                                    "Failed to add Sink of type RTSP to the Pipeline. Reason: {reason}"
                                ));
                            }
                        } else {
                            return Err(anyhow!("No Pipeline available to add RTSP sink"));
                        }
                    }
                    Err(reason) => {
                        return Err(anyhow!(
                            "Failed to create Sink of type RTSP. Reason: {reason}"
                        ));
                    }
                }
            }
        }

        let sink_id = Arc::new(Manager::generate_uuid(None));
        if !video_and_stream_information
            .stream_information
            .extended_configuration
            .as_ref()
            .map(|e| e.disable_thumbnails)
            .unwrap_or_default()
        {
            match create_image_sink(sink_id.clone(), &video_and_stream_information) {
                Ok(sink) => {
                    if let Some(pipeline) = stream.pipeline.as_mut() {
                        if let Err(reason) = pipeline.add_sink(sink).await {
                            return Err(anyhow!(
                            "Failed to add Sink of type Image to the Pipeline. Reason: {reason}"
                        ));
                        }
                    } else {
                        return Err(anyhow!("No Pipeline available to add Image sink"));
                    }
                }
                Err(reason) => {
                    return Err(anyhow!(
                        "Failed to create Sink of type Image. Reason: {reason}"
                    ));
                }
            }
        }

        if !video_and_stream_information
            .stream_information
            .extended_configuration
            .as_ref()
            .map(|e| e.disable_zenoh)
            .unwrap_or_default()
            && crate::cli::manager::enable_zenoh()
        {
            let encoding = match &video_and_stream_information
                .stream_information
                .configuration
            {
                CaptureConfiguration::Video(video_configuraiton) => {
                    video_configuraiton.encode.clone()
                }
                CaptureConfiguration::Redirect(_) => {
                    return Err(anyhow!(
                        "Redirect CaptureConfiguration means the stream was not initialized yet"
                    ));
                }
            };

            if matches!(encoding, VideoEncodeType::H264 | VideoEncodeType::H265) {
                let sink_id = Arc::new(Manager::generate_uuid(None));
                match create_zenoh_sink(sink_id.clone(), &video_and_stream_information).await {
                    Ok(sink) => {
                        if let Some(pipeline) = stream.pipeline.as_mut() {
                            if let Err(reason) = pipeline.add_sink(sink).await {
                                return Err(anyhow!(
                                "Failed to add Sink of type Zenoh to the Pipeline. Reason: {reason}"
                            ));
                            }
                        } else {
                            return Err(anyhow!("No Pipeline available to add Zenoh sink"));
                        }
                    }
                    Err(reason) => {
                        return Err(anyhow!(
                            "Failed to create Sink of type Zenoh. Reason: {reason}"
                        ));
                    }
                }
            } else {
                debug!(
                    "Zenoh Sink was not added because the encoding {encoding:?} is not supported"
                );
            }
        }

        // Start the pipeline. This will automatically start sinks with linked proxy-isolated pipelines
        stream
            .pipeline
            .as_ref()
            .context("No Pipeline")?
            .inner_state_as_ref()
            .pipeline_runner
            .start()?;

        // Start all the sinks
        if let Some(pipeline) = stream.pipeline.as_mut() {
            let pipeline_state = pipeline.inner_state_mut();
            for sink in pipeline_state.sinks.values() {
                if let Err(error) = sink.start() {
                    warn!("Failed to start sink: {error:?}");
                }
            }
        }

        Ok(stream)
    }
}

impl Drop for StreamState {
    #[instrument(level = "debug", skip(self), fields(pipeline_id = self.pipeline_id.to_string()))]
    fn drop(&mut self) {
        let Some(pipeline) = self.pipeline.as_ref() else {
            return;
        };

        let pipeline_state = pipeline.inner_state_as_ref();
        let pipeline = &pipeline_state.pipeline;

        // Post EOS so elements can flush gracefully.
        let pipeline_weak = pipeline.downgrade();
        let eos_handle = std::thread::Builder::new()
            .name("PipelineEos".into())
            .spawn(move || {
                if let Some(pipeline) = pipeline_weak.upgrade() {
                    if let Err(error) = pipeline.post_message(::gst::message::Eos::new()) {
                        error!("Failed posting Eos message into Pipeline bus. Reason: {error:?}");
                    }
                }
            })
            .ok();

        // Run set_state(Null) in a separate thread so we can bound the wait.
        // rtspsrc can block here indefinitely when the remote RTSP server is
        // unresponsive or when jitterbuffer excision races with teardown.
        let pipeline_clone = pipeline.clone();
        let null_handle = std::thread::Builder::new()
            .name("PipelineSetNull".into())
            .spawn(move || {
                if let Err(error) = pipeline_clone.set_state(::gst::State::Null) {
                    error!("Failed setting Pipeline state to Null. Reason: {error:?}");
                }
            });

        const NULL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
        if let Ok(handle) = null_handle {
            let start = std::time::Instant::now();
            while !handle.is_finished() {
                if start.elapsed() >= NULL_TIMEOUT {
                    warn!(
                        "set_state(Null) timed out after {NULL_TIMEOUT:?}, \
                         continuing cleanup; teardown thread will finish in background"
                    );
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        }

        if pipeline.current_state() != ::gst::State::Null {
            if let Err(error) =
                wait_for_element_state(pipeline.downgrade(), ::gst::State::Null, 100, 5)
            {
                warn!("Pipeline did not reach Null state: {error:?}");
            }
        }

        if let Some(handle) = eos_handle {
            let _ = handle.join();
        }
        let _ = pipeline_state;

        // Remove all Sinks after the pipeline is stopped
        let pipeline_state = self
            .pipeline
            .as_mut()
            .expect("No Pipeline")
            .inner_state_mut();
        let sink_ids = &pipeline_state
            .sinks
            .keys()
            .cloned()
            .collect::<Vec<uuid::Uuid>>();
        for sink_id in sink_ids {
            if let Err(error) = pipeline_state.remove_sink(sink_id) {
                warn!("Failed unlinking Sink {sink_id:?} from Pipeline. Reason: {error:?}");
            }
        }
    }
}

#[instrument(level = "debug", skip_all)]
fn validate_endpoints(video_and_stream_information: &VideoAndStreamInformation) -> Result<()> {
    let endpoints = &video_and_stream_information.stream_information.endpoints;

    if endpoints.is_empty() {
        return Err(anyhow!("Endpoints are empty"));
    }

    if endpoints.iter().filter(|&e| e.scheme() == "rtsp").count() > 1 {
        return Err(anyhow!("Only one RTSP endpoint is supported at time"));
    }

    let encode = match &video_and_stream_information
        .stream_information
        .configuration
    {
        CaptureConfiguration::Video(configuration) => configuration.encode.clone(),
        CaptureConfiguration::Redirect(_) => VideoEncodeType::Unknown("Redirect stream".into()),
    };

    let errors: Vec<anyhow::Error> = endpoints.iter().filter_map(|endpoint| {

        let scheme = endpoint.scheme();

        if matches!(
            video_and_stream_information.video_source,
            VideoSourceType::Redirect(_)
        ) {
            match scheme {
                "udp" | "udp265" | "rtsp" => return None,
                _ => return Some(anyhow!(
                    "The URL's scheme for REDIRECT endpoints should be \"udp\", \"udp265\", or \"rtsp\", but was: {scheme:?}",
                ))
            };
        }

        if scheme.starts_with("rtsp") {
            if RTSPScheme::try_from(scheme).is_err() {
                return Some(anyhow!(
                    "Endpoint with rtsp scheme should use one of the following variant schemes: {:?}. Endpoint: {endpoint:?}",
                    RTSPScheme::VALUES
                ));
            }

            // RTSP endpoints should contain host, port, and path
            if endpoint.host().is_none() || endpoint.port().is_none() || endpoint.path().is_empty() {
                return Some(anyhow!(
                    "Endpoint with rtsp scheme should contain host, port, and path. Endpoint: {endpoint:?}"
                ));
            }
            let expected_port = RTSPServer::port();
            if endpoint.port() != Some(expected_port) {
                return Some(anyhow!(
                    "Endpoint with rtsp scheme should use port {expected_port:?}. Endpoint: {endpoint:?}"
                ));
            }

            return None;
        };

        match scheme {
            "udp" => {
                if VideoEncodeType::H265 == encode {
                    return Some(anyhow!("Endpoint with udp scheme only supports H264, encode type is H265, the scheme should be udp265."));
                }

                // UDP endpoints should contain both host and port
                if endpoint.host().is_none() || endpoint.port().is_none()
                {
                    return Some(anyhow!(
                        "Endpoint with udp scheme should contain host and port. Endpoint: {endpoint:?}"
                    ));
                }
            }
            "udp265" => {
                if VideoEncodeType::H265 != encode {
                    return Some(anyhow!("Endpoint with udp265 scheme only supports H265 encode. Encode: {encode:?}, Endpoint: {endpoints:?}"));
                }
            }
            _ => {
                return Some(anyhow!(
                    "Scheme is not accepted as stream endpoint: {scheme}"
                ));
            }
        }

        None
    }).collect();

    if !errors.is_empty() {
        return Err(anyhow!(
            "One or more endpoints are invalid. List of Errors:\n{errors:?}",
        ));
    }

    Ok(())
}
