//! Camera controls for libcamera sources via GStreamer `libcamerasrc` GObject properties.
//!
//! MCM's control API is V4L-shaped (`i64` values). Float properties are exposed as
//! milli-units (`value * 1000`) so they fit that API without changing clients.
//!
//! Listing is generic: every `GST_PARAM_CONTROLLABLE` read-write property whose
//! GObject type we can map (bool, enum, integer, float). Names, types, enum
//! nicks, and min/max/default come from the ParamSpec — not a control table.
//! Array/boxed/string properties are skipped because the `i64` API cannot
//! represent them. When a numeric ParamSpec is type-wide (`±G_MAXFLOAT` /
//! `G_MININT`), slider limits and defaults are overlaid from Raspberry Pi IPA
//! `ControlInfo` in `libcamera/src/ipa/rpi/common/ipa_base.cpp`.
//!
//! `libcamerasrc` copies GObject sets into the next `Request` from the
//! streaming thread (`applyControls` in `queueRequest`). HTTP set/get never
//! touches live GObject values: a src-pad BUFFER probe applies dirty pending
//! on the streaming thread after the current `queueRequest`, so the next
//! request carries the control. Pipeline start also applies pending.
//! `{property}-mode=manual` is set when that GObject property exists, so Auto
//! IPA does not ignore the matching value.
//!
//! Listed *current* values prefer a pending set from this process, then the
//! ParamSpec / IPA default. Specs (names, ranges) are cached per device.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Mutex, MutexGuard, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
};

use glib::prelude::*;
use gst::prelude::*;
use tracing::*;

use crate::{
    controls::types::{
        Control, ControlBool, ControlMenu, ControlOption, ControlSlider, ControlState, ControlType,
    },
    stream::manager::{LiveSourceLookup, try_any_live_libcamerasrc, try_live_source_element},
};

const FLOAT_SCALE: f64 = 1000.0;
/// MAVLink `param_id` decimal encoding only round-trips ≤8 digits (see `mavlink::utils`).
const CONTROL_ID_SPACE: u64 = 100_000_000;
/// Treat ParamSpec min/max as hollow when either side is this large (e.g. ±FLT_MAX).
const HOLLOW_BOUND: f64 = 1_000_000.0;

static PENDING: OnceLock<Mutex<HashMap<String, BTreeMap<String, i64>>>> = OnceLock::new();
static DIRTY_CAMERAS: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
static ANY_DIRTY: AtomicBool = AtomicBool::new(false);
static SHARED_PROBE: OnceLock<Mutex<Option<gst::Element>>> = OnceLock::new();
static LISTED_CONTROLS: OnceLock<Mutex<HashMap<String, Vec<Control>>>> = OnceLock::new();

struct SliderControlSpec<'a> {
    name: &'a str,
    id: u64,
    state: ControlState,
    min: i64,
    max: i64,
    default: i64,
    cpp_type: &'a str,
}

/// Raspberry Pi IPA `ControlInfo` slider limits, in MCM API units.
///
/// Copied from `ipa_base.cpp` (`ipaControls` / `ipaColourControls` / `ipaAfControls`).
/// Floats are milli-units ([`FLOAT_SCALE`]). Used only when the GObject ParamSpec
/// range is type-wide.
struct IpaSliderLimits {
    min: i64,
    max: i64,
    default: i64,
}

impl IpaSliderLimits {
    fn from_float(min: f64, max: f64, default: f64) -> Self {
        Self {
            min: float_to_api(min),
            max: float_to_api(max),
            default: float_to_api(default),
        }
    }
}

/// Stable control id from a `libcamerasrc` property name.
///
/// IDs are capped below [`CONTROL_ID_SPACE`] so they MAVLink-round-trip through
/// decimal `param_id` encoding (≤8 decimal digits).
#[instrument(level = "debug")]
pub fn control_id_for_property(name: &str) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in name.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash % CONTROL_ID_SPACE
}

/// Apply any pending control values to a freshly configured `libcamerasrc`.
///
/// `*-mode` / `*-enable` properties are applied before the rest so gated value
/// properties take effect. Safe before PLAYING (streaming thread is not running).
#[instrument(level = "debug", skip(element))]
pub fn apply_pending_to_element(camera_name: &str, element: &gst::Element) {
    let pending = recoverable_lock(pending_controls());
    let Some(values) = pending.get(camera_name).cloned() else {
        return;
    };
    drop(pending);
    clear_dirty(camera_name);
    apply_values_to_element(camera_name, element, &values);
}

/// Apply dirty pending controls from the `libcamerasrc` streaming thread.
///
/// No-op when nothing is dirty. Must not be called from the HTTP thread.
#[instrument(level = "debug", skip(element))]
pub fn apply_dirty_pending_to_element(camera_name: &str, element: &gst::Element) {
    if !ANY_DIRTY.load(Ordering::Acquire) {
        return;
    }
    let Some(values) = take_dirty_pending(camera_name) else {
        return;
    };
    apply_values_to_element(camera_name, element, &values);
}

/// Install a src-pad probe that applies dirty pending controls on the streaming thread.
#[instrument(level = "debug", skip(element))]
pub fn install_live_apply_probe(element: &gst::Element, camera_name: &str) {
    let Some(src_pad) = element.static_pad("src") else {
        warn!(
            "libcamerasrc has no src pad; live control apply probe not installed for {camera_name:?}"
        );
        return;
    };
    let camera_name = camera_name.to_string();
    src_pad.add_probe(gst::PadProbeType::BUFFER, move |pad, _probe_info| {
        if let Some(parent) = pad.parent()
            && let Ok(element) = parent.downcast::<gst::Element>()
        {
            apply_dirty_pending_to_element(&camera_name, &element);
        }
        gst::PadProbeReturn::Ok
    });
}

/// Cache a `libcamerasrc` property by name and mark it dirty for the streaming thread.
///
/// Live apply is the src-pad probe, not a lookup of the live element at SET time.
#[instrument(level = "debug")]
pub fn set_control_by_name(camera_name: &str, property: &str, value: i64) -> std::io::Result<()> {
    store_pending(camera_name, property, value);
    if let Ok((element, _)) = control_element(camera_name) {
        store_companion_mode(camera_name, property, &element);
        store_related_agc_controls(camera_name, property, value, &element);
    } else {
        warn!(
            "Queued libcamera control {property}={value} for {camera_name:?} without companion (no control element)"
        );
    }
    mark_dirty(camera_name);
    debug!("Queued libcamera control {property}={value} for {camera_name:?}");
    Ok(())
}

/// Reset every listed control to its listed default (ParamSpec, or IPA overlay).
#[instrument(level = "debug")]
pub fn reset_controls(camera_name: &str) -> Result<(), Vec<std::io::Error>> {
    for control in list_controls(camera_name) {
        if control.state.is_inactive {
            continue;
        }
        store_pending(
            camera_name,
            &control.name,
            control_default(&control.configuration),
        );
    }
    mark_dirty(camera_name);
    Ok(())
}

/// List controls for `camera_name`.
///
/// Float properties are exposed as milli-units in the `i64` slider API. On failure
/// to create/inspect `libcamerasrc`, returns an empty list (and logs a warning).
#[instrument(level = "debug")]
pub fn list_controls(camera_name: &str) -> Vec<Control> {
    if let Some(cached) = cached_listed_controls(camera_name) {
        return overlay_current_values(camera_name, cached);
    }
    let (element, _is_live) = match control_element(camera_name) {
        Ok(pair) => pair,
        Err(error) => {
            warn!("Failed listing libcamera controls for {camera_name:?}: {error}");
            return vec![];
        }
    };
    let listed = list_controls_on_element(camera_name, &element);
    if !listed.is_empty() {
        store_listed_controls(camera_name, listed.clone());
    }
    overlay_current_values(camera_name, listed)
}

/// Read the current API value for `control_id`.
///
/// Prefers the pending cache, then the listed default.
#[instrument(level = "debug")]
pub fn control_value_by_id(camera_name: &str, control_id: u64) -> std::io::Result<i64> {
    let Some(control) = find_control(camera_name, control_id) else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("Control ID {control_id} was not found for libcamera device {camera_name:?}"),
        ));
    };
    Ok(control_current_value(&control.configuration))
}

/// Find a single control by id, or `None` if missing / enumeration failed.
#[instrument(level = "debug")]
pub fn find_control(camera_name: &str, control_id: u64) -> Option<Control> {
    list_controls(camera_name)
        .into_iter()
        .find(|control| control.id == control_id)
}

fn pending_controls() -> &'static Mutex<HashMap<String, BTreeMap<String, i64>>> {
    PENDING.get_or_init(|| Mutex::new(HashMap::new()))
}

fn dirty_cameras() -> &'static Mutex<HashSet<String>> {
    DIRTY_CAMERAS.get_or_init(|| Mutex::new(HashSet::new()))
}

fn listed_controls() -> &'static Mutex<HashMap<String, Vec<Control>>> {
    LISTED_CONTROLS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn recoverable_lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn mark_dirty(camera_name: &str) {
    recoverable_lock(dirty_cameras()).insert(camera_name.to_string());
    ANY_DIRTY.store(true, Ordering::Release);
}

fn clear_dirty(camera_name: &str) {
    let mut dirty = recoverable_lock(dirty_cameras());
    dirty.remove(camera_name);
    if dirty.is_empty() {
        ANY_DIRTY.store(false, Ordering::Release);
    }
}

fn take_dirty_pending(camera_name: &str) -> Option<BTreeMap<String, i64>> {
    let mut dirty = recoverable_lock(dirty_cameras());
    if !dirty.remove(camera_name) {
        return None;
    }
    if dirty.is_empty() {
        ANY_DIRTY.store(false, Ordering::Release);
    }
    drop(dirty);
    recoverable_lock(pending_controls())
        .get(camera_name)
        .cloned()
}

fn cached_listed_controls(camera_name: &str) -> Option<Vec<Control>> {
    recoverable_lock(listed_controls())
        .get(camera_name)
        .cloned()
}

fn store_listed_controls(camera_name: &str, controls: Vec<Control>) {
    recoverable_lock(listed_controls()).insert(camera_name.to_string(), controls);
}

fn overlay_current_values(camera_name: &str, mut controls: Vec<Control>) -> Vec<Control> {
    for control in &mut controls {
        let default = control_default(&control.configuration);
        let value = property_current_api_value(camera_name, &control.name, default);
        match &mut control.configuration {
            ControlType::Bool(bool_control) => bool_control.value = value,
            ControlType::Slider(slider) => slider.value = value,
            ControlType::Menu(menu) => menu.value = value,
        }
    }
    controls
}

fn apply_values_to_element(
    camera_name: &str,
    element: &gst::Element,
    values: &BTreeMap<String, i64>,
) {
    let mode_like: Vec<&String> = values
        .keys()
        .filter(|name| is_mode_like_property(name))
        .collect();
    let others: Vec<&String> = values
        .keys()
        .filter(|name| !is_mode_like_property(name))
        .collect();

    for property in mode_like.into_iter().chain(others) {
        let value = values[property];
        if let Err(error) = set_property_from_api(element, property, value) {
            warn!(
                "Failed applying libcamera control {property:?}={value} on {camera_name:?}: {error}"
            );
        } else {
            debug!("Applied libcamera control {property:?}={value} on {camera_name:?}");
        }
    }
}

fn resolve_control_id(name: &str, used: &mut BTreeSet<u64>) -> u64 {
    let base = control_id_for_property(name);
    let mut id = base;
    while used.contains(&id) {
        id = (id + 1) % CONTROL_ID_SPACE;
        if id == base {
            break;
        }
    }
    used.insert(id);
    id
}

fn is_mode_like_property(name: &str) -> bool {
    name.ends_with("-mode") || name.ends_with("-enable")
}

/// `{property}-mode` when `property` itself is not already a mode/enable gate.
fn companion_mode_property(property: &str) -> Option<String> {
    if is_mode_like_property(property) {
        return None;
    }
    Some(format!("{property}-mode"))
}

fn enum_value_by_nick(element: &gst::Element, property: &str, nick: &str) -> Option<i64> {
    let param_spec = element.find_property(property)?;
    let enum_class = glib::EnumClass::with_type(param_spec.value_type())?;
    enum_class
        .values()
        .iter()
        .find(|enum_value| enum_value.nick().eq_ignore_ascii_case(nick))
        .map(|enum_value| i64::from(enum_value.value()))
}

/// If `{property}-mode` exists, cache its `manual` enumerator so the value is not ignored.
fn store_companion_mode(camera_name: &str, property: &str, element: &gst::Element) {
    let Some(mode_property) = companion_mode_property(property) else {
        return;
    };
    store_enum_nick(camera_name, element, &mode_property, "manual");
}

/// AGC axes compensate each other. Freeze gain when setting shutter, and leave
/// gain in Auto when setting EV so `ExposureValue` still has an AE axis.
///
/// `AeEnable` is a wrapper that patches both modes (`camera.cpp` `patchControlList`).
fn store_related_agc_controls(
    camera_name: &str,
    property: &str,
    value: i64,
    element: &gst::Element,
) {
    if let Some(nick) = analogue_gain_mode_nick_for(property) {
        store_enum_nick(camera_name, element, "analogue-gain-mode", nick);
    }
    if property == "exposure-time" {
        store_pending_if_present(camera_name, element, "ae-enable", 0);
    }
    if property == "exposure-time-mode"
        && enum_value_by_nick(element, property, "manual") == Some(value)
    {
        store_enum_nick(camera_name, element, "analogue-gain-mode", "manual");
        store_pending_if_present(camera_name, element, "ae-enable", 0);
    }
}

fn analogue_gain_mode_nick_for(property: &str) -> Option<&'static str> {
    match property {
        "exposure-time" => Some("manual"),
        "exposure-value" => Some("auto"),
        _ => None,
    }
}

fn store_enum_nick(camera_name: &str, element: &gst::Element, property: &str, nick: &str) {
    let Some(value) = enum_value_by_nick(element, property, nick) else {
        return;
    };
    store_pending(camera_name, property, value);
}

fn store_pending_if_present(camera_name: &str, element: &gst::Element, property: &str, value: i64) {
    if element.find_property(property).is_none() {
        return;
    }
    store_pending(camera_name, property, value);
}

fn pspec_range_is_type_wide(min: i64, max: i64) -> bool {
    (min == i64::from(i32::MIN) && max == i64::from(i32::MAX))
        || (min == 0 && max == i64::from(u32::MAX))
        || (min == i64::MIN && max == i64::MAX)
}

fn float_pspec_range_is_usable(min: f64, max: f64) -> bool {
    min.is_finite()
        && max.is_finite()
        && max > min
        && min.abs() < HOLLOW_BOUND
        && max.abs() < HOLLOW_BOUND
}

fn is_controllable_property(param_spec: &glib::ParamSpec) -> bool {
    param_spec.flags().contains(gst::PARAM_FLAG_CONTROLLABLE)
        && param_spec.flags().contains(glib::ParamFlags::READABLE)
        && param_spec.flags().contains(glib::ParamFlags::WRITABLE)
        && !param_spec
            .flags()
            .contains(glib::ParamFlags::CONSTRUCT_ONLY)
}

/// ponytail: Raspberry Pi IPA `ControlInfo` only; upgrade to `Camera::controls()` per device.
fn ipa_slider_limits(name: &str) -> Option<IpaSliderLimits> {
    Some(match name {
        "analogue-gain" => IpaSliderLimits::from_float(1.0, 16.0, 1.0),
        "brightness" => IpaSliderLimits::from_float(-1.0, 1.0, 0.0),
        "contrast" => IpaSliderLimits::from_float(0.0, 32.0, 1.0),
        "saturation" => IpaSliderLimits::from_float(0.0, 32.0, 1.0),
        "sharpness" => IpaSliderLimits::from_float(0.0, 16.0, 1.0),
        "exposure-value" => IpaSliderLimits::from_float(-8.0, 8.0, 0.0),
        "lens-position" => IpaSliderLimits::from_float(0.0, 32.0, 1.0),
        // yaml default 2.2; rkisp1/softisp `ControlInfo(0.1, 10.0, 2.2)`.
        "gamma" => IpaSliderLimits::from_float(0.1, 10.0, 2.2),
        // RPi AGC clamps digital gain to `[1.0, max_digital_gain]` (default 4.0).
        "digital-gain" => IpaSliderLimits::from_float(1.0, 4.0, 1.0),
        "exposure-time" => IpaSliderLimits {
            min: 1,
            max: 66_666,
            default: 20_000,
        },
        "ae-flicker-period" => IpaSliderLimits {
            min: 100,
            max: 1_000_000,
            default: 100,
        },
        "colour-temperature" => IpaSliderLimits {
            min: 100,
            max: 100_000,
            default: 100,
        },
        _ => return None,
    })
}

fn ipa_bool_default(name: &str) -> Option<i64> {
    match name {
        "ae-enable" | "awb-enable" => Some(1),
        _ => None,
    }
}

fn integer_slider_limits(name: &str, min: i64, max: i64, default: i64) -> Option<(i64, i64, i64)> {
    let pspec_usable = max > min && !pspec_range_is_type_wide(min, max);
    if pspec_usable {
        return Some((min, max, default));
    }
    if let Some(overlay) = ipa_slider_limits(name) {
        return Some((overlay.min, overlay.max, overlay.default));
    }
    if max > min {
        Some((min, max, default))
    } else {
        None
    }
}

fn float_slider_limits(name: &str, min: f64, max: f64, default: f64) -> Option<(i64, i64, i64)> {
    if float_pspec_range_is_usable(min, max) {
        let default = if default.is_finite() {
            float_to_api(default)
        } else {
            0
        };
        return Some((float_to_api(min), float_to_api(max), default));
    }
    if let Some(overlay) = ipa_slider_limits(name) {
        return Some((overlay.min, overlay.max, overlay.default));
    }
    let api_min = float_to_api(min);
    let api_max = float_to_api(max);
    if api_max > api_min {
        let default = if default.is_finite() {
            float_to_api(default)
        } else {
            0
        };
        Some((api_min, api_max, default))
    } else {
        None
    }
}

fn float_to_api(value: f64) -> i64 {
    let scaled = value * FLOAT_SCALE;
    if !scaled.is_finite() {
        return 0;
    }
    scaled.clamp(i64::MIN as f64, i64::MAX as f64) as i64
}

fn float_from_api(value: i64) -> f64 {
    value as f64 / FLOAT_SCALE
}

fn store_pending(camera_name: &str, property: &str, value: i64) {
    recoverable_lock(pending_controls())
        .entry(camera_name.to_string())
        .or_default()
        .insert(property.to_string(), value);
}

fn pending_value(camera_name: &str, property: &str) -> Option<i64> {
    recoverable_lock(pending_controls())
        .get(camera_name)?
        .get(property)
        .copied()
}

fn probe_element() -> std::io::Result<gst::Element> {
    gst::ElementFactory::make("libcamerasrc")
        .build()
        .map_err(|error| {
            std::io::Error::other(format!("Failed to create libcamerasrc element: {error}"))
        })
}

/// One shared `libcamerasrc` for offline control enumeration (libcamera allows
/// only one CameraManager per process).
fn shared_probe_element() -> std::io::Result<gst::Element> {
    let mutex = SHARED_PROBE.get_or_init(|| Mutex::new(None));
    let mut guard = mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(element) = guard.as_ref() {
        return Ok(element.clone());
    }
    let element = probe_element()?;
    *guard = Some(element.clone());
    Ok(element)
}

fn control_element(camera_name: &str) -> std::io::Result<(gst::Element, bool)> {
    match try_live_source_element(camera_name) {
        LiveSourceLookup::Found(element) => Ok((element, true)),
        LiveSourceLookup::NotStreaming => {
            if let LiveSourceLookup::Found(element) = try_any_live_libcamerasrc() {
                return Ok((element, false));
            }
            Ok((shared_probe_element()?, false))
        }
        LiveSourceLookup::Busy => {
            if let LiveSourceLookup::Found(element) = try_any_live_libcamerasrc() {
                return Ok((element, false));
            }
            if let Some(element) = SHARED_PROBE
                .get()
                .and_then(|mutex| mutex.lock().ok())
                .and_then(|guard| guard.as_ref().cloned())
            {
                return Ok((element, false));
            }
            Err(std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                format!("Stream manager busy while listing libcamera controls for {camera_name:?}"),
            ))
        }
    }
}

fn control_default(configuration: &ControlType) -> i64 {
    match configuration {
        ControlType::Bool(control) => control.default,
        ControlType::Slider(control) => control.default,
        ControlType::Menu(control) => control.default,
    }
}

fn control_current_value(configuration: &ControlType) -> i64 {
    match configuration {
        ControlType::Bool(control) => control.value,
        ControlType::Slider(control) => control.value,
        ControlType::Menu(control) => control.value,
    }
}

fn property_current_api_value(camera_name: &str, property: &str, default: i64) -> i64 {
    pending_value(camera_name, property).unwrap_or(default)
}

fn set_property_from_api(
    element: &gst::Element,
    property: &str,
    value: i64,
) -> std::io::Result<()> {
    let Some(param_spec) = element.find_property(property) else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("Property {property:?} not found on libcamerasrc"),
        ));
    };

    if let Some(enum_class) = glib::EnumClass::with_type(param_spec.value_type()) {
        let enum_int = i32::try_from(value).map_err(|error| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("Enum value {value} does not fit in i32: {error}"),
            )
        })?;
        let Some(enum_value) = enum_class.to_value(enum_int) else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("Enum value {value} is not valid for property {property:?}"),
            ));
        };
        return set_gobject_property(element, property, &enum_value);
    }

    let value_type = param_spec.value_type();
    if value_type == bool::static_type() {
        return set_gobject_property(element, property, &(value != 0).to_value());
    }
    if value_type == i32::static_type() {
        let narrowed = i32::try_from(value).map_err(|error| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("Value {value} does not fit in i32: {error}"),
            )
        })?;
        return set_gobject_property(element, property, &narrowed.to_value());
    }
    if value_type == u32::static_type() {
        let narrowed = u32::try_from(value).map_err(|error| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("Value {value} does not fit in u32: {error}"),
            )
        })?;
        return set_gobject_property(element, property, &narrowed.to_value());
    }
    if value_type == i64::static_type() {
        return set_gobject_property(element, property, &value.to_value());
    }
    if value_type == u64::static_type() {
        let narrowed = u64::try_from(value).map_err(|error| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("Value {value} does not fit in u64: {error}"),
            )
        })?;
        return set_gobject_property(element, property, &narrowed.to_value());
    }
    if value_type == f32::static_type() {
        let float_value = float_from_api(value) as f32;
        return set_gobject_property(element, property, &float_value.to_value());
    }
    if value_type == f64::static_type() {
        let float_value = float_from_api(value);
        return set_gobject_property(element, property, &float_value.to_value());
    }

    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        format!(
            "Unsupported libcamerasrc property type for {property:?}: {:?}",
            param_spec.value_type()
        ),
    ))
}

fn set_gobject_property(
    element: &gst::Element,
    property: &str,
    value: &glib::Value,
) -> std::io::Result<()> {
    catch_unwind(AssertUnwindSafe(|| {
        element.set_property_from_value(property, value);
    }))
    .map_err(|_| std::io::Error::other(format!("libcamerasrc panicked while setting {property:?}")))
}

fn list_controls_on_element(camera_name: &str, element: &gst::Element) -> Vec<Control> {
    catch_unwind(AssertUnwindSafe(|| {
        let mut used_ids = BTreeSet::new();
        let mut controls = element
            .list_properties()
            .iter()
            .filter_map(|param_spec| control_from_pspec(param_spec, &mut used_ids))
            .collect::<Vec<_>>();
        controls.sort_by(|left, right| left.name.cmp(&right.name));
        controls
    }))
    .unwrap_or_else(|_| {
        warn!("libcamerasrc panicked while listing controls for {camera_name:?}");
        vec![]
    })
}

fn control_from_pspec(
    param_spec: &glib::ParamSpec,
    used_ids: &mut BTreeSet<u64>,
) -> Option<Control> {
    let name = param_spec.name();
    if !is_controllable_property(param_spec) {
        return None;
    }

    let id = resolve_control_id(name, used_ids);
    let state = ControlState {
        is_disabled: false,
        is_inactive: false,
    };

    if let Some(enum_class) = glib::EnumClass::with_type(param_spec.value_type()) {
        let options = enum_class
            .values()
            .iter()
            .map(|enum_value| ControlOption {
                name: enum_value.nick().to_string(),
                value: i64::from(enum_value.value()),
            })
            .collect::<Vec<_>>();
        if options.is_empty() {
            return None;
        }
        let default = param_spec
            .downcast_ref::<glib::ParamSpecEnum>()
            .map(|param_spec_enum| i64::from(param_spec_enum.default_value_as_i32()))
            .unwrap_or(options[0].value);
        return Some(Control {
            name: name.to_string(),
            cpp_type: "int32".to_string(),
            id,
            state,
            configuration: ControlType::Menu(ControlMenu {
                default,
                value: default,
                options,
            }),
        });
    }

    let value_type = param_spec.value_type();
    if value_type == bool::static_type() {
        let pspec_default = param_spec
            .downcast_ref::<glib::ParamSpecBoolean>()
            .map(|param_spec_bool| i64::from(param_spec_bool.default_value()))
            .unwrap_or(0);
        let default = ipa_bool_default(name).unwrap_or(pspec_default);
        return Some(Control {
            name: name.to_string(),
            cpp_type: "bool".to_string(),
            id,
            state,
            configuration: ControlType::Bool(ControlBool {
                default,
                value: default,
            }),
        });
    }

    if value_type == i32::static_type() {
        let (min, max, default) = param_spec
            .downcast_ref::<glib::ParamSpecInt>()
            .map(|param_spec_int| {
                (
                    i64::from(param_spec_int.minimum()),
                    i64::from(param_spec_int.maximum()),
                    i64::from(param_spec_int.default_value()),
                )
            })
            .unwrap_or((i64::from(i32::MIN), i64::from(i32::MAX), 0));
        let (min, max, default) = integer_slider_limits(name, min, max, default)?;
        return Some(slider_control(SliderControlSpec {
            name,
            id,
            state,
            min,
            max,
            default,
            cpp_type: "int64",
        }));
    }

    if value_type == u32::static_type() {
        let (min, max, default) = param_spec
            .downcast_ref::<glib::ParamSpecUInt>()
            .map(|param_spec_uint| {
                (
                    i64::from(param_spec_uint.minimum()),
                    i64::from(param_spec_uint.maximum()),
                    i64::from(param_spec_uint.default_value()),
                )
            })
            .unwrap_or((0, i64::from(u32::MAX), 0));
        let (min, max, default) = integer_slider_limits(name, min, max, default)?;
        return Some(slider_control(SliderControlSpec {
            name,
            id,
            state,
            min,
            max,
            default,
            cpp_type: "int64",
        }));
    }

    if value_type == i64::static_type() {
        let (min, max, default) = param_spec
            .downcast_ref::<glib::ParamSpecInt64>()
            .map(|param_spec_int64| {
                (
                    param_spec_int64.minimum(),
                    param_spec_int64.maximum(),
                    param_spec_int64.default_value(),
                )
            })
            .unwrap_or((i64::MIN, i64::MAX, 0));
        let (min, max, default) = integer_slider_limits(name, min, max, default)?;
        return Some(slider_control(SliderControlSpec {
            name,
            id,
            state,
            min,
            max,
            default,
            cpp_type: "int64",
        }));
    }

    if value_type == f32::static_type() || value_type == f64::static_type() {
        let (pspec_min, pspec_max, pspec_default) = if let Some(param_spec_float) =
            param_spec.downcast_ref::<glib::ParamSpecFloat>()
        {
            (
                f64::from(param_spec_float.minimum()),
                f64::from(param_spec_float.maximum()),
                f64::from(param_spec_float.default_value()),
            )
        } else if let Some(param_spec_double) = param_spec.downcast_ref::<glib::ParamSpecDouble>() {
            (
                param_spec_double.minimum(),
                param_spec_double.maximum(),
                param_spec_double.default_value(),
            )
        } else {
            (f64::NEG_INFINITY, f64::INFINITY, 0.0)
        };

        let (min, max, default) = float_slider_limits(name, pspec_min, pspec_max, pspec_default)?;

        return Some(slider_control(SliderControlSpec {
            name,
            id,
            state,
            min,
            max,
            default,
            cpp_type: "int64",
        }));
    }

    None
}

fn slider_control(spec: SliderControlSpec<'_>) -> Control {
    Control {
        name: spec.name.to_string(),
        cpp_type: spec.cpp_type.to_string(),
        id: spec.id,
        state: spec.state,
        configuration: ControlType::Slider(ControlSlider {
            default: spec.default,
            value: spec.default,
            step: 1,
            max: spec.max,
            min: spec.min,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_ids_are_stable() {
        assert_eq!(
            control_id_for_property("exposure-time"),
            control_id_for_property("exposure-time")
        );
        assert_ne!(
            control_id_for_property("exposure-time"),
            control_id_for_property("analogue-gain")
        );
        assert!(control_id_for_property("exposure-time") < CONTROL_ID_SPACE);
    }

    #[test]
    fn control_ids_mavlink_roundtrip() {
        fn mavlink_roundtrip(id: u64) -> Option<u64> {
            const N: usize = 16;
            let id_string = id.to_string();
            let bytes = id_string.as_bytes();
            let len = bytes.len().min(N);
            let mut buf = [0u8; N];
            buf[..len].copy_from_slice(&bytes[..len]);

            let mut parse_buf = [0u8; std::mem::size_of::<u64>()];
            let parse_len = parse_buf.len().min(N);
            parse_buf.copy_from_slice(&buf[..parse_len]);
            let Ok(id_string) =
                std::str::from_utf8(&parse_buf).map(|s| s.trim_end_matches(char::from(0)))
            else {
                return None;
            };
            id_string.parse().ok()
        }

        for name in [
            "exposure-time",
            "analogue-gain",
            "brightness",
            "contrast",
            "af-mode",
            "ae-enable",
            "awb-enable",
            "digital-gain",
            "gamma",
        ] {
            let id = control_id_for_property(name);
            assert!(id < CONTROL_ID_SPACE, "id for {name} exceeds MAVLink space");
            assert_eq!(
                mavlink_roundtrip(id),
                Some(id),
                "roundtrip failed for {name}"
            );
        }
    }

    #[test]
    fn float_scale_roundtrips() {
        assert_eq!(float_to_api(1.5), 1500);
        assert!((float_from_api(1500) - 1.5).abs() < f64::EPSILON);
    }

    #[test]
    fn companion_mode_property_is_generic() {
        assert_eq!(
            companion_mode_property("analogue-gain").as_deref(),
            Some("analogue-gain-mode")
        );
        assert_eq!(
            companion_mode_property("exposure-time").as_deref(),
            Some("exposure-time-mode")
        );
        assert_eq!(companion_mode_property("analogue-gain-mode"), None);
        assert_eq!(companion_mode_property("ae-enable"), None);
        assert_eq!(
            companion_mode_property("brightness").as_deref(),
            Some("brightness-mode")
        );
    }

    #[test]
    fn mode_like_uses_suffix_not_name_table() {
        assert!(is_mode_like_property("analogue-gain-mode"));
        assert!(is_mode_like_property("ae-enable"));
        assert!(is_mode_like_property("awb-enable"));
        assert!(!is_mode_like_property("analogue-gain"));
        assert!(!is_mode_like_property("brightness"));
    }

    #[test]
    fn type_wide_pspec_ranges_are_hollow() {
        assert!(pspec_range_is_type_wide(
            i64::from(i32::MIN),
            i64::from(i32::MAX)
        ));
        assert!(!pspec_range_is_type_wide(1000, 10_667));
        assert!(!float_pspec_range_is_usable(
            f64::NEG_INFINITY,
            f64::INFINITY
        ));
        assert!(float_pspec_range_is_usable(-1.0, 1.0));
        assert_eq!(
            integer_slider_limits(
                "unknown-control",
                i64::from(i32::MIN),
                i64::from(i32::MAX),
                0
            ),
            Some((i64::from(i32::MIN), i64::from(i32::MAX), 0))
        );
        assert_eq!(
            integer_slider_limits("unknown-control", 1, 1_000_000, 0),
            Some((1, 1_000_000, 0))
        );
        let (min, max, _default) = float_slider_limits(
            "unknown-control",
            f64::from(-f32::MAX),
            f64::from(f32::MAX),
            0.0,
        )
        .expect("finite after clamp");
        assert!(max > min);
        assert_eq!(
            float_slider_limits("unknown-control", -1.0, 1.0, 0.0),
            Some((float_to_api(-1.0), float_to_api(1.0), 0))
        );
    }

    #[test]
    fn hollow_pspec_uses_ipa_overlay_limits() {
        assert_eq!(
            float_slider_limits("brightness", f64::from(-f32::MAX), f64::from(f32::MAX), 0.0,),
            Some((float_to_api(-1.0), float_to_api(1.0), 0))
        );
        assert_eq!(
            float_slider_limits(
                "analogue-gain",
                f64::from(-f32::MAX),
                f64::from(f32::MAX),
                0.0,
            ),
            Some((float_to_api(1.0), float_to_api(16.0), float_to_api(1.0)))
        );
        assert_eq!(
            float_slider_limits("contrast", f64::from(-f32::MAX), f64::from(f32::MAX), 0.0,),
            Some((float_to_api(0.0), float_to_api(32.0), float_to_api(1.0)))
        );
        assert_eq!(
            float_slider_limits("gamma", f64::from(-f32::MAX), f64::from(f32::MAX), 0.0,),
            Some((float_to_api(0.1), float_to_api(10.0), float_to_api(2.2)))
        );
        assert_eq!(
            float_slider_limits(
                "digital-gain",
                f64::from(-f32::MAX),
                f64::from(f32::MAX),
                0.0,
            ),
            Some((float_to_api(1.0), float_to_api(4.0), float_to_api(1.0)))
        );
        assert_eq!(
            integer_slider_limits("exposure-time", i64::from(i32::MIN), i64::from(i32::MAX), 0),
            Some((1, 66_666, 20_000))
        );
        assert_eq!(ipa_bool_default("ae-enable"), Some(1));
        assert_eq!(ipa_bool_default("awb-enable"), Some(1));
        assert_eq!(ipa_bool_default("stats-output-enable"), None);
    }

    #[test]
    fn usable_pspec_range_beats_ipa_overlay() {
        assert_eq!(
            float_slider_limits("brightness", -0.5, 0.5, 0.1),
            Some((float_to_api(-0.5), float_to_api(0.5), float_to_api(0.1)))
        );
        assert_eq!(
            integer_slider_limits("exposure-time", 100, 10_000, 500),
            Some((100, 10_000, 500))
        );
    }

    #[test]
    fn exposure_time_freezes_analogue_gain_mode() {
        assert_eq!(analogue_gain_mode_nick_for("exposure-time"), Some("manual"));
        assert_eq!(analogue_gain_mode_nick_for("exposure-value"), Some("auto"));
        assert_eq!(analogue_gain_mode_nick_for("brightness"), None);
        assert_eq!(analogue_gain_mode_nick_for("analogue-gain"), None);
    }

    #[test]
    fn overlay_current_values_prefers_pending() {
        let camera = "test-camera-overlay";
        store_pending(camera, "brightness", 250);
        let controls = vec![Control {
            name: "brightness".into(),
            cpp_type: "int64".into(),
            id: 1,
            state: ControlState::default(),
            configuration: ControlType::Slider(ControlSlider {
                default: 0,
                value: 0,
                step: 1,
                max: 1000,
                min: -1000,
            }),
        }];
        let overlaid = overlay_current_values(camera, controls);
        match &overlaid[0].configuration {
            ControlType::Slider(slider) => assert_eq!(slider.value, 250),
            other => panic!("expected slider, got {other:?}"),
        }
    }

    #[test]
    fn overlay_current_values_keeps_default_without_pending() {
        let camera = "test-camera-overlay-default";
        let controls = vec![Control {
            name: "brightness".into(),
            cpp_type: "int64".into(),
            id: 1,
            state: ControlState::default(),
            configuration: ControlType::Slider(ControlSlider {
                default: 0,
                value: 999,
                step: 1,
                max: 1000,
                min: -1000,
            }),
        }];
        let overlaid = overlay_current_values(camera, controls);
        match &overlaid[0].configuration {
            ControlType::Slider(slider) => assert_eq!(slider.value, 0),
            other => panic!("expected slider, got {other:?}"),
        }
    }

    #[test]
    fn pending_value_preferred_when_idle() {
        let camera = "test-camera-pending";
        store_pending(camera, "brightness", 250);
        assert_eq!(pending_value(camera, "brightness"), Some(250));
    }

    #[test]
    fn pending_zero_beats_semantic_default() {
        let camera = "test-camera-saturation-zero";
        store_pending(camera, "saturation", 0);
        assert_eq!(pending_value(camera, "saturation"), Some(0));
    }

    #[test]
    fn pending_value_roundtrips() {
        let camera = "test-camera-pending-roundtrip";
        store_pending(camera, "brightness", 500);
        assert_eq!(pending_value(camera, "brightness"), Some(500));
        store_pending(camera, "brightness", 250);
        assert_eq!(pending_value(camera, "brightness"), Some(250));
    }

    #[test]
    fn dirty_pending_latest_value_wins() {
        let camera = "test-camera-dirty-latest";
        store_pending(camera, "brightness", -1000);
        mark_dirty(camera);
        store_pending(camera, "brightness", 1000);
        mark_dirty(camera);
        let values = take_dirty_pending(camera).expect("dirty pending should be present");
        assert_eq!(values.get("brightness"), Some(&1000));
        assert!(take_dirty_pending(camera).is_none());
    }

    #[test]
    fn dirty_pending_includes_companion_mode() {
        let camera = "test-camera-dirty-order";
        store_pending(camera, "analogue-gain", 2000);
        store_pending(camera, "analogue-gain-mode", 1);
        mark_dirty(camera);
        let values = take_dirty_pending(camera).expect("dirty pending should be present");
        assert_eq!(values.get("analogue-gain-mode"), Some(&1));
        assert_eq!(values.get("analogue-gain"), Some(&2000));
        assert!(take_dirty_pending(camera).is_none());
    }

    #[test]
    fn controllable_flag_is_gstreamer_user_bit() {
        assert_eq!(gst::PARAM_FLAG_CONTROLLABLE, glib::ParamFlags::USER_1);
    }
}
