use std::net::SocketAddr;
use std::thread;

use anyhow::{anyhow, Context, Result};
use async_std::task;
use futures::{SinkExt, StreamExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc::{self, UnboundedSender};
use tokio_tungstenite::tungstenite;

use tracing::*;

use crate::stream::manager::Manager;

use super::signalling_protocol::{self, *};

pub const DEFAULT_SIGNALLING_ENDPOINT: &str = "ws://0.0.0.0:6021";

/// Interface between the session manager and the WebRTC Signalling Server, which should be implemented by both sides to retain all coupling.
pub trait WebRTCSessionManagementInterface {
    fn add_session(bind: &BindOffer, sender: UnboundedSender<Result<Message>>)
        -> Result<SessionId>;
    fn remove_session(bind: &BindAnswer, _reason: String) -> Result<()>;

    /// This handle should interface the Signalling Server (directly or by means of a session manager) to the WebRTCBinInterface::handle_sdp.
    fn handle_sdp(bind: &BindAnswer, sdp: &RTCSessionDescription) -> Result<()>;

    /// This handle should interface the Signalling Server (directly or by means of a session manager) to the WebRTCBinInterface::handle_ice.
    fn handle_ice(bind: &BindAnswer, sdp_m_line_index: u32, candidate: &str) -> Result<()>;
}

/// Interface between the stream manager and the WebRTC Signalling Server, which should be implemented by both sides to retain all coupling.
pub trait StreamManagementInterface<T> {
    fn add_stream(stream: crate::stream::Stream) -> Result<()>;
    fn remove_stream(stream_id: &PeerId) -> Result<()>;
    fn streams_information() -> Vec<T>;
    fn generate_uuid() -> uuid::Uuid;
}

#[derive(Debug)]
pub struct SignallingServer {
    _server_thread_handle: std::thread::JoinHandle<()>,
}

impl Default for SignallingServer {
    #[instrument(level = "trace")]
    fn default() -> Self {
        Self {
            _server_thread_handle: thread::Builder::new()
                .name("SignallingServer".to_string())
                .spawn(SignallingServer::run_main_loop)
                .expect("Failed spawing SignallingServer thread"),
        }
    }
}

impl SignallingServer {
    #[instrument(level = "debug")]
    fn run_main_loop() {
        let endpoint = url::Url::parse(DEFAULT_SIGNALLING_ENDPOINT).unwrap();

        debug!("Starting Signalling server on {endpoint:?}...");

        match task::block_on(SignallingServer::runner(endpoint.clone())) {
            Ok(_) => debug!("Signalling server successively Started!"),
            Err(error) => error!("Error starting Signalling server on {endpoint:?}: {error:?}"),
        }
    }

    #[instrument(level = "debug")]
    async fn runner(endpoint: url::Url) -> Result<()> {
        let host = endpoint
            .host()
            .context(format!("Failed to get the host from {endpoint:#?}"))?;
        let port = endpoint
            .port()
            .context(format!("Failed to get the port from {endpoint:#?}"))?;

        let addr = format!("{host}:{port}").parse::<SocketAddr>()?;

        // Create the event loop and TCP listener we'll accept connections on.
        let listener = TcpListener::bind(&addr).await?;
        debug!("Signalling server: listening on: {addr:?}");

        while let Ok((stream, _)) = listener.accept().await {
            let peer = stream
                .peer_addr()
                .expect("connected streams should have a peer address");
            debug!("Peer address: {peer:?}");

            tokio::spawn(Self::accept_connection(peer, stream));
        }

        Ok(())
    }

    #[instrument(level = "debug")]
    async fn accept_connection(peer: SocketAddr, stream: TcpStream) {
        type Error = tungstenite::Error;

        if let Err(e) = Self::handle_connection(peer, stream).await {
            match e {
                Error::ConnectionClosed | Error::Protocol(_) | Error::Utf8 => (),
                err => error!("Error processing connection: {}", err),
            }
        }
    }

    #[instrument(level = "debug")]
    async fn handle_connection(peer: SocketAddr, stream: TcpStream) -> tungstenite::Result<()> {
        let (mut ws_sender, mut ws_receiver) = tokio_tungstenite::accept_async(stream)
            .await
            .expect("Failed to accept")
            .split();

        info!("New WebSocket connection: {peer:?}");

        // This MPSC channel is used to transmit messages to websocket from Session
        let (mpsc_sender, mut mpsc_receiver) = mpsc::unbounded_channel::<Result<Message>>();

        // Create a sender task, which receives from the mpsc channel
        tokio::spawn(async move {
            while let Some(result) = mpsc_receiver.recv().await {
                // Close the channel if receives an error
                let message = match result {
                    Ok(message) => message,
                    Err(error) => {
                        error!("{error}");
                        mpsc_receiver.close();
                        break;
                    }
                };

                let protocol = Some(Protocol::from(message));

                trace!("Sending..: {protocol:#?}");

                // Transform our Protocol into a tungstenite's Message
                let message: Option<tungstenite::Message> =
                    protocol.map(|protocol| protocol.try_into().unwrap());

                if let Some(message) = message {
                    if let Err(error) = ws_sender.send(message).await {
                        error!("Failed repassing message from the MPSC to the WebSocket. Reason: {error:?}");
                        break;
                    }
                }
            }

            info!("MPSC channel closed.");
            if let Err(reason) = ws_sender.close().await {
                error!("Failed closing WebSocket channel. Reason: {reason}");
            }
        });

        while let Some(msg) = ws_receiver.next().await {
            let msg = match msg {
                Ok(msg) => match msg {
                    msg @ tungstenite::Message::Text(_) => msg,
                    tungstenite::Message::Close(_) => break,
                    _ => continue,
                },
                Err(error) => {
                    error!("Failed receiving message from WebSocket. Reason: {error:?}.");
                    break;
                }
            };

            if let Err(error) = Self::handle_message(msg.clone(), &mpsc_sender).await {
                error!("Failed handling message. Reason: {error:?}.");
                break;
            }
        }

        info!("Websocket closed.");

        Ok(())
    }

    #[instrument(level = "debug")]
    async fn handle_message(
        msg: tungstenite::Message,
        sender: &mpsc::UnboundedSender<Result<Message>>,
    ) -> Result<()> {
        let protocol = match Protocol::try_from(msg) {
            Ok(protocol) => protocol,
            Err(error) => {
                // Parsing errors should not be propagated, otherwise it will close the WebSocket.
                warn!("Ignoring received message. Reason: {error:#?}");
                return Ok(());
            }
        };

        trace!("Received: {protocol:#?}");
        let answer = match protocol.message {
            Message::Question(question) => {
                match question {
                    Question::PeerId => Some(Answer::PeerId(PeerIdAnswer {
                        id: Self::generate_uuid(),
                    })),
                    Question::AvailableStreams => {
                        // This looks something dumb, but in fact, by keeping signalling_protocol::Stream and
                        // webrtc_manager::VideoAndStreamInformation as different things, we can change internal logics
                        // without changing the protocol's interface.
                        let streams = Self::streams_information();
                        Some(Answer::AvailableStreams(streams))
                    }
                    Question::StartSession(bind) => {
                        // After this point, any further negotiation will be sent from webrtcbin,
                        // which will use this mpsc channel's sender to queue the message for the
                        // WebSocket, which will receive and send it to the consumer via WebSocket.
                        Self::add_session(&bind, sender.clone())
                            .context("Failed adding session.")?;

                        None
                    }
                    Question::EndSession(end_session_question) => {
                        let bind = end_session_question.bind;
                        let reason = end_session_question.reason;

                        if let Err(error) = Self::remove_session(&bind, reason) {
                            error!("Failed removing session {bind:?}. Reason: {error}",);
                        }
                        return Err(anyhow!("Session {bind:?} ended by consumer"));
                    }
                }
            }
            Message::Answer(answer) => {
                return Err(anyhow!("Ignoring message {answer:#?}"));
            }
            Message::Negotiation(negotiation) => match negotiation {
                Negotiation::MediaNegotiation(negotiation) => {
                    let bind = negotiation.bind;
                    let sdp = negotiation.sdp;

                    Self::handle_sdp(&bind, &sdp).context("Failed handling SDP")?;

                    None
                }
                Negotiation::IceNegotiation(negotiation) => {
                    let bind = negotiation.bind;
                    let candidate = negotiation.ice.candidate.context("No candidate -> Done")?;
                    let sdp_m_line_index = negotiation
                        .ice
                        .sdp_m_line_index
                        .context("Missing sdp_m_line_index")?;

                    Self::handle_ice(&bind, sdp_m_line_index, &candidate)
                        .context("Failed handling ICE")?;

                    None
                }
            },
        };

        if let Some(answer) = answer {
            if let Err(reason) = sender.send(Ok(Message::from(answer))) {
                return Err(anyhow!(
                    "Failed sending message to mpsc channel. Reason: {reason:#?}"
                ));
            }
        }

        Ok(())
    }
}

impl WebRTCSessionManagementInterface for SignallingServer {
    fn add_session(
        bind: &BindOffer,
        sender: UnboundedSender<Result<Message>>,
    ) -> Result<SessionId> {
        Manager::add_session(bind, sender)
    }

    fn remove_session(bind: &BindAnswer, reason: String) -> Result<()> {
        Manager::remove_session(bind, reason)
    }

    fn handle_sdp(bind: &BindAnswer, sdp: &RTCSessionDescription) -> Result<()> {
        Manager::handle_sdp(bind, sdp)
    }

    fn handle_ice(bind: &BindAnswer, sdp_m_line_index: u32, candidate: &str) -> Result<()> {
        Manager::handle_ice(bind, sdp_m_line_index, candidate)
    }
}

impl StreamManagementInterface<Stream> for SignallingServer {
    fn add_stream(stream: crate::stream::Stream) -> Result<()> {
        Manager::add_stream(stream)
    }

    fn remove_stream(stream_id: &PeerId) -> Result<()> {
        Manager::remove_stream(stream_id)
    }

    fn streams_information() -> Vec<Stream> {
        let streams = Manager::streams_information();

        streams
            .iter()
            .filter_map(|stream| {
                let (height, width, encode, interval) =
                    match &stream.video_and_stream.stream_information.configuration {
                        crate::stream::types::CaptureConfiguration::Video(configuration) => {
                            // Filter out non-H264 local streams
                            if configuration.encode != crate::video::types::VideoEncodeType::H264 {
                                trace!("Stream {:?} will not be listed in available streams because it's encoding isn't H264 (it's {:?} instead)", stream.video_and_stream.name, configuration.encode);
                                return None;
                            }
                            (
                                Some(configuration.height),
                                Some(configuration.width),
                                Some(format!("{:#?}", configuration.encode)),
                                Some(
                                    (configuration.frame_interval.numerator as f32
                                        / configuration.frame_interval.denominator as f32)
                                        .to_string(),
                                ),
                            )
                        }
                        crate::stream::types::CaptureConfiguration::Redirect(_) => {
                            (None, None, None, None)
                        }
                    };

                let source = Some(
                    stream
                        .video_and_stream
                        .video_source
                        .inner()
                        .source_string()
                        .to_string(),
                );

                let name = stream.video_and_stream.name.clone();
                let id = stream.id;

                Some(Stream {
                    id,
                    name,
                    encode,
                    height,
                    width,
                    interval,
                    source,
                    created: None,
                })
            })
            .collect()
    }

    fn generate_uuid() -> uuid::Uuid {
        Manager::generate_uuid()
    }
}

impl TryFrom<tungstenite::Message> for signalling_protocol::Protocol {
    type Error = anyhow::Error;

    #[instrument(level = "trace")]
    fn try_from(value: tungstenite::Message) -> Result<Self, Self::Error> {
        let msg = value.to_text()?;

        let protocol = serde_json::from_str::<signalling_protocol::Protocol>(msg)?;

        Ok(protocol)
    }
}

impl TryInto<tungstenite::Message> for signalling_protocol::Protocol {
    type Error = anyhow::Error;

    #[instrument(level = "trace", skip(self))]
    fn try_into(self) -> Result<tungstenite::Message, Self::Error> {
        let json_str = serde_json::to_string(&self)?;

        let msg = tungstenite::Message::Text(json_str);

        Ok(msg)
    }
}
