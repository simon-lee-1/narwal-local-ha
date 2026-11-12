use crate::{
    control::{CommandInput, LocalInput},
    plan::{Dampness, MopPlan, Settings},
};
use rumqttc::{AsyncClient, Event, Incoming, LastWill, MqttOptions, QoS};
use serde::Deserialize;
use serde_json::{Value, json};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, watch};

const MAX_STATE_BYTES: usize = 65536;
const MAX_IMAGE_BYTES: usize = 2 * 1024 * 1024;
const MAX_OUTBOX: usize = 32;
const MAX_AGE: u64 = 30;
const MAX_ROOM_CLEAN_BYTES: usize = 2048;

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MqttConfig {
    pub host: String,
    pub port: u16,
    pub username: Option<String>,
    pub password: Option<String>,
    pub topic_prefix: String,
    pub client_id: String,
}

#[derive(Clone, Debug, PartialEq)]
struct Publication {
    topic: String,
    payload: Vec<u8>,
    retain: bool,
}

pub(crate) fn options(config: &MqttConfig) -> Result<MqttOptions, String> {
    let valid_id = !config.client_id.is_empty()
        && config.client_id.len() <= 64
        && config
            .client_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    let valid_topic = !config.topic_prefix.is_empty()
        && config.topic_prefix.len() <= 256
        && config.topic_prefix.split('/').all(|s| !s.is_empty())
        && !config
            .topic_prefix
            .chars()
            .any(|c| c.is_control() || c == '+' || c == '#');
    let valid_host = !config.host.is_empty()
        && config.host.len() <= 255
        && !config
            .host
            .chars()
            .any(|c| c.is_control() || c.is_whitespace() || c == '/');
    let valid_login = config
        .username
        .as_ref()
        .is_none_or(|s| !s.is_empty() && s.len() <= 256)
        && config.password.as_ref().is_none_or(|s| s.len() <= 8192)
        && (config.password.is_none() || config.username.is_some());
    if !valid_id || !valid_topic || !valid_host || !valid_login || config.port == 0 {
        return Err("invalid_mqtt_config".into());
    }
    let mut options = MqttOptions::new(&config.client_id, &config.host, config.port);
    options.set_keep_alive(Duration::from_secs(15));
    options.set_clean_session(true);
    options.set_inflight(4);
    options.set_request_channel_capacity(MAX_OUTBOX);
    options.set_max_packet_size(4096, MAX_IMAGE_BYTES + 1024);
    options.set_last_will(LastWill::new(
        format!("{}/availability", config.topic_prefix),
        "offline",
        QoS::AtLeastOnce,
        true,
    ));
    if let Some(username) = &config.username {
        options.set_credentials(username, config.password.as_deref().unwrap_or(""));
    }
    Ok(options)
}
fn publication(topic: String, payload: Vec<u8>) -> Publication {
    Publication {
        topic,
        payload,
        retain: true,
    }
}
fn json_publication(topic: String, payload: Value) -> Publication {
    publication(
        topic,
        serde_json::to_vec(&payload).expect("JSON Value serialization"),
    )
}
fn discovery(config: &MqttConfig, commands: &[&str]) -> Vec<Publication> {
    let prefix = &config.topic_prefix;
    let id = &config.client_id;
    let unique = format!("narwal_local_{id}");
    let availability = format!("{prefix}/availability");
    let state_topic = format!("{prefix}/state");
    let device = json!({
        "identifiers":[unique],
        "name":"Narwal J1",
        "manufacturer":"Narwal",
        "model":"J1 local replacement",
    });
    let common = |name: Option<&str>, suffix: &str| {
        json!({
            "name":name, "unique_id":format!("{unique}{suffix}"),
            "availability_topic":availability,
            "payload_available":"online", "payload_not_available":"offline",
            "device":device,
        })
    };
    let mut vacuum = common(None, "");
    vacuum["schema"] = json!("state");
    vacuum["command_topic"] = json!(format!("{prefix}/command"));
    vacuum["state_topic"] = json!(state_topic);
    vacuum["json_attributes_topic"] = json!(state_topic);
    vacuum["payload_start"] = json!("start");
    vacuum["payload_pause"] = json!("pause");
    vacuum["payload_stop"] = json!("stop");
    vacuum["payload_return_to_base"] = json!("return_to_base");
    vacuum["supported_features"] = json!(
        commands
            .iter()
            .map(|command| {
                if *command == "return_to_base" {
                    "return_home"
                } else {
                    command
                }
            })
            .collect::<Vec<_>>()
    );
    let mut battery = common(Some("Battery"), "_battery");
    battery["state_topic"] = json!(state_topic);
    battery["device_class"] = json!("battery");
    battery["unit_of_measurement"] = json!("%");
    battery["state_class"] = json!("measurement");
    battery["value_template"] = json!("{{ value_json.status.battery_per | default(none) }}");
    let mut error = common(Some("Error code"), "_error");
    error["state_topic"] = json!(state_topic);
    error["entity_category"] = json!("diagnostic");
    error["value_template"] = json!("{{ value_json.status.error_code | default(none) }}");
    let mut mode = common(Some("Reported mode"), "_mode");
    mode["state_topic"] = json!(state_topic);
    mode["value_template"] = json!("{{ value_json.reported_mode | default('Unknown') }}");
    mode["icon"] = json!("mdi:robot-vacuum");
    let mut job = common(Some("Job status"), "_job_status");
    job["state_topic"] = json!(state_topic);
    job["value_template"] = json!("{{ value_json.job_status | default('Unknown') }}");
    job["icon"] = json!("mdi:robot-vacuum");
    let mut progress = common(Some("Cleaning progress"), "_progress");
    progress["state_topic"] = json!(state_topic);
    progress["unit_of_measurement"] = json!("%");
    progress["state_class"] = json!("measurement");
    progress["value_template"] = json!("{{ value_json.cleaning_progress | default(none) }}");
    let mut area = common(Some("Cleaned area"), "_area");
    area["state_topic"] = json!(state_topic);
    area["unit_of_measurement"] = json!("m²");
    area["state_class"] = json!("measurement");
    area["value_template"] = json!("{{ value_json.cleaned_area | default(none) }}");
    let mut docked = common(Some("Docked"), "_docked");
    docked["state_topic"] = json!(state_topic);
    docked["value_template"] = json!(
        "{{ 'ON' if value_json.status.station_contact in [true, 1] else 'OFF' if value_json.status.station_contact in [false, 0] else none }}"
    );
    let mut camera = common(Some("Room map"), "_map");
    camera.as_object_mut().unwrap().remove("availability_topic");
    camera["availability"] = json!([
        {"topic":availability,"payload_available":"online","payload_not_available":"offline"},
        {"topic":format!("{prefix}/map/availability"),"payload_available":"online","payload_not_available":"offline"}
    ]);
    camera["availability_mode"] = json!("all");
    camera["topic"] = json!(format!("{prefix}/map/image"));
    let mut image = common(Some("Room map"), "_map_image");
    image.as_object_mut().unwrap().remove("availability_topic");
    image["availability"] = camera["availability"].clone();
    image["availability_mode"] = json!("all");
    image["image_topic"] = camera["topic"].clone();
    image["image_encoding"] = json!("raw");
    image["content_type"] = json!("image/png");
    vec![
        json_publication(format!("homeassistant/vacuum/{id}/config"), vacuum),
        json_publication(format!("homeassistant/sensor/{id}_battery/config"), battery),
        json_publication(format!("homeassistant/sensor/{id}_error/config"), error),
        json_publication(format!("homeassistant/sensor/{id}_mode/config"), mode),
        json_publication(format!("homeassistant/sensor/{id}_job_status/config"), job),
        json_publication(
            format!("homeassistant/sensor/{id}_progress/config"),
            progress,
        ),
        json_publication(format!("homeassistant/sensor/{id}_area/config"), area),
        json_publication(
            format!("homeassistant/binary_sensor/{id}_docked/config"),
            docked,
        ),
        json_publication(format!("homeassistant/camera/{id}_map/config"), camera),
        json_publication(format!("homeassistant/image/{id}_map/config"), image),
    ]
}
struct PlanView {
    rooms: Vec<(u64, String)>,
    selected: MopPlan,
}
fn plan_view(snapshot: &Value) -> Option<PlanView> {
    let raw = snapshot.get("mop_settings")?.as_object()?;
    let settings: Settings = serde_json::from_value(json!({
        "version":raw.get("version").cloned().unwrap_or(json!(1)),
        "rooms":raw.get("rooms")?,
        "selected":raw.get("selected")?,
    }))
    .ok()?;
    settings.validate().ok()?;
    Some(PlanView {
        rooms: settings
            .rooms
            .into_iter()
            .map(|room| (room.id, room.name))
            .collect(),
        selected: settings.selected,
    })
}
fn configured_discovery(
    config: &MqttConfig,
    commands: &[&str],
    snapshot: &Value,
) -> Vec<Publication> {
    let mut publications = discovery(config, commands);
    publications.extend(editor_discovery(config, snapshot));
    let Some(plan) = plan_view(snapshot) else {
        return publications;
    };
    let id = &config.client_id;
    let prefix = &config.topic_prefix;
    let device = json!({"identifiers":[format!("narwal_local_{id}")],
        "name":"Narwal J1","manufacturer":"Narwal","model":"J1 local replacement"});
    let common = |name: &str, suffix: &str| {
        json!({"name":name,"unique_id":format!("narwal_local_{id}_{suffix}"),
            "device":device,"state_topic":format!("{prefix}/state"),
            "command_topic":format!("{prefix}/{suffix}/set"),
            "availability_topic":format!("{prefix}/availability"),
            "payload_available":"online","payload_not_available":"offline",
            "retain":false,"qos":0,"optimistic":false})
    };
    let mut room = common("Room", "room");
    room["options"] = json!(plan.rooms.iter().map(|(_, name)| name).collect::<Vec<_>>());
    room["value_template"] = json!("{{ value_json.selected_room | default(none) }}");
    let mut passes = common("Cleaning passes", "passes");
    passes["min"] = json!(1);
    passes["max"] = json!(3);
    passes["step"] = json!(1);
    passes["mode"] = json!("box");
    passes["command_template"] = json!("{{ value | int }}");
    passes["value_template"] = json!("{{ value_json.mop_settings.selected.passes }}");
    let mut dampness = common("Mop dampness", "dampness");
    dampness["options"] = json!(["Normal", "Wet"]);
    dampness["value_template"] = json!("{{ value_json.mop_settings.selected.dampness }}");
    publications.extend([
        json_publication(format!("homeassistant/select/{id}_room/config"), room),
        json_publication(format!("homeassistant/number/{id}_passes/config"), passes),
        json_publication(
            format!("homeassistant/select/{id}_dampness/config"),
            dampness,
        ),
    ]);
    publications
}
fn enabled_commands(snapshot: &Value) -> Vec<&'static str> {
    if snapshot.get("commands_enabled").and_then(Value::as_bool) != Some(true) {
        return Vec::new();
    }
    let Some(commands) = snapshot.get("enabled_commands").and_then(Value::as_array) else {
        return Vec::new();
    };
    ["start", "pause", "stop", "return_to_base"]
        .into_iter()
        .filter(|command| commands.iter().any(|value| value.as_str() == Some(command)))
        .collect()
}
fn age(snapshot: &Value, elapsed: Duration) -> Option<u64> {
    snapshot
        .get("age_seconds")?
        .as_u64()?
        .checked_add(elapsed.as_secs())
}
fn online(snapshot: &Value, elapsed: Duration) -> bool {
    snapshot.get("online").and_then(Value::as_bool) == Some(true)
        && snapshot.get("connected").and_then(Value::as_bool) == Some(true)
        && age(snapshot, elapsed).is_some_and(|seconds| seconds <= MAX_AGE)
        && snapshot.get("status").is_some_and(Value::is_object)
}
fn command<'a>(
    config: &MqttConfig,
    topic: &str,
    retain: bool,
    duplicate: bool,
    payload: &'a [u8],
    snapshot: &Value,
    elapsed: Duration,
) -> Option<&'a str> {
    if retain || duplicate || payload.len() > 128 || !online(snapshot, elapsed) {
        return None;
    }
    let value = std::str::from_utf8(payload).ok()?;
    let prefix = &config.topic_prefix;
    if topic == format!("{prefix}/command") {
        if value == "start"
            && snapshot
                .get("mop_settings")
                .is_some_and(|value| !value.is_null())
            && plan_view(snapshot).is_none()
        {
            return None;
        }
        return enabled_commands(snapshot).contains(&value).then_some(value);
    }
    let plan = plan_view(snapshot)?;
    let allowed = if topic == format!("{prefix}/room/set") {
        plan.rooms.iter().any(|(_, name)| name == value)
    } else if topic == format!("{prefix}/passes/set") {
        ["1", "2", "3"].contains(&value)
    } else if topic == format!("{prefix}/dampness/set") {
        ["Normal", "Wet"].contains(&value)
    } else {
        false
    };
    allowed.then_some(value)
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RoomCleanRequest {
    map_id: String,
    revision: String,
    rooms: Vec<u64>,
    passes: u8,
    dampness: Dampness,
}
fn room_clean_input(
    retain: bool,
    duplicate: bool,
    payload: &[u8],
    snapshot: &Value,
    elapsed: Duration,
) -> Option<LocalInput> {
    if retain
        || duplicate
        || payload.len() > MAX_ROOM_CLEAN_BYTES
        || !online(snapshot, elapsed)
        || !enabled_commands(snapshot).contains(&"start")
        || snapshot["map_tracking"]["map_current"] != true
    {
        return None;
    }
    let status = &snapshot["status"];
    let mode = status["clean_mode"].as_u64()?;
    if !matches!(mode, 2 | 3)
        || status["error_code"] != 0
        || status["work_status"] != 0
        || !(status["station_contact"] == true || status["station_contact"] == 1)
    {
        // Room selection starts a new task only. Ordinary Start remains the
        // Resume control, which intentionally keeps an existing task's rooms.
        return None;
    }
    if status["robot_status"] != 100 {
        let expected = match (mode, status["robot_status"].as_u64()?) {
            (2, 301) => "vacuum_end_task",
            (3, 303) => "end_task",
            _ => return None,
        };
        if status["main_task"] != 1
            || !status["task_queue"].is_array()
            || snapshot["room_clean_preparation"].as_str() != Some(expected)
        {
            return None;
        }
    }
    let request: RoomCleanRequest = serde_json::from_slice(payload).ok()?;
    if request.map_id.is_empty()
        || request.map_id.len() > 256
        || snapshot["map"]["map_id"]
            .as_str()
            .map(str::to_owned)
            .or_else(|| snapshot["map"]["map_id"].as_u64().map(|id| id.to_string()))
            .as_deref()
            != Some(request.map_id.as_str())
        || !editor_token(&request.revision)
        || snapshot["room_catalog"]["map_id"].as_str() != Some(request.map_id.as_str())
        || snapshot["room_catalog"]["revision"].as_str() != Some(request.revision.as_str())
    {
        return None;
    }
    let settings = plan_view(snapshot)?;
    let allowed: Vec<_> = settings.rooms.iter().map(|(id, _)| *id).collect();
    let plan = MopPlan {
        rooms: request.rooms,
        passes: request.passes,
        dampness: request.dampness,
    };
    plan.validate(&allowed).ok()?;
    // Use the existing bounded queue and serial engine owner. It obtains
    // fresh robot feedback, validates the plan again and reports refusals.
    Some(LocalInput::RoomCommand {
        command: CommandInput {
            command: "start".into(),
            rooms: Vec::new(),
            mop: Some(plan),
        },
        map_id: request.map_id,
        revision: request.revision,
    })
}
fn local_input(
    config: &MqttConfig,
    topic: &str,
    retain: bool,
    duplicate: bool,
    payload: &[u8],
    snapshot: &Value,
    elapsed: Duration,
) -> Option<LocalInput> {
    let prefix = &config.topic_prefix;
    if let Some(suffix) = topic.strip_prefix(&format!("{prefix}/room_editor/")) {
        return editor_input(suffix, retain, duplicate, payload, snapshot, elapsed);
    }
    if topic == format!("{prefix}/command/room_clean") {
        return room_clean_input(retain, duplicate, payload, snapshot, elapsed);
    }
    let value = command(config, topic, retain, duplicate, payload, snapshot, elapsed)?;
    if topic == format!("{prefix}/command") {
        Some(LocalInput::Command(CommandInput {
            command: value.to_owned(),
            rooms: Vec::new(),
            // The serial engine reads saved choices after preceding setting
            // updates. Only an explicit room-clean request pins its own plan.
            mop: None,
        }))
    } else if topic == format!("{prefix}/room/set") {
        Some(LocalInput::SelectRoom(value.to_owned()))
    } else if topic == format!("{prefix}/passes/set") {
        Some(LocalInput::SetPasses(value.parse().ok()?))
    } else if topic == format!("{prefix}/dampness/set") {
        let dampness: Dampness = serde_json::from_value(json!(value)).ok()?;
        Some(LocalInput::SetDampness(dampness))
    } else {
        None
    }
}
fn checked_state(snapshot: &Value) -> Result<(), String> {
    // The HTTP map card receives this bounded geometry, but public_map_tracking
    // omits it. Do not let detailed private outlines disable MQTT telemetry.
    let private_geometry = snapshot.pointer("/map_tracking/display/rooms");
    let private_catalog = snapshot.get("room_catalog");
    let mut pending = vec![(snapshot, 0)];
    let mut count = 0usize;
    while let Some((value, depth)) = pending.pop() {
        if private_geometry.is_some_and(|geometry| std::ptr::eq(value, geometry))
            || private_catalog.is_some_and(|catalog| std::ptr::eq(value, catalog))
        {
            continue;
        }
        count += 1;
        if count > MAX_STATE_BYTES || depth > 32 {
            return Err("invalid_mqtt_state".into());
        }
        match value {
            Value::Object(fields) => {
                if fields.len() > MAX_STATE_BYTES {
                    return Err("invalid_mqtt_state".into());
                }
                pending.extend(fields.values().map(|v| (v, depth + 1)));
            }
            Value::Array(items) => {
                if items.len() > MAX_STATE_BYTES {
                    return Err("invalid_mqtt_state".into());
                }
                pending.extend(items.iter().map(|v| (v, depth + 1)));
            }
            Value::String(text) if text.len() > MAX_STATE_BYTES => {
                return Err("invalid_mqtt_state".into());
            }
            _ => {}
        }
    }
    struct Limited(Vec<u8>);
    impl std::io::Write for Limited {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if bytes.len() > MAX_STATE_BYTES - self.0.len() {
                return Err(std::io::ErrorKind::FileTooLarge.into());
            }
            self.0.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    fn without_fields(value: &Value, paths: &[&[&str]]) -> Value {
        let Some(fields) = value.as_object() else {
            return value.clone();
        };
        Value::Object(
            fields
                .iter()
                .filter_map(|(name, child)| {
                    if paths.iter().any(|path| path.len() == 1 && path[0] == name) {
                        return None;
                    }
                    let child_paths: Vec<_> = paths
                        .iter()
                        .filter(|path| path.len() > 1 && path[0] == name)
                        .map(|path| &path[1..])
                        .collect();
                    Some((
                        name.clone(),
                        if child_paths.is_empty() {
                            child.clone()
                        } else {
                            without_fields(child, &child_paths)
                        },
                    ))
                })
                .collect(),
        )
    }
    let bounded = without_fields(
        snapshot,
        &[&["room_catalog"], &["map_tracking", "display", "rooms"]],
    );
    serde_json::to_writer(Limited(Vec::new()), &bounded).map_err(|_| "invalid_mqtt_state".into())
}
fn receipt_fields(value: Option<&Value>) -> Option<Value> {
    let source = value?.as_object()?;
    let command = source.get("command")?.as_str()?;
    let outcome = source.get("outcome")?.as_str()?;
    if ![
        "start",
        "pause",
        "stop",
        "return_to_base",
        "resume",
        "end_task",
        "vacuum_end_task",
        "vacuum_start",
        "unknown",
    ]
    .contains(&command)
        || ![
            "sending",
            "no_action_needed",
            "before_send_refused",
            "delivery_uncertain",
            "observed_expected_state",
            "rejected",
        ]
        .contains(&outcome)
    {
        return None;
    }
    let mut result = json!({"command":command,"outcome":outcome});
    for field in [
        "no_op",
        "transport_attempted",
        "write_completed",
        "acknowledged",
        "verified",
    ] {
        if let Some(value) = source.get(field).and_then(Value::as_bool) {
            result[field] = json!(value);
        } else if field == "transport_attempted" && source.get(field) == Some(&Value::Null) {
            result[field] = Value::Null;
        }
    }
    // Only publish a bounded pair that could be a validated Start plan.
    // Repeated wire IDs represent passes; they do not prove actual coverage.
    if ["start", "vacuum_start"].contains(&command)
        && let Some(rooms) = source.get("requested_rooms").and_then(Value::as_array)
        && (1..=32).contains(&rooms.len())
        && let Some(order) = source
            .get("requested_clean_order")
            .and_then(Value::as_array)
        && (1..=96).contains(&order.len())
        && rooms
            .iter()
            .all(|room| room.as_u64().is_some_and(|id| id <= 255))
        && rooms
            .iter()
            .enumerate()
            .all(|(index, room)| !rooms[..index].contains(room))
        && order
            .iter()
            .all(|room| room.as_u64().is_some_and(|id| id <= 255))
        && (1..=3).any(|passes| {
            rooms
                .iter()
                .flat_map(|room| std::iter::repeat_n(room, passes))
                .eq(order.iter())
        })
    {
        result["requested_rooms"] = json!(rooms);
        result["requested_clean_order"] = json!(order);
    }
    if let Some(reason) = source.get("reason").and_then(Value::as_str)
        && [
            "robot_busy",
            "previous_motion_intent_unresolved",
            "command_guards_unverified",
            "command_source_state_refused",
            "command_state_missing",
            "robot_status_stale",
            "robot_offline",
            "robot_disconnected",
            "request_timeout",
            "motion_persistence_failed",
            "motion_persistence_required",
            "room_selection_invalid",
            "room_selection_state_unverified",
            "room_catalog_changed",
            "room_catalog_unavailable_or_write_pending",
            "mop_plan_invalid",
            "mop_settings_unavailable",
            "mop_context_refused",
            "paused_plan_mismatch",
            "mop_end_task_not_cleared",
            "vacuum_end_task_not_cleared",
            "internal_command_only",
            "mop_start_requires_idle_task",
            "mop_resume_requires_paused_task",
            "mop_pause_requires_active_task",
            "mop_settings_persistence_failed",
            "vacuum_profile_unavailable",
        ]
        .contains(&reason)
    {
        result["reason"] = json!(reason);
    }
    Some(result)
}
fn public_receipt(value: Option<&Value>) -> Option<Value> {
    let mut result = receipt_fields(value)?;
    // One internal preparation phase is diagnostic only. It cannot recursively
    // copy arbitrary peer data or expand the public MQTT command whitelist.
    if result["command"] == "start"
        && let Some(preparation) = value.and_then(|source| source.get("preparation"))
        && preparation
            .get("command")
            .and_then(Value::as_str)
            .is_some_and(|command| ["end_task", "vacuum_end_task"].contains(&command))
        && let Some(preparation) = receipt_fields(Some(preparation))
    {
        result["preparation"] = preparation;
    }
    Some(result)
}
fn job_status(status: &Value) -> &'static str {
    let code = status["robot_status"].as_u64();
    let work = status["work_status"].as_u64();
    let mode = status["clean_mode"].as_u64();
    let docked = status["station_contact"] == true || status["station_contact"] == 1;
    let away = status["station_contact"] == false || status["station_contact"] == 0;
    if status["error_code"]
        .as_u64()
        .is_some_and(|error| error != 0)
    {
        return "Error";
    }
    if work == Some(1) && status["task_name"] == "SysStartOperation" {
        return match mode {
            Some(2) => "Preparing to vacuum",
            Some(3) => "Preparing to mop",
            _ => "Preparing",
        };
    }
    match (code, work, mode, docked, away) {
        (Some(406), Some(1), Some(3), true, _) => "Washing mop",
        (Some(302), Some(1), Some(3), _, true) => "Mopping",
        (Some(300), Some(1), Some(2), _, true) => "Vacuuming",
        (Some(301), Some(0), Some(2), _, _) => "Vacuum paused",
        (Some(600), _, _, _, true) => "Returning to dock",
        (Some(700), _, _, _, true) => "Leaving dock",
        (Some(303), Some(0), Some(3), _, true) => "Paused",
        (Some(303), Some(0), Some(3), true, _) if status["main_task"] == 1 => {
            "Docked (cleaning task paused)"
        }
        (Some(100), Some(0), _, _, _) => "Idle",
        (_, Some(1), _, true, _) => "Dock cycle",
        _ => "Unknown",
    }
}
fn public_map_tracking(value: Option<&Value>) -> Option<Value> {
    let value = value?.as_object()?;
    let mut out = json!({"map_current":value.get("map_current")?.as_bool()?});
    for (field, sources) in [
        ("robot", ["reported_pose", "dock_contact", "unavailable"]),
        ("dock", ["map_base_station", "unavailable", "unavailable"]),
    ] {
        let source = value.get(field)?.as_object()?;
        let available = source.get("available")?.as_bool()?;
        let provenance = source.get("source")?.as_str()?;
        if !sources.contains(&provenance) {
            return None;
        }
        let mut point = json!({"available":available,"source":provenance});
        if available {
            for coordinate in ["x", "y"] {
                point[coordinate] =
                    json!(source.get(coordinate)?.as_f64().filter(|n| n.is_finite())?);
            }
            if let Some(age) = source.get("age_seconds").and_then(Value::as_u64) {
                point["age_seconds"] = json!(age);
            }
        }
        out[field] = point;
    }
    Some(out)
}
fn state_publication(
    config: &MqttConfig,
    snapshot: &Value,
    elapsed: Duration,
) -> Result<Publication, String> {
    checked_state(snapshot)?;
    let mut output = json!({
        "online":online(snapshot,elapsed),
        "connected":snapshot.get("connected").and_then(Value::as_bool).unwrap_or(false),
        "commands_enabled":!enabled_commands(snapshot).is_empty(),
        "enabled_commands":enabled_commands(snapshot),
        "status":snapshot.get("status").filter(|v|v.is_object()).cloned().unwrap_or(json!({})),
    });
    let status = &output["status"];
    let docked = status["station_contact"] == true || status["station_contact"] == 1;
    let away = status["station_contact"] == false || status["station_contact"] == 0;
    let mop_context = status["clean_mode"] == 3 && status["main_task"] == 1;
    let vacuum_context = status["clean_mode"] == 2 && status["main_task"] == 1;
    let state = if status["error_code"].as_i64().is_some_and(|code| code != 0) {
        Some("error")
    } else if docked {
        Some("docked")
    } else if away
        && status["work_status"] == 1
        && (status["robot_status"] == 302 || (vacuum_context && status["robot_status"] == 300))
    {
        Some("cleaning")
    } else if away
        && status["robot_status"] == 600
        && (status["work_status"] == 0 || status["work_status"] == 1)
    {
        Some("returning")
    } else if away && mop_context && status["robot_status"] == 303 && status["work_status"] == 0 {
        // J1 mop pause uses status 303; the T10 status 301 mapping is not applied.
        Some("paused")
    } else if !away && (status["charge_status"] == true || status["charge_status"] == 1) {
        Some("docked")
    } else if status["work_status"] == 0 && status["task_name"].as_str() == Some("") {
        Some("idle")
    } else {
        // Keep unfamiliar J1 task/state codes as raw feedback rather than infer motion.
        None
    };
    let battery = status["battery_per"].as_u64().filter(|n| *n <= 100);
    let progress = status["working_progress"]
        .as_f64()
        .filter(|n| (0.0..=100.0).contains(n));
    let area = status["cleaned_area"]
        .as_f64()
        .filter(|n| n.is_finite() && *n >= 0.0);
    let washing = docked && status["robot_status"] == 406 && status["work_status"] == 1;
    let native_mode = status["clean_mode"].as_u64();
    output["job_status"] = json!(if online(snapshot, elapsed) {
        job_status(status)
    } else {
        "Unavailable"
    });
    if let Some(mut tracking) = public_map_tracking(snapshot.get("map_tracking")) {
        let stale = tracking["robot"]["age_seconds"]
            .as_u64()
            .is_none_or(|age| age.saturating_add(elapsed.as_secs()) >= 30);
        if !online(snapshot, elapsed) || tracking["map_current"] != true || stale {
            tracking["robot"] = json!({"available":false,"source":"unavailable"});
        }
        // Keep age internally for expiry, without publishing a changing
        // counter while the robot marker itself is unchanged.
        if let Some(robot) = tracking["robot"].as_object_mut() {
            robot.remove("age_seconds");
        }
        output["map_tracking"] = tracking;
    }
    output["reported_mode"] = json!(match native_mode {
        Some(2) => "Vacuum",
        Some(3) => "Mop",
        _ => "Unknown",
    });
    output["dampness_applicable"] = json!(native_mode == Some(3));
    output["supported_passes"] = match native_mode {
        Some(2 | 3) => json!([1, 2, 3]),
        _ => json!([]),
    };
    // An explicit null clears the previous HA activity for unfamiliar feedback.
    output["state"] = json!(state);
    if washing && state != Some("error") {
        output["activity"] = json!("washing_mop");
    } else if let Some(state) = state {
        output["activity"] = json!(state);
    }
    if let Some(battery) = battery {
        output["battery_level"] = json!(battery);
    }
    if let Some(progress) = progress {
        output["cleaning_progress"] = json!(progress);
    }
    if let Some(area) = area {
        output["cleaned_area"] = json!(area);
    }
    if let Some(plan) = plan_view(snapshot) {
        if plan.selected.rooms.len() == 1
            && let Some((_, name)) = plan
                .rooms
                .iter()
                .find(|(id, _)| *id == plan.selected.rooms[0])
        {
            output["selected_room"] = json!(name);
        }
        output["mop_settings"] = json!({
            "rooms":plan.rooms.iter().map(|(id,name)|json!({"id":id,"name":name})).collect::<Vec<_>>(),
            "selected":plan.selected,
        });
    }
    for field in ["last_command", "last_refusal"] {
        if let Some(receipt) = public_receipt(snapshot.get(field)) {
            output[field] = receipt;
        }
    }
    for field in ["motion_intent_unresolved", "motion_persistence_ready"] {
        if let Some(value) = snapshot.get(field).and_then(Value::as_bool) {
            output[field] = json!(value);
        }
    }
    let p = json_publication(format!("{}/state", config.topic_prefix), output);
    if p.payload.len() > MAX_STATE_BYTES {
        return Err("invalid_mqtt_state".into());
    }
    Ok(p)
}
fn availability_publication(config: &MqttConfig, value: bool) -> Publication {
    publication(
        format!("{}/availability", config.topic_prefix),
        if value {
            b"online".to_vec()
        } else {
            b"offline".to_vec()
        },
    )
}
fn image_publications(
    config: &MqttConfig,
    image: Option<&[u8]>,
) -> Result<Vec<Publication>, String> {
    let mut publications = Vec::new();
    let image = image.filter(|image| {
        if image.len() > MAX_IMAGE_BYTES
            || image.len() < 33
            || &image[..8] != b"\x89PNG\r\n\x1a\n"
            || &image[8..16] != b"\0\0\0\rIHDR"
        {
            return false;
        }
        let width = u32::from_be_bytes(image[16..20].try_into().unwrap());
        let height = u32::from_be_bytes(image[20..24].try_into().unwrap());
        width > 0
            && height > 0
            && width <= 8192
            && height <= 8192
            && u64::from(width) * u64::from(height) <= 16_777_216
    });
    // A rejected image only makes its image entity unavailable. Telemetry and
    // commands stay connected, and the next valid image can restore it.
    if let Some(image) = image {
        publications.push(publication(
            format!("{}/map/image", config.topic_prefix),
            image.to_vec(),
        ));
    }
    publications.push(publication(
        format!("{}/map/availability", config.topic_prefix),
        if image.is_some() {
            b"online".to_vec()
        } else {
            b"offline".to_vec()
        },
    ));
    Ok(publications)
}
fn announcement(
    config: &MqttConfig,
    snapshot: &Value,
    elapsed: Duration,
    image: Option<&[u8]>,
) -> Result<Vec<Publication>, String> {
    let mut publications = configured_discovery(config, &enabled_commands(snapshot), snapshot);
    publications.push(state_publication(config, snapshot, elapsed)?);
    publications.push(catalog_publication(config, snapshot));
    publications.push(editor_state_publication(config, snapshot, elapsed));
    publications.extend(image_publications(config, image)?);
    publications.push(availability_publication(config, online(snapshot, elapsed)));
    Ok(publications)
}
#[derive(Default)]
struct Availability {
    last: Option<(bool, Instant)>,
    states: Vec<(Publication, Instant)>,
}
impl Availability {
    fn queue_state(
        &mut self,
        outbox: &mut Outbox,
        value: Publication,
        now: Instant,
    ) -> Result<(), String> {
        let position = self
            .states
            .iter()
            .position(|(last, _)| last.topic == value.topic);
        if position.is_some_and(|position| {
            let (last, at) = &self.states[position];
            *last == value && now.saturating_duration_since(*at) < Duration::from_secs(60)
        }) {
            return Ok(());
        }
        // Remember only successfully queued payloads, so an enqueue error
        // cannot suppress a later attempt to publish the same state.
        outbox.push(value.clone())?;
        if let Some(position) = position {
            self.states[position] = (value, now);
        } else {
            self.states.push((value, now));
        }
        Ok(())
    }
    fn due(&mut self, value: bool, now: Instant) -> bool {
        let due = self.last.is_none_or(|(last, at)| {
            last != value || now.saturating_duration_since(at) >= Duration::from_secs(60)
        });
        if due {
            self.last = Some((value, now));
        }
        due
    }
}
#[derive(Default)]
struct Outbox(Vec<Publication>);
impl Outbox {
    fn push(&mut self, value: Publication) -> Result<(), String> {
        if value.payload.len() > MAX_IMAGE_BYTES {
            return Err("mqtt_payload_limit".into());
        }
        if let Some(position) = self.0.iter().position(|p| p.topic == value.topic) {
            self.0.remove(position);
        }
        if self.0.len() >= MAX_OUTBOX {
            return Err("mqtt_outbox_full".into());
        }
        self.0.push(value);
        Ok(())
    }
    fn extend(&mut self, values: Vec<Publication>) -> Result<(), String> {
        for value in values {
            self.push(value)?;
        }
        Ok(())
    }
    fn flush(&mut self, client: &AsyncClient) {
        while let Some(next) = self.0.first() {
            // Never await a full request channel while this task owns the event loop.
            if client
                .try_publish(
                    &next.topic,
                    QoS::AtMostOnce,
                    next.retain,
                    next.payload.clone(),
                )
                .is_err()
            {
                break;
            }
            self.0.remove(0);
        }
    }
}
fn observe(state: &mut watch::Receiver<Value>) -> Result<(Value, Instant), String> {
    let current = state.borrow_and_update();
    checked_state(&current)?;
    Ok((current.clone(), Instant::now()))
}
fn reconcile(
    config: &MqttConfig,
    snapshot: &Value,
    elapsed: Duration,
    outbox: &mut Outbox,
    availability: &mut Availability,
    published_features: &mut Option<(Vec<&'static str>, Option<u64>, Option<Value>)>,
    now: Instant,
) -> Result<(), String> {
    availability.queue_state(outbox, state_publication(config, snapshot, elapsed)?, now)?;
    let commands = enabled_commands(snapshot);
    let fingerprint = (
        commands.clone(),
        snapshot["status"]["clean_mode"].as_u64(),
        catalog_metadata(snapshot),
    );
    let catalog_changed = published_features
        .as_ref()
        .is_none_or(|previous| previous.2 != fingerprint.2);
    if published_features.as_ref() != Some(&fingerprint) {
        outbox.extend(configured_discovery(config, &commands, snapshot))?;
        *published_features = Some(fingerprint);
    }
    if catalog_changed {
        outbox.push(catalog_publication(config, snapshot))?;
    }
    availability.queue_state(
        outbox,
        editor_state_publication(config, snapshot, elapsed),
        now,
    )?;
    let current = online(snapshot, elapsed);
    if availability.due(current, now) {
        outbox.push(availability_publication(config, current))?;
    }
    Ok(())
}
pub async fn run(
    config: MqttConfig,
    mut state: watch::Receiver<Value>,
    mut image: watch::Receiver<Option<Vec<u8>>>,
    mut edit_image: watch::Receiver<Option<Vec<u8>>>,
    commands: mpsc::Sender<LocalInput>,
) -> Result<(), String> {
    let mqtt_options = options(&config)?;
    let (mut snapshot, mut observed) = observe(&mut state)?;
    let mut image_open = true;
    let mut edit_image_open = true;
    loop {
        // A new client drops the previous transport's pending publications. A
        // reconnect rebuilds state from current watches instead of replaying health.
        let (client, mut eventloop) = AsyncClient::new(mqtt_options.clone(), MAX_OUTBOX);
        eventloop.network_options.set_connection_timeout(5);
        let mut connected = false;
        let mut outbox = Outbox::default();
        let mut availability = Availability::default();
        let mut published_features = None;
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        'connection: loop {
            // Keep the same poll future alive across watch/timer branches.
            // In particular, status updates must not restart a slow handshake.
            let poll = eventloop.poll();
            tokio::pin!(poll);
            loop {
                if connected {
                    outbox.flush(&client);
                }
                tokio::select! {
                    _ = commands.closed() => return Ok(()),
                    changed = state.changed() => {
                        if changed.is_err() { return Ok(()); }
                        (snapshot, observed) = observe(&mut state)?;
                        if connected {
                            reconcile(&config,&snapshot,observed.elapsed(),&mut outbox,
                                &mut availability,&mut published_features,Instant::now())?;
                        }
                    },
                    changed = image.changed(), if image_open => {
                        if changed.is_err() { image_open = false; }
                        if connected {
                            outbox.extend(image_publications(&config,image.borrow_and_update().as_deref())?)?;
                        }
                    },
                    changed = edit_image.changed(), if edit_image_open => {
                        if changed.is_err() { edit_image_open = false; }
                        if connected {
                            outbox.extend(preview_image_publications(&config,edit_image.borrow_and_update().as_deref())?)?;
                        }
                    },
                    _ = tick.tick(), if connected => {
                        // Marker expiry can precede robot-status expiry.
                        // Re-evaluate both; unchanged payloads stay suppressed.
                        reconcile(&config,&snapshot,observed.elapsed(),&mut outbox,
                            &mut availability,&mut published_features,Instant::now())?;
                    },
                    event = &mut poll => {
                        match event {
                        Ok(Event::Incoming(Incoming::ConnAck(_))) => {
                            connected = true;
                            // Subscribe at QoS 0: a reconnect must not replay a motion request.
                            client.try_subscribe(format!("{}/command",config.topic_prefix),QoS::AtMostOnce)
                                .map_err(|_|"mqtt_subscribe_failed")?;
                            for suffix in ["room/set", "passes/set", "dampness/set", "command/room_clean",
                                "room_editor/draft", "room_editor/save", "room_editor/cancel", "room_editor/refresh",
                                "room_editor/name/set", "room_editor/room/set"] {
                                client.try_subscribe(format!("{}/{suffix}",config.topic_prefix),QoS::AtMostOnce)
                                    .map_err(|_|"mqtt_subscribe_failed")?;
                            }
                            client.try_subscribe("homeassistant/status",QoS::AtMostOnce)
                                .map_err(|_|"mqtt_subscribe_failed")?;
                            if state.has_changed().map_err(|_|"mqtt_state_closed")? {
                                (snapshot,observed) = observe(&mut state)?;
                            }
                            outbox.extend(announcement(&config,&snapshot,observed.elapsed(),
                                image.borrow_and_update().as_deref())?)?;
                            outbox.extend(preview_image_publications(&config,edit_image.borrow_and_update().as_deref())?)?;
                            published_features = Some((enabled_commands(&snapshot),
                                snapshot["status"]["clean_mode"].as_u64(),catalog_metadata(&snapshot)));
                            reconcile(&config,&snapshot,observed.elapsed(),&mut outbox,
                                &mut availability,&mut published_features,Instant::now())?;
                        },
                        Ok(Event::Incoming(Incoming::Publish(message))) => {
                            if state.has_changed().map_err(|_|"mqtt_state_closed")? {
                                // Gate against the newest watch value even if its select arm
                                // has not run yet. Dispatch revalidates again before actuation.
                                (snapshot,observed) = observe(&mut state)?;
                                reconcile(&config,&snapshot,observed.elapsed(),&mut outbox,
                                    &mut availability,&mut published_features,Instant::now())?;
                            }
                            if message.topic == "homeassistant/status" && message.payload.as_ref() == b"online" {
                                outbox.extend(announcement(&config,&snapshot,observed.elapsed(),
                                    image.borrow().as_deref())?)?;
                                outbox.extend(preview_image_publications(&config,edit_image.borrow().as_deref())?)?;
                                // HA birth forces a complete announcement and
                                // starts a new heartbeat from these queued states.
                                availability = Availability::default();
                                reconcile(&config,&snapshot,observed.elapsed(),&mut outbox,
                                    &mut availability,&mut published_features,Instant::now())?;
                            } else if let Some(value) = local_input(&config,&message.topic,
                                message.retain,message.dup,&message.payload,&snapshot,observed.elapsed())
                            {
                                // Full/closed queues reject rather than retain commands for later.
                                let _ = commands.try_send(value);
                            }
                        },
                        Ok(_) => {},
                        Err(_) => break 'connection,
                        }
                        break;
                    }
                }
            }
        }
        drop(eventloop);
        drop(client);
        // Continue observing freshness during reconnect backoff.
        let backoff = tokio::time::sleep(Duration::from_secs(2));
        tokio::pin!(backoff);
        loop {
            tokio::select! {
                _ = &mut backoff => break,
                _ = commands.closed() => return Ok(()),
                changed = state.changed() => {
                    if changed.is_err() { return Ok(()); }
                    (snapshot,observed) = observe(&mut state)?;
                }
            }
        }
    }
}

const MAX_CATALOG_BYTES: usize = 512 * 1024;
const MAX_EDITOR_BYTES: usize = 2048;

fn editor_token(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_.:-".contains(&b))
}
fn catalog_metadata(snapshot: &Value) -> Option<Value> {
    let source = snapshot.get("room_catalog")?;
    for field in ["map_id", "revision"] {
        if !editor_token(source[field].as_str()?) {
            return None;
        }
    }
    if source["coordinate_space"] != "display_pixels" {
        return None;
    }
    let rooms = source["rooms"].as_array()?;
    if rooms.is_empty() || rooms.len() > 32 {
        return None;
    }
    let mut result = Vec::new();
    let mut ids = Vec::new();
    let mut names = Vec::new();
    let mut total_points = 0usize;
    for room in rooms {
        let id = room["id"].as_u64().filter(|id| *id < 63)?;
        let name = room["name"].as_str()?;
        if ids.contains(&id)
            || names.contains(&name)
            || name.is_empty()
            || name.len() > 128
            || name.chars().any(char::is_control)
        {
            return None;
        }
        ids.push(id);
        names.push(name);
        let outline = room["outline"].as_array()?;
        total_points = total_points.checked_add(outline.len())?;
        if (!outline.is_empty() && outline.len() < 3) || total_points > 32768 {
            return None;
        }
        for point in outline {
            let point = point.as_array()?;
            if point.len() != 2
                || point.iter().any(|v| {
                    !v.as_f64()
                        .is_some_and(|n| n.is_finite() && (0.0..=8192.0).contains(&n))
                })
            {
                return None;
            }
        }
        let mut out = json!({"id":id,"name":name,"outline":outline,"default_state":"unselected"});
        if let Some(label) = room.get("label")
            && let (Some(x), Some(y)) = (label["x"].as_f64(), label["y"].as_f64())
            && x.is_finite()
            && y.is_finite()
        {
            out["label"] = json!({"text":name,"x":x,"y":y,"offset_y":0,
                "rendered_in_image":label["rendered_in_image"] == true});
        }
        result.push(out);
    }
    let value = json!({"map_id":source["map_id"],"revision":source["revision"],
        "coordinate_space":"display_pixels","rooms":result});
    (serde_json::to_vec(&value).ok()?.len() <= MAX_CATALOG_BYTES).then_some(value)
}
fn catalog_value(snapshot: &Value) -> Option<Value> {
    let mut catalog = catalog_metadata(snapshot)?;
    // A missing outline must not erase healthy rooms or fabricate coverage
    // over an interior hole. Keep all names available to the separate editor.
    catalog["rooms"].as_array_mut()?.retain(|r| {
        r["outline"]
            .as_array()
            .is_some_and(|outline| outline.len() >= 3)
    });
    (!catalog["rooms"].as_array()?.is_empty()).then_some(catalog)
}
fn catalog_publication(config: &MqttConfig, snapshot: &Value) -> Publication {
    json_publication(
        format!("{}/room_catalog", config.topic_prefix),
        catalog_value(snapshot).unwrap_or(json!({"revision":null,"rooms":[]})),
    )
}
fn editor_state_publication(
    config: &MqttConfig,
    snapshot: &Value,
    elapsed: Duration,
) -> Publication {
    let source = &snapshot["room_editor"];
    let allowed = [
        "idle",
        "draft_ready",
        "draft_blocked",
        "saving",
        "confirmed",
        "refused",
        "unavailable",
    ];
    let state = source["state"]
        .as_str()
        .filter(|s| allowed.contains(s))
        .unwrap_or("unavailable");
    let fresh = online(snapshot, elapsed);
    let mut value = json!({"state":if fresh {state}else{"unavailable"},
        "can_save":fresh && state=="draft_ready" && source["can_save"]==true,
        "unresolved_write":source["unresolved_write"]==true});
    for field in [
        "draft_id",
        "operation",
        "summary",
        "message",
        "name",
        "selected_room",
    ] {
        value[field] = source[field]
            .as_str()
            .filter(|text| text.len() <= 512 && !text.chars().any(char::is_control))
            .map_or(Value::Null, |text| json!(text));
    }
    value["capabilities"] = json!({
        "rename":source["capabilities"]["rename"]==true,
        "split":source["capabilities"]["split"]==true,
        "merge":source["capabilities"]["merge"]==true,
        "names_are_local":source["capabilities"]["names_are_local"]==true});
    json_publication(format!("{}/room_editor/state", config.topic_prefix), value)
}
fn editor_discovery(config: &MqttConfig, snapshot: &Value) -> Vec<Publication> {
    let id = &config.client_id;
    let prefix = &config.topic_prefix;
    let common = |name: &str, suffix: &str| {
        json!({"name":name,"unique_id":format!("narwal_local_{id}_{suffix}"),
            "device":{"identifiers":[format!("narwal_local_{id}")],"name":"Narwal J1",
                "manufacturer":"Narwal","model":"J1 local replacement"},
            "availability_topic":format!("{prefix}/availability"),
            "payload_available":"online","payload_not_available":"offline"})
    };
    let mut catalog = common("Room catalog", "room_catalog");
    catalog["state_topic"] = json!(format!("{prefix}/room_catalog"));
    catalog["value_template"] = json!("{{ value_json.revision | default('unavailable', true) }}");
    catalog["json_attributes_topic"] = catalog["state_topic"].clone();
    catalog["icon"] = json!("mdi:floor-plan");
    let mut editor = common("Room editor", "room_editor");
    editor["state_topic"] = json!(format!("{prefix}/room_editor/state"));
    editor["value_template"] = json!("{{ value_json.state | default('unavailable') }}");
    editor["json_attributes_topic"] = editor["state_topic"].clone();
    let mut name = common("Room name", "room_name");
    name["state_topic"] = editor["state_topic"].clone();
    name["value_template"] = json!("{{ value_json.name | default('', true) }}");
    name["command_topic"] = json!(format!("{prefix}/room_editor/name/set"));
    name["min"] = json!(0);
    name["max"] = json!(128);
    name["mode"] = json!("text");
    name["retain"] = json!(false);
    name["qos"] = json!(0);
    name["optimistic"] = json!(false);
    let mut room = common("Editor room", "editor_room");
    room["state_topic"] = editor["state_topic"].clone();
    room["value_template"] = json!("{{ value_json.selected_room | default(none) }}");
    room["command_topic"] = json!(format!("{prefix}/room_editor/room/set"));
    room["retain"] = json!(false);
    room["qos"] = json!(0);
    room["optimistic"] = json!(false);
    room["options"] = catalog_metadata(snapshot)
        .map(|value| {
            json!(
                value["rooms"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|room| room["name"].clone())
                    .collect::<Vec<_>>()
            )
        })
        .unwrap_or(json!(["Unavailable"]));
    let mut image = common("Room edit preview", "room_edit_preview");
    image.as_object_mut().unwrap().remove("availability_topic");
    image["availability"] = json!([
        {"topic":format!("{prefix}/availability"),"payload_available":"online","payload_not_available":"offline"},
        {"topic":format!("{prefix}/room_editor/preview/availability"),"payload_available":"online","payload_not_available":"offline"}]);
    image["availability_mode"] = json!("all");
    image["image_topic"] = json!(format!("{prefix}/room_editor/preview/image"));
    image["image_encoding"] = json!("raw");
    image["content_type"] = json!("image/png");
    [
        ("sensor", "room_catalog", catalog),
        ("sensor", "room_editor", editor),
        ("text", "room_name", name),
        ("select", "editor_room", room),
        ("image", "room_edit_preview", image),
    ]
    .into_iter()
    .map(|(domain, suffix, value)| {
        json_publication(
            format!("homeassistant/{domain}/{id}_{suffix}/config"),
            value,
        )
    })
    .collect()
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DraftReference {
    draft_id: String,
}
fn editor_input(
    suffix: &str,
    retain: bool,
    duplicate: bool,
    payload: &[u8],
    snapshot: &Value,
    elapsed: Duration,
) -> Option<LocalInput> {
    use crate::engine::{DraftEdit, EditorInput};
    if retain || duplicate || payload.len() > MAX_EDITOR_BYTES || !online(snapshot, elapsed) {
        return None;
    }
    if suffix == "refresh" {
        let body: Value = serde_json::from_slice(payload).ok()?;
        return body
            .as_object()
            .is_some_and(serde_json::Map::is_empty)
            .then_some(LocalInput::Editor(EditorInput::Refresh));
    }
    let editor = &snapshot["room_editor"];
    if editor["unresolved_write"] == true || editor["state"] == "saving" {
        return None;
    }
    match suffix {
        "name/set" => {
            let name = std::str::from_utf8(payload).ok()?;
            (name.len() <= 128 && !name.chars().any(char::is_control))
                .then(|| LocalInput::EditorName(name.to_string()))
        }
        "room/set" => {
            let name = std::str::from_utf8(payload).ok()?;
            catalog_metadata(snapshot)?["rooms"]
                .as_array()?
                .iter()
                .any(|room| room["name"] == name)
                .then(|| LocalInput::EditorRoom(name.to_string()))
        }
        "draft" => {
            if !editor["draft_id"].is_null() || snapshot["map_tracking"]["map_current"] != true {
                return None;
            }
            let edit: DraftEdit = serde_json::from_slice(payload).ok()?;
            let catalog = catalog_metadata(snapshot)?;
            let (operation, map_id, revision, rooms) = match &edit {
                DraftEdit::Rename {
                    map_id,
                    revision,
                    rooms,
                } if rooms.len() == 1 => ("rename", map_id, revision, Some(rooms)),
                DraftEdit::Merge {
                    map_id,
                    revision,
                    rooms,
                } if rooms.len() == 2 && rooms[0] != rooms[1] => {
                    ("merge", map_id, revision, Some(rooms))
                }
                DraftEdit::Split {
                    map_id,
                    revision,
                    line,
                } if line[0] != line[1]
                    && line
                        .iter()
                        .flatten()
                        .all(|n| n.is_finite() && (0.0..=8192.0).contains(n)) =>
                {
                    ("split", map_id, revision, None)
                }
                _ => return None,
            };
            if editor["capabilities"][operation] != true
                || catalog["map_id"] != map_id.as_str()
                || catalog["revision"] != revision.as_str()
                || rooms.is_some_and(|rooms| {
                    rooms.iter().any(|id| {
                        !catalog["rooms"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .any(|room| room["id"] == *id)
                    })
                })
            {
                return None;
            }
            Some(LocalInput::Editor(EditorInput::Draft { edit }))
        }
        "save" | "cancel" => {
            let request: DraftReference = serde_json::from_slice(payload).ok()?;
            if !editor_token(&request.draft_id)
                || editor["draft_id"] != request.draft_id
                || (suffix == "save"
                    && (editor["state"] != "draft_ready" || editor["can_save"] != true))
            {
                return None;
            }
            Some(LocalInput::Editor(if suffix == "save" {
                EditorInput::Save {
                    draft_id: request.draft_id,
                }
            } else {
                EditorInput::Cancel {
                    draft_id: request.draft_id,
                }
            }))
        }
        _ => None,
    }
}

fn preview_image_publications(
    config: &MqttConfig,
    image: Option<&[u8]>,
) -> Result<Vec<Publication>, String> {
    let mut publications = image_publications(config, image)?;
    for publication in &mut publications {
        publication.topic = publication.topic.replacen(
            &format!("{}/map/", config.topic_prefix),
            &format!("{}/room_editor/preview/", config.topic_prefix),
            1,
        );
    }
    Ok(publications)
}
