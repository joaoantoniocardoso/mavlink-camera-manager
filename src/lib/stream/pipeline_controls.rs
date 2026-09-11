use std::panic::{AssertUnwindSafe, catch_unwind};

use crate::{
    controls::{
        gst_element_controls::{
            PIPELINE_CONTROL_ID_OFFSET, enum_value_by_nick, float_to_api, list_encoder_controls,
            pipeline_control_id_for_element_property, set_property_from_api,
        },
        types::{Control, ControlBool, ControlMenu, ControlState, ControlType},
    },
    stream::{
        Stream,
        types::{CaptureConfiguration, PropertyValue, SourceConfiguration},
    },
    video_stream::types::VideoAndStreamInformation,
};
use anyhow::{Context, Result, anyhow};
use glib::prelude::*;
use gst::prelude::*;

const SYNTHETIC_ELEMENT: &str = "stream";
const AUTO_RESTART_ON_CONFIG_CHANGE: &str = "auto-restart-on-config-change";
const RESTART_STREAM: &str = "restart-stream";

pub fn auto_restart_control_id() -> u64 {
    pipeline_control_id_for_element_property(SYNTHETIC_ELEMENT, AUTO_RESTART_ON_CONFIG_CHANGE)
}

pub fn restart_stream_control_id() -> u64 {
    pipeline_control_id_for_element_property(SYNTHETIC_ELEMENT, RESTART_STREAM)
}

pub fn is_pipeline_control_id(control_id: u64) -> bool {
    control_id >= PIPELINE_CONTROL_ID_OFFSET
}

pub fn list_pipeline_controls(
    stream: &Stream,
    video_and_stream_information: &VideoAndStreamInformation,
) -> Vec<Control> {
    let encoder_controls = list_encoder_controls_for_stream(stream, video_and_stream_information);
    let synthetic_controls = list_synthetic_controls(video_and_stream_information);
    encoder_controls
        .into_iter()
        .chain(synthetic_controls)
        .collect()
}

pub fn pipeline_controls_for_mavlink(
    stream: &Stream,
    video_and_stream_information: &VideoAndStreamInformation,
) -> Vec<Control> {
    list_pipeline_controls(stream, video_and_stream_information)
        .into_iter()
        .filter(|control| control.cpp_type != "string")
        .collect()
}

pub fn pipeline_control_value_by_id(
    stream: &Stream,
    video_and_stream_information: &VideoAndStreamInformation,
    control_id: u64,
) -> std::io::Result<i64> {
    let Some(control) = find_pipeline_control(stream, video_and_stream_information, control_id)
    else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("Pipeline control {control_id} not found"),
        ));
    };
    Ok(control_current_value(&control.configuration))
}

/// Returns true when the caller must restart the stream after writing this
/// configuration back to the live `Stream` (so rebuild sees the new values).
pub fn set_pipeline_control(
    stream: &Stream,
    video_and_stream_information: &mut VideoAndStreamInformation,
    control_id: u64,
    value: i64,
) -> Result<bool> {
    if control_id == restart_stream_control_id() {
        return Ok(value != 0);
    }

    if control_id == auto_restart_control_id() {
        set_auto_restart_on_config_change(video_and_stream_information, value != 0);
        return Ok(false);
    }

    let control = find_pipeline_control(stream, video_and_stream_information, control_id)
        .context("Pipeline encoder control not found")?;

    if control.mutable_in_playing {
        if let Some(encoder) = try_live_encoder_element(stream) {
            set_property_from_api(&encoder, &control.name, value).map_err(|error| {
                anyhow!(
                    "Failed setting live encoder property {control_name}: {error}",
                    control_name = control.name
                )
            })?;
        }
    }

    let encoder_factory = encoder_factory_name(video_and_stream_information);
    let probe_element = gst::ElementFactory::make(&encoder_factory).build().ok();
    let property_value = api_value_to_property_value(&control, value, probe_element.as_ref());
    update_encoder_property(video_and_stream_information, &control.name, property_value)?;

    if control.requires_restart {
        if auto_restart_on_config_change(video_and_stream_information) {
            return Ok(true);
        }
        stream.set_restart_needed(true);
    }

    Ok(false)
}

pub fn reset_pipeline_controls(
    stream: &Stream,
    video_and_stream_information: &mut VideoAndStreamInformation,
) {
    if let CaptureConfiguration::Video(video_configuration) = &mut video_and_stream_information
        .stream_information
        .configuration
    {
        if let SourceConfiguration::ManualTranscoding(manual_config) =
            &mut video_configuration.source_configuration
        {
            manual_config.encoder_properties.clear();
        }
        video_configuration.auto_restart_on_config_change = false;
    }

    stream.set_restart_needed(true);
}

fn list_encoder_controls_for_stream(
    stream: &Stream,
    video_and_stream_information: &VideoAndStreamInformation,
) -> Vec<Control> {
    let CaptureConfiguration::Video(video_configuration) = &video_and_stream_information
        .stream_information
        .configuration
    else {
        return vec![];
    };

    let SourceConfiguration::ManualTranscoding(manual_config) =
        &video_configuration.source_configuration
    else {
        return vec![];
    };

    let factory_name = encoder_factory_name(video_and_stream_information);
    let probe_element = gst::ElementFactory::make(&factory_name).build().ok();
    let mut controls = list_encoder_controls(&factory_name);
    overlay_encoder_properties(
        &mut controls,
        &manual_config.encoder_properties,
        probe_element.as_ref(),
    );
    if let Some(encoder) = try_live_encoder_element(stream) {
        overlay_live_values(&mut controls, &encoder);
    }
    controls
}

fn list_synthetic_controls(
    video_and_stream_information: &VideoAndStreamInformation,
) -> Vec<Control> {
    let auto_restart = auto_restart_on_config_change(video_and_stream_information);
    vec![
        synthetic_bool_control(
            AUTO_RESTART_ON_CONFIG_CHANGE,
            auto_restart_control_id(),
            auto_restart,
            false,
        ),
        synthetic_bool_control(RESTART_STREAM, restart_stream_control_id(), false, true),
    ]
}

fn synthetic_bool_control(name: &str, id: u64, value: bool, write_only: bool) -> Control {
    let value = i64::from(value);
    Control {
        name: name.to_string(),
        element: SYNTHETIC_ELEMENT.to_string(),
        cpp_type: "bool".to_string(),
        id,
        state: ControlState::default(),
        configuration: ControlType::Bool(ControlBool {
            default: 0,
            value: if write_only { 0 } else { value },
        }),
        mutable_in_playing: true,
        requires_restart: false,
    }
}

fn find_pipeline_control(
    stream: &Stream,
    video_and_stream_information: &VideoAndStreamInformation,
    control_id: u64,
) -> Option<Control> {
    list_pipeline_controls(stream, video_and_stream_information)
        .into_iter()
        .find(|control| control.id == control_id)
}

fn encoder_factory_name(video_and_stream_information: &VideoAndStreamInformation) -> String {
    let CaptureConfiguration::Video(video_configuration) = &video_and_stream_information
        .stream_information
        .configuration
    else {
        return "x264enc".to_string();
    };

    match &video_configuration.source_configuration {
        SourceConfiguration::ManualTranscoding(manual_config) => {
            if manual_config.encoder.is_empty() {
                "x264enc".to_string()
            } else {
                manual_config.encoder.clone()
            }
        }
        _ => "x264enc".to_string(),
    }
}

fn auto_restart_on_config_change(video_and_stream_information: &VideoAndStreamInformation) -> bool {
    let CaptureConfiguration::Video(video_configuration) = &video_and_stream_information
        .stream_information
        .configuration
    else {
        return false;
    };
    video_configuration.auto_restart_on_config_change
}

fn set_auto_restart_on_config_change(
    video_and_stream_information: &mut VideoAndStreamInformation,
    value: bool,
) {
    if let CaptureConfiguration::Video(video_configuration) = &mut video_and_stream_information
        .stream_information
        .configuration
    {
        video_configuration.auto_restart_on_config_change = value;
    }
}

fn update_encoder_property(
    video_and_stream_information: &mut VideoAndStreamInformation,
    property_name: &str,
    property_value: PropertyValue,
) -> Result<()> {
    let CaptureConfiguration::Video(video_configuration) = &mut video_and_stream_information
        .stream_information
        .configuration
    else {
        return Err(anyhow!("Stream configuration is not video capture"));
    };

    let SourceConfiguration::ManualTranscoding(manual_config) =
        &mut video_configuration.source_configuration
    else {
        return Err(anyhow!("Stream is not using manual transcoding"));
    };

    manual_config
        .encoder_properties
        .insert(property_name.to_string(), property_value);
    Ok(())
}

fn overlay_encoder_properties(
    controls: &mut [Control],
    encoder_properties: &std::collections::BTreeMap<String, PropertyValue>,
    probe_element: Option<&gst::Element>,
) {
    for control in controls {
        let Some(property_value) = encoder_properties.get(&control.name) else {
            continue;
        };
        let api_value = property_value_to_api(property_value, control, probe_element);
        set_control_value(control, api_value);
    }
}

fn overlay_live_values(controls: &mut [Control], encoder: &gst::Element) {
    for control in controls {
        if let Some(value) = read_property_as_api(encoder, &control.name) {
            set_control_value(control, value);
        }
    }
}

fn property_value_to_api(
    property_value: &PropertyValue,
    control: &Control,
    probe_element: Option<&gst::Element>,
) -> i64 {
    match property_value {
        PropertyValue::Bool(boolean) => i64::from(*boolean),
        PropertyValue::Integer(integer) => *integer,
        PropertyValue::Number(number) => float_to_api(*number),
        PropertyValue::String(string) => probe_element
            .and_then(|element| enum_value_by_nick(element, &control.name, string))
            .unwrap_or(0),
    }
}

fn api_value_to_property_value(
    control: &Control,
    value: i64,
    probe_element: Option<&gst::Element>,
) -> PropertyValue {
    match &control.configuration {
        ControlType::Bool(_) => PropertyValue::Bool(value != 0),
        ControlType::Menu(ControlMenu { options, .. }) => {
            if let Some(option) = options.iter().find(|option| option.value == value) {
                PropertyValue::String(option.name.clone())
            } else if let Some(element) = probe_element
                && let Some(param_spec) = element.find_property(&control.name)
                && let Some(enum_class) = glib::EnumClass::with_type(param_spec.value_type())
                && let Ok(enum_int) = i32::try_from(value)
                && let Some(enum_value) = enum_class
                    .values()
                    .iter()
                    .find(|enum_value| enum_value.value() == enum_int)
            {
                PropertyValue::String(enum_value.nick().to_string())
            } else {
                PropertyValue::Integer(value)
            }
        }
        ControlType::Slider(_) | ControlType::Flags(_) => PropertyValue::Integer(value),
    }
}

fn set_control_value(control: &mut Control, value: i64) {
    match &mut control.configuration {
        ControlType::Bool(bool_control) => bool_control.value = value,
        ControlType::Slider(slider) => slider.value = value,
        ControlType::Menu(menu) => menu.value = value,
        ControlType::Flags(flags) => flags.value = value,
    }
}

fn control_current_value(configuration: &ControlType) -> i64 {
    match configuration {
        ControlType::Bool(control) => control.value,
        ControlType::Slider(control) => control.value,
        ControlType::Menu(control) => control.value,
        ControlType::Flags(control) => control.value,
    }
}

fn try_live_encoder_element(stream: &Stream) -> Option<gst::Element> {
    let state_guard = stream.state.try_read().ok()?;
    let state = state_guard.as_ref()?;
    let pipeline = state.pipeline.as_ref()?;
    pipeline.inner_state_as_ref().pipeline.by_name("encoder")
}

fn read_property_as_api(element: &gst::Element, property: &str) -> Option<i64> {
    catch_unwind(AssertUnwindSafe(|| {
        let param_spec = element.find_property(property)?;
        let value = element.property_value(property);
        if glib::EnumClass::with_type(param_spec.value_type()).is_some() {
            return value.get::<i32>().ok().map(i64::from);
        }

        let value_type = param_spec.value_type();
        if value_type == bool::static_type() {
            return value.get::<bool>().ok().map(|boolean| i64::from(boolean));
        }
        if value_type == i32::static_type() {
            return value.get::<i32>().ok().map(i64::from);
        }
        if value_type == u32::static_type() {
            return value.get::<u32>().ok().map(|unsigned| i64::from(unsigned));
        }
        if value_type == i64::static_type() {
            return value.get::<i64>().ok();
        }
        if value_type == u64::static_type() {
            return value
                .get::<u64>()
                .ok()
                .and_then(|unsigned| i64::try_from(unsigned).ok());
        }
        if value_type == f32::static_type() {
            return value
                .get::<f32>()
                .ok()
                .map(|float| float_to_api(f64::from(float)));
        }
        if value_type == f64::static_type() {
            return value.get::<f64>().ok().map(float_to_api);
        }
        None
    }))
    .ok()
    .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::controls::gst_element_controls::PIPELINE_CONTROL_ID_OFFSET;

    #[test]
    fn pipeline_control_ids_are_in_pipeline_namespace() {
        let bitrate_id = pipeline_control_id_for_element_property("x264enc", "bitrate");
        assert!(bitrate_id >= PIPELINE_CONTROL_ID_OFFSET);
    }

    #[test]
    fn synthetic_control_ids_are_in_pipeline_namespace() {
        assert!(auto_restart_control_id() >= PIPELINE_CONTROL_ID_OFFSET);
        assert!(restart_stream_control_id() >= PIPELINE_CONTROL_ID_OFFSET);
        assert_ne!(auto_restart_control_id(), restart_stream_control_id());
    }
}
