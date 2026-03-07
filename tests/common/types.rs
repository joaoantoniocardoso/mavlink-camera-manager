#![allow(dead_code)]

use serde::{Deserialize, Serialize};
use url::Url;
use uuid::Uuid;

// -- Info (/info) -----------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct Development {
    pub number_of_tasks: usize,
}

#[derive(Debug, Deserialize)]
pub struct Info {
    pub name: String,
    pub version: String,
    pub sha: String,
    pub build_date: String,
    pub authors: String,
    pub development: Development,
}

// -- Video sources (/v4l) ---------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct ApiVideoSource {
    pub name: String,
    pub source: String,
    pub formats: Vec<Format>,
    pub controls: Vec<serde_json::Value>,
    pub blocked: bool,
}

#[derive(Debug, Deserialize)]
pub struct Format {
    pub encode: String,
    pub sizes: Vec<Size>,
}

#[derive(Debug, Deserialize)]
pub struct Size {
    pub width: u32,
    pub height: u32,
    pub intervals: Vec<FrameInterval>,
}

// -- Streams (/streams) -----------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FrameInterval {
    pub numerator: u32,
    pub denominator: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VideoCaptureConfiguration {
    pub encode: String,
    pub height: u32,
    pub width: u32,
    pub frame_interval: FrameInterval,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RedirectCaptureConfiguration {}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum CaptureConfiguration {
    Video(VideoCaptureConfiguration),
    Redirect(RedirectCaptureConfiguration),
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ExtendedConfiguration {
    pub thermal: bool,
    pub disable_mavlink: bool,
    pub disable_zenoh: bool,
    pub disable_thumbnails: bool,
    pub disable_lazy: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamInformation {
    pub endpoints: Vec<Url>,
    pub configuration: CaptureConfiguration,
    pub extended_configuration: Option<ExtendedConfiguration>,
}

#[derive(Debug, Deserialize)]
pub struct VideoAndStreamInformation {
    pub name: String,
    pub stream_information: StreamInformation,
    pub video_source: serde_json::Value,
}

#[derive(Debug, Deserialize)]
pub struct MavlinkComponent {
    pub system_id: u8,
    pub component_id: u8,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StreamStatusState {
    Running,
    Idle,
    Stopped,
}

#[derive(Debug, Deserialize)]
pub struct StreamStatus {
    pub id: Uuid,
    pub running: bool,
    pub state: StreamStatusState,
    pub error: Option<String>,
    pub video_and_stream: VideoAndStreamInformation,
    pub mavlink: Option<MavlinkComponent>,
}

// -- POST /streams body -----------------------------------------------------

#[derive(Debug, Serialize)]
pub struct PostStream {
    pub name: String,
    pub source: String,
    pub stream_information: StreamInformation,
}

// -- WebRTC signalling protocol (lightweight mirror) ------------------------

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SignallingProtocol {
    #[serde(flatten)]
    pub message: SignallingMessage,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", content = "content", rename_all = "camelCase")]
pub enum SignallingMessage {
    Question(SignallingQuestion),
    Answer(SignallingAnswer),
    Negotiation(serde_json::Value),
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", content = "content", rename_all = "camelCase")]
pub enum SignallingQuestion {
    PeerId,
    AvailableStreams,
    StartSession(BindOffer),
    EndSession(EndSessionQuestion),
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", content = "content", rename_all = "camelCase")]
pub enum SignallingAnswer {
    PeerId(PeerIdAnswer),
    AvailableStreams(Vec<AvailableStream>),
    StartSession(BindAnswer),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BindOffer {
    pub consumer_id: Uuid,
    pub producer_id: Uuid,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BindAnswer {
    pub consumer_id: Uuid,
    pub producer_id: Uuid,
    pub session_id: Uuid,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct EndSessionQuestion {
    #[serde(flatten)]
    pub bind: BindAnswer,
    pub reason: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PeerIdAnswer {
    pub id: Uuid,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct AvailableStream {
    pub id: Uuid,
    pub name: String,
    pub encode: Option<String>,
    pub height: Option<u32>,
    pub width: Option<u32>,
    pub interval: Option<String>,
    pub source: Option<String>,
    pub created: Option<String>,
}
