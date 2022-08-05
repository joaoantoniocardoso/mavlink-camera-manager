use crate::cli;
use crate::network;
use crate::settings;
use crate::stream::types::StreamType;
use crate::video::types::VideoSourceType;
use crate::video_stream::types::VideoAndStreamInformation;

use log::*;
use simple_error::SimpleError;
use url::Url;

use std::convert::TryInto;
use std::sync::{Arc, Mutex};

lazy_static! {
    static ref ID_CONTROL: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(vec![]));
}

#[derive(Clone, Debug)]
#[allow(dead_code)]
pub struct MavlinkCameraComponent {
    // MAVLink specific information
    system_id: u8,
    component_id: u8,
    stream_id: u8,

    vendor_name: String,
    model_name: String,
    firmware_version: u32,
    resolution_h: u16,
    resolution_v: u16,
    framerate: f32,
    bitrate: u32,
    rotation: u16,
    hfov: u16,
    thermal: bool,
}

#[derive(Clone)]
pub struct MavlinkCameraInformation {
    component: MavlinkCameraComponent,
    mavlink_connection_string: String,
    mavlink_stream_type: mavlink::common::VideoStreamType,
    video_stream_uri: Url,
    video_stream_name: String,
    video_source_type: VideoSourceType,
    vehicle: Arc<Box<dyn mavlink::MavConnection<mavlink::common::MavMessage> + Sync + Send>>,
}

#[derive(Clone, Debug, PartialEq)]
enum ThreadState {
    DEAD,
    RUNNING,
    ZOMBIE,
    RESTART,
}

#[derive(Debug)]
#[allow(dead_code)]
pub struct MavlinkCameraHandle {
    mavlink_camera_information: Arc<Mutex<MavlinkCameraInformation>>,
    thread_state: Arc<Mutex<ThreadState>>,
    heartbeat_thread: std::thread::JoinHandle<()>,
    receive_message_thread: std::thread::JoinHandle<()>,
}

// Debug definition to avoid problems with vehicle type
impl std::fmt::Debug for MavlinkCameraInformation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MavlinkCameraInformation")
            .field("component", &self.component)
            .field("mavlink_connection_string", &self.mavlink_connection_string)
            .field("mavlink_stream_type", &self.mavlink_stream_type)
            .field("video_stream_uri", &self.video_stream_uri)
            .field("video_source_type", &self.video_source_type)
            .finish()
    }
}

impl MavlinkCameraComponent {
    fn try_new(video_and_stream_information: &VideoAndStreamInformation) -> Option<Self> {
        let mut vector = ID_CONTROL.lock().unwrap();

        // Find the closer ID available
        let mut id: u8 = 0;
        loop {
            if vector.contains(&id) {
                id += 1;
                continue;
            } else {
                vector.push(id);
                break;
            }
        }

        let (resolution_h, resolution_v, framerate) = match &video_and_stream_information
            .stream_information
            .configuration
        {
            crate::stream::types::CaptureConfiguration::VIDEO(cfg) => {
                let framerate =
                    cfg.frame_interval.denominator as f32 / cfg.frame_interval.numerator as f32;
                (cfg.height as u16, cfg.width as u16, framerate)
            }
            crate::stream::types::CaptureConfiguration::REDIRECT(_) => (0, 0, 0.0),
        };

        let thermal = video_and_stream_information
            .stream_information
            .extended_configuration
            .clone()
            .unwrap_or_default()
            .thermal;

        Some(Self {
            system_id: 1,
            component_id: mavlink::common::MavComponent::MAV_COMP_ID_CAMERA as u8 + id,
            stream_id: 1, // Starts at 1, 0 is for broadcast.

            vendor_name: video_and_stream_information
                .video_source
                .inner()
                .name()
                .to_string(), // TODO: see what is more appropriate
            model_name: video_and_stream_information.name.clone(), // TODO: see what is more appropriate
            firmware_version: 0,
            resolution_h,
            resolution_v,
            bitrate: 5000,
            rotation: 0,
            hfov: 90,
            framerate,
            thermal,
        })
    }
}

impl Drop for MavlinkCameraComponent {
    fn drop(&mut self) {
        // Remove id from used ids
        let id = self.component_id - mavlink::common::MavComponent::MAV_COMP_ID_CAMERA as u8;
        let mut vector = ID_CONTROL.lock().unwrap();
        let position = vector.iter().position(|&vec_id| vec_id == id).unwrap();
        vector.remove(position);
    }
}

impl From<&StreamType> for mavlink::common::VideoStreamType {
    fn from(stream: &StreamType) -> Self {
        match stream {
            StreamType::UDP(_) => mavlink::common::VideoStreamType::VIDEO_STREAM_TYPE_RTPUDP,
            StreamType::RTSP(_) => mavlink::common::VideoStreamType::VIDEO_STREAM_TYPE_RTSP,
            StreamType::REDIRECT(video_strem_redirect) => {
                match video_strem_redirect.scheme.as_str() {
                    "rtsp" => mavlink::common::VideoStreamType::VIDEO_STREAM_TYPE_RTSP,
                    "mpegts" => mavlink::common::VideoStreamType::VIDEO_STREAM_TYPE_MPEG_TS_H264,
                    "tcp" => mavlink::common::VideoStreamType::VIDEO_STREAM_TYPE_TCP_MPEG,
                    "udp" => mavlink::common::VideoStreamType::VIDEO_STREAM_TYPE_RTPUDP,
                    format @ _ => {
                        debug!("Unknown format: {format:#?}, using UDP as fallback.");
                        mavlink::common::VideoStreamType::VIDEO_STREAM_TYPE_RTPUDP
                    }
                }
            }
            // TODO: update WEBRTC arm with the correct type once mavlink starts to support it.
            // Note: For now this is fine because most of the clients doesn't seems to be using mavtype to determine the stream type,
            // instead, they're parsing the URI's scheme itself, so as long as we pass a known scheme, it should be enough.
            StreamType::WEBRTC(_) => mavlink::common::VideoStreamType::VIDEO_STREAM_TYPE_RTSP,
        }
    }
}

impl MavlinkCameraInformation {
    fn try_new(
        video_and_stream_information: &VideoAndStreamInformation,
        stream: &StreamType,
    ) -> Option<Self> {
        let video_stream_uri = video_and_stream_information
            .stream_information
            .endpoints
            .first()?
            .to_owned();

        let video_stream_name = video_and_stream_information.name.clone();

        let mavlink_stream_type = mavlink::common::VideoStreamType::from(stream);
        let video_source_type = video_and_stream_information.video_source.clone();

        let component = MavlinkCameraComponent::try_new(video_and_stream_information)?;

        let mavlink_connection_string = settings::manager::mavlink_endpoint()?;
        let vehicle = Arc::new(mavlink::connect(&mavlink_connection_string).unwrap());

        let this = Self {
            component,
            mavlink_connection_string,
            mavlink_stream_type,
            video_stream_uri,
            video_stream_name,
            video_source_type,
            vehicle,
        };

        debug!("Starting new MAVLink camera: {this:#?}");

        Some(this)
    }

    pub fn cam_definition_uri(&self) -> Option<Url> {
        // Get the current remotely accessible link (from default interface)
        // to our camera XML file.
        // This can't be a parameter because the default network route might
        // change between the time of the MavlinkCameraInformation creation
        // and the time MAVLink connection is negotiated with the other MAVLink
        // systems.
        let visible_qgc_ip_address = network::utils::get_ipv4_addresses()
            .last()
            .unwrap_or(&std::net::Ipv4Addr::UNSPECIFIED)
            .to_string();
        let server_port = cli::manager::server_address()
            .split(':')
            .collect::<Vec<&str>>()[1];
        let video_source_path = self.video_source_type.inner().source_string();
        Url::parse(&format!(
            "http://{visible_qgc_ip_address}:{server_port}/xml?file={video_source_path}"
        ))
        .ok()
    }
}

impl MavlinkCameraHandle {
    pub fn try_new(
        video_and_stream_information: &VideoAndStreamInformation,
        stream: &StreamType,
    ) -> Option<Self> {
        let mavlink_camera_information: Arc<Mutex<MavlinkCameraInformation>> =
            Arc::new(Mutex::new(MavlinkCameraInformation::try_new(
                video_and_stream_information,
                stream,
            )?));

        let thread_state = Arc::new(Mutex::new(ThreadState::RUNNING));

        let heartbeat_mavlink_information = mavlink_camera_information.clone();
        let receive_message_mavlink_information = mavlink_camera_information.clone();

        let heartbeat_state = thread_state.clone();
        let receive_message_state = thread_state.clone();

        Some(Self {
            mavlink_camera_information,
            thread_state,
            heartbeat_thread: std::thread::spawn(move || {
                heartbeat_loop(heartbeat_state, heartbeat_mavlink_information)
            }),
            receive_message_thread: std::thread::spawn(move || {
                receive_message_loop(receive_message_state, receive_message_mavlink_information)
            }),
        })
    }
}

impl Drop for MavlinkCameraHandle {
    fn drop(&mut self) {
        debug!("Dropping {self:#?}");
        let mut state = self.thread_state.lock().unwrap();
        *state = ThreadState::DEAD;
    }
}

fn heartbeat_loop(
    atomic_thread_state: Arc<Mutex<ThreadState>>,
    mavlink_camera_information: Arc<Mutex<MavlinkCameraInformation>>,
) {
    let mut header = mavlink::MavHeader::default();
    let mavlink_camera_information = mavlink_camera_information.as_ref().lock().unwrap();
    header.system_id = mavlink_camera_information.component.system_id;
    header.component_id = mavlink_camera_information.component.component_id;
    let vehicle = mavlink_camera_information.vehicle.clone();
    drop(mavlink_camera_information);

    loop {
        std::thread::sleep(std::time::Duration::from_secs(1));

        let mut heartbeat_state = atomic_thread_state.as_ref().lock().unwrap().clone();
        if heartbeat_state == ThreadState::ZOMBIE {
            continue;
        }
        if heartbeat_state == ThreadState::DEAD {
            break;
        }

        if heartbeat_state == ThreadState::RESTART {
            heartbeat_state = ThreadState::RUNNING;
            drop(heartbeat_state);

            std::thread::sleep(std::time::Duration::from_secs(3));
            continue;
        }

        debug!("sending heartbeat");
        if let Err(error) = vehicle.as_ref().send(&header, &heartbeat_message()) {
            error!("Failed to send heartbeat: {:?}", error);
        }
    }
}

fn receive_message_loop(
    atomic_thread_state: Arc<Mutex<ThreadState>>,
    mavlink_camera_information: Arc<Mutex<MavlinkCameraInformation>>,
) {
    let mut header = mavlink::MavHeader::default();
    let information = mavlink_camera_information.as_ref().lock().unwrap();
    header.system_id = information.component.system_id;
    header.component_id = information.component.component_id;

    let vehicle = information.vehicle.clone();
    drop(information);
    let vehicle = vehicle.as_ref();
    loop {
        let loop_state = atomic_thread_state.as_ref().lock().unwrap().clone();
        if loop_state == ThreadState::DEAD {
            break;
        }

        match vehicle.recv() {
            Ok((_header, msg)) => {
                match msg {
                    // Check if there is any camera information request from gcs
                    mavlink::common::MavMessage::COMMAND_LONG(command_long) => {
                        if command_long.target_system != header.system_id {
                            debug!(
                                "Ignoring COMMAND_LONG, wrong system id: expect {}, but got {}.",
                                header.system_id, command_long.target_system
                            );
                            continue;
                        }

                        if command_long.target_component != header.component_id {
                            debug!(
                                "Ignoring COMMAND_LONG, wrong component id: expect {}, but got {}.",
                                header.component_id, command_long.target_component
                            );
                            continue;
                        }

                        match command_long.command {
                            mavlink::common::MavCmd::MAV_CMD_REQUEST_CAMERA_INFORMATION => {
                                debug!("Sending camera_information..");
                                let information =
                                    mavlink_camera_information.as_ref().lock().unwrap();
                                let source_string =
                                    information.video_source_type.inner().source_string();
                                let vendor_name = information.video_source_type.inner().name();

                                let ips = network::utils::get_ipv4_addresses();
                                let visible_qgc_ip_address = &ips.last().unwrap().to_string();
                                let server_port = cli::manager::server_address()
                                    .split(":")
                                    .collect::<Vec<&str>>()[1];

                                if let Err(error) = vehicle.send(
                                    &header,
                                    &camera_information(
                                        vendor_name,
                                        vendor_name,
                                        &format!("{visible_qgc_ip_address}:{server_port}"),
                                        source_string,
                                    ),
                                ) {
                                    warn!("Failed to send camera_information: {:?}", error);
                                }
                            }
                            mavlink::common::MavCmd::MAV_CMD_REQUEST_CAMERA_SETTINGS => {
                                debug!("Sending camera_settings..");
                                if let Err(error) = vehicle.send(&header, &camera_settings()) {
                                    warn!("Failed to send camera_settings: {:?}", error);
                                }
                            }
                            mavlink::common::MavCmd::MAV_CMD_REQUEST_STORAGE_INFORMATION => {
                                debug!("Sending camera_storage_information..");
                                if let Err(error) =
                                    vehicle.send(&header, &camera_storage_information())
                                {
                                    warn!("Failed to send camera_storage_information: {:?}", error);
                                }
                            }
                            mavlink::common::MavCmd::MAV_CMD_REQUEST_CAMERA_CAPTURE_STATUS => {
                                debug!("Sending camera_capture_status..");
                                if let Err(error) = vehicle.send(&header, &camera_capture_status())
                                {
                                    warn!("Failed to send camera_capture_status: {:?}", error);
                                }
                            }
                            mavlink::common::MavCmd::MAV_CMD_REQUEST_VIDEO_STREAM_INFORMATION => {
                                debug!("Sending video_stream_information..");
                                let information =
                                    mavlink_camera_information.as_ref().lock().unwrap();
                                let source_string =
                                    information.video_source_type.inner().source_string();

                                // Remove localhost address with public ip
                                let mut video_url = information.video_stream_uri.clone();
                                if let Ok(address) = std::net::Ipv4Addr::from_str(
                                    video_url.host_str().unwrap_or_default(),
                                ) {
                                    if address == std::net::Ipv4Addr::UNSPECIFIED {
                                        let ips = network::utils::get_ipv4_addresses();
                                        let visible_qgc_ip_address =
                                            &ips.last().unwrap().to_string();
                                        let _ = video_url.set_host(Some(visible_qgc_ip_address));
                                    }
                                }

                                if let Err(error) = vehicle.send(
                                    &header,
                                    &video_stream_information(
                                        &source_string,
                                        &video_url.to_string(),
                                        information.mavlink_stream_type,
                                        information.thermal,
                                    ),
                                ) {
                                    warn!("Failed to send video_stream_information: {:?}", error);
                                }
                            }
                            mavlink::common::MavCmd::MAV_CMD_RESET_CAMERA_SETTINGS => {
                                let information =
                                    &mavlink_camera_information.as_ref().lock().unwrap();
                                let source_string =
                                    information.video_source_type.inner().source_string();
                                let component = &information.component;
                                drop(information);

                                let mut param_result =
                                    mavlink::common::MavResult::MAV_RESULT_ACCEPTED;
                                if let Err(error) =
                                    crate::video::video_source::reset_controls(source_string)
                                {
                                    error!(
                                        "Failed to reset {source_string:?} controls with its default values. Reason: {error:#?}",
                                    );
                                    param_result = mavlink::common::MavResult::MAV_RESULT_DENIED;
                                }

                                if let Err(error) = vehicle.send(
                                    &header,
                                    &mavlink::common::MavMessage::COMMAND_ACK(
                                        mavlink::common::COMMAND_ACK_DATA {
                                            command: mavlink::common::MavCmd::MAV_CMD_RESET_CAMERA_SETTINGS,
                                            result: param_result,
                                            target_system: component.system_id,
                                            target_component: component.component_id,
                                            ..Default::default()
                                        }
                                    ),
                                ) {
                                    warn!("Failed to send COMMAND_ACK for MAV_CMD_RESET_CAMERA_SETTINGS: {error:?}");
                                }
                            }
                            message => {
                                let information =
                                    mavlink_camera_information.as_ref().lock().unwrap();
                                warn!(
                                    "Message {message:#?}, Camera: {information:#?}, ignoring command: {:#?}",
                                    command_long.command
                                );
                            }
                        }
                    }
                    mavlink::common::MavMessage::PARAM_EXT_SET(param_ext_set) => {
                        if param_ext_set.target_system != header.system_id {
                            debug!(
                                "Ignoring PARAM_EXT_SET, wrong system id: expect {}, but got {}.",
                                header.system_id, param_ext_set.target_system
                            );
                            continue;
                        }

                        if param_ext_set.target_component != header.component_id {
                            debug!(
                                "Ignoring PARAM_EXT_SET, wrong component id: expect {}, but got {}.",
                                header.component_id,
                                param_ext_set.target_component
                            );
                            continue;
                        }

                        let control_id = match control_id_from_param_id(&param_ext_set.param_id) {
                            Some(value) => value,
                            None => continue,
                        };

                        let control_value = match control_value_from_param_value(
                            &param_ext_set.param_value,
                            &param_ext_set.param_type,
                        ) {
                            Some(value) => value,
                            None => continue,
                        };

                        let mut param_result = mavlink::common::ParamAck::PARAM_ACK_ACCEPTED;
                        if let Err(error) = mavlink_camera_information
                            .as_ref()
                            .lock()
                            .unwrap()
                            .video_source_type
                            .inner()
                            .set_control_by_id(control_id, control_value)
                        {
                            error!(
                                "Failed to set parameter {control_id:?} with value {control_value:?}. Reason: {error:#?}",
                            );
                            param_result = mavlink::common::ParamAck::PARAM_ACK_FAILED;
                        }

                        if let Err(error) = vehicle.send(
                            &header,
                            &mavlink::common::MavMessage::PARAM_EXT_ACK(
                                mavlink::common::PARAM_EXT_ACK_DATA {
                                    param_id: param_ext_set.param_id,
                                    param_value: param_ext_set.param_value,
                                    param_type: param_ext_set.param_type,
                                    param_result,
                                },
                            ),
                        ) {
                            warn!("Failed to send video_stream_information: {error:?}");
                        }
                    }
                    mavlink::common::MavMessage::PARAM_EXT_REQUEST_READ(param_ext_req) => {
                        if param_ext_req.target_system != header.system_id {
                            debug!(
                                "Ignoring {:#?}, wrong system id: expect {}, but got {}.",
                                1, header.system_id, param_ext_req.target_system
                            );
                            continue;
                        }

                        if param_ext_req.target_component != header.component_id {
                            debug!(
                                "Ignoring PARAM_EXT_REQUEST_READ, wrong component id: expect {}, but got {}.",
                                header.component_id,
                                param_ext_req.target_component
                            );
                            continue;
                        }

                        let information = mavlink_camera_information.as_ref().lock().unwrap();
                        let controls = &information.video_source_type.inner().controls();
                        let (param_index, control_id) =
                            match get_param_index_and_control_id(&param_ext_req, controls) {
                                Some(value) => value,
                                None => continue,
                            };

                        let param_id = param_id_from_control_id(control_id);

                        let control_value = match information
                            .video_source_type
                            .inner()
                            .control_value_by_id(control_id)
                        {
                            Ok(value) => value,
                            Err(error) => {
                                error!(
                                    "Failed to get parameter {control_id:?}. Reason: {error:#?}",
                                );
                                continue;
                            }
                        };

                        let param_value = param_value_from_control_value(control_value, 128);

                        if let Err(error) = vehicle.send(
                            &header,
                            &mavlink::common::MavMessage::PARAM_EXT_VALUE(
                                mavlink::common::PARAM_EXT_VALUE_DATA {
                                    param_count: 1,
                                    param_index,
                                    param_id,
                                    param_value,
                                    param_type:
                                        mavlink::common::MavParamExtType::MAV_PARAM_EXT_TYPE_INT64,
                                },
                            ),
                        ) {
                            warn!("Failed to send video_stream_information: {error:?}");
                        }
                    }
                    mavlink::common::MavMessage::PARAM_EXT_REQUEST_LIST(param_ext_req) => {
                        if param_ext_req.target_system != header.system_id {
                            debug!(
                                "Ignoring PARAM_EXT_REQUEST_LIST, wrong system id: expect {}, but got {}.",
                                header.system_id,
                                param_ext_req.target_system
                            );
                            continue;
                        }

                        if param_ext_req.target_component != header.component_id {
                            debug!(
                                "Ignoring PARAM_EXT_REQUEST_LIST, wrong component id: expect {}, but got {}.",
                                header.component_id,
                                param_ext_req.target_component
                            );
                            continue;
                        }

                        let controls = mavlink_camera_information
                            .as_ref()
                            .lock()
                            .unwrap()
                            .video_source_type
                            .inner()
                            .controls();

                        controls
                        .iter()
                        .enumerate()
                        .for_each(|(param_index, control)| {
                            let param_id = param_id_from_control_id(control.id);

                            let control_value = match &control.configuration {
                                crate::video::types::ControlType::Bool(bool) => bool.value,
                                crate::video::types::ControlType::Slider(slider) => slider.value,
                                crate::video::types::ControlType::Menu(menu) => menu.value,
                            };

                            let param_value = param_value_from_control_value(control_value, 128);

                            if let Err(error) = vehicle.send(
                                &header,
                                &mavlink::common::MavMessage::PARAM_EXT_VALUE(
                                    mavlink::common::PARAM_EXT_VALUE_DATA {
                                        param_count: controls.len() as u16,
                                        param_index: param_index as u16,
                                        param_id,
                                        param_value,
                                        param_type:
                                            mavlink::common::MavParamExtType::MAV_PARAM_EXT_TYPE_INT64,
                                    },
                                ),
                            ) {
                                warn!("Failed to send video_stream_information: {error:?}");
                            }

                        });
                    }
                    //TODO: Handle all necessary QGC messages to setup camera
                    // We receive a bunch of heartbeat messages, we can ignore it
                    mavlink::common::MavMessage::HEARTBEAT(_) => {}
                    // Any other message that is not a heartbeat or command_long
                    _ => {
                        let information = mavlink_camera_information.as_ref().lock().unwrap();
                        debug!("Camera: {:#?}, Ignoring: {:#?}", information, msg);
                    }
                }
            }
            Err(error) => {
                let information = mavlink_camera_information.as_ref().lock().unwrap();
                error!("Camera: {:#?}, Recv error: {:#?}", information, error);
            }
        }
    }
}

fn param_value_from_control_value(control_value: i64, length: usize) -> Vec<char> {
    let mut param_value = control_value
        .to_le_bytes()
        .iter()
        .map(|&byte| byte as char)
        .collect::<Vec<char>>();
    // Workaround for https://github.com/mavlink/rust-mavlink/issues/111
    param_value.resize(length, Default::default());
    param_value
}

fn control_value_from_param_value(
    param_value: &Vec<char>,
    param_type: &mavlink::common::MavParamExtType,
) -> Option<i64> {
    let bytes: Vec<u8> = param_value.iter().map(|c| *c as u8).collect();
    let control_value = match param_type {
        mavlink::common::MavParamExtType::MAV_PARAM_EXT_TYPE_UINT8 => {
            Ok(u8::from_ne_bytes(bytes[0..1].try_into().unwrap()) as i64)
        }
        mavlink::common::MavParamExtType::MAV_PARAM_EXT_TYPE_INT32 => {
            Ok(i32::from_ne_bytes(bytes[0..4].try_into().unwrap()) as i64)
        }
        mavlink::common::MavParamExtType::MAV_PARAM_EXT_TYPE_INT64 => {
            Ok(i64::from_ne_bytes(bytes[0..8].try_into().unwrap()))
        }
        something_else => Err(SimpleError::new(format!(
            "Received parameter of untreatable type: {something_else:#?}",
        ))),
    };
    if let Err(error) = control_value {
        error!("Failed to parse parameter value: {error:#?}");
        return None;
    }
    control_value.ok()
}

fn get_param_index_and_control_id(
    param_ext_req: &mavlink::common::PARAM_EXT_REQUEST_READ_DATA,
    controls: &Vec<crate::video::types::Control>,
) -> Option<(u16, u64)> {
    let param_index = param_ext_req.param_index;
    // Use param_index if it is !=1, otherwise, use param_id. For more information: https://mavlink.io/en/messages/common.html#PARAM_EXT_REQUEST_READ
    let (param_index, control_id) = if param_index == -1 {
        let control_id = match control_id_from_param_id(&param_ext_req.param_id) {
            Some(value) => value,
            None => return None,
        };

        match &controls.iter().position(|control| control_id == control.id) {
            Some(param_index) => (*param_index as i16, control_id),
            None => {
                error!("Failed to find control id {control_id}.");
                return None;
            }
        }
    } else {
        match &controls.get(param_index as usize) {
            Some(control) => (param_index, control.id),
            None => {
                error!("Failed to find control index {param_index}.");
                return None;
            }
        }
    };
    Some((param_index as u16, control_id))
}

fn param_id_from_control_id(id: u64) -> [char; 16] {
    let mut param_id: [char; 16] = Default::default();
    id.to_string()
        .chars()
        .zip(param_id.iter_mut())
        .for_each(|(a, b)| *b = a);
    param_id
}

fn control_id_from_param_id(param_id: &[char; 16]) -> Option<u64> {
    let control_id = param_id
        .iter()
        .collect::<String>()
        .trim_end_matches(char::from(0))
        .parse::<u64>();
    if let Err(error) = control_id {
        error!("Failed to parse control id: {error:#?}");
        return None;
    }
    control_id.ok()
}

#[derive(Debug)]
struct SysInfo {
    time_boot_ms: u32,
    total_capacity: f32,
    used_capacity: f32,
    available_capacity: f32,
}

fn sys_info() -> SysInfo {
    //Both uses KB
    let mut local_total_capacity = 0;
    let mut local_available_capacity = 0;

    match sys_info::disk_info() {
        Ok(disk_info) => {
            local_available_capacity = disk_info.free;
            local_total_capacity = disk_info.total;
        }

        Err(error) => {
            warn!("Failed to fetch disk info: {:#?}", error);
        }
    }

    let boottime_ms = match sys_info::boottime() {
        Ok(bootime) => bootime.tv_usec / 1000,
        Err(error) => {
            warn!("Failed to fetch boottime info: {:#?}", error);
            0
        }
    };

    return SysInfo {
        time_boot_ms: boottime_ms as u32,
        total_capacity: local_total_capacity as f32 / f32::powf(2.0, 10.0),
        used_capacity: ((local_total_capacity - local_available_capacity) as f32)
            / f32::powf(2.0, 10.0),
        available_capacity: local_available_capacity as f32 / f32::powf(2.0, 10.0),
    };
}

//TODO: finish this messages
fn heartbeat_message() -> mavlink::common::MavMessage {
    mavlink::common::MavMessage::HEARTBEAT(mavlink::common::HEARTBEAT_DATA {
        custom_mode: 0,
        mavtype: mavlink::common::MavType::MAV_TYPE_CAMERA,
        autopilot: mavlink::common::MavAutopilot::MAV_AUTOPILOT_INVALID,
        base_mode: mavlink::common::MavModeFlag::empty(),
        system_status: mavlink::common::MavState::MAV_STATE_STANDBY,
        mavlink_version: 0x3,
    })
}

fn camera_information(information: &MavlinkCameraInformation) -> mavlink::common::MavMessage {
    let vendor_name = from_string_to_u8_array_with_size_32(&information.component.vendor_name);
    let model_name = from_string_to_u8_array_with_size_32(&information.component.vendor_name);
    let cam_definition_uri = from_string_to_vec_char_with_defined_size_and_null_terminator(
        &information.cam_definition_uri().unwrap().to_string(),
        140,
    );

    let sys_info = sys_info();

    mavlink::common::MavMessage::CAMERA_INFORMATION(mavlink::common::CAMERA_INFORMATION_DATA {
        time_boot_ms: sys_info.time_boot_ms,
        firmware_version: 0,
        focal_length: 0.0,
        sensor_size_h: 0.0,
        sensor_size_v: 0.0,
        flags: mavlink::common::CameraCapFlags::CAMERA_CAP_FLAGS_HAS_VIDEO_STREAM,
        resolution_h: information.component.resolution_h,
        resolution_v: information.component.resolution_v,
        cam_definition_version: 0,
        vendor_name,
        model_name,
        lens_id: 0,
        cam_definition_uri,
    })
}

fn camera_settings() -> mavlink::common::MavMessage {
    let sys_info = sys_info();

    mavlink::common::MavMessage::CAMERA_SETTINGS(mavlink::common::CAMERA_SETTINGS_DATA {
        time_boot_ms: sys_info.time_boot_ms,
        zoomLevel: 0.0,
        focusLevel: 0.0,
        mode_id: mavlink::common::CameraMode::CAMERA_MODE_VIDEO,
    })
}

fn camera_storage_information() -> mavlink::common::MavMessage {
    let sys_info = sys_info();

    mavlink::common::MavMessage::STORAGE_INFORMATION(mavlink::common::STORAGE_INFORMATION_DATA {
        time_boot_ms: sys_info.time_boot_ms,
        total_capacity: sys_info.total_capacity,
        used_capacity: sys_info.used_capacity,
        available_capacity: sys_info.available_capacity,
        read_speed: 1000.0,
        write_speed: 1000.0,
        storage_id: 0,
        storage_count: 0,
        status: mavlink::common::StorageStatus::STORAGE_STATUS_READY,
    })
}

fn camera_capture_status() -> mavlink::common::MavMessage {
    let sys_info = sys_info();

    mavlink::common::MavMessage::CAMERA_CAPTURE_STATUS(
        mavlink::common::CAMERA_CAPTURE_STATUS_DATA {
            time_boot_ms: sys_info.time_boot_ms,
            image_interval: 0.0,
            recording_time_ms: 0,
            available_capacity: sys_info.available_capacity,
            image_status: 0,
            video_status: 0,
            image_count: 0,
        },
    )
}

fn video_stream_information(information: &MavlinkCameraInformation) -> mavlink::common::MavMessage {
    let name = from_string_to_char_array_with_size_32(&information.video_stream_name);
    let uri = from_string_to_vec_char_with_defined_size_and_null_terminator(
        &information.video_stream_uri.to_string(),
        140,
    );

    //The only important information here is the mavtype and uri variables, everything else is fake
    mavlink::common::MavMessage::VIDEO_STREAM_INFORMATION(
        mavlink::common::VIDEO_STREAM_INFORMATION_DATA {
            framerate: information.component.framerate,
            bitrate: information.component.bitrate,
            flags: get_stream_status_flag(&information.component),
            resolution_h: information.component.resolution_h,
            resolution_v: information.component.resolution_v,
            rotation: information.component.rotation,
            hfov: information.component.hfov,
            stream_id: information.component.stream_id,
            count: 0,
            mavtype: information.mavlink_stream_type,
            name,
            uri,
        },
    )
}

fn from_string_to_u8_array_with_size_32(src: &String) -> [u8; 32] {
    let bytes = src.as_bytes();
    let mut dst = [0u8; 32];
    let len = std::cmp::min(bytes.len(), 32);
    dst[..len].copy_from_slice(&bytes[..len]);
    dst
}

fn from_string_to_char_array_with_size_32(src: &String) -> [char; 32] {
    let chars: Vec<char> = src.chars().collect();
    let mut dst = ['\0'; 32];
    let len = std::cmp::min(chars.len(), 32);
    dst[..len].copy_from_slice(&chars[..len]);
    dst
}

fn from_string_to_vec_char_with_defined_size_and_null_terminator(
    src: &String,
    size: usize,
) -> Vec<char> {
    let mut uri = src.chars().collect::<Vec<char>>();
    uri.truncate(size);
    uri.push('\0');
    uri
}

fn get_stream_status_flag(
    component: &MavlinkCameraComponent,
) -> mavlink::common::VideoStreamStatusFlags {
    match component.thermal {
        true => mavlink::common::VideoStreamStatusFlags::VIDEO_STREAM_STATUS_FLAGS_THERMAL,
        false => mavlink::common::VideoStreamStatusFlags::VIDEO_STREAM_STATUS_FLAGS_RUNNING,
    }
}
