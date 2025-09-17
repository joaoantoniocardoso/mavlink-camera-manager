use std::{collections::VecDeque, pin::Pin, sync::Arc};

use anyhow::{anyhow, Context, Result};
use gst::prelude::*;
use tracing::*;

use crate::{
    stream::gst::utils::wait_for_element_state_async,
    video_stream::types::VideoAndStreamInformation,
};

#[derive(Debug)]
pub struct PipelineRunner {
    start: tokio::sync::mpsc::Sender<()>,
    handle: Option<tokio::task::JoinHandle<()>>,
    pipeline_id: Arc<uuid::Uuid>,
}

struct PipelineRunnerContext {
    pipeline_weak: gst::glib::WeakRef<gst::Pipeline>,
    last_known_position: Option<gst::ClockTime>,
    last_position_change: Option<std::time::Instant>,
    freeze_reported: bool,
    last_heartbeat_logged: Option<std::time::Duration>,
    pipeline_started: bool,
    frame_deltas: VecDeque<std::time::Duration>,
    frame_duration: Option<std::time::Duration>,
    timeout_duration: std::time::Duration,
    tick_duration: std::time::Duration,
    next_tick: Pin<Box<tokio::time::Sleep>>,
}

impl Drop for PipelineRunner {
    #[instrument(level = "debug", skip(self), fields(pipeline_id = self.pipeline_id.to_string()))]
    fn drop(&mut self) {
        debug!("Dropping PipelineRunner...");

        if let Some(handle) = self.handle.take() {
            if !handle.is_finished() {
                handle.abort();
                tokio::spawn(async move {
                    let _ = handle.await;
                    debug!("PipelineRunner task aborted");
                });
            } else {
                debug!("PipelineRunner task nicely finished!");
            }
        }

        debug!("PipelineRunner Dropped!");
    }
}

impl PipelineRunner {
    #[instrument(level = "debug", skip(pipeline))]
    pub fn try_new(
        pipeline: &gst::Pipeline,
        pipeline_id: &Arc<uuid::Uuid>,
        allow_block: bool,
        video_and_stream_information: &VideoAndStreamInformation,
    ) -> Result<Self> {
        let pipeline_weak = pipeline.downgrade();

        let (start_tx, start_rx) = tokio::sync::mpsc::channel(1);

        debug!("Starting PipelineRunner task...");

        let span = span!(
            Level::DEBUG,
            "PipelineRunner task",
            id = pipeline_id.to_string()
        );
        let task_handle = tokio::spawn({
            let video_and_stream_information = video_and_stream_information.clone();
            let pipeline_id = pipeline_id.clone();
            async move {
                debug!("task started!");
                match Self::runner(
                    pipeline_weak,
                    pipeline_id,
                    start_rx,
                    allow_block,
                    &video_and_stream_information,
                )
                .await
                {
                    Ok(_) => debug!("task ended with no errors"),
                    Err(error) => warn!("task ended with error: {error:#?}"),
                };
            }
            .instrument(span)
        });

        Ok(Self {
            start: start_tx,
            handle: Some(task_handle),
            pipeline_id: pipeline_id.clone(),
        })
    }

    #[instrument(level = "debug", skip(self), fields(pipeline_id = self.pipeline_id.to_string()))]
    pub fn start(&self) -> Result<()> {
        let start = self.start.clone();
        tokio::spawn(async move {
            debug!("Pipeline Start task started!");
            if let Err(error) = start.send(()).await {
                error!("Failed to send start command: {error:#?}");
            }
            debug!("Pipeline Start task ended");
        });

        Ok(())
    }

    #[instrument(level = "debug", skip(self))]
    pub fn is_running(&self) -> bool {
        self.handle
            .as_ref()
            .map(|handle| !handle.is_finished())
            .unwrap_or(false)
    }

    #[instrument(
        level = "debug",
        skip(pipeline_weak, start, video_and_stream_information)
    )]
    async fn runner(
        pipeline_weak: gst::glib::WeakRef<gst::Pipeline>,
        pipeline_id: Arc<uuid::Uuid>,
        mut start: tokio::sync::mpsc::Receiver<()>,
        allow_block: bool,
        video_and_stream_information: &VideoAndStreamInformation,
    ) -> Result<()> {
        let (finish_tx, mut finish) = tokio::sync::mpsc::channel(1);

        let pipeline = pipeline_weak
            .upgrade()
            .context("Unable to access the Pipeline from its weak reference")?;

        let (bus_tx, bus_rx) = tokio::sync::mpsc::unbounded_channel::<gst::Message>();
        let bus = pipeline
            .bus()
            .context("Unable to access the pipeline bus")?;
        bus.set_sync_handler(move |_, msg| {
            let _ = bus_tx.send(msg.to_owned());
            gst::BusSyncReply::Drop
        });

        /* Iterate messages on the bus until an error or EOS occurs,
         * although in this example the only error we'll hopefully
         * get is if the user closes the output window */
        debug!("Starting BusWatcher task...");
        tokio::spawn(bus_watcher_task(
            pipeline_weak.clone(),
            pipeline_id.clone(),
            bus_rx,
            finish_tx,
        ));

        // Wait until start receive the signal
        debug!("PipelineRunner waiting for start command...");
        loop {
            tokio::select! {
                reason = finish.recv() => {
                    return Err(anyhow!("{reason:?}"));
                }
                start_cmd = start.recv() => {
                    match start_cmd {
                        Some(()) => {
                            debug!("PipelineRunner received start command");

                            let pipeline = pipeline_weak
                                .upgrade()
                                .context("Unable to access the Pipeline from its weak reference")?;

                            if pipeline.current_state() != gst::State::Playing {
                                if let Err(error) = pipeline.set_state(gst::State::Playing) {
                                    error!(
                                        "Failed setting Pipeline {pipeline_id} to Playing state. Reason: {error:?}"
                                    );
                                    continue;
                                }
                            }

                            if let Err(error) = wait_for_element_state_async(
                                pipeline_weak.clone(),
                                gst::State::Playing,
                                100,
                                5,
                            ).await {
                                return Err(anyhow!("{error:?}"));
                            }

                            break;
                        }
                        None => {
                            return Err(anyhow!("start channel closed before sending command"));
                        }
                    }

                }
            };
        }

        debug!("PipelineRunner started!");

        let frame_interval = match &video_and_stream_information
            .stream_information
            .configuration
        {
            crate::stream::types::CaptureConfiguration::Video(video_capture_configuration) => {
                video_capture_configuration.frame_interval.clone()
            }
            crate::stream::types::CaptureConfiguration::Redirect(_) => {
                return Err(anyhow!(
                    "PipelineRunner aborted: Redirect CaptureConfiguration means the stream was not initialized yet"
                ));
            }
        };

        let initial_frame_duration =
            if frame_interval.denominator > 0 && frame_interval.numerator > 0 {
                Some(std::time::Duration::from_secs_f64(
                    frame_interval.denominator as f64 / frame_interval.numerator as f64,
                ))
            } else {
                warn!("Invalid frame_interval {frame_interval:?}, using fallback of 1 FPS");
                Some(std::time::Duration::from_secs(1))
            };

        let timeout = Self::calculate_adaptive_timeout(initial_frame_duration);
        let tick_interval = Self::calculate_tick_interval(timeout);

        debug!(
            "Using tick_interval={tick_interval:?}, timeout={timeout:?} based on initial frame_duration={initial_frame_duration:?}"
        );

        let mut context = PipelineRunnerContext::new(
            pipeline_weak,
            initial_frame_duration,
            timeout,
            tick_interval,
        );

        loop {
            tokio::select! {
                reason = finish.recv() => {
                    return Err(anyhow!("{reason:?}"));
                }
                _ = &mut context.next_tick => {
                    if !allow_block {
                        // Restart pipeline if pipeline position do not change,
                        // occur if usb connection is lost and gst do not detect it
                        if let Err(error) = context.handle_pipeline_tick().await {
                            return Err(anyhow!("{error:?}"));
                        }
                    }
                    // Schedule next tick based on CURRENT tick_duration (which may have been adapted)
                    context.next_tick.as_mut().reset(tokio::time::Instant::now() + context.tick_duration);
                }
            }
        }
    }

    fn calculate_adaptive_timeout(
        frame_duration: Option<std::time::Duration>,
    ) -> std::time::Duration {
        frame_duration
            .map(|duration| std::cmp::max(std::time::Duration::from_secs(1), duration * 10))
            .unwrap_or(std::time::Duration::from_secs(5))
    }

    fn calculate_tick_interval(timeout: std::time::Duration) -> std::time::Duration {
        std::cmp::min(
            std::cmp::max(std::time::Duration::from_millis(200), timeout / 2),
            std::time::Duration::from_secs(1),
        )
    }
}

impl PipelineRunnerContext {
    fn new(
        pipeline_weak: gst::glib::WeakRef<gst::Pipeline>,
        initial_frame_duration: Option<std::time::Duration>,
        timeout: std::time::Duration,
        tick_interval: std::time::Duration,
    ) -> Self {
        Self {
            pipeline_weak,
            last_known_position: None,
            last_position_change: None,
            freeze_reported: false,
            last_heartbeat_logged: None,
            pipeline_started: false,
            frame_deltas: VecDeque::with_capacity(10),
            frame_duration: initial_frame_duration,
            timeout_duration: timeout,
            tick_duration: tick_interval,
            next_tick: Box::pin(tokio::time::sleep(tick_interval)),
        }
    }

    async fn handle_pipeline_tick(&mut self) -> Result<()> {
        let pipeline = self
            .pipeline_weak
            .upgrade()
            .context("Unable to access the Pipeline from its weak reference")?;

        let current_position = pipeline
            .query_position::<gst::ClockTime>()
            .ok_or_else(|| anyhow!("Failed to query pipeline position"))?;

        trace!("Queried pipeline position: {current_position:?}");

        if current_position.nseconds() == 0 {
            trace!("Position is zero — ignoring for freeze detection");
            self.last_known_position = Some(current_position);
            return Ok(());
        }

        let now = std::time::Instant::now();

        if !self.pipeline_started {
            info!("Pipeline received first non-zero position: {current_position:?}");
            self.pipeline_started = true;
            self.last_known_position = Some(current_position);
            self.last_position_change = Some(now);
            return Ok(());
        }

        if let Some(previous_position) = self.last_known_position {
            if previous_position.nseconds() == 0 {
                trace!("Previous position was zero — resetting baseline");
                self.last_known_position = Some(current_position);
                self.last_position_change = Some(now);
                return Ok(());
            }

            let position_changed = previous_position != current_position;
            trace!(
                "Position {}changed: prev={previous_position:?}, current={current_position:?}",
                if position_changed { "" } else { "un" }
            );

            if !position_changed {
                self.handle_frozen_pipeline(now);
            } else {
                self.handle_moving_pipeline(now, previous_position, current_position);
            }
        } else {
            trace!("No previous position recorded — initializing");
            self.last_known_position = Some(current_position);
            self.last_position_change = Some(now);
        }

        Ok(())
    }

    fn handle_frozen_pipeline(&mut self, now: std::time::Instant) {
        if let Some(last_change) = self.last_position_change {
            let elapsed = now.duration_since(last_change);
            trace!("Position unchanged for {elapsed:?}");

            if elapsed >= self.timeout_duration {
                if !self.freeze_reported {
                    warn!(
                        "Pipeline has been frozen for {elapsed:?} (expected frame every {:?})",
                        self.frame_duration
                            .unwrap_or_else(|| std::time::Duration::from_secs(1))
                    );
                    self.freeze_reported = true;
                    self.last_heartbeat_logged = Some(std::time::Duration::from_secs(0));
                } else if let Some(last_hb) = self.last_heartbeat_logged {
                    if elapsed.as_secs() >= last_hb.as_secs() + 5 {
                        info!(
                            "Still frozen for {elapsed:?} (timeout was {timeout:?})",
                            timeout = self.timeout_duration
                        );
                        self.last_heartbeat_logged = Some(elapsed);
                    }
                }
            }
        }
    }

    fn handle_moving_pipeline(
        &mut self,
        now: std::time::Instant,
        previous_position: gst::ClockTime,
        current_position: gst::ClockTime,
    ) {
        if previous_position.nseconds() != 0
            && current_position.nseconds() > previous_position.nseconds()
        {
            let observed_ns = current_position.nseconds() - previous_position.nseconds();
            let observed_duration = std::time::Duration::from_nanos(observed_ns);

            trace!("Observed frame delta: {observed_duration:?} ({observed_ns} ns)");

            if observed_duration > std::time::Duration::from_millis(1)
                && observed_duration < std::time::Duration::from_secs(10)
            {
                self.frame_deltas.push_back(observed_duration);
                if self.frame_deltas.len() > 10 {
                    self.frame_deltas.pop_front();
                }

                if self.frame_deltas.len() >= 5 {
                    if let Some(new_median) =
                        Self::calculate_median_frame_duration(&self.frame_deltas)
                    {
                        let current_fd = self.frame_duration.unwrap_or_default();
                        let diff = if new_median > current_fd {
                            new_median - current_fd
                        } else {
                            current_fd - new_median
                        };

                        if diff > current_fd / 10 || self.frame_duration.is_none() {
                            self.frame_duration = Some(new_median);
                            self.timeout_duration =
                                PipelineRunner::calculate_adaptive_timeout(self.frame_duration);
                            self.tick_duration =
                                PipelineRunner::calculate_tick_interval(self.timeout_duration);

                            // ⚡ Schedule next tick with new duration
                            self.next_tick
                                .as_mut()
                                .reset(tokio::time::Instant::now() + self.tick_duration);

                            debug!(
                                "Adapted frame_duration to {frame_duration:?} (median of last {} samples)",
                                self.frame_deltas.len(),
                                frame_duration = self.frame_duration
                            );
                        }
                    }
                }
            }
        }

        if self.freeze_reported {
            if let Some(last_change) = self.last_position_change {
                let frozen_duration = now.duration_since(last_change);
                warn!("Pipeline recovered after being frozen for {frozen_duration:?}");
                self.freeze_reported = false;
                self.last_heartbeat_logged = None;
            }
        }

        self.last_known_position = Some(current_position);
        self.last_position_change = Some(now);
    }

    fn calculate_median_frame_duration(
        frame_deltas: &VecDeque<std::time::Duration>,
    ) -> Option<std::time::Duration> {
        let len = frame_deltas.len();
        if len == 0 {
            return None;
        }

        let mut sorted: Vec<_> = frame_deltas.iter().collect();
        sorted.sort();

        let median_ns = if len % 2 == 0 {
            let a = sorted[len / 2 - 1].as_nanos();
            let b = sorted[len / 2].as_nanos();
            (a + b) / 2
        } else {
            sorted[len / 2].as_nanos()
        };

        Some(std::time::Duration::from_nanos(median_ns as u64))
    }
}

#[instrument(level = "debug", skip(pipeline_weak, bus_rx, finish_tx))]
async fn bus_watcher_task(
    pipeline_weak: gst::glib::WeakRef<gst::Pipeline>,
    pipeline_id: Arc<uuid::Uuid>,
    mut bus_rx: tokio::sync::mpsc::UnboundedReceiver<gst::Message>,
    finish_tx: tokio::sync::mpsc::Sender<String>,
) {
    debug!("BusWatcher task started!");

    while let Some(message) = bus_rx.recv().await {
        use gst::MessageView;

        let Some(pipeline) = pipeline_weak.upgrade() else {
            break;
        };

        match message.view() {
            MessageView::Eos(eos) => {
                pipeline.debug_to_dot_file_with_ts(
                    gst::DebugGraphDetails::all(),
                    format!("pipeline-{pipeline_id}-eos"),
                );
                let msg = format!("Received EndOfStream: {eos:?}");
                trace!(msg);
                let _ = finish_tx.send(msg).await;
                break;
            }
            MessageView::Error(error) => {
                let msg = format!(
                    "Error from {:?}: {} ({:?})",
                    error.src().map(|s| s.path_string()),
                    error.error(),
                    error.debug()
                );
                pipeline.debug_to_dot_file_with_ts(
                    gst::DebugGraphDetails::all(),
                    format!("pipeline-{pipeline_id}-error"),
                );
                trace!(msg);
                let _ = finish_tx.send(msg).await;
                break;
            }
            MessageView::StateChanged(state) => {
                pipeline.debug_to_dot_file_with_ts(
                    gst::DebugGraphDetails::all(),
                    format!(
                        "pipeline-{pipeline_id}-{:?}-to-{:?}",
                        state.old(),
                        state.current()
                    ),
                );

                trace!(
                    "State changed from {:?}: {:?} to {:?} ({:?})",
                    state.src().map(|s| s.path_string()),
                    state.old(),
                    state.current(),
                    state.pending()
                );
            }
            MessageView::Latency(latency) => {
                let current_latency = pipeline.latency();
                trace!("Latency message: {latency:?}. Current latency: {current_latency:?}",);
                if let Err(error) = pipeline.recalculate_latency() {
                    warn!("Failed to recalculate latency: {error:?}");
                }
                let new_latency = pipeline.latency();
                if current_latency != new_latency {
                    debug!("New latency: {new_latency:?}");
                }
            }
            other_message => trace!("{other_message:#?}"),
        }
    }

    debug!("BusWatcher task ended!");
}
