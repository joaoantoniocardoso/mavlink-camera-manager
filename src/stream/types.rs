use super::{
    stream_backend::StreamBackend, video_stream_redirect::VideoStreamRedirect,
    video_stream_rtsp::VideoStreamRtsp, video_stream_tcp::VideoStreamTcp,
    video_stream_udp::VideoStreamUdp, video_stream_webrtc::VideoStreamWebRTC,
};
use crate::{
    video::types::{FrameInterval, VideoEncodeType},
    video_stream::types::VideoAndStreamInformation,
};

use mavlink::common::VideoStreamType;
use paperclip::actix::Apiv2Schema;
use serde::{Deserialize, Serialize};
use simple_error::SimpleError;
use url::Url;

#[derive(Debug)]
#[allow(dead_code)]
pub enum StreamType {
    UDP(VideoStreamUdp),
    TCP(VideoStreamTcp),
    RTSP(VideoStreamRtsp),
    REDIRECT(VideoStreamRedirect),
    WEBRTC(VideoStreamWebRTC),
}

impl StreamType {
    pub fn inner(&self) -> &(dyn StreamBackend + '_) {
        match self {
            StreamType::UDP(backend) => backend,
            StreamType::TCP(backend) => backend,
            StreamType::RTSP(backend) => backend,
            StreamType::REDIRECT(backend) => backend,
            StreamType::WEBRTC(backend) => backend,
        }
    }

    pub fn mut_inner(&mut self) -> &mut (dyn StreamBackend + '_) {
        match self {
            StreamType::UDP(backend) => backend,
            StreamType::TCP(backend) => backend,
            StreamType::RTSP(backend) => backend,
            StreamType::REDIRECT(backend) => backend,
            StreamType::WEBRTC(backend) => backend,
        }
    }
}

#[derive(PartialEq, Debug)]
pub enum TransportProtocolType {
    RTP,
    MPEGTS,
    MPEG,
}

#[derive(PartialEq, Debug)]
pub enum EndpointType {
    UDP,
    TCP,
    RTSP,
    WebRTCLocal,
    WebRTCRemoteTurn,
    WebRTCRemoteStun,
    WebRTCRemoteSignalling,
}

const UDP: &str = "udp";
const TCP: &str = "tcp";
const RTSP: &str = "rtsp";
const WEBRTC: &str = "webrtc";
const WS: &str = "ws";
const TURN: &str = "turn";
const STUN: &str = "stun";
const MPEGTS: &str = "mpegts";
const TCPMPEG: &str = "tcpmpeg";

pub trait SchemeParser {
    fn transport_protocol(&self) -> Result<TransportProtocolType, SimpleError>;
    fn endpoint_type(&self) -> Result<EndpointType, SimpleError>;
    fn video_stream_type(&self) -> Result<VideoStreamType, SimpleError>;
}

impl SchemeParser for url::Url {
    fn transport_protocol(&self) -> Result<TransportProtocolType, SimpleError> {
        match self.scheme() {
            UDP | TCP | RTSP | WEBRTC | WS | TURN | STUN => Ok(TransportProtocolType::RTP),
            MPEGTS => Ok(TransportProtocolType::MPEGTS),
            TCPMPEG => Ok(TransportProtocolType::MPEG),
            unknown => Err(SimpleError::new(format!(
                "Error: unknownw protocol {unknown:#?}"
            ))),
        }
    }

    fn endpoint_type(&self) -> Result<EndpointType, SimpleError> {
        match self.scheme() {
            UDP | MPEGTS => Ok(EndpointType::UDP),
            TCP | TCPMPEG => Ok(EndpointType::TCP),
            RTSP => Ok(EndpointType::RTSP),
            TURN => Ok(EndpointType::WebRTCRemoteTurn),
            STUN => Ok(EndpointType::WebRTCRemoteStun),
            WS => Ok(EndpointType::WebRTCRemoteSignalling),
            WEBRTC => Ok(EndpointType::WebRTCLocal),
            unknown => Err(SimpleError::new(format!(
                "Error: unknownw protocol {unknown:#?}"
            ))),
        }
    }

    fn video_stream_type(&self) -> Result<VideoStreamType, SimpleError> {
        match (self.transport_protocol()?, self.endpoint_type()?) {
            (TransportProtocolType::RTP, EndpointType::UDP) => {
                Ok(VideoStreamType::VIDEO_STREAM_TYPE_RTPUDP)
            }
            (TransportProtocolType::RTP, EndpointType::RTSP) => {
                Ok(VideoStreamType::VIDEO_STREAM_TYPE_RTSP)
            }
            (TransportProtocolType::MPEGTS, EndpointType::UDP) => {
                Ok(VideoStreamType::VIDEO_STREAM_TYPE_MPEG_TS_H264)
            }
            (TransportProtocolType::MPEG, EndpointType::TCP) => {
                Ok(VideoStreamType::VIDEO_STREAM_TYPE_TCP_MPEG)
            }
            (protocol, endpoint) => Err(SimpleError::new(format!(
                "Endpoint Type {endpoint:?} not supported for Transfer Protocol {protocol:?}."
            ))),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
pub struct VideoCaptureConfiguration {
    pub encode: VideoEncodeType,
    pub height: u32,
    pub width: u32,
    pub frame_interval: FrameInterval,
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
pub struct RedirectCaptureConfiguration {}

#[derive(Apiv2Schema, Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum CaptureConfiguration {
    VIDEO(VideoCaptureConfiguration),
    REDIRECT(RedirectCaptureConfiguration),
}

#[derive(Apiv2Schema, Clone, Debug, PartialEq, Deserialize, Serialize)]
pub struct ExtendedConfiguration {
    pub thermal: bool,
}

impl Default for ExtendedConfiguration {
    fn default() -> Self {
        Self { thermal: false }
    }
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize, Apiv2Schema)]
pub struct StreamInformation {
    pub endpoints: Vec<Url>,
    pub configuration: CaptureConfiguration,
    pub extended_configuration: Option<ExtendedConfiguration>,
}

#[derive(Apiv2Schema, Debug, Deserialize, Serialize)]
pub struct StreamStatus {
    pub running: bool,
    pub video_and_stream: VideoAndStreamInformation,
}
