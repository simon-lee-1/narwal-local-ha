//! Bounded parser for the public legacy Narwal room-layer format.
//! Robot and base positions use the upstream legacy direct map-coordinate
//! transform; origin_angle is metadata, not an extra rotation of those points.
use base64::{Engine as _, engine::general_purpose::STANDARD};
use flate2::{Decompress, FlushDecompress, Status};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

const MAX_PIXELS: usize = 4 * 1024 * 1024;
const MAX_COMPRESSED: usize = 16 * 1024 * 1024;
const MAX_EXPANDED: usize = 64 * 1024 * 1024;
const MAX_ROOMS: usize = 4096;
const MAX_OUTLINE_EDGES: usize = 65_536;
const MAX_OUTLINE_VERTICES: usize = 4096;
pub(crate) const MAX_TRAIL_JOIN_PIXELS: i32 = 64;
const PALETTE: [[u8; 3]; 16] = [
    [191, 219, 254],
    [167, 243, 208],
    [253, 230, 138],
    [253, 186, 116],
    [249, 168, 212],
    [196, 181, 253],
    [147, 197, 253],
    [110, 231, 183],
    [252, 211, 77],
    [251, 146, 60],
    [244, 114, 182],
    [167, 139, 250],
    [125, 211, 252],
    [94, 234, 212],
    [190, 242, 100],
    [216, 180, 254],
];

#[derive(Clone, Debug, Serialize)]
pub struct Room {
    pub room_id: u64,
    #[serde(rename = "type")]
    pub room_type: Option<i64>,
    pub name: Option<String>,
    pub source: &'static str,
    pub label_x: Option<f64>,
    pub label_y: Option<f64>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

/// A measured position, with an explicit break before unknown movement.
/// This is a route trace, not a cleaned-area mask or robot-width footprint.
#[derive(Clone, Debug)]
pub(crate) struct TrailPoint {
    pub point: Point,
    pub starts_segment: bool,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DisplayTransform {
    #[default]
    Identity,
    #[serde(rename = "rotate180")]
    Rotate180,
    FlipHorizontal,
    FlipVertical,
}
impl DisplayTransform {
    fn pixel(self, width: u32, height: u32, x: u32, y: u32) -> (u32, u32) {
        match self {
            Self::Identity => (x, y),
            Self::Rotate180 => (width - 1 - x, height - 1 - y),
            Self::FlipHorizontal => (width - 1 - x, y),
            Self::FlipVertical => (x, height - 1 - y),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct MapDisplay {
    pub transform: DisplayTransform,
    pub room_labels: bool,
    /// Paint the optional pose-history line. Hidden by default; reported
    /// cleaning shading and route telemetry remain independent of this setting.
    pub show_trail: bool,
}

#[derive(Debug, Serialize)]
pub struct DisplayLabel {
    pub room_id: u64,
    pub text: String,
    pub source: &'static str,
    pub x: u32,
    pub y: u32,
}

/// A card-selectable room boundary. Coordinates are displayed image pixels.
/// An omitted outline keeps an honest label without filling unsupported holes.
#[derive(Clone, Debug, Serialize)]
pub struct DisplayRoomOutline {
    pub room_id: u64,
    pub name: String,
    pub label_x: u32,
    pub label_y: u32,
    pub outline: Vec<[u32; 2]>,
    pub components: usize,
    pub omitted_cells: usize,
    pub outline_error: Option<&'static str>,
}

#[derive(Clone, Debug, Serialize)]
pub struct RoomMap {
    pub map_id: Value,
    pub rooms: Vec<Room>,
    pub clean_order: Value,
    pub width: u32,
    pub height: u32,
    pub resolution: Option<f64>,
    pub origin_x: Option<f64>,
    pub origin_y: Option<f64>,
    pub origin_angle: Option<f64>,
    pub dock: Option<Point>,
    pub layer_encoding: &'static str,
    #[serde(skip_serializing)]
    layer: Vec<u64>,
    #[serde(skip_serializing)]
    room_outlines: Vec<DisplayRoomOutline>,
}


/// A bounded device-reported cleaning grid, independent of room-map geometry.
#[derive(Clone)]
pub struct CleanLayer {
    map_id: Value,
    width: u32,
    height: u32,
    resolution: f64,
    origin_x: f64,
    origin_y: f64,
    origin_angle: f64,
    cells: Vec<u8>,
    pass_cells: [usize; 4],
}
const MAX_CLEAN_CELLS: usize = 1024 * 1024;
const MAX_CLEAN_ENCODED: usize = 1024 * 1024;
const MAX_CLEAN_EXPANDED: usize = 4 * 1024 * 1024;

/// Admit the additional value 5 observed in vacuum telemetry, without assigning
/// it a cleaning meaning. The renderer shades only 1..=3, so 5 is transparent.
pub fn parse_reported_clean_layer(value: &Value) -> Result<CleanLayer, String> {
    parse_clean_layer_inner(value, true)
}

/// Decode the optional `/pita/map` result's own cleaning grid. Each packed
/// unsigned word holds four low-nibble-first cells. Values 1..=3 report passes;
/// 0 and 4 are left unshaded because 4's meaning is not qualified.
pub fn parse_clean_layer(value: &Value) -> Result<CleanLayer, String> {
    parse_clean_layer_inner(value, false)
}

fn parse_clean_layer_inner(
    value: &Value,
    allow_unclassified_five: bool,
) -> Result<CleanLayer, String> {
    let map_id = match value.get("map_id") {
        Some(id @ Value::Number(_)) => id.clone(),
        Some(id @ Value::String(text)) if !text.is_empty() && text.len() <= 256 => id.clone(),
        _ => return Err("cleaning map_id must be a supplied number or nonempty string".into()),
    };
    let dimension = |key| -> Result<u32, String> {
        value
            .get(key)
            .and_then(Value::as_f64)
            .filter(|n| n.is_finite() && *n > 0. && n.fract() == 0. && *n <= MAX_CLEAN_CELLS as f64)
            .map(|n| n as u32)
            .ok_or_else(|| format!("invalid cleaning {key}"))
    };
    let width = dimension("width")?;
    let height = dimension("height")?;
    let cell_count = (width as usize)
        .checked_mul(height as usize)
        .filter(|n| *n <= MAX_CLEAN_CELLS)
        .ok_or("cleaning grid exceeds cell limit")?;
    let required_number = |key| -> Result<f64, String> {
        optional_number(value, key)?.ok_or_else(|| format!("cleaning {key} is required"))
    };
    let resolution = required_number("resolution")?;
    if resolution <= 0. {
        return Err("cleaning resolution must be positive".into());
    }
    let origin_x = required_number("origin_x")?;
    let origin_y = required_number("origin_y")?;
    let origin_angle = required_number("origin_angle")?;
    let encoded = value
        .get("show_layer")
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty() && text.len() <= MAX_CLEAN_ENCODED)
        .ok_or("show_layer must be nonempty base64 within encoded size limit")?;
    let compressed = STANDARD
        .decode(encoded)
        .map_err(|_| "show_layer is not strict standard base64")?;
    let word_count = cell_count.div_ceil(4);
    // A u16 needs at most five decimal digits, plus comma and whitespace.
    // Geometry also bounds decompression for tiny declared grids.
    let expanded_limit = MAX_CLEAN_EXPANDED.min(word_count * 7 + 2);
    let decoded = inflate_exact(&compressed, expanded_limit)
        .map_err(|error| error.replacen("room_layer", "show_layer", 1))?;
    let words: Vec<u16> = serde_json::from_slice(&decoded)
        .map_err(|_| "show_layer must decode to a flat unsigned 16-bit integer array")?;
    if words.len() != word_count {
        return Err("show_layer word count does not match cleaning geometry".into());
    }
    let mut cells = Vec::with_capacity(cell_count);
    let mut pass_cells = [0; 4];
    for word in words {
        for shift in [0, 4, 8, 12] {
            let cell = ((word >> shift) & 15) as u8;
            if cells.len() == cell_count {
                if cell != 0 {
                    return Err("show_layer has nonzero tail padding".into());
                }
                continue;
            }
            if cell > 4 && !(allow_unclassified_five && cell == 5) {
                return Err("show_layer contains an unsupported cell value".into());
            }
            if (1..=3).contains(&cell) {
                pass_cells[cell as usize] += 1;
            }
            cells.push(cell);
        }
    }
    Ok(CleanLayer {
        map_id,
        width,
        height,
        resolution,
        origin_x,
        origin_y,
        origin_angle,
        cells,
        pass_cells,
    })
}
impl CleanLayer {
    pub fn matches_map(&self, map: &RoomMap) -> bool { self.map_id == map.map_id }

    /// The saved wire data supplies the same ID as a plan-map decimal string
    /// and a pose u64. Match that lossless spelling without float conversion,
    /// trimming, signs, leading zeroes, or normalization of other ID formats.
    pub fn matches_wire_map(&self, map: &RoomMap) -> bool {
        if self.matches_map(map) {
            return true;
        }
        match (&self.map_id, &map.map_id) {
            (Value::String(text), Value::Number(number))
            | (Value::Number(number), Value::String(text)) => {
                number.as_u64().is_some_and(|number| *text == number.to_string())
            }
            _ => false,
        }
    }

    /// Publish counts and format metadata, never the private grid or identity.
    pub fn summary(&self) -> Value {
        serde_json::json!({
            "source": "device_show_layer",
            "encoding": "base64-zlib-json-u16-low-nibble-first",
            "width": self.width,
            "height": self.height,
            "cleaned_cells": self.pass_cells[1..=3].iter().sum::<usize>(),
            "reported_pass_cells": {
                "1": self.pass_cells[1],
                "2": self.pass_cells[2],
                "3": self.pass_cells[3],
            },
            "origin_angle_rotation_applied": false,
            "unqualified_state_four_shaded": false,
        })
    }

    fn world_index(&self, x: f64, y: f64) -> Option<usize> {
        if !self.origin_angle.is_finite() || !self.resolution.is_finite() || self.resolution <= 0. {
            return None;
        }
        // Match the existing direct coordinate convention before display
        // transforms. origin_angle is metadata, not an additional rotation.
        let px = (x - self.origin_x) / self.resolution;
        let py = (y - self.origin_y) / self.resolution;
        if !px.is_finite() || !py.is_finite() || px < 0. || py < 0.
            || px >= self.width as f64 || py >= self.height as f64
        {
            return None;
        }
        let ix = (px.round() as u32).min(self.width - 1);
        let iy = (py.round() as u32).min(self.height - 1);
        Some(iy as usize * self.width as usize + ix as usize)
    }
}

impl std::fmt::Debug for CleanLayer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CleanLayer").field("summary", &self.summary()).finish()
    }
}

/// Parse the `/pita/plan_map/get` result object. Unsupported layouts fail
/// explicitly; this function never fetches a cloud map or synthesizes one.
pub fn parse(value: &Value) -> Result<RoomMap, String> {
    let map_id = match value.get("map_id") {
        Some(id @ Value::Number(_)) => id.clone(),
        Some(id @ Value::String(text)) if !text.is_empty() && text.len() <= 256 => id.clone(),
        _ => return Err("map_id must be a supplied number or nonempty string".into()),
    };
    let data = value
        .get("map_data")
        .filter(|v| v.is_object())
        .ok_or("unsupported map format: missing map_data object")?;
    let dimension = |name| -> Result<u32, String> {
        let number = data
            .get(name)
            .and_then(Value::as_u64)
            .filter(|n| *n > 0 && *n <= MAX_PIXELS as u64)
            .ok_or_else(|| format!("invalid map {name}"))?;
        Ok(number as u32)
    };
    let width = dimension("width")?;
    let height = dimension("height")?;
    let cells = (width as usize)
        .checked_mul(height as usize)
        .filter(|n| *n <= MAX_PIXELS)
        .ok_or("map exceeds pixel limit")?;
    let encoded = data
        .get("room_layer")
        .and_then(Value::as_str)
        .ok_or("unsupported map format: room_layer must be base64 text")?;
    if encoded.is_empty() || encoded.len() > MAX_COMPRESSED.div_ceil(3) * 4 {
        return Err("room_layer compressed size limit exceeded or empty".into());
    }
    let compressed = STANDARD
        .decode(encoded)
        .map_err(|_| "room_layer is not strict standard base64")?;
    if compressed.len() > MAX_COMPRESSED {
        return Err("room_layer compressed size limit exceeded".into());
    }
    // One u64 decimal value plus a comma takes at most 21 bytes. This also
    // prevents tiny declared maps from expanding into huge whitespace blobs.
    let limit = MAX_EXPANDED.min(cells * 21 + 2);
    let decoded = inflate_exact(&compressed, limit)?;
    let layer: Vec<u64> = serde_json::from_slice(&decoded)
        .map_err(|_| "unsupported room_layer: expected flat unsigned-integer JSON array")?;
    if layer.len() != cells {
        return Err("room_layer cell count does not match width times height".into());
    }
    let resolution = optional_number(data, "resolution")?;
    if resolution.is_some_and(|r| r <= 0.0) {
        return Err("resolution must be positive".into());
    }
    let origin_x = optional_number(data, "origin_x")?;
    let origin_y = optional_number(data, "origin_y")?;
    let origin_angle = optional_number(data, "origin_angle")?;
    let dock = match (
        optional_number(data, "base_station_x")?,
        optional_number(data, "base_station_y")?,
    ) {
        (Some(x), Some(y)) => Some(Point { x, y }),
        (None, None) => None,
        _ => return Err("dock coordinates must supply both x and y".into()),
    };
    let mut rooms = Vec::new();
    let mut ids = BTreeSet::new();
    let attributes = match value.get("room_attr") {
        None | Some(Value::Null) => None,
        Some(attrs) => Some(
            attrs
                .get("attr")
                .and_then(Value::as_array)
                .ok_or("room_attr.attr must be an array")?,
        ),
    };
    if let Some(attributes) = attributes {
        if attributes.len() > MAX_ROOMS {
            return Err("room count limit exceeded".into());
        }
        for attr in attributes {
            let room_id = attr
                .get("room_id")
                .and_then(Value::as_u64)
                .ok_or("room_id must be an unsigned integer")?;
            if !ids.insert(room_id) {
                return Err("duplicate room_id".into());
            }
            let room_type = match attr.get("type") {
                None | Some(Value::Null) => None,
                Some(v) => Some(v.as_i64().ok_or("room type must be an integer")?),
            };
            let name = match attr.get("name") {
                None | Some(Value::Null) => None,
                Some(Value::String(name)) if name.len() <= 4096 => Some(name.clone()),
                _ => return Err("room name must be bounded text".into()),
            };
            rooms.push(Room {
                room_id,
                room_type,
                name,
                source: "room_attr",
                label_x: None,
                label_y: None,
            });
        }
    }
    let clean_order = value.get("clean_order").unwrap_or(&Value::Null);
    if !clean_order.is_null() {
        let orders = clean_order
            .as_object()
            .ok_or("clean_order must be an object")?;
        if orders.len() > 64 {
            return Err("clean_order field count limit exceeded".into());
        }
        for (name, order) in orders {
            if name.len() > 256 {
                return Err("clean_order field name limit exceeded".into());
            }
            let order = order
                .as_array()
                .ok_or("clean_order entries must be arrays")?;
            if order.len() > MAX_ROOMS {
                return Err("clean_order entry limit exceeded".into());
            }
            for id in order {
                let room_id = id
                    .as_u64()
                    .ok_or("clean_order room IDs must be unsigned integers")?;
                if ids.insert(room_id) {
                    if rooms.len() >= MAX_ROOMS {
                        return Err("room count limit exceeded".into());
                    }
                    // Empty room_attr is valid on the observed J1. Keep only
                    // the actual order ID, with no invented label or type.
                    rooms.push(Room {
                        room_id,
                        room_type: None,
                        name: None,
                        source: "clean_order",
                        label_x: None,
                        label_y: None,
                    });
                }
            }
        }
    }
    let mut centroids = [[0_u64; 3]; 64];
    for (index, value) in layer.iter().enumerate() {
        if value.is_power_of_two() {
            let sum = &mut centroids[value.trailing_zeros() as usize];
            sum[0] += (index % width as usize) as u64;
            sum[1] += (index / width as usize) as u64;
            sum[2] += 1;
        }
    }
    // A mean can lie outside a concave room. Keep the nearest actual single-bit
    // room cell as its label anchor, with deterministic row-major tie breaking.
    let mut anchors = [None::<(f64, u32, u32)>; 64];
    for (index, value) in layer.iter().enumerate() {
        if !value.is_power_of_two() {
            continue;
        }
        let id = value.trailing_zeros() as usize;
        let [sum_x, sum_y, count] = centroids[id];
        let x = (index % width as usize) as u32;
        let y = (index / width as usize) as u32;
        let distance = (x as f64 - sum_x as f64 / count as f64).powi(2)
            + (y as f64 - sum_y as f64 / count as f64).powi(2);
        if anchors[id].is_none_or(|(previous, _, _)| distance < previous) {
            anchors[id] = Some((distance, x, y));
        }
    }
    for room in &mut rooms {
        // J1 reserves bit zero for background; room IDs start at zero.
        if let Some((_, x, y)) = usize::try_from(room.room_id)
            .ok()
            .and_then(|i| i.checked_add(1))
            .and_then(|i| anchors.get(i))
            .and_then(Option::as_ref)
        {
            room.label_x = Some(*x as f64);
            room.label_y = Some(*y as f64);
        }
    }
    let mut map = RoomMap {
        map_id,
        rooms,
        clean_order: clean_order.clone(),
        width,
        height,
        resolution,
        origin_x,
        origin_y,
        origin_angle,
        dock,
        layer_encoding: "zlib-base64-json-room-bitmask",
        layer,
        room_outlines: Vec::new(),
    };
    // Static geometry is traced once, not on every live position/status update.
    map.room_outlines = map.calculate_room_outlines();
    Ok(map)
}

fn optional_number(value: &Value, key: &str) -> Result<Option<f64>, String> {
    match value.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(number) => number
            .as_f64()
            .filter(|n| n.is_finite())
            .map(Some)
            .ok_or_else(|| format!("{key} must be finite numeric data")),
    }
}

fn inflate_exact(input: &[u8], limit: usize) -> Result<Vec<u8>, String> {
    let mut inflater = Decompress::new(true);
    let mut result = Vec::with_capacity(limit.min(64 * 1024));
    let mut chunk = [0_u8; 8192];
    loop {
        let before_in = inflater.total_in();
        let before_out = inflater.total_out();
        let available = chunk.len().min(limit - result.len() + 1);
        let status = inflater
            .decompress(
                &input[before_in as usize..],
                &mut chunk[..available],
                FlushDecompress::None,
            )
            .map_err(|_| "room_layer is not a valid zlib stream")?;
        let written = (inflater.total_out() - before_out) as usize;
        if result.len() + written > limit {
            return Err("room_layer expanded size limit exceeded".into());
        }
        result.extend_from_slice(&chunk[..written]);
        if status == Status::StreamEnd {
            if inflater.total_in() as usize != input.len() {
                return Err("room_layer contains trailing zlib data".into());
            }
            return Ok(result);
        }
        if inflater.total_in() == before_in && inflater.total_out() == before_out {
            return Err("room_layer zlib stream is incomplete".into());
        }
    }
}

impl RoomMap {
    pub fn display_room_outlines(&self, transform: DisplayTransform) -> Vec<DisplayRoomOutline> {
        self.room_outlines.iter().cloned().map(|mut room| {
            (room.label_x, room.label_y) =
                transform.pixel(self.width, self.height, room.label_x, room.label_y);
            for [x, y] in &mut room.outline {
                // Polygon vertices lie on pixel edges, so reflect about the
                // full image size; pixel-center labels use size minus one.
                (*x, *y) = match transform {
                    DisplayTransform::Identity => (*x, *y),
                    DisplayTransform::Rotate180 => (self.width-*x,self.height-*y),
                    DisplayTransform::FlipHorizontal => (self.width-*x,*y),
                    DisplayTransform::FlipVertical => (*x,self.height-*y),
                };
            }
            room
        }).collect()
    }

    fn calculate_room_outlines(&self) -> Vec<DisplayRoomOutline> {
        #[derive(Default)]
        struct Components {
            largest: Vec<usize>,
            count: usize,
            total_cells: usize,
        }
        let mut wanted = [false; 64];
        for room in &self.rooms {
            if room.room_id < 63 {
                wanted[room.room_id as usize + 1] = true;
            }
        }
        let mut components: [Components; 64] = std::array::from_fn(|_| Components::default());
        let mut visited = vec![false; self.layer.len()];
        let width = self.width as usize;
        for (index, &mask) in self.layer.iter().enumerate() {
            if visited[index] || !mask.is_power_of_two() {
                continue;
            }
            let bit = mask.trailing_zeros() as usize;
            if !wanted[bit] {
                continue;
            }
            let mut cells = vec![index];
            visited[index] = true;
            let mut position = 0;
            while position < cells.len() {
                let current = cells[position];
                let x = current % width;
                let mut visit = |neighbor: usize| {
                    if !visited[neighbor] && self.layer[neighbor] == mask {
                        visited[neighbor] = true;
                        cells.push(neighbor);
                    }
                };
                if x > 0 { visit(current-1); }
                if x+1 < width { visit(current+1); }
                if current >= width { visit(current-width); }
                if current+width < self.layer.len() { visit(current+width); }
                position += 1;
            }
            let summary = &mut components[bit];
            summary.count += 1;
            summary.total_cells += cells.len();
            // Row-major discovery keeps the first component on equal sizes.
            if cells.len() > summary.largest.len() {
                summary.largest = cells;
            }
        }
        self.rooms.iter().filter_map(|room| {
            let bit = room.room_id.checked_add(1).filter(|bit| *bit < 64)? as usize;
            let summary = &components[bit];
            let cells = &summary.largest;
            if cells.is_empty() {
                return None;
            }
            let (sum_x, sum_y) = cells.iter().fold((0_u64,0_u64), |(x,y), index|
                (x+(index%width) as u64,y+(index/width) as u64));
            let center_x = sum_x as f64 / cells.len() as f64;
            let center_y = sum_y as f64 / cells.len() as f64;
            let anchor = cells.iter().min_by(|a,b| {
                let distance = |index: usize| ((index%width) as f64-center_x).powi(2)
                    + ((index/width) as f64-center_y).powi(2);
                distance(**a).total_cmp(&distance(**b)).then_with(|| a.cmp(b))
            }).copied()?;
            let (outline, outline_error) = match self.trace_component(cells,1_u64<<bit) {
                Ok(outline) => (outline,None),
                Err(error) => (Vec::new(),Some(error)),
            };
            Some(DisplayRoomOutline {
                room_id: room.room_id,
                name: room.name.as_deref().map(str::trim).filter(|name|!name.is_empty())
                    .map(str::to_owned).unwrap_or_else(||format!("Room {}",room.room_id+1)),
                label_x: (anchor%width) as u32,
                label_y: (anchor/width) as u32,
                outline,
                components: summary.count,
                omitted_cells: summary.total_cells-cells.len(),
                outline_error,
            })
        }).collect()
    }

    fn trace_component(&self, cells: &[usize], mask: u64) -> Result<Vec<[u32;2]>, &'static str> {
        let width = self.width as usize;
        let mut edges = BTreeMap::<[u32;2],[u32;2]>::new();
        for &index in cells {
            let x = (index%width) as u32;
            let y = (index/width) as u32;
            let mut edge = |from, to| {
                if edges.len() >= MAX_OUTLINE_EDGES {
                    return Err("boundary_vertex_limit");
                }
                // Diagonal pinches have ambiguous vertices; a filled polygon
                // could cross other regions, so use a label-only fallback.
                if edges.insert(from,to).is_some() {
                    return Err("non_simple_boundary");
                }
                Ok(())
            };
            // Clockwise cell edges keep the room interior on the right.
            if y == 0 || self.layer[index-width] != mask {
                edge([x,y],[x+1,y])?;
            }
            if x+1 == self.width || self.layer[index+1] != mask {
                edge([x+1,y],[x+1,y+1])?;
            }
            if y+1 == self.height || self.layer[index+width] != mask {
                edge([x+1,y+1],[x,y+1])?;
            }
            if x == 0 || self.layer[index-1] != mask {
                edge([x,y+1],[x,y])?;
            }
        }
        let start = *edges.keys().next().ok_or("empty_boundary")?;
        let mut current = start;
        let mut path = Vec::new();
        loop {
            if path.len() >= edges.len() {
                return Err("non_simple_boundary");
            }
            path.push(current);
            current = *edges.get(&current).ok_or("non_simple_boundary")?;
            if current == start {
                break;
            }
        }
        // A single card polygon cannot subtract holes. Do not highlight a
        // neighboring room or an unclassified interior as part of this room.
        if path.len() != edges.len() {
            return Err("holes_not_supported");
        }
        let vertices: Vec<_> = path.iter().enumerate().filter_map(|(i,&point)| {
            let previous = path[(i+path.len()-1)%path.len()];
            let next = path[(i+1)%path.len()];
            ((previous[0] != point[0] || point[0] != next[0])
                && (previous[1] != point[1] || point[1] != next[1]))
                .then_some(point)
        }).collect();
        if vertices.len() > MAX_OUTLINE_VERTICES {
            return Err("boundary_vertex_limit");
        }
        if vertices.len() < 4 {
            return Err("non_simple_boundary");
        }
        Ok(vertices)
    }

    pub fn png(&self, pose: Option<&Value>) -> Result<Vec<u8>, String> {
        self.png_with_trail(pose, std::iter::empty())
    }

    pub(crate) fn png_with_trail<'a>(
        &self,
        pose: Option<&Value>,
        trail: impl IntoIterator<Item = &'a TrailPoint>,
    ) -> Result<Vec<u8>, String> {
        self.png_display(pose, trail, MapDisplay { show_trail: true, ..MapDisplay::default() })
    }

    pub(crate) fn png_display<'a>(
        &self,
        pose: Option<&Value>,
        trail: impl IntoIterator<Item = &'a TrailPoint>,
        display: MapDisplay,
    ) -> Result<Vec<u8>, String> {
        self.png_display_with_cleaning(pose, trail, display, None)
    }

    pub(crate) fn png_display_with_cleaning<'a>(
        &self,
        pose: Option<&Value>,
        trail: impl IntoIterator<Item = &'a TrailPoint>,
        display: MapDisplay,
        cleaning: Option<&CleanLayer>,
    ) -> Result<Vec<u8>, String> {
        let cells = (self.width as usize)
            .checked_mul(self.height as usize)
            .filter(|n| *n > 0 && *n <= MAX_PIXELS && *n == self.layer.len())
            .ok_or("invalid map geometry")?;
        let mut rgba = vec![0_u8; cells * 4];
        for (value, pixel) in self.layer.iter().zip(rgba.as_chunks_mut::<4>().0) {
            if *value == 0 {
                continue;
            }
            let color = if value.is_power_of_two() {
                PALETTE[value.trailing_zeros() as usize % PALETTE.len()]
            } else {
                [0x37, 0x41, 0x51]
            };
            pixel[..3].copy_from_slice(&color);
            pixel[3] = 255;
        }
        if let Some(cleaning) = cleaning.filter(|layer| layer.matches_wire_map(self))
            && let (Some(origin_x), Some(origin_y), Some(resolution)) =
                (self.origin_x, self.origin_y, self.resolution)
            && origin_x.is_finite() && origin_y.is_finite()
            && resolution.is_finite() && resolution > 0.
        {
            // Sample categorically from the cleaning grid's independent
            // geometry. Each target pixel is shaded once; overlapping source
            // cells cannot accumulate extra darkness during resampling.
            for y in 0..self.height {
                for x in 0..self.width {
                    let Some(index) = cleaning.world_index(
                        origin_x + x as f64 * resolution,
                        origin_y + y as f64 * resolution,
                    ) else {
                        continue;
                    };
                    let factor = match cleaning.cells.get(index) {
                        Some(1) => 80,
                        Some(2) => 60,
                        Some(3) => 40,
                        _ => continue,
                    };
                    let offset = (y as usize * self.width as usize + x as usize) * 4;
                    let pixel = &mut rgba[offset..offset + 4];
                    if pixel[3] != 0 {
                        for channel in &mut pixel[..3] {
                            *channel = (*channel as u16 * factor / 100) as u8;
                        }
                    }
                }
            }
        }
        // One pixel is a route line, independent of brush width or pass count.
        // Draw behind the current robot/base markers. Invalid points break
        // continuity even if a future caller bypasses the engine's checks.
        if display.show_trail {
            let mut previous = None;
            for sample in trail {
                let Some(pixel) = self.world_pixel(sample.point.x, sample.point.y) else {
                    previous = None;
                    continue;
                };
                let from = if sample.starts_segment {
                    pixel
                } else {
                    previous.filter(|(x,y): &(i32,i32)| {
                        (pixel.0-x).abs().max((pixel.1-y).abs()) <= MAX_TRAIL_JOIN_PIXELS
                    }).unwrap_or(pixel)
                };
                self.route_line(&mut rgba, from, pixel);
                previous = Some(pixel);
            }
        }
        if let Some(dock) = &self.dock
            && let Some((x, y)) = self.world_pixel(dock.x, dock.y)
        {
            self.marker(&mut rgba, x, y, true);
        }
        if let Some(pose) = pose
            && let (Some(x), Some(y)) = (
                pose.get("x").and_then(Value::as_f64),
                pose.get("y").and_then(Value::as_f64),
            )
            && let Some((x, y)) = self.world_pixel(x, y)
        {
            self.marker(&mut rgba, x, y, false);
        }
        if display.transform != DisplayTransform::Identity {
            let mut transformed = vec![0; rgba.len()];
            for y in 0..self.height {
                for x in 0..self.width {
                    let (dx, dy) = display.transform.pixel(self.width, self.height, x, y);
                    let from = (y as usize * self.width as usize + x as usize) * 4;
                    let to = (dy as usize * self.width as usize + dx as usize) * 4;
                    transformed[to..to + 4].copy_from_slice(&rgba[from..from + 4]);
                }
            }
            rgba = transformed;
        }
        // Labels are painted after the display transform so text stays upright.
        // The raw map coordinate system, received poses and route stay unchanged.
        if display.room_labels {
            for label in self.display_labels(display.transform) {
                self.room_label(&mut rgba, &label);
            }
            // A label may cross a robot/base position. Keep both markers above
            // the text layer; their symmetric glyphs need only transformed
            // anchors, while all world-coordinate input remains unchanged.
            if let Some(dock) = &self.dock
                && let Some((x,y)) = self.world_pixel(dock.x,dock.y)
            {
                let (x,y)=display.transform.pixel(self.width,self.height,x as u32,y as u32);
                self.marker(&mut rgba,x as i32,y as i32,true);
            }
            if let Some(pose) = pose
                && let (Some(x),Some(y)) = (
                    pose.get("x").and_then(Value::as_f64),
                    pose.get("y").and_then(Value::as_f64),
                )
                && let Some((x,y)) = self.world_pixel(x,y)
            {
                let (x,y)=display.transform.pixel(self.width,self.height,x as u32,y as u32);
                self.marker(&mut rgba,x as i32,y as i32,false);
            }
        }
        // Unknown path schemas and unverified heading units are deliberately
        // not interpreted. The caller retains the source telemetry.
        let mut bytes = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut bytes, self.width, self.height);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().map_err(|_| "PNG header failed")?;
            writer
                .write_image_data(&rgba)
                .map_err(|_| "PNG encoding failed")?;
            writer.finish().map_err(|_| "PNG finalization failed")?;
        }
        Ok(bytes)
    }

    pub fn display_labels(&self, transform: DisplayTransform) -> Vec<DisplayLabel> {
        self.rooms.iter().filter_map(|room| {
            if room.room_id >= 63 {
                return None;
            }
            let x = room.label_x.filter(|x| x.is_finite() && *x >= 0.0 && *x < self.width as f64)? as u32;
            let y = room.label_y.filter(|y| y.is_finite() && *y >= 0.0 && *y < self.height as f64)? as u32;
            if self.layer.get(y as usize * self.width as usize + x as usize) != Some(&(1u64 << (room.room_id + 1))) {
                return None;
            }
            let (x,y) = transform.pixel(self.width, self.height, x, y);
            Some(DisplayLabel {
                room_id:room.room_id,
                text:format!("Room {}",room.room_id+1),
                source:"numbered_room_id",
                x,y,
            })
        }).collect()
    }

    fn room_label(&self, rgba: &mut [u8], label: &DisplayLabel) {
        let width = label.text.len() as u32 * 4 + 3;
        let height = 9;
        if self.width < width || self.height < height {
            return;
        }
        let left = label.x.saturating_sub(width / 2).min(self.width - width);
        let top = label.y.saturating_sub(height / 2).min(self.height - height);
        // Paint only the upright glyphs; preserve the room and cleaning pixels
        // between them instead of covering them with a rectangular label.
        for (character, byte) in label.text.bytes().enumerate() {
            let rows = match byte {
                b'R' => [6,5,6,5,5],
                b'o' | b'0' => [7,5,5,5,7],
                b'm' => [5,7,7,5,5],
                b'1' => [2,6,2,2,7],
                b'2' => [7,1,7,4,7],
                b'3' => [7,1,7,1,7],
                b'4' => [5,5,7,1,1],
                b'5' => [7,4,7,1,7],
                b'6' => [7,4,7,5,7],
                b'7' => [7,1,1,1,1],
                b'8' => [7,5,7,5,7],
                b'9' => [7,5,7,1,7],
                _ => [0;5],
            };
            for (row, bits) in rows.iter().enumerate() {
                for column in 0..3 {
                    if bits & (1 << (2-column)) != 0 {
                        let x = left as usize + 2 + character*4 + column;
                        let y = top as usize + 2 + row;
                        let index = (y*self.width as usize+x)*4;
                        rgba[index..index+4].copy_from_slice(&[17,24,39,255]);
                    }
                }
            }
        }
    }

    fn route_line(&self, rgba: &mut [u8], from: (i32, i32), to: (i32, i32)) {
        let (mut x, mut y) = from;
        let dx = (to.0 - x).abs();
        let dy = -(to.1 - y).abs();
        let sx = if x < to.0 { 1 } else { -1 };
        let sy = if y < to.1 { 1 } else { -1 };
        let mut error = dx + dy;
        loop {
            let index = (y as usize * self.width as usize + x as usize) * 4;
            rgba[index..index + 4].copy_from_slice(&[30, 64, 175, 255]);
            if (x, y) == to {
                break;
            }
            let twice = 2 * error;
            if twice >= dy {
                error += dy;
                x += sx;
            }
            if twice <= dx {
                error += dx;
                y += sy;
            }
        }
    }

    pub(crate) fn world_pixel(&self, x: f64, y: f64) -> Option<(i32, i32)> {
        // ha-narwal-t10 mapdata.py uses this same transform for robot and base.
        // Applying origin_angle again puts the measured base outside the map.
        let px = (x - self.origin_x?) / self.resolution?;
        let py = (y - self.origin_y?) / self.resolution?;
        if !px.is_finite()
            || !py.is_finite()
            || px < 0.0
            || py < 0.0
            || px >= self.width as f64
            || py >= self.height as f64
        {
            return None;
        }
        Some((
            px.round().min((self.width - 1) as f64) as i32,
            py.round().min((self.height - 1) as f64) as i32,
        ))
    }

    fn marker(&self, rgba: &mut [u8], x: i32, y: i32, dock: bool) {
        let robot_radius = (self.width.min(self.height) / 80).clamp(1, 4) as i32;
        let radius = robot_radius + if dock { 3 } else { 1 };
        for dy in -radius..=radius {
            for dx in -radius..=radius {
                let px = x + dx;
                let py = y + dy;
                if (dock || dx * dx + dy * dy <= radius * radius)
                    && px >= 0
                    && py >= 0
                    && px < self.width as i32
                    && py < self.height as i32
                {
                    let index = (py as usize * self.width as usize + px as usize) * 4;
                    let inside = if dock {
                        dx.abs() < radius && dy.abs() < radius
                    } else {
                        dx * dx + dy * dy <= robot_radius * robot_radius
                    };
                    let color = match (dock, inside) {
                        (true, true) => [255, 255, 255, 255],
                        (true, false) => [31, 41, 55, 255],
                        (false, true) => [220, 20, 60, 255],
                        (false, false) => [255, 255, 255, 255],
                    };
                    rgba[index..index + 4].copy_from_slice(&color);
                }
            }
        }
    }
}


