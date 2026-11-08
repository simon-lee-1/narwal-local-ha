//! Bounded, local room-map drafts. This module never sends a robot command.
//! Room names are HA aliases; native attribute names remain unchanged.
use crate::maps::{DisplayTransform, RoomMap};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use flate2::{Compression, write::ZlibEncoder};
use ring::digest::{Context, SHA256};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::BTreeMap, io::Write};

const MAX_ROOMS: usize = 32;
const MAX_CELLS: usize = 1024 * 1024;
const MAX_RAW_BYTES: usize = 8 * 1024 * 1024;
const MAX_HISTORY: usize = 16;
const MAX_HISTORY_BYTES: usize = 32 * 1024 * 1024;
const MAX_SET_BYTES: usize = 2 * 1024 * 1024;

/// Project a qualified getter candidate or captured wire body into the J1
/// setter contract. The original candidate and its stored angle stay intact.
pub fn validated_set_params(raw: &Value) -> Result<Value, String> {
    validated_set(raw).map(|(params, _, _)| params)
}

/// Confirm the candidate's geometry and native attributes, independently of
/// compression and attribute ordering. Preserve its original angle and dock
/// pose: projecting both sides to the null wire angle would hide changes.
pub fn map_write_matches(candidate: &Value, observed: &Value) -> Result<bool, String> {
    let expected_profile = native_profile(candidate)?;
    let actual_profile = native_profile(observed)?;
    let (_, expected_map, expected_attrs) = validated_set(candidate)?;
    let (_, actual_map, actual_attrs) = validated_set(observed)?;
    if expected_profile != actual_profile {
        return Ok(false);
    }
    let geometry_equal = ["origin_x", "origin_y", "resolution"]
        .iter()
        .all(|key| candidate["map_data"][key].as_f64() == observed["map_data"][key].as_f64());
    let pose_equal = match expected_profile {
        NativeProfile::Getter => {
            candidate["map_data"]["origin_angle"].as_f64()
                == observed["map_data"]["origin_angle"].as_f64()
                && getter_dock_pose(candidate)? == getter_dock_pose(observed)?
        }
        NativeProfile::CapturedWire => {
            candidate["map_data"]["origin_angle"] == observed["map_data"]["origin_angle"]
        }
    };
    Ok(candidate["map_id"] == observed["map_id"]
        && expected_map.width == actual_map.width
        && expected_map.height == actual_map.height
        && geometry_equal
        && pose_equal
        && expected_map.room_layer() == actual_map.room_layer()
        && expected_attrs == actual_attrs)
}

type ValidatedSet = (Value, RoomMap, BTreeMap<u64, Value>);

#[derive(Clone, Copy, PartialEq, Eq)]
enum NativeProfile {
    Getter,
    CapturedWire,
}

fn native_profile(raw: &Value) -> Result<NativeProfile, String> {
    let wrapper = raw
        .get("room_attr")
        .and_then(Value::as_object)
        .filter(|wrapper| wrapper.len() == 1)
        .ok_or("editor_native_attributes_unqualified")?;
    let attrs = wrapper
        .get("attr")
        .and_then(Value::as_array)
        .filter(|attrs| !attrs.is_empty() && attrs.len() <= MAX_ROOMS)
        .ok_or("editor_native_attributes_unqualified")?;
    let mut profile = None;
    for attr in attrs {
        let count = attr
            .as_object()
            .ok_or("editor_native_attributes_unqualified")?
            .len();
        let current = match count {
            3 => NativeProfile::Getter,
            6 => NativeProfile::CapturedWire,
            _ => return Err("editor_native_attributes_unqualified".into()),
        };
        if profile.is_some_and(|previous| previous != current) {
            return Err("editor_native_attributes_unqualified".into());
        }
        profile = Some(current);
    }
    profile.ok_or("editor_native_attributes_unqualified".into())
}

fn getter_dock_pose(raw: &Value) -> Result<Option<[f64; 3]>, String> {
    let fields = ["base_station_x", "base_station_y", "base_station_angle"];
    let data = raw
        .get("map_data")
        .and_then(Value::as_object)
        .ok_or("editor_map_data_required")?;
    if fields.iter().all(|key| !data.contains_key(*key)) {
        return Ok(None);
    }
    let mut pose = [0.0; 3];
    for (index, key) in fields.iter().enumerate() {
        pose[index] = data
            .get(*key)
            .and_then(Value::as_f64)
            .filter(|value| value.is_finite())
            .ok_or("editor_map_geometry_unqualified")?;
    }
    Ok(Some(pose))
}

fn validate_getter_fields(raw: &Value) -> Result<(), String> {
    let root = raw.as_object().ok_or("editor_map_data_required")?;
    if root.keys().any(|key| {
        ![
            "map_id",
            "map_data",
            "room_attr",
            "clean_order",
            "humidity",
            "user_forbidden",
        ]
        .contains(&key.as_str())
    }) {
        return Err("editor_getter_metadata_unqualified".into());
    }
    let data = raw
        .get("map_data")
        .and_then(Value::as_object)
        .ok_or("editor_map_data_required")?;
    if data.keys().any(|key| {
        ![
            "width",
            "height",
            "origin_x",
            "origin_y",
            "origin_angle",
            "resolution",
            "room_layer",
            "base_station_x",
            "base_station_y",
            "base_station_angle",
        ]
        .contains(&key.as_str())
    }) {
        return Err("editor_getter_metadata_unqualified".into());
    }
    getter_dock_pose(raw)?;
    Ok(())
}

fn getter_wire_type(value: &Value) -> Result<i64, String> {
    // These four pairs were observed for matching room/map identities in an
    // acknowledged setter and a later getter. No general byte cast is assumed.
    match value.as_i64() {
        Some(254) => Ok(-2),
        Some(253) => Ok(-3),
        Some(252) => Ok(-4),
        Some(251) => Ok(-5),
        _ => Err("editor_native_attributes_unqualified".into()),
    }
}

fn validated_set(raw: &Value) -> Result<ValidatedSet, String> {
    check_json(raw)?;
    let profile = native_profile(raw)?;
    if profile == NativeProfile::Getter {
        validate_getter_fields(raw)?;
    }
    let map_id = raw.get("map_id").ok_or("editor_map_id_required")?;
    if !map_id.as_u64().is_some()
        && !map_id.as_str().is_some_and(|value| {
            !value.is_empty() && value.len() <= 256 && !value.chars().any(char::is_control)
        })
    {
        return Err("editor_map_id_invalid".into());
    }
    let data = raw
        .get("map_data")
        .and_then(Value::as_object)
        .ok_or("editor_map_data_required")?;
    let mut projected = serde_json::Map::new();
    for key in [
        "width",
        "height",
        "origin_x",
        "origin_y",
        "origin_angle",
        "resolution",
        "room_layer",
    ] {
        projected.insert(
            key.into(),
            data.get(key).ok_or("editor_map_geometry_required")?.clone(),
        );
    }
    if !["origin_x", "origin_y"].iter().all(|key| {
        data.get(*key)
            .and_then(Value::as_f64)
            .is_some_and(f64::is_finite)
    }) || !data
        .get("resolution")
        .and_then(Value::as_f64)
        .is_some_and(|value| value.is_finite() && value > 0.0)
        || match profile {
            NativeProfile::Getter => !data
                .get("origin_angle")
                .and_then(Value::as_f64)
                .is_some_and(f64::is_finite),
            NativeProfile::CapturedWire => data.get("origin_angle") != Some(&Value::Null),
        }
    {
        return Err("editor_map_geometry_unqualified".into());
    }
    // The acknowledged setter used null. This does not establish angle
    // persistence; readback above compares the original getter's finite angle.
    projected.insert("origin_angle".into(), Value::Null);
    let wrapper = raw
        .get("room_attr")
        .and_then(Value::as_object)
        .filter(|wrapper| wrapper.len() == 1)
        .ok_or("editor_native_attributes_unqualified")?;
    let attrs = wrapper
        .get("attr")
        .and_then(Value::as_array)
        .filter(|attrs| !attrs.is_empty() && attrs.len() <= MAX_ROOMS)
        .ok_or("editor_native_attributes_unqualified")?;
    let mut wire_attrs = Vec::with_capacity(attrs.len());
    for attr in attrs {
        let attr = attr
            .as_object()
            .ok_or("editor_native_attributes_unqualified")?;
        if !attr
            .get("room_id")
            .and_then(Value::as_u64)
            .is_some_and(|id| id <= 62)
            || attr.get("name").and_then(Value::as_str) != Some("")
        {
            return Err("editor_native_attributes_unqualified".into());
        }
        match profile {
            NativeProfile::Getter => {
                let room_type = getter_wire_type(
                    attr.get("type")
                        .ok_or("editor_native_attributes_unqualified")?,
                )?;
                wire_attrs.push(json!({
                    "room_id":attr["room_id"], "type":room_type, "name":"",
                    "cType":-2, "cName":"", "keepName":false
                }));
            }
            NativeProfile::CapturedWire => {
                if !matches!(attr.get("type").and_then(Value::as_i64), Some(-5..=-2))
                    || attr.get("cType").and_then(Value::as_i64) != Some(-2)
                    || attr.get("keepName").and_then(Value::as_bool) != Some(false)
                    || attr.get("cName").and_then(Value::as_str) != Some("")
                {
                    return Err("editor_native_attributes_unqualified".into());
                }
                wire_attrs.push(Value::Object(attr.clone()));
            }
        }
    }
    // This exact order and four-field body were captured with a correlated
    // encrypted code-zero response. clean_order is a getter-only field here.
    let params = json!({
        "map_id":map_id,
        "operations":["map_data","room_attr"],
        "map_data":projected,
        "room_attr":{"attr":wire_attrs}
    });
    if serde_json::to_vec(&params)
        .map_err(|_| "editor_map_set_encoding")?
        .len()
        > MAX_SET_BYTES
    {
        return Err("editor_map_set_too_large".into());
    }
    let (map, catalog, geometry_refusal) = parse_raw(&params)?;
    if geometry_refusal.is_some() {
        return Err("editor_unknown_room_membership".into());
    }
    Ok((params, map, catalog))
}

/// Pixel-edge coordinates in the displayed image, including its outer edges.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DisplayLine {
    pub start: [f64; 2],
    pub end: [f64; 2],
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
pub enum EditAction {
    RenameAlias {
        room_id: u64,
        name: String,
    },
    Split {
        room_id: u64,
        new_room_id: u64,
    },
    Merge {
        kept_room_id: u64,
        removed_room_id: u64,
    },
}

#[derive(Clone, Debug, Serialize)]
pub struct DraftRoom {
    pub id: u64,
    pub name: String,
    pub native_name: Option<String>,
    pub cells: usize,
    pub overlap_cells: usize,
    pub label: Option<[u32; 2]>,
    pub outline: Vec<[u32; 2]>,
    pub outline_error: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct DraftPreview {
    pub map_id: Value,
    pub base_revision: String,
    pub revision: String,
    pub width: u32,
    pub height: u32,
    pub action_count: usize,
    pub can_undo: bool,
    pub geometry_available: bool,
    pub geometry_refusal: Option<&'static str>,
    pub rooms: Vec<DraftRoom>,
    pub actions: Vec<EditAction>,
}

#[derive(Clone)]
struct Snapshot {
    raw: Value,
    aliases: BTreeMap<u64, String>,
    bytes: usize,
}

#[derive(Clone)]
struct DraftState {
    raw: Value,
    map: RoomMap,
    aliases: BTreeMap<u64, String>,
    catalog: BTreeMap<u64, Value>,
    geometry_refusal: Option<&'static str>,
    revision: String,
}

/// A draft is bound to the original decoded map and metadata, independently
/// of zlib compression. Callers must recheck the base before any eventual save.
/// A draft is not proof of a supported map-set operation or a successful write.
#[derive(Clone)]
pub struct RoomDraft {
    base_raw: Value,
    base_aliases: BTreeMap<u64, String>,
    base_revision: String,
    current: DraftState,
    history: Vec<Snapshot>,
    history_bytes: usize,
    actions: Vec<EditAction>,
}

impl RoomDraft {
    pub fn new(raw: Value, aliases: BTreeMap<u64, String>) -> Result<Self, String> {
        let current = DraftState::new(raw.clone(), aliases.clone())?;
        let base_revision = map_revision(&raw, current.map.room_layer())?;
        Ok(Self {
            base_raw: raw,
            base_aliases: aliases,
            base_revision,
            current,
            history: Vec::new(),
            history_bytes: 0,
            actions: Vec::new(),
        })
    }

    pub fn revision_for(raw: &Value) -> Result<String, String> {
        let (map, _, _) = parse_raw(raw)?;
        map_revision(raw, map.room_layer())
    }

    pub fn base_revision(&self) -> &str {
        &self.base_revision
    }
    pub fn revision(&self) -> &str {
        &self.current.revision
    }
    pub fn base_raw(&self) -> &Value {
        &self.base_raw
    }
    pub fn base_aliases(&self) -> &BTreeMap<u64, String> {
        &self.base_aliases
    }
    pub fn raw_candidate(&self) -> &Value {
        &self.current.raw
    }
    pub fn aliases(&self) -> &BTreeMap<u64, String> {
        &self.current.aliases
    }
    pub fn action_count(&self) -> usize {
        self.actions.len()
    }
    pub fn actions(&self) -> &[EditAction] {
        &self.actions
    }
    pub fn geometry_changed(&self) -> bool {
        self.actions
            .iter()
            .any(|action| !matches!(action, EditAction::RenameAlias { .. }))
    }
    pub fn aliases_changed(&self) -> bool {
        self.current.aliases != self.base_aliases
    }
    pub fn dimensions(&self) -> (u32, u32) {
        (self.current.map.width, self.current.map.height)
    }

    pub fn validate_base(&self, current_raw: &Value) -> Result<(), String> {
        if Self::revision_for(current_raw)? != self.base_revision {
            return Err("editor_base_revision_changed".into());
        }
        Ok(())
    }

    /// A rendering-only copy. It must never be used as a robot-name write.
    pub fn preview_raw(&self) -> Value {
        let mut raw = self.current.raw.clone();
        if let Some(attrs) = raw
            .pointer_mut("/room_attr/attr")
            .and_then(Value::as_array_mut)
        {
            for attr in attrs {
                if let Some(name) = attr
                    .get("room_id")
                    .and_then(Value::as_u64)
                    .and_then(|id| self.current.aliases.get(&id))
                {
                    attr["name"] = json!(name);
                }
            }
        }
        // Some getters retain default-order references to obsolete rooms.
        // Do not turn those references into selectable, geometry-free rooms.
        filter_orders(&mut raw, |id| self.current.catalog.contains_key(&id));
        raw
    }

    pub fn preview(&self, transform: DisplayTransform) -> Result<DraftPreview, String> {
        let map = crate::maps::parse(&self.preview_raw())?;
        let outlines = map.display_room_outlines(transform);
        let mut rooms = Vec::with_capacity(self.current.catalog.len());
        for (&id, attr) in &self.current.catalog {
            let mask = room_mask(id)?;
            let cells = self
                .current
                .map
                .room_layer()
                .iter()
                .filter(|value| **value & mask != 0)
                .count();
            let overlap_cells = self
                .current
                .map
                .room_layer()
                .iter()
                .filter(|value| **value & mask != 0 && (**value & !1).count_ones() > 1)
                .count();
            let outline = outlines.iter().find(|room| room.room_id == id);
            rooms.push(DraftRoom {
                id,
                name: effective_name(id, attr, &self.current.aliases),
                native_name: attr.get("name").and_then(Value::as_str).map(str::to_owned),
                cells,
                overlap_cells,
                label: outline.map(|room| [room.label_x, room.label_y]),
                outline: outline.map(|room| room.outline.clone()).unwrap_or_default(),
                outline_error: outline
                    .and_then(|room| room.outline_error)
                    .map(str::to_owned),
            });
        }
        Ok(DraftPreview {
            map_id: map.map_id,
            base_revision: self.base_revision.clone(),
            revision: self.current.revision.clone(),
            width: map.width,
            height: map.height,
            action_count: self.actions.len(),
            can_undo: !self.history.is_empty(),
            geometry_available: self.current.geometry_refusal.is_none(),
            geometry_refusal: self.current.geometry_refusal,
            rooms,
            actions: self.actions.clone(),
        })
    }

    pub fn rename_alias(&mut self, room_id: u64, name: &str) -> Result<(), String> {
        self.require_room(room_id)?;
        let name = checked_name(name.trim())?;
        if self.current.aliases.get(&room_id) == Some(&name) {
            return Ok(());
        }
        let mut aliases = self.current.aliases.clone();
        aliases.insert(room_id, name.clone());
        self.install(
            self.current.raw.clone(),
            aliases,
            EditAction::RenameAlias { room_id, name },
        )
    }

    /// Split membership by the line through its endpoints. The endpoints must
    /// span the selected room; a short gesture cannot silently extend a cut.
    /// Cells on the nonpositive side keep the original room ID. No room,
    /// background, or unknown membership bit is removed from another room.
    pub fn split(
        &mut self,
        room_id: u64,
        line: DisplayLine,
        transform: DisplayTransform,
    ) -> Result<u64, String> {
        self.require_geometry()?;
        self.require_room(room_id)?;
        if self.current.catalog.len() >= MAX_ROOMS {
            return Err("editor_room_limit".into());
        }
        let new_room_id = (0..63)
            .find(|id| !self.current.catalog.contains_key(id))
            .ok_or("editor_room_id_exhausted")?;
        let (width, height) = self.dimensions();
        let line = grid_line(line, transform, width, height)?;
        let dx = line.end[0] - line.start[0];
        let dy = line.end[1] - line.start[1];
        let squared_length = dx * dx + dy * dy;
        let tolerance = squared_length.max(1.0) * 1e-12;
        let original_mask = room_mask(room_id)?;
        let new_mask = room_mask(new_room_id)?;
        let mut layer = self.current.map.room_layer().to_vec();
        let mut selected = 0usize;
        let mut moved = 0usize;
        for (index, value) in layer.iter_mut().enumerate() {
            if *value & original_mask == 0 {
                continue;
            }
            selected += 1;
            let x = (index % width as usize) as f64 + 0.5 - line.start[0];
            let y = (index / width as usize) as f64 + 0.5 - line.start[1];
            let along = dx * x + dy * y;
            if along < -tolerance || along > squared_length + tolerance {
                return Err("editor_split_line_too_short".into());
            }
            if dx * y - dy * x > tolerance {
                *value = (*value & !original_mask) | new_mask;
                moved += 1;
            }
        }
        if selected == 0 || moved == 0 || moved == selected {
            return Err("editor_split_requires_two_regions".into());
        }
        if !regions_touch(&layer, width as usize, original_mask, new_mask)
            || !connected(&layer, width as usize, original_mask)
            || !connected(&layer, width as usize, new_mask)
        {
            return Err("editor_split_regions_not_connected".into());
        }
        let mut raw = self.current.raw.clone();
        raw["map_data"]["room_layer"] = json!(encode_layer(&layer)?);
        // Only the identifier is derived. Native room metadata is not guessed.
        let mut new_attr = self.current.catalog[&room_id].clone();
        new_attr["room_id"] = json!(new_room_id);
        raw["room_attr"]["attr"]
            .as_array_mut()
            .ok_or("editor_room_attributes_required")?
            .push(new_attr);
        let mut aliases = self.current.aliases.clone();
        aliases.insert(new_room_id, self.unique_default_name(new_room_id));
        self.install(
            raw,
            aliases,
            EditAction::Split {
                room_id,
                new_room_id,
            },
        )?;
        Ok(new_room_id)
    }

    /// The surviving room keeps its ID and complete native attribute record.
    pub fn merge(&mut self, keep_id: u64, remove_id: u64) -> Result<(), String> {
        self.require_geometry()?;
        self.require_room(keep_id)?;
        self.require_room(remove_id)?;
        if keep_id == remove_id {
            return Err("editor_merge_requires_two_rooms".into());
        }
        let keep_mask = room_mask(keep_id)?;
        let remove_mask = room_mask(remove_id)?;
        let width = self.current.map.width as usize;
        let mut layer = self.current.map.room_layer().to_vec();
        if !regions_touch(&layer, width, keep_mask, remove_mask) {
            return Err("editor_merge_rooms_not_adjacent".into());
        }
        for value in &mut layer {
            if *value & remove_mask != 0 {
                *value = (*value & !remove_mask) | keep_mask;
            }
        }
        if !connected(&layer, width, keep_mask) {
            return Err("editor_merge_region_not_connected".into());
        }
        let mut raw = self.current.raw.clone();
        raw["map_data"]["room_layer"] = json!(encode_layer(&layer)?);
        raw["room_attr"]["attr"]
            .as_array_mut()
            .ok_or("editor_room_attributes_required")?
            .retain(|attr| attr.get("room_id").and_then(Value::as_u64) != Some(remove_id));
        // Read-model cleanup only. The observed setter does not send this
        // getter field; a future writer must select its verified wire fields.
        filter_orders(&mut raw, |id| id != remove_id);
        let mut aliases = self.current.aliases.clone();
        aliases.remove(&remove_id);
        self.install(
            raw,
            aliases,
            EditAction::Merge {
                kept_room_id: keep_id,
                removed_room_id: remove_id,
            },
        )
    }

    pub fn undo(&mut self) -> Result<(), String> {
        let previous = self.history.last().ok_or("editor_nothing_to_undo")?;
        let state = DraftState::new(previous.raw.clone(), previous.aliases.clone())?;
        let bytes = previous.bytes;
        self.history.pop();
        self.history_bytes -= bytes;
        self.actions.pop();
        self.current = state;
        Ok(())
    }

    fn require_room(&self, room_id: u64) -> Result<(), String> {
        self.current
            .catalog
            .contains_key(&room_id)
            .then_some(())
            .ok_or_else(|| "editor_unknown_room".into())
    }

    fn require_geometry(&self) -> Result<(), String> {
        match self.current.geometry_refusal {
            Some(reason) => Err(reason.into()),
            None => Ok(()),
        }
    }

    fn unique_default_name(&self, room_id: u64) -> String {
        let names = self
            .current
            .catalog
            .iter()
            .map(|(&id, attr)| effective_name(id, attr, &self.current.aliases))
            .collect::<Vec<_>>();
        let base = format!("Room {}", room_id + 1);
        let mut candidate = base.clone();
        let mut suffix = 2;
        while names.contains(&candidate) {
            candidate = format!("{base} ({suffix})");
            suffix += 1;
        }
        candidate
    }

    fn install(
        &mut self,
        raw: Value,
        aliases: BTreeMap<u64, String>,
        action: EditAction,
    ) -> Result<(), String> {
        if self.history.len() >= MAX_HISTORY {
            return Err("editor_history_limit".into());
        }
        let bytes = serde_json::to_vec(&self.current.raw)
            .map_err(|_| "editor_map_encoding")?
            .len()
            + serde_json::to_vec(&self.current.aliases)
                .map_err(|_| "editor_alias_encoding")?
                .len();
        if self.history_bytes.saturating_add(bytes) > MAX_HISTORY_BYTES {
            return Err("editor_history_limit".into());
        }
        // Parse and validate the entire result before changing any draft state.
        let state = DraftState::new(raw, aliases)?;
        self.history.push(Snapshot {
            raw: self.current.raw.clone(),
            aliases: self.current.aliases.clone(),
            bytes,
        });
        self.history_bytes += bytes;
        self.actions.push(action);
        self.current = state;
        Ok(())
    }
}

impl DraftState {
    fn new(raw: Value, aliases: BTreeMap<u64, String>) -> Result<Self, String> {
        let (map, catalog, geometry_refusal) = parse_raw(&raw)?;
        for (&id, name) in &aliases {
            if !catalog.contains_key(&id) {
                return Err("editor_unknown_room_alias".into());
            }
            if checked_name(name)? != *name {
                return Err("editor_invalid_room_name".into());
            }
        }
        let mut names = std::collections::BTreeSet::new();
        for (&id, attr) in &catalog {
            let name = checked_name(&effective_name(id, attr, &aliases))?;
            if !names.insert(name) {
                return Err("editor_duplicate_room_name".into());
            }
        }
        let base = map_revision(&raw, map.room_layer())?;
        let mut digest = Context::new(&SHA256);
        digest.update(b"narwal-room-editor-draft-v1\0");
        digest.update(base.as_bytes());
        digest.update(&serde_json::to_vec(&aliases).map_err(|_| "editor_alias_encoding")?);
        let revision = hex_digest(digest);
        Ok(Self {
            raw,
            map,
            aliases,
            catalog,
            geometry_refusal,
            revision,
        })
    }
}

type ParsedMap = (RoomMap, BTreeMap<u64, Value>, Option<&'static str>);

fn parse_raw(raw: &Value) -> Result<ParsedMap, String> {
    check_json(raw)?;
    let width = raw
        .pointer("/map_data/width")
        .and_then(Value::as_u64)
        .ok_or("editor_invalid_dimensions")?;
    let height = raw
        .pointer("/map_data/height")
        .and_then(Value::as_u64)
        .ok_or("editor_invalid_dimensions")?;
    if width == 0
        || height == 0
        || width
            .checked_mul(height)
            .is_none_or(|cells| cells > MAX_CELLS as u64)
    {
        return Err("editor_map_too_large".into());
    }
    let attrs = raw
        .pointer("/room_attr/attr")
        .and_then(Value::as_array)
        .filter(|attrs| !attrs.is_empty() && attrs.len() <= MAX_ROOMS)
        .ok_or("editor_room_attributes_required")?;
    let mut catalog = BTreeMap::new();
    let mut known_bits = 1u64;
    for attr in attrs {
        let id = attr
            .get("room_id")
            .and_then(Value::as_u64)
            .ok_or("editor_invalid_room_id")?;
        known_bits |= room_mask(id)?;
        if catalog.insert(id, attr.clone()).is_some() {
            return Err("editor_duplicate_room_id".into());
        }
    }
    let map = crate::maps::parse(raw)?;
    let geometry_refusal = map
        .room_layer()
        .iter()
        .any(|value| value & !known_bits != 0)
        .then_some("editor_unknown_room_membership");
    Ok((map, catalog, geometry_refusal))
}

fn check_json(raw: &Value) -> Result<(), String> {
    let mut pending = vec![(raw, 0usize)];
    let mut nodes = 0usize;
    while let Some((value, depth)) = pending.pop() {
        nodes += 1;
        if depth > 32 || nodes > 1024 * 1024 {
            return Err("editor_map_too_complex".into());
        }
        match value {
            Value::Object(fields) => pending.extend(fields.values().map(|v| (v, depth + 1))),
            Value::Array(items) => pending.extend(items.iter().map(|v| (v, depth + 1))),
            _ => {}
        }
    }
    if serde_json::to_vec(raw)
        .map_err(|_| "editor_map_encoding")?
        .len()
        > MAX_RAW_BYTES
    {
        return Err("editor_map_too_large".into());
    }
    Ok(())
}

fn room_mask(room_id: u64) -> Result<u64, String> {
    // This format reserves bit zero for background, leaving IDs 0 through 62.
    room_id
        .checked_add(1)
        .filter(|bit| *bit < 64)
        .map(|bit| 1u64 << bit)
        .ok_or_else(|| "editor_room_id_not_encodable".into())
}

fn checked_name(name: &str) -> Result<String, String> {
    if name.trim().is_empty()
        || name != name.trim()
        || name.len() > 128
        || name.chars().any(char::is_control)
    {
        return Err("editor_invalid_room_name".into());
    }
    Ok(name.to_owned())
}

fn effective_name(room_id: u64, attr: &Value, aliases: &BTreeMap<u64, String>) -> String {
    aliases
        .get(&room_id)
        .cloned()
        .or_else(|| {
            attr.get("name")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map(str::to_owned)
        })
        .unwrap_or_else(|| format!("Room {}", room_id + 1))
}

fn grid_line(
    mut line: DisplayLine,
    transform: DisplayTransform,
    width: u32,
    height: u32,
) -> Result<DisplayLine, String> {
    for point in [&mut line.start, &mut line.end] {
        if !point[0].is_finite()
            || !point[1].is_finite()
            || point[0] < 0.0
            || point[0] > width as f64
            || point[1] < 0.0
            || point[1] > height as f64
        {
            return Err("editor_invalid_split_line".into());
        }
        *point = match transform {
            DisplayTransform::Identity => *point,
            DisplayTransform::Rotate180 => [width as f64 - point[0], height as f64 - point[1]],
            DisplayTransform::FlipHorizontal => [width as f64 - point[0], point[1]],
            DisplayTransform::FlipVertical => [point[0], height as f64 - point[1]],
        };
    }
    if (line.end[0] - line.start[0]).hypot(line.end[1] - line.start[1]) < 1.0 {
        return Err("editor_invalid_split_line".into());
    }
    Ok(line)
}

fn neighbors(index: usize, width: usize, length: usize) -> impl Iterator<Item = usize> {
    let x = index % width;
    [
        (x > 0).then(|| index - 1),
        (x + 1 < width).then(|| index + 1),
        (index >= width).then(|| index - width),
        (index + width < length).then(|| index + width),
    ]
    .into_iter()
    .flatten()
}

fn regions_touch(layer: &[u64], width: usize, a: u64, b: u64) -> bool {
    layer.iter().enumerate().any(|(index, value)| {
        value & a != 0
            && (value & b != 0
                || neighbors(index, width, layer.len()).any(|neighbor| layer[neighbor] & b != 0))
    })
}

fn connected(layer: &[u64], width: usize, mask: u64) -> bool {
    let Some(first) = layer.iter().position(|value| value & mask != 0) else {
        return false;
    };
    let count = layer.iter().filter(|value| **value & mask != 0).count();
    let mut visited = vec![false; layer.len()];
    let mut queue = Vec::with_capacity(count.min(4096));
    visited[first] = true;
    queue.push(first);
    let mut position = 0usize;
    while position < queue.len() {
        for neighbor in neighbors(queue[position], width, layer.len()) {
            if !visited[neighbor] && layer[neighbor] & mask != 0 {
                visited[neighbor] = true;
                queue.push(neighbor);
            }
        }
        position += 1;
    }
    queue.len() == count
}

fn encode_layer(layer: &[u64]) -> Result<String, String> {
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
    serde_json::to_writer(&mut encoder, layer).map_err(|_| "editor_layer_encoding")?;
    encoder.flush().map_err(|_| "editor_layer_encoding")?;
    Ok(STANDARD.encode(encoder.finish().map_err(|_| "editor_layer_encoding")?))
}

fn filter_orders(raw: &mut Value, keep: impl Fn(u64) -> bool) {
    if let Some(orders) = raw.get_mut("clean_order").and_then(Value::as_object_mut) {
        for order in orders.values_mut().filter_map(Value::as_array_mut) {
            order.retain(|id| id.as_u64().is_some_and(&keep));
        }
    }
}

fn canonical(value: &Value) -> Value {
    match value {
        Value::Object(fields) => Value::Object(
            fields
                .iter()
                .collect::<BTreeMap<_, _>>()
                .into_iter()
                .map(|(key, value)| (key.clone(), canonical(value)))
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(canonical).collect()),
        other => other.clone(),
    }
}

fn map_revision(raw: &Value, layer: &[u64]) -> Result<String, String> {
    let mut metadata = raw.clone();
    metadata["map_data"]["room_layer"] = Value::Null;
    let bytes = serde_json::to_vec(&canonical(&metadata)).map_err(|_| "editor_map_encoding")?;
    let mut digest = Context::new(&SHA256);
    digest.update(b"narwal-plan-map-v1\0");
    digest.update(&(bytes.len() as u64).to_le_bytes());
    digest.update(&bytes);
    digest.update(&(layer.len() as u64).to_le_bytes());
    for cell in layer {
        digest.update(&cell.to_le_bytes());
    }
    Ok(hex_digest(digest))
}

fn hex_digest(digest: Context) -> String {
    digest
        .finish()
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
