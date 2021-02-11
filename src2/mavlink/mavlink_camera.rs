use std::sync::{Arc, Mutex};

#[derive(Debug)]
pub struct MavlinkCameraComponent {
    // MAVLink specific information
    system_id: u8,
    component_id: u8,

    vendor_name: String,
    model_name: String,
    firmware_version: u32,
    resolution_h: f32,
    resolution_v: f32,
}

pub struct MavlinkCameraInformation {
    component: MavlinkCameraComponent,
    mavlink_connection_string: String,
    video_stream_uri: String,
    vehicle: Arc<Box<dyn mavlink::MavConnection<mavlink::common::MavMessage> + Sync + Send>>,
}

#[derive(PartialEq)]
enum ThreadState {
    DEAD,
    RUNNING,
    ZOMBIE,
    RESTART,
}

pub struct MavlinkCameraHandle {
    mavlink_camera_information: Arc<Mutex<MavlinkCameraInformation>>,
    heartbeat_state: Arc<Mutex<ThreadState>>,
    heartbeat_thread: std::thread::JoinHandle<()>,
    receive_message_state: Arc<Mutex<ThreadState>>, //TODO: unify states
    receive_message_thread: std::thread::JoinHandle<()>,
}

impl Default for MavlinkCameraComponent {
    fn default() -> Self {
        Self {
            system_id: 1,
            component_id: mavlink::common::MavComponent::MAV_COMP_ID_CAMERA as u8,

            vendor_name: Default::default(),
            model_name: Default::default(),
            firmware_version: 0,
            resolution_h: 0.0,
            resolution_v: 0.0,
        }
    }
}

impl MavlinkCameraInformation {
    fn new(mavlink_connection_string: &'static str) -> Self {
        Self {
            component: Default::default(),
            mavlink_connection_string: mavlink_connection_string.into(),
            video_stream_uri:
                "rtsp://wowzaec2demo.streamlock.net:554/vod/mp4:BigBuckBunny_115k.mov".into(),
            vehicle: Arc::new(mavlink::connect(&mavlink_connection_string).unwrap()),
        }
    }
}

impl MavlinkCameraHandle {
    pub fn new() -> Self {
        let mavlink_camera_information: Arc<Mutex<MavlinkCameraInformation>> = Arc::new(
            Mutex::new(MavlinkCameraInformation::new("udpout:0.0.0.0:14550")),
        );

        let heartbeat_state = Arc::new(Mutex::new(ThreadState::RUNNING));
        let receive_message_state = Arc::new(Mutex::new(ThreadState::ZOMBIE));

        let heartbeat_mavlink_information = mavlink_camera_information.clone();
        let receive_message_mavlink_information = mavlink_camera_information.clone();

        Self {
            mavlink_camera_information: mavlink_camera_information.clone(),
            heartbeat_state: heartbeat_state.clone(),
            heartbeat_thread: std::thread::spawn(move || {
                heartbeat_loop(heartbeat_state.clone(), heartbeat_mavlink_information)
            }),
            receive_message_state: receive_message_state.clone(),
            receive_message_thread: std::thread::spawn(move || {
                receive_message_loop(
                    receive_message_state.clone(),
                    receive_message_mavlink_information,
                )
            }),
        }
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

        let mut heartbeat_state = atomic_thread_state.as_ref().lock().unwrap();
        if *heartbeat_state == ThreadState::ZOMBIE {
            continue;
        }
        if *heartbeat_state == ThreadState::DEAD {
            break;
        }

        if *heartbeat_state == ThreadState::RESTART {
            *heartbeat_state = ThreadState::RUNNING;
            drop(heartbeat_state);

            std::thread::sleep(std::time::Duration::from_secs(3));
            continue;
        }

        println!("send!");
        if let Err(error) = vehicle.as_ref().send(&header, &heartbeat_message()) {
            eprintln!("Failed to send heartbeat: {:?}", error);
        }
    }
}

fn receive_message_loop(
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
        let vehicle = vehicle.as_ref();
        match vehicle.recv() {
            Ok((_header, msg)) => {
                match msg {
                    // Check if there is any camera information request from gcs
                    mavlink::common::MavMessage::COMMAND_LONG(command_long) => {
                        match command_long.command {
                            mavlink::common::MavCmd::MAV_CMD_REQUEST_CAMERA_INFORMATION => {
                                println!("Sending camera_information..");
                                if let Err(error) = vehicle.send(&header, &camera_information()) {
                                    println!("Failed to send camera_information: {:?}", error);
                                }
                            }
                            mavlink::common::MavCmd::MAV_CMD_REQUEST_CAMERA_SETTINGS => {
                                println!("Sending camera_settings..");
                                if let Err(error) = vehicle.send(&header, &camera_settings()) {
                                    println!("Failed to send camera_settings: {:?}", error);
                                }
                            }
                            mavlink::common::MavCmd::MAV_CMD_REQUEST_STORAGE_INFORMATION => {
                                println!("Sending camera_storage_information..");
                                if let Err(error) =
                                    vehicle.send(&header, &camera_storage_information())
                                {
                                    println!(
                                        "Failed to send camera_storage_information: {:?}",
                                        error
                                    );
                                }
                            }
                            mavlink::common::MavCmd::MAV_CMD_REQUEST_CAMERA_CAPTURE_STATUS => {
                                println!("Sending camera_capture_status..");
                                if let Err(error) = vehicle.send(&header, &camera_capture_status())
                                {
                                    println!("Failed to send camera_capture_status: {:?}", error);
                                }
                            }
                            mavlink::common::MavCmd::MAV_CMD_REQUEST_VIDEO_STREAM_INFORMATION => {
                                println!("Sending video_stream_information..");
                                if let Err(error) =
                                    vehicle.send(&header, &video_stream_information())
                                {
                                    println!(
                                        "Failed to send video_stream_information: {:?}",
                                        error
                                    );
                                }
                            }
                            _ => {
                                //TODO: reworking this prints
                                println!("Ignoring command: {:?}", command_long.command);
                            }
                        }
                    }
                    // We receive a bunch of heartbeat messages, we can ignore it
                    mavlink::common::MavMessage::HEARTBEAT(_) => {}
                    // Any other message that is not a heartbeat or command_long
                    _ => {
                        //TODO: reworking this prints
                        println!("Ignoring: {:?}", msg);
                    }
                }
            }
            Err(e) => {
                println!("recv error: {:?}", e);
            }
        }
    }
}

fn heartbeat_message() -> mavlink::common::MavMessage {
    mavlink::common::MavMessage::HEARTBEAT(mavlink::common::HEARTBEAT_DATA {
        custom_mode: 0,
        mavtype: mavlink::common::MavType::MAV_TYPE_CAMERA,
        autopilot: mavlink::common::MavAutopilot::MAV_AUTOPILOT_GENERIC,
        base_mode: mavlink::common::MavModeFlag::empty(),
        system_status: mavlink::common::MavState::MAV_STATE_STANDBY,
        mavlink_version: 0x3,
    })
}

fn camera_information() -> mavlink::common::MavMessage {
    // Create a fixed size array with the camera name
    let name_str = String::from("name");
    let mut name: [u8; 32] = [0; 32];
    for index in 0..name_str.len() as usize {
        name[index] = name_str.as_bytes()[index];
    }

    // Send path to our camera configuration file
    let uri: Vec<char> = format!("{}", "http://0.0.0.0").chars().collect();

    // Send fake data
    mavlink::common::MavMessage::CAMERA_INFORMATION(mavlink::common::CAMERA_INFORMATION_DATA {
        time_boot_ms: 0,
        firmware_version: 0,
        focal_length: 0.0,
        sensor_size_h: 0.0,
        sensor_size_v: 0.0,
        flags: mavlink::common::CameraCapFlags::CAMERA_CAP_FLAGS_HAS_VIDEO_STREAM,
        resolution_h: 0,
        resolution_v: 0,
        cam_definition_version: 0,
        vendor_name: name,
        model_name: name,
        lens_id: 0,
        cam_definition_uri: uri,
    })
}

fn camera_settings() -> mavlink::common::MavMessage {
    //Send fake data
    mavlink::common::MavMessage::CAMERA_SETTINGS(mavlink::common::CAMERA_SETTINGS_DATA {
        time_boot_ms: 0,
        zoomLevel: 0.0,
        focusLevel: 0.0,
        mode_id: mavlink::common::CameraMode::CAMERA_MODE_VIDEO,
    })
}

fn camera_storage_information() -> mavlink::common::MavMessage {
    //Send fake data
    mavlink::common::MavMessage::STORAGE_INFORMATION(mavlink::common::STORAGE_INFORMATION_DATA {
        time_boot_ms: 0,
        total_capacity: 102400.0,
        used_capacity: 0.0,
        available_capacity: 102400.0,
        read_speed: 1000.0,
        write_speed: 1000.0,
        storage_id: 0,
        storage_count: 0,
        status: mavlink::common::StorageStatus::STORAGE_STATUS_READY,
    })
}

fn camera_capture_status() -> mavlink::common::MavMessage {
    //Send fake data
    mavlink::common::MavMessage::CAMERA_CAPTURE_STATUS(
        mavlink::common::CAMERA_CAPTURE_STATUS_DATA {
            time_boot_ms: 0,
            image_interval: 0.0,
            recording_time_ms: 0,
            available_capacity: 10000.0,
            image_status: 0,
            video_status: 0,
            image_count: 0,
        },
    )
}

fn video_stream_information() -> mavlink::common::MavMessage {
    let name_str = String::from("name");
    let mut name: [char; 32] = ['\0'; 32];
    for index in 0..name_str.len() as u32 {
        name[index as usize] = name_str.as_bytes()[index as usize] as char;
    }

    let uri: Vec<char> = format!("{}\0", "udp:0.0.0.0:5601").chars().collect();

    //The only important information here is the mavtype and uri variables, everything else is fake
    mavlink::common::MavMessage::VIDEO_STREAM_INFORMATION(
        mavlink::common::VIDEO_STREAM_INFORMATION_DATA {
            framerate: 30.0,
            bitrate: 1000,
            flags: mavlink::common::VideoStreamStatusFlags::VIDEO_STREAM_STATUS_FLAGS_RUNNING,
            resolution_h: 1000,
            resolution_v: 1000,
            rotation: 0,
            hfov: 0,
            stream_id: 1, // Starts at 1, 0 is for broadcast
            count: 0,
            mavtype: mavlink::common::VideoStreamType::VIDEO_STREAM_TYPE_RTSP,
            name: name,
            uri: uri,
        },
    )
}