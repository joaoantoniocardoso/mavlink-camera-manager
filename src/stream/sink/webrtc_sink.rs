use crate::cli;
use anyhow::{anyhow, Context, Result};
use gst::prelude::*;
use tokio::sync::mpsc;
use tracing::*;

use super::SinkInterface;
use crate::stream::manager::Manager;
use crate::stream::webrtc::signalling_protocol as p;

use crate::stream::webrtc::signalling_server::WebRTCSessionManagementInterface;
use crate::stream::webrtc::turn_server::DEFAULT_TURN_ENDPOINT;

#[derive(Debug)]
pub struct WebRTCSink {
    pub queue: gst::Element,
    pub webrtcsink: gst::Element,
    pub webrtcsink_sink_pad: gst::Pad,
    pub tee_src_pad: Option<gst::Pad>,
    pub bind: p::BindAnswer,
    /// MPSC channel's sender to send messages to the respective Websocket from Signaller server. Err can be used to end the WebSocket.
    pub sender: mpsc::UnboundedSender<Result<p::Message>>,
    pub end_reason: Option<String>,
}
impl SinkInterface for WebRTCSink {
    #[instrument(level = "debug", skip(self))]
    fn link(
        self: &mut WebRTCSink,
        pipeline: &gst::Pipeline,
        pipeline_id: &uuid::Uuid,
        tee_src_pad: gst::Pad,
    ) -> Result<()> {
        // Link
        let sink_id = &self.get_id();

        // Set Tee's src pad
        if self.tee_src_pad.is_some() {
            return Err(anyhow!(
                "Tee's src pad from webrtcsink {sink_id} has already been configured"
            ));
        }
        self.tee_src_pad.replace(tee_src_pad);
        let Some(tee_src_pad) = &self.tee_src_pad else {
            unreachable!()
        };

        // Block data flow to prevent any data before set Playing, which would cause an error
        let Some(tee_src_pad_data_blocker) = tee_src_pad
            .add_probe(gst::PadProbeType::BLOCK_DOWNSTREAM, |_pad, _info| {
                gst::PadProbeReturn::Ok
            })
        else {
            let msg =
                "Failed adding probe to Tee's src pad to block data before going to playing state"
                    .to_string();
            error!(msg);

            if let Some(parent) = tee_src_pad.parent_element() {
                parent.release_request_pad(tee_src_pad)
            }

            return Err(anyhow!(msg));
        };

        // Add the Sink elements to the Pipeline
        let elements = &[&self.queue, &self.webrtcsink];
        if let Err(add_err) = pipeline.add_many(elements) {
            let msg = format!("Failed to add WebRTCSink's elements to the Pipeline: {add_err:?}");
            error!(msg);

            if let Some(parent) = tee_src_pad.parent_element() {
                parent.release_request_pad(tee_src_pad)
            }

            return Err(anyhow!(msg));
        }

        // Link the queue's src pad to the WebRTCSink's sink pad
        let queue_src_pad = &self
            .queue
            .static_pad("src")
            .expect("No sink pad found on Queue");
        if let Err(link_err) = queue_src_pad.link(&self.webrtcsink_sink_pad) {
            let msg =
                format!("Failed to link Queue's src pad with WebRTCSink's sink pad: {link_err:?}");
            error!(msg);

            if let Some(parent) = tee_src_pad.parent_element() {
                parent.release_request_pad(tee_src_pad)
            }

            if let Err(remove_err) = pipeline.remove_many(elements) {
                error!("Failed to remove elements from pipeline: {remove_err:?}");
            }

            return Err(anyhow!(msg));
        }

        // Link the new Tee's src pad to the queue's sink pad
        let queue_sink_pad = &self
            .queue
            .static_pad("sink")
            .expect("No sink pad found on Queue");
        if let Err(link_err) = tee_src_pad.link(queue_sink_pad) {
            let msg = format!("Failed to link Tee's src pad with Queue's sink pad: {link_err:#?}");
            error!(msg);

            if let Err(unlink_err) = queue_src_pad.unlink(&self.webrtcsink_sink_pad) {
                error!(
                    "Failed to unlink Queue's src pad from WebRTCSink's sink pad: {unlink_err:?}"
                );
            }

            if let Some(parent) = tee_src_pad.parent_element() {
                parent.release_request_pad(tee_src_pad)
            }
            if let Err(remove_err) = pipeline.remove_many(elements) {
                error!("Failed to remove elements from pipeline: {remove_err:?}");
            }

            return Err(anyhow!(msg));
        }

        // Syncronize added and linked elements
        if let Err(sync_err) = pipeline.sync_children_states() {
            let msg = format!("Failed to synchronize children states: {sync_err:?}");
            error!(msg);

            if let Err(unlink_err) = queue_src_pad.unlink(&self.webrtcsink_sink_pad) {
                error!(
                    "Failed to unlink Queue's src pad from WebRTCSink's sink pad: {unlink_err:?}"
                );
            }

            if let Err(unlink_err) = tee_src_pad.unlink(queue_sink_pad) {
                error!("Failed to unlink Tee's src pad from Queue's sink pad: {unlink_err:?}");
            }

            if let Some(parent) = tee_src_pad.parent_element() {
                parent.release_request_pad(tee_src_pad)
            }

            if let Err(remove_err) = pipeline.remove_many(elements) {
                error!("Failed to remove elements from pipeline: {remove_err:?}");
            }

            return Err(anyhow!(msg));
        }

        // Unblock data to go through this added Tee src pad
        tee_src_pad.remove_probe(tee_src_pad_data_blocker);

        // TODO: Workaround for bug: https://gitlab.freedesktop.org/gstreamer/gst-plugins-bad/-/issues/1539
        // Reasoning: because we are not receiving the Disconnected | Failed | Closed of WebRTCPeerConnectionState,
        // we are directly connecting to webrtcbin->transceiver->transport->connect_state_notify:
        // When the bug is solved, we should remove this code and use WebRTCPeerConnectionState instead.
        // let webrtcbin_clone = self.webrtcbin.downgrade();
        // let bind_clone = self.bind.clone();
        // let rtp_sender = transceiver
        //     .sender()
        //     .context("Failed getting transceiver's RTP sender element")?;
        // rtp_sender.connect_notify(Some("transport"), move |rtp_sender, _pspec| {
        //     let transport = rtp_sender.property::<gst_webrtc::WebRTCDTLSTransport>("transport");

        //     let bind = bind_clone.clone();
        //     let webrtcbin_clone = webrtcbin_clone.clone();
        //     transport.connect_state_notify(move |transport| {
        //         use gst_webrtc::WebRTCDTLSTransportState::*;

        //         let bind = bind.clone();
        //         let state = transport.state();
        //         debug!("DTLS Transport Connection changed to {state:#?}");
        //         match state {
        //             Failed | Closed => {
        //                 if let Some(webrtcbin) = webrtcbin_clone.upgrade() {
        //                     if webrtcbin.current_state() == gst::State::Playing {
        //                         // Closing the channel from the same thread can cause a deadlock, so we are calling it from another one:
        //                         std::thread::Builder::new()
        //                             .name("DTLSKiller".to_string())
        //                             .spawn(move || {
        //                                 let bind = &bind.clone();
        //                                 if let Err(error) = Manager::remove_session(
        //                                     bind,
        //                                     format!(
        //                                         "DTLS Transport connection closed with: {state:?}"
        //                                     ),
        //                                 ) {
        //                                     error!("Failed removing session {bind:#?}: {error}");
        //                                 }
        //                             })
        //                             .expect("Failed spawing DTLSKiller thread");
        //                     }
        //                 }
        //             }
        //             _ => (),
        //         }
        //     });
        // });

        Ok(())
    }

    #[instrument(level = "debug", skip(self))]
    fn unlink(&self, pipeline: &gst::Pipeline, pipeline_id: &uuid::Uuid) -> Result<()> {
        let Some(tee_src_pad) = &self.tee_src_pad else {
            warn!("Tried to unlink Sink from a pipeline without a Tee src pad.");
            return Ok(());
        };

        // Block data flow to prevent any data from holding the Pipeline elements alive
        if tee_src_pad
            .add_probe(gst::PadProbeType::BLOCK_DOWNSTREAM, |_pad, _info| {
                gst::PadProbeReturn::Ok
            })
            .is_none()
        {
            warn!(
                "Failed adding probe to Tee's src pad to block data before going to playing state"
            );
        }

        // Unlink the queue's src pad to the WebRTCSink's sink pad
        let queue_src_pad = &self
            .queue
            .static_pad("src")
            .expect("No sink pad found on Queue");
        if let Err(unlink_err) = queue_src_pad.unlink(&self.webrtcsink_sink_pad) {
            error!("Failed to unlink Queue's src pad from WebRTCSink's sink pad: {unlink_err:?}");
        }

        // Unlink the new Tee's src pad to the queue's sink pad
        let queue_sink_pad = &self
            .queue
            .static_pad("sink")
            .expect("No sink pad found on Queue");
        if let Err(unlink_err) = tee_src_pad.unlink(queue_sink_pad) {
            error!("Failed to unlink Tee's src pad from Queue's sink pad: {unlink_err:?}");
        }

        // Release Tee's src pad
        if let Some(parent) = tee_src_pad.parent_element() {
            parent.release_request_pad(tee_src_pad)
        }

        // Remove the Sink's elements from the Source's pipeline
        let elements = &[&self.queue, &self.webrtcsink];
        if let Err(remove_err) = pipeline.remove_many(elements) {
            warn!("Failed removing WebRTCSink's elements from pipeline: {remove_err:?}");
        }

        // Set Queue to null
        if let Err(state_err) = self.queue.set_state(gst::State::Null) {
            warn!("Failed to set Queue's state to NULL: {state_err:#?}");
        }

        // Set Sink to null
        if let Err(state_err) = self.webrtcsink.set_state(gst::State::Null) {
            warn!("Failed to set WebRTCSink's to NULL: {state_err:#?}");
        }

        Ok(())
    }

    #[instrument(level = "trace", skip(self))]
    fn get_id(&self) -> uuid::Uuid {
        self.bind.session_id
    }

    #[instrument(level = "trace", skip(self))]
    fn get_sdp(&self) -> Result<gst_sdp::SDPMessage> {
        Err(anyhow!(
            "Not available: WebRTC Sink should only be connected by means of its Signalling protocol."
        ))
    }

    #[instrument(level = "debug", skip(self))]
    fn start(&self) -> Result<()> {
        Ok(())
    }

    #[instrument(level = "debug", skip(self))]
    fn eos(&self) {
        let webrtcsink_weak = self.webrtcsink.downgrade();
        std::thread::spawn(move || {
            let webrtcsink = webrtcsink_weak.upgrade().unwrap();
            if let Err(error) = webrtcsink.post_message(gst::message::Eos::new()) {
                error!("Failed posting Eos message into Sink bus. Reason: {error:?}");
            }
        });
    }
}

impl WebRTCSink {
    fn make_signaller(
        bind: p::BindAnswer,
        sender: mpsc::UnboundedSender<Result<p::Message>>,
    ) -> Result<crate::stream::webrtc::signaller::Signaller> {
        let signaller =
            gst::glib::Object::builder::<crate::stream::webrtc::signaller::Signaller>().build();

        // Here we bridge messages from our Signaller to our Signalling Server

        let sender_weak = sender.downgrade();
        signaller.connect("start", false, move |values| {
            let signaller = values[0]
                .get::<crate::stream::webrtc::signaller::Signaller>()
                .expect("Invalid argument");

            let sender = sender_weak.upgrade()?;

            let message = p::Answer::StartSession(bind).into();

            if let Err(error) = sender.send(Ok(message)) {
                error!("Failed posting StartSession message SignallingServer: {error:?}");
            }

            // Trigger WebRTCSink to start the negotiation
            signaller.emit_by_name::<()>(
                "session-requested",
                &[
                    &bind.session_id.to_string().as_str(),
                    &bind.consumer_id.to_string().as_str(),
                    &None::<gst_webrtc::WebRTCSessionDescription>,
                ],
            );

            // signaller.emit_by_name::<()>(
            //     "session-started",
            //     &[
            //         &bind.session_id.to_string().as_str(),
            //         &bind.consumer_id.to_string().as_str(),
            //     ],
            // );

            Some(true.into())
        });

        // TODO: USE THIS TO DECONSTRUCT THINGS / JOIN TASKS, IF ANY
        let sender_weak = sender.downgrade();
        signaller.connect("stop", false, move |values| {
            let _signaller = values[0]
                .get::<crate::stream::webrtc::signaller::Signaller>()
                .expect("Invalid argument");

            let sender = sender_weak.upgrade()?;

            debug!("stop");

            // let message = p::Question::EndSession(p::EndSessionQuestion {
            //     bind,
            //     reason: "Stop from Signaller".to_string(),
            // })
            // .into();

            // if let Err(error) = sender.send(Ok(message)) {
            //     error!("Failed posting EndSession message SignallingServer: {error:?}");
            // }

            Some(true.into())
        });

        let sender_weak = sender.downgrade();
        signaller.connect("send-session-description", false, move |values| {
            let _signaller = values[0]
                .get::<crate::stream::webrtc::signaller::Signaller>()
                .expect("Invalid argument");
            let session_id = values[1].get::<String>().expect("Invalid argument");
            let sdp = values[2]
                .get::<gst_webrtc::WebRTCSessionDescription>()
                .expect("Invalid argument");

            let sender = sender_weak.upgrade()?;

            // Recreate the SDP answer with our customized SDP
            let sdp = customize_sdp(&sdp.sdp()).ok()?.to_string();

            debug!("send-session-description: {session_id:?}: {sdp:?}");

            let message = p::MediaNegotiation {
                bind,
                sdp: p::RTCSessionDescription::Offer(p::Sdp { sdp }),
            }
            .into();

            if let Err(error) = sender.send(Ok(message)) {
                error!("Failed posting MediaNegotiation message SignallingServer: {error:?}");
            }

            Some(true.into())
        });

        let sender_weak = sender.downgrade();
        signaller.connect("send-ice", false, move |values| {
            let _signaller = values[0]
                .get::<crate::stream::webrtc::signaller::Signaller>()
                .expect("Invalid argument");
            let session_id = values[1].get::<String>().expect("Invalid argument");
            let candidate = values[2].get::<String>().expect("Invalid argument");
            let sdp_m_line_index = values[3].get::<u32>().expect("Invalid argument");
            let sdp_mid = values[4].get::<Option<String>>().expect("Invalid argument");

            let sender = sender_weak.upgrade()?;

            debug!("send-ice: {session_id:?}: {candidate:?}, {sdp_m_line_index:?}, {sdp_mid:?}");

            let message = p::IceNegotiation {
                bind,
                ice: p::RTCIceCandidateInit {
                    candidate: Some(candidate),
                    sdp_mid,
                    sdp_m_line_index: Some(sdp_m_line_index),
                    username_fragment: None,
                },
            }
            .into();

            if let Err(error) = sender.send(Ok(message)) {
                error!("Failed posting IceNegotiation message SignallingServer: {error:?}");
            }

            Some(true.into())
        });

        let sender_weak = sender.downgrade();
        signaller.connect("end-session", false, move |values| {
            let _signaller = values[0]
                .get::<crate::stream::webrtc::signaller::Signaller>()
                .expect("Invalid argument");
            let session_id = values[1].get::<String>().expect("Invalid argument");

            let sender = sender_weak.upgrade()?;

            debug!("end-session: {session_id:?}");

            let message = p::Question::EndSession(p::EndSessionQuestion {
                bind,
                reason: "Ended from Signaller".to_string(),
            })
            .into();

            if let Err(error) = sender.send(Ok(message)) {
                error!("Failed posting EndSession message SignallingServer: {error:?}");
            }

            Some(true.into())
        });

        Ok(signaller)
    }

    #[instrument(level = "debug")]
    pub fn try_new(
        bind: p::BindAnswer,
        sender: mpsc::UnboundedSender<Result<p::Message>>,
    ) -> Result<Self> {
        let queue = gst::ElementFactory::make("queue")
            .property_from_str("leaky", "downstream") // Throw away any data
            .property("silent", true)
            .property("flush-on-eos", true)
            .property("max-size-buffers", 0u32) // Disable buffers
            .build()?;

        let signaller = Self::make_signaller(bind, sender.clone())?;
        // let signaller = gst::glib::Object::builder::<gstrswebrtc::signaller::Signaller>().build(); // In case we want to use the original Signaller

        let webrtcsink = gstrswebrtc::webrtcsink::BaseWebRTCSink::with_signaller(
            gstrswebrtc::signaller::Signallable::from(signaller),
        )
        .upcast::<gst::Element>();

        webrtcsink.set_property("async-handling", true);
        webrtcsink.set_property_from_str("congestion-control", "disabled");
        webrtcsink.set_property("do-fec", false);
        webrtcsink.set_property("do-retransmission", false);
        webrtcsink.set_property("enable-data-channel-navigation", false);
        webrtcsink.set_property_from_str("ice-transport-policy", "all");
        webrtcsink
            .set_property_from_str("stun-server", cli::manager::stun_server_address().as_str());
        webrtcsink.set_property_from_str(
            "turn-servers",
            format!("<{DEFAULT_TURN_ENDPOINT:?}>").as_str(),
        );
        webrtcsink.set_property_from_str("video-caps", "video/x-h264");

        let webrtcsink_sink_pad = webrtcsink
            .request_pad_simple("video_%u")
            .context("Failed requesting sink pad for webrtcsink")?;

        Ok(WebRTCSink {
            queue,
            webrtcsink,
            webrtcsink_sink_pad,
            tee_src_pad: None,
            bind,
            sender,
            end_reason: None,
        })
    }

    // // Whenever webrtcbin tells us that (re-)negotiation is needed, simply ask
    // // for a new offer SDP from webrtcbin without any customization and then
    // // asynchronously send it to the peer via the WebSocket connection
    // #[instrument(level = "debug", skip(self))]
    // fn on_negotiation_needed(&self, webrtcbin: &gst::Element) -> Result<()> {
    //     let this = self.clone();
    //     let webrtcbin_weak = webrtcbin.downgrade();
    //     let promise = gst::Promise::with_change_func(move |reply| {
    //         let reply = match reply {
    //             Ok(Some(reply)) => reply,
    //             Ok(None) => {
    //                 error!("Offer creation future got no response");
    //                 return;
    //             }
    //             Err(error) => {
    //                 error!("Failed to send SDP offer: {error:?}");
    //                 return;
    //             }
    //         };
    //
    //         let offer = match reply.get_optional::<gst_webrtc::WebRTCSessionDescription>("offer") {
    //             Ok(Some(offer)) => offer,
    //             Ok(None) => {
    //                 error!("Response got no \"offer\"");
    //                 return;
    //             }
    //             Err(error) => {
    //                 error!("Failed to send SDP offer: {error:?}");
    //                 return;
    //             }
    //         };
    //
    //         if let Some(webrtcbin) = webrtcbin_weak.upgrade() {
    //             if let Err(error) = this.on_offer_created(&webrtcbin, &offer) {
    //                 error!("Failed to send SDP offer: {error}");
    //             }
    //         }
    //     });
    //
    //     webrtcbin.emit_by_name::<()>("create-offer", &[&None::<gst::Structure>, &promise]);
    //
    //     Ok(())
    // }

    // Once webrtcbin has create the offer SDP for us, handle it by sending it to the peer via the
    // WebSocket connection
    #[instrument(level = "debug", skip(self))]
    fn on_offer_created(
        &self,
        webrtcbin: &gst::Element,
        offer: &gst_webrtc::WebRTCSessionDescription,
    ) -> Result<()> {
        // Recreate the SDP offer with our customized SDP
        let offer =
            gst_webrtc::WebRTCSessionDescription::new(offer.type_(), customize_sdp(&offer.sdp())?);

        let Ok(sdp) = offer.sdp().as_text() else {
            return Err(anyhow!("Failed reading the received SDP"));
        };

        // All good, then set local description
        webrtcbin.emit_by_name::<()>("set-local-description", &[&offer, &None::<gst::Promise>]);

        debug!("Sending SDP offer to peer. Offer:\n{sdp}");

        let message = p::MediaNegotiation {
            bind: self.bind,
            sdp: p::RTCSessionDescription::Offer(p::Sdp { sdp }),
        }
        .into();

        self.sender.send(Ok(message))?;

        Ok(())
    }

    // Once webrtcbin has create the answer SDP for us, handle it by sending it to the peer via the
    // WebSocket connection
    #[instrument(level = "debug", skip(self))]
    fn on_answer_created(
        &self,
        _webrtcbin: &gst::Element,
        answer: &gst_webrtc::WebRTCSessionDescription,
    ) -> Result<()> {
        // Recreate the SDP answer with our customized SDP
        let answer = gst_webrtc::WebRTCSessionDescription::new(
            answer.type_(),
            customize_sdp(&answer.sdp())?,
        );

        let Ok(sdp) = answer.sdp().as_text() else {
            return Err(anyhow!("Failed reading the received SDP"));
        };

        debug!("Sending SDP answer to peer. Answer:\n{sdp}");

        // All good, then set local description
        let message = p::MediaNegotiation {
            bind: self.bind,
            sdp: p::RTCSessionDescription::Answer(p::Sdp { sdp }),
        }
        .into();

        self.sender
            .send(Ok(message))
            .context("Failed to send SDP answer")?;

        Ok(())
    }

    #[instrument(level = "debug", skip(self))]
    fn on_ice_candidate(
        &self,
        _webrtcbin: &gst::Element,
        sdp_m_line_index: &u32,
        candidate: &str,
    ) -> Result<()> {
        let message = p::IceNegotiation {
            bind: self.bind,
            ice: p::RTCIceCandidateInit {
                candidate: Some(candidate.to_owned()),
                sdp_mid: None,
                sdp_m_line_index: Some(sdp_m_line_index.to_owned()),
                username_fragment: None,
            },
        }
        .into();

        self.sender.send(Ok(message))?;

        debug!("ICE candidate created!");

        Ok(())
    }

    #[instrument(level = "debug", skip(self))]
    fn on_ice_gathering_state_change(
        &self,
        state: &gst_webrtc::WebRTCICEGatheringState,
    ) -> Result<()> {
        if let gst_webrtc::WebRTCICEGatheringState::Complete = state {
            debug!("ICE gathering complete")
        }

        Ok(())
    }

    #[instrument(level = "debug", skip(self))]
    fn on_ice_connection_state_change(
        &self,
        state: &gst_webrtc::WebRTCICEConnectionState,
    ) -> Result<()> {
        use gst_webrtc::WebRTCICEConnectionState::*;

        debug!("ICE connection changed to {state:#?}");
        match state {
            Completed => {
                // let srcpads = webrtcbin.src_pads();
                // if let Some(srcpad) = srcpads.first() {
                //     srcpad.send_event(
                //         gst_video::UpstreamForceKeyUnitEvent::builder()
                //             .all_headers(true)
                //             .build(),
                //     );
                // }
            }
            Failed | Closed | Disconnected => {
                if let Err(error) =
                    Manager::remove_session(&self.bind, format!("ICE closed with: {state:?}"))
                {
                    error!("Failed removing session {:#?}: {error}", self.bind);
                }
            }
            _ => (),
        };

        Ok(())
    }

    #[instrument(level = "debug", skip(self))]
    fn on_connection_state_change(
        &self,
        state: &gst_webrtc::WebRTCPeerConnectionState,
    ) -> Result<()> {
        use gst_webrtc::WebRTCPeerConnectionState::*;

        debug!("Connection changed to {state:#?}");
        match state {
            // TODO: This would be the desired workflow, but it is not being detected, so we are using a workaround connecting direcly to the DTLS Transport connection state in the Session constructor.
            Disconnected | Failed | Closed => {
                warn!("For mantainers: Peer connection lost was detected by WebRTCPeerConnectionState, we should remove the workaround. State: {state:#?}");
                // self.close("Connection lost"); // TODO: Keep this line commented until the forementioned bug is solved.
            }
            _ => (),
        }

        Ok(())
    }

    #[instrument(level = "debug", skip(self))]
    pub fn handle_sdp(&self, sdp: &gst_webrtc::WebRTCSessionDescription) -> Result<()> {
        let signaller = self
            .webrtcsink
            .property::<crate::stream::webrtc::signaller::Signaller>("signaller");

        signaller.emit_by_name::<()>(
            "session-description",
            &[&self.bind.session_id.to_string().as_str(), &sdp],
        );

        Ok(())
    }

    #[instrument(level = "debug", skip(self))]
    pub fn handle_ice(&self, sdp_m_line_index: &u32, candidate: &str) -> Result<()> {
        let signaller = self
            .webrtcsink
            .property::<crate::stream::webrtc::signaller::Signaller>("signaller");

        signaller.emit_by_name::<()>(
            "handle-ice",
            &[
                &self.bind.session_id.to_string().as_str(),
                &sdp_m_line_index,
                &None::<String>, //sdp_mid is ignored by BaseWebRTCSink
                &candidate,
            ],
        );

        Ok(())
    }
}

/// Because GSTreamer's WebRTCBin often crashes when receiving an invalid SDP,
/// we use Mozzila's SDP parser to manipulate the SDP Message before giving it to GStreamer
fn customize_sdp(sdp: &gst_sdp::SDPMessage) -> Result<gst_sdp::SDPMessage> {
    let mut sdp = webrtc_sdp::parse_sdp(sdp.as_text()?.as_str(), false)?;

    for media in sdp.media.iter_mut() {
        let attributes = media.get_attributes().to_vec(); // This clone is necessary to avoid imutable borrow after a mutable borrow
        for attribute in attributes {
            use webrtc_sdp::attribute_type::SdpAttribute::*;

            match attribute {
                // Filter out unsupported/unwanted attributes
                Recvonly | Sendrecv | Inactive => {
                    media.remove_attribute((&attribute).into());
                    debug!("Removed unsupported/unwanted attribute: {attribute:?}");
                }
                // Customize FMTP
                // Here we are lying to the peer about our profile-level-id (to constrained-baseline) so any browser can accept it
                Fmtp(mut fmtp) => {
                    const CONSTRAINED_BASELINE_LEVEL_ID: u32 = 0x42e01f;
                    fmtp.parameters.profile_level_id = CONSTRAINED_BASELINE_LEVEL_ID;
                    fmtp.parameters.level_asymmetry_allowed = true;

                    let attribute = webrtc_sdp::attribute_type::SdpAttribute::Fmtp(fmtp);

                    debug!("FMTP attribute customized: {attribute:?}");

                    media.set_attribute(attribute)?;
                }
                _ => continue,
            }
        }
    }

    gst_sdp::SDPMessage::parse_buffer(sdp.to_string().as_bytes()).map_err(anyhow::Error::msg)
}
