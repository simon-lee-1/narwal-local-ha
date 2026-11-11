use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::Path,
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Entities {
    topic_prefix: String,
    vacuum_entity: String,
    map_entity: String,
    job_entity: String,
    mode_entity: String,
    battery_entity: String,
    progress_entity: String,
    area_entity: String,
    dock_entity: String,
    error_entity: String,
    dampness_entity: String,
    room_catalog_entity: String,
    room_editor_entity: String,
    room_name_entity: String,
    editor_room_entity: String,
    room_preview_entity: String,
}

fn require(value: bool, message: &str) -> Result<(), String> {
    if value { Ok(()) } else { Err(message.into()) }
}

fn valid_entity(value: &str, domain: &str) -> bool {
    value.len() <= 255
        && value
            .strip_prefix(&format!("{domain}."))
            .is_some_and(|name| {
                !name.is_empty()
                    && name
                        .bytes()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_')
            })
}

fn validate_entities(e: &Entities) -> Result<(), String> {
    require(
        !e.topic_prefix.is_empty()
            && e.topic_prefix.len() <= 256
            && e.topic_prefix.split('/').all(|part| !part.is_empty())
            && !e
                .topic_prefix
                .chars()
                .any(|c| c.is_control() || c == '+' || c == '#'),
        "topic_prefix must be 1–256 UTF-8 bytes with nonempty levels and no controls or MQTT wildcards",
    )?;
    for (entity, domain) in [
        (&e.vacuum_entity, "vacuum"),
        (&e.map_entity, "image"),
        (&e.job_entity, "sensor"),
        (&e.mode_entity, "sensor"),
        (&e.battery_entity, "sensor"),
        (&e.progress_entity, "sensor"),
        (&e.area_entity, "sensor"),
        (&e.dock_entity, "binary_sensor"),
        (&e.error_entity, "sensor"),
        (&e.dampness_entity, "select"),
        (&e.room_catalog_entity, "sensor"),
        (&e.room_editor_entity, "sensor"),
        (&e.room_name_entity, "text"),
        (&e.editor_room_entity, "select"),
        (&e.room_preview_entity, "image"),
    ] {
        require(
            valid_entity(entity, domain),
            "invalid entity ID or domain in entity configuration",
        )?;
    }
    Ok(())
}

fn current_rooms(state: &Value) -> Result<(String, Vec<Value>), String> {
    require(
        state["connected"] == true && state["online"] == true,
        "robot must be locally connected",
    )?;
    require(
        state["age_seconds"].as_u64().is_some_and(|age| age <= 30),
        "robot state is not fresh",
    )?;
    require(
        state["map_tracking"]["map_current"] == true,
        "current connection has no current map",
    )?;
    let display = &state["map_tracking"]["display"];
    require(
        display["coordinate_space"] == "display_pixels",
        "room coordinates are not display pixels",
    )?;
    let map_id = state["map"]["map_id"].as_str().ok_or("missing map ID")?;
    // The map pin is inserted into a HA template in Mop mode. Constrain its
    // representation so a map ID cannot introduce template expressions.
    require(
        !map_id.is_empty()
            && map_id.len() <= 256
            && map_id
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"-_.:".contains(&c)),
        "map ID has an unsupported representation",
    )?;
    let width = state["map"]["width"].as_u64().ok_or("missing map width")?;
    let height = state["map"]["height"]
        .as_u64()
        .ok_or("missing map height")?;
    require(
        width > 0 && height > 0 && width <= 4096 && height <= 4096,
        "invalid map dimensions",
    )?;
    let catalog = state["mop_settings"]["rooms"]
        .as_array()
        .ok_or("missing configured room catalog")?;
    require(
        !catalog.is_empty() && catalog.len() <= 32,
        "invalid configured room count",
    )?;
    let mut allowed = BTreeSet::new();
    for room in catalog {
        let id = room["id"].as_u64().ok_or("invalid configured room ID")?;
        require(
            id <= 255 && allowed.insert(id),
            "invalid or duplicate configured room ID",
        )?;
    }
    let candidates = display["rooms"]
        .as_array()
        .ok_or("missing display room outlines")?;
    let mut found = BTreeSet::new();
    let mut rooms = Vec::new();
    for room in candidates {
        let id = room["room_id"].as_u64().ok_or("invalid map room ID")?;
        if !allowed.contains(&id) {
            continue;
        }
        require(
            found.insert(id),
            "duplicate configured room in map metadata",
        )?;
        require(
            room["outline_error"].is_null(),
            "configured room has an ambiguous or unavailable outline",
        )?;
        let outline = room["outline"].as_array().ok_or("missing room outline")?;
        require(
            outline.len() >= 3 && outline.len() <= 65536,
            "invalid room outline length",
        )?;
        for point in outline {
            let point = point.as_array().ok_or("invalid outline point")?;
            require(point.len() == 2, "invalid outline point dimensions")?;
            require(
                point[0].as_u64().is_some_and(|x| x <= width)
                    && point[1].as_u64().is_some_and(|y| y <= height),
                "outline point lies outside the displayed map",
            )?;
        }
        rooms.push(json!({"id":id, "outline":outline, "default_state":"unselected"}));
    }
    require(found == allowed, "configured room missing from current map")?;
    Ok((map_id.to_owned(), rooms))
}

fn action(service: &str, e: &Entities) -> Value {
    json!({"action":"call-service","service":service,"service_data":{"entity_id":e.vacuum_entity}})
}

fn unicode_escapes(value: &str) -> String {
    value
        .encode_utf16()
        .map(|unit| format!("\\u{unit:04x}"))
        .collect()
}

fn command_topic(e: &Entities, suffix: &str, templated: bool) -> String {
    let topic = format!("{}{suffix}", e.topic_prefix);
    if !templated {
        return topic;
    }
    // The card renders JSON.stringify(service_data) as Jinja in Mop mode.
    // Emit JSON escapes through a literal so topic text cannot become a
    // template expression or break the surrounding JSON string.
    format!("{{{{ '{}' }}}}", unicode_escapes(&topic))
}

fn template_card_topic(e: &Entities, suffix: &str) -> String {
    // Config Template Card evaluates strings containing ${...}. Supply a
    // literal expression instead of allowing topic text to become JS source.
    format!(
        "${{\"{}\"}}",
        unicode_escapes(&format!("{}{suffix}", e.topic_prefix))
    )
}

fn map_unavailable_card(e: &Entities, translations: &Value) -> Value {
    let button = |name: &str, icon: &str, service: &str| {
        json!({"type":"button","name":name,"icon":icon,"tap_action":action(service, e),
            "hold_action":{"action":"none"},"double_tap_action":{"action":"none"}})
    };
    let labels = serde_json::to_string(translations).expect("packaged error labels");
    json!({"type":"conditional","conditions":[
        {"condition":"state","entity":e.vacuum_entity,"state_not":"unavailable"},
        {"condition":"or","conditions":[
            {"condition":"state","entity":e.map_entity,"state":"unavailable"},
            {"condition":"state","entity":e.map_entity,"state":"unknown"}
        ]}
    ],"card":{"type":"vertical-stack","cards":[
        {"type":"markdown","content":"**Waiting for the map.** Job status and local controls remain available. Room selection will return when the map is ready."},
        {"type":"entities","title":"Current job","show_header_toggle":false,"entities":[
            {"entity":e.job_entity,"name":"Job"},
            {"entity":e.mode_entity,"name":"Mode"},
            {"entity":e.battery_entity,"name":"Battery"},
            {"entity":e.progress_entity,"name":"Progress"},
            {"entity":e.area_entity,"name":"Cleaned area"},
            {"entity":e.dock_entity,"name":"At dock"}
        ]},
        {"type":"markdown","content":format!(
            "{{% set code = states('{}') %}}{{% set labels = {} %}}**Robot status:** {{{{ labels.get(code, 'Unrecognized robot error.') }}}}",
            e.error_entity, labels)},
        {"type":"horizontal-stack","cards":[
            {"type":"conditional","conditions":[{"condition":"or","conditions":[
                {"condition":"state","entity":e.mode_entity,"state":"Mop"},
                {"condition":"state","entity":e.mode_entity,"state":"Vacuum"}
            ]}],"card":button("Return to dock","mdi:home-import-outline","vacuum.return_to_base")},
            {"type":"conditional","conditions":[
                {"condition":"state","entity":e.mode_entity,"state":"Mop"},
                {"condition":"state","entity":e.vacuum_entity,"state":"cleaning"}
            ],"card":button("Pause","mdi:pause","vacuum.pause")},
            {"type":"conditional","conditions":[
                {"condition":"state","entity":e.mode_entity,"state":"Mop"},
                {"condition":"state","entity":e.vacuum_entity,"state":"paused"},
                {"condition":"state","entity":e.error_entity,"state":"0"}
            ],"card":button("Resume current task","mdi:play-pause","vacuum.start")}
        ]}
    ]}})
}

fn map_card(mode: &str, e: &Entities, translations: &Value) -> Value {
    let mop = mode == "Mop";
    let handler = format!(
        "{}-{}-map",
        e.vacuum_entity.replace('.', "-"),
        mode.to_lowercase()
    );
    let mut payload = String::from(
        "{\"map_id\":[[map_id_json]],\"revision\":[[map_revision_json]],\"rooms\":[[selection]],\"passes\":",
    );
    payload.push_str("[[repeats]]");
    payload.push_str(",\"dampness\":\"");
    if mop {
        payload.push_str(&format!("{{{{ states('{}') }}}}", e.dampness_entity));
    } else {
        payload.push_str("Normal");
    }
    payload.push_str("\"}");
    let mut icons = vec![
        json!({"icon":"mdi:selection-remove","tooltip":"Clear room selection",
            "tap_action":{"action":"fire-dom-event","xiaomi_vacuum_map_card":{
                "action_handler_id":handler,"action":"selection.clear"}}}),
        json!({"icon":"mdi:home-import-outline","tooltip":"Return to dock",
            "tap_action":action("vacuum.return_to_base", e)}),
    ];
    if mop {
        icons.extend([
            json!({"icon":"mdi:pause","tooltip":"Pause",
                "conditions":[{"entity":e.vacuum_entity,"value":"cleaning"}],
                "tap_action":action("vacuum.pause", e)}),
            json!({"icon":"mdi:play-pause","tooltip":"Resume current task",
                "conditions":[{"entity":e.vacuum_entity,"value":"paused"}],
                "tap_action":action("vacuum.start", e)}),
            json!({"type":"menu","menu_id":"dampness","icon_id":"dampness",
                "entity":e.dampness_entity,"available_values_attribute":"options",
                "icon":"mdi:water","tooltip":"Mop dampness",
                "icon_mapping":{"Normal":"mdi:water","Wet":"mdi:water-plus"},
                "tap_action":{"action":"call-service","service":"select.select_option",
                    "service_data":{"entity_id":e.dampness_entity,"option":"[[value]]"}}}),
        ]);
    }
    json!({
        "type":"custom:xiaomi-vacuum-map-card","title":"Narwal J1",
        "action_handler_id":handler,"entity":e.vacuum_entity,
        "map_source":{"camera":e.map_entity},"calibration_source":{"identity":true},
        "room_source":{"entity":e.room_catalog_entity},
        "map_locked":true,"two_finger_pan":true,"clean_selection_on_start":true,"debug":false,
        "icons":icons,
        "tiles":[
            {"entity":e.job_entity,"label":"Job","icon":"mdi:robot-vacuum"},
            {"entity":e.mode_entity,"label":"Mode","icon":if mop {"mdi:spray-bottle"} else {"mdi:vacuum"}},
            {"entity":e.battery_entity,"label":"Battery","icon":"mdi:battery","unit":"%","precision":0},
            {"entity":e.progress_entity,"label":"Progress","icon":"mdi:progress-check","unit":"%","precision":0},
            {"entity":e.area_entity,"label":"Cleaned area","icon":"mdi:texture-box","unit":"m²","precision":1},
            {"entity":e.dock_entity,"label":"Dock","icon":"mdi:home","translations":{"on":"At dock","off":"Away"}},
            {"entity":e.error_entity,"label":"Robot status","icon":"mdi:alert-circle-check-outline",
                "precision":0,"translations":translations}
        ],
        "map_modes":[{
            "name":if mop {"Mop rooms"} else {"Vacuum rooms"},
            "icon":"mdi:floor-plan","selection_type":"ROOM","id_type":"number",
            "max_selections":32,"repeats_type":"EXTERNAL",
            "max_repeats":3,"run_immediately":false,
            // Whole-value variable lookup prevents topic text containing
            // [[selection]] or another card token from being substituted.
            "variables":{"narwal_command_topic":command_topic(e, "/command/room_clean", mop)},
            "service_call_schema":{"service":"mqtt.publish","evaluate_data_as_template":mop,
                "service_data":{"topic":"[[narwal_command_topic]]",
                    "retain":false,"qos":0,"payload":payload}},
            "predefined_selections":[]
        }]
    })
}

fn editor_view(e: &Entities) -> Value {
    let handler = format!("{}-room-editor", e.vacuum_entity.replace('.', "-"));
    let mut modes = Vec::new();
    for (operation, name, icon, selection, count, field) in [
        (
            "rename",
            "Rename · prepare draft",
            "mdi:rename",
            "ROOM",
            1,
            "rooms",
        ),
        (
            "merge",
            "Merge · prepare draft",
            "mdi:vector-union",
            "ROOM",
            2,
            "rooms",
        ),
        (
            "split",
            "Split · prepare draft",
            "mdi:vector-line",
            "MANUAL_PATH",
            2,
            "line",
        ),
    ] {
        let payload = format!(
            "{{\"operation\":\"{operation}\",\"map_id\":[[map_id_json]],\
             \"revision\":[[map_revision_json]],\"{field}\":[[selection]]}}"
        );
        let mut mode = json!({
            "name":name,"icon":icon,"selection_type":selection,"id_type":"number",
            "max_selections":count,"repeats_type":"NONE","run_immediately":false,
            "coordinates_rounding":false,
            "variables":{"narwal_command_topic":command_topic(e, "/room_editor/draft", false)},
            "service_call_schema":{"service":"mqtt.publish","evaluate_data_as_template":false,
                "service_data":{"topic":"[[narwal_command_topic]]",
                    "qos":0,"retain":false,"payload":payload}}
        });
        if selection == "ROOM" {
            mode["predefined_selections"] = json!([]);
        }
        modes.push(mode);
    }
    let draft = format!("states['{}']?.attributes", e.room_editor_entity);
    let save_guard = format!(
        "states['{}']?.state === 'draft_ready' && {draft}?.can_save === true \
         && typeof {draft}?.draft_id === 'string' && {draft}.draft_id.length > 0",
        e.room_editor_entity
    );
    let cancel_guard = format!(
        "['draft_ready','draft_blocked','refused'].includes(states['{}']?.state) \
         && typeof {draft}?.draft_id === 'string' && {draft}.draft_id.length > 0",
        e.room_editor_entity
    );
    let button = |save: bool| {
        let operation = if save { "save" } else { "cancel" };
        let guard = if save { &save_guard } else { &cancel_guard };
        let action = format!("${{{guard} ? 'call-service' : 'none'}}");
        json!({"type":"button","name":if save {"Save"} else {"Cancel draft"},
            "icon":if save {"mdi:content-save"} else {"mdi:cancel"},
            "tap_action":{"action":action,"service":"mqtt.publish","service_data":{
                "topic":template_card_topic(e, &format!("/room_editor/{operation}")),
                "qos":0,"retain":false,
                "payload":format!("${{JSON.stringify({{draft_id: {draft}?.draft_id ?? ''}})}}")
            }},
            "hold_action":{"action":"none"},"double_tap_action":{"action":"none"}})
    };
    let actions = json!({
        "type":"custom:config-template-card","entities":[e.room_editor_entity],
        "card":{"type":"horizontal-stack","cards":[
            {"type":"conditional","conditions":[
                {"condition":"state","entity":e.room_editor_entity,"state":"draft_ready"}
            ],"card":button(true)},button(false)
        ]}
    });
    let cards = vec![
        json!({"type":"conditional","conditions":[{"condition":"or","conditions":[
            {"condition":"state","entity":e.room_editor_entity,"state":"unavailable"},
            {"condition":"state","entity":e.room_editor_entity,"state":"unknown"}
        ]}],"card":{"type":"markdown","title":"Room editor unavailable","content":
            "Check the local bridge connection and room-editor setup. \
             Edits cannot be saved until the editor reports ready."}}),
        json!({"type":"markdown","content":
            "**Prepare, review, then Save.** Rename: enter a new name and select one room. \
             Merge: select two adjacent rooms. Split: choose the target room below, then mark \
             two points for the dividing line in Split mode. Press ▶ to prepare the draft.\n\n\
             Selection and text input do not change the saved map. Review the preview and feedback, \
             then **Save** or **Cancel draft** before preparing another edit."}),
        json!({"type":"entities","title":"Room edit","show_header_toggle":false,"entities":[
            {"entity":e.room_name_entity,"name":"New room name"},
            {"entity":e.editor_room_entity,"name":"Room to split"},
            {"entity":e.room_editor_entity,"name":"Editor status"},
            {"type":"attribute","entity":e.room_editor_entity,"attribute":"operation","name":"Operation"},
            {"type":"attribute","entity":e.room_editor_entity,"attribute":"summary","name":"Draft"},
            {"type":"attribute","entity":e.room_editor_entity,"attribute":"message","name":"Feedback"},
            {"type":"attribute","entity":e.room_editor_entity,"attribute":"capabilities","name":"Available edits"}
        ]}),
        json!({"type":"custom:xiaomi-vacuum-map-card","title":"Room editor",
            "entity":e.vacuum_entity,"action_handler_id":handler,
            "map_source":{"camera":e.map_entity},"room_source":{"entity":e.room_catalog_entity},
            "calibration_source":{"identity":true},"map_locked":true,"two_finger_pan":true,
            "clean_selection_on_start":true,"debug":false,"tiles":[],
            "icons":[{"icon":"mdi:selection-remove","tooltip":"Clear room or split-line selection",
                "tap_action":{"action":"fire-dom-event","xiaomi_vacuum_map_card":{
                    "action_handler_id":handler,"action":"selection.clear"}}}],
            "map_modes":modes}),
        json!({"type":"conditional","conditions":[
            {"condition":"state","entity":e.room_editor_entity,"state_not":"idle"},
            {"condition":"state","entity":e.room_editor_entity,"state_not":"unavailable"},
            {"condition":"state","entity":e.room_preview_entity,"state_not":"unavailable"},
            {"condition":"state","entity":e.room_preview_entity,"state_not":"unknown"}
        ],"card":{"type":"picture-entity","entity":e.room_preview_entity,"name":"Draft preview",
            "show_state":false,"show_name":true,"tap_action":{"action":"none"}}}),
        actions,
        json!({"type":"button","name":"Refresh result","icon":"mdi:refresh",
            "tap_action":{"action":"call-service","service":"mqtt.publish","service_data":{
                "topic":format!("{}/room_editor/refresh",e.topic_prefix),
                "payload":"{}","qos":0,"retain":false}},
            "hold_action":{"action":"none"},"double_tap_action":{"action":"none"}}),
        json!({"type":"markdown","content":
            "Save is available only when the backend reports a valid, supported draft. \
             Refusals and an unavailable writer appear above. Saving stays pending until \
             the backend verifies the resulting map. A confirmed edit refreshes room outlines \
             and clears old selections in both views."}),
    ];
    json!({"title":"Rooms","path":"rooms","icon":"mdi:floor-plan",
        "type":"sections","max_columns":2,"sections":[{"type":"grid","column_span":2,
            "cards":[{"type":"vertical-stack","grid_options":{"columns":"full"},"cards":cards}]}]})
}

fn generate(state: &Value, e: &Entities) -> Result<Value, String> {
    validate_entities(e)?;
    current_rooms(state)?;
    let mut translations: Value = serde_json::from_str(include_str!("../../error-labels.json"))
        .map_err(|_| "invalid packaged error labels")?;
    let labels =
        serde_json::to_string(&translations).map_err(|_| "error label serialization failed")?;
    translations["0"] = json!("No error");
    let error_text = format!(
        "{{% set code = states('{}') %}}{{% set error_labels = {} %}}**{{{{ error_labels.get(code, 'Unrecognized robot error.') }}}}**\n\n\
         {{% if code == '3018' %}}Refill and reseat the clean-water tank.{{% elif code == '3019' %}}Empty and reseat the wastewater tank.{{% endif %}}\n\n\
         A new cleaning command is refused until the robot reports the fault has cleared.\n\n*Error code: {{{{ code }}}}*",
        e.error_entity, labels
    );
    let mut cards = vec![
        json!({"type":"conditional","conditions":[
            {"condition":"state","entity":e.error_entity,"state_not":"0"},
            {"condition":"state","entity":e.error_entity,"state_not":"unknown"},
            {"condition":"state","entity":e.error_entity,"state_not":"unavailable"}
        ],"card":{"type":"markdown","title":"Robot needs attention","content":error_text}}),
        json!({"type":"conditional","conditions":[
            {"condition":"state","entity":e.vacuum_entity,"state":"unavailable"}
        ],"card":{"type":"markdown","content":"**J1 is offline.** Waiting for the local connection. Live position and controls will return when the robot reconnects."}}),
    ];
    for mode in ["Vacuum", "Mop"] {
        cards.push(json!({"type":"conditional","conditions":[
            {"condition":"state","entity":e.mode_entity,"state":mode},
            {"condition":"state","entity":e.vacuum_entity,"state_not":"unavailable"},
            {"condition":"state","entity":e.map_entity,"state_not":"unavailable"},
            {"condition":"state","entity":e.map_entity,"state_not":"unknown"}
        ],"card":map_card(mode, e, &translations)}));
    }
    cards.push(map_unavailable_card(e, &translations));
    cards.push(json!({"type":"markdown","content":
        "**Tap a room area to select it; tap it again to remove it.** Check the highlighted rooms, then press ▶ to clean all selected rooms. **Clear room selection** removes every selection.\n\n\
         **Robot:** red circle · **Dock:** white square. Darker areas show robot-reported cleaning passes. Progress and area may reset or retain the previous job after docking."}));
    Ok(
        json!({"title":"Narwal","views":[{"title":"Narwal","path":"overview","icon":"mdi:robot-vacuum",
        "type":"sections","max_columns":2,"sections":[{"type":"grid","column_span":2,
        "cards":[{"type":"vertical-stack","grid_options":{"columns":"full"},"cards":cards}]}]},
        editor_view(e)]}),
    )
}

fn read_json(path: &Path, private: bool) -> Result<Value, String> {
    let metadata = fs::symlink_metadata(path).map_err(|_| "input file unavailable")?;
    require(
        metadata.is_file() && metadata.len() <= 16 * 1024 * 1024,
        "input must be a regular file of at most 16 MiB",
    )?;
    if private {
        require(
            metadata.permissions().mode() & 0o077 == 0,
            "state snapshot must be private (mode 0600)",
        )?;
    }
    let bytes = fs::read(path).map_err(|_| "input file cannot be read")?;
    serde_json::from_slice(&bytes).map_err(|_| "input is not valid JSON".into())
}

fn run() -> Result<(), String> {
    let args: Vec<_> = std::env::args_os().collect();
    require(
        args.len() == 4,
        "usage: narwal-dashboard-config STATE.json ENTITIES.json NEW-DASHBOARD.json",
    )?;
    let input = read_json(Path::new(&args[1]), true)?;
    let state = input.get("local_status").unwrap_or(&input);
    let config = read_json(Path::new(&args[2]), false)?;
    let entities: Entities =
        serde_json::from_value(config).map_err(|_| "invalid entity configuration fields")?;
    let dashboard = generate(state, &entities)?;
    let output = Path::new(&args[3]);
    let parent = output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let metadata = fs::symlink_metadata(parent).map_err(|_| "output directory unavailable")?;
    require(
        metadata.is_dir() && metadata.permissions().mode() & 0o077 == 0,
        "output must be in an existing private directory (mode 0700)",
    )?;
    let mut bytes =
        serde_json::to_vec_pretty(&dashboard).map_err(|_| "dashboard serialization failed")?;
    bytes.push(b'\n');
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(output)
        .map_err(|_| "cannot create private output; existing files are never overwritten")?;
    file.write_all(&bytes)
        .map_err(|_| "dashboard write failed")?;
    file.sync_all().map_err(|_| "dashboard sync failed")?;
    println!(
        "Created private dashboard configuration. No network requests or robot commands were sent."
    );
    Ok(())
}

fn main() {
    if let Err(message) = run() {
        eprintln!("{message}");
        std::process::exit(1);
    }
}
