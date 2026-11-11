# Interactive local map dashboard

The dashboard builds on
[Xiaomi Vacuum Map Card](https://github.com/PiotrMachowski/Lovelace-Xiaomi-Vacuum-Map-card),
with the included `room_source` patch and the bridge's local MQTT entities. It needs no Xiaomi,
Dreame or Narwal cloud integration.

## Frontend resources

Copy [resources/xiaomi-vacuum-map-card.js](resources/xiaomi-vacuum-map-card.js)
to `/config/www/narwal/xiaomi-vacuum-map-card.js` on your HA installation.
Register `/local/narwal/xiaomi-vacuum-map-card.js` as a JavaScript module
resource. Use this resource in place of the original Xiaomi card resource;
loading both registers the same custom element twice.

Install [Config Template Card](https://github.com/custom-cards/config-template-card)
**v1.3.6** through HACS and register its module resource. It is used only for
the two draft action buttons. The live maps use the Xiaomi card directly.
See [FRONTEND.md](FRONTEND.md) for source pins, checksums, build and license details.

Enable the standard Home Assistant MQTT integration and verify the discovered
entities and native map image. Update the bridge before opening the editor.
Enable its private `room_editor_file` store as described in
[ROOM_EDITOR.md](../../docs/ROOM_EDITOR.md).
A missing custom-card error means one of these resources has not loaded;
check its module resource entry and reload the browser.

## Generate your private configuration

Room outlines and the map ID belong to your installation. The Rust helper
checks a current `/api/state` snapshot and an entity mapping; it produces a
complete dashboard JSON file with Cleaning and Rooms views. The dashboard
reads room geometry from the local catalog entity, so the generated file
does not freeze a copy of the floorplan. The helper makes no network requests,
changes no settings and sends no robot commands.

Build it from the repository root:

```sh
cargo build --locked --release \
  --manifest-path deploy/dashboard/generator/Cargo.toml
```

Use a private directory **outside this checkout**. The following commands
run on the bridge host, where the HTTP endpoint is loopback-only:

```sh
umask 077
mkdir -m 700 ~/narwal-dashboard-private
cp deploy/dashboard/entities.example.json \
  ~/narwal-dashboard-private/entities.private.json
curl --fail --max-time 10 http://127.0.0.1:35394/api/state \
  > ~/narwal-dashboard-private/state.private.json
```

Edit `entities.private.json` in a local editor so every entity ID matches your
discovered HA entities and `topic_prefix` matches your bridge configuration.
The prefix follows the same rules as the runtime: 1–256 UTF-8 bytes, nonempty
slash-separated levels, and no control characters or MQTT wildcards (`+`, `#`).
Spaces, dots and Unicode are supported and preserved literally.
The example IDs are placeholders. The initial vacuum object ID may include
the MQTT client ID; copying the example without checking it is insufficient.
The map entity must be the discovered native **image** entity.
The mapping also needs the room catalog/status sensors, proposed-name text,
split-room selector and draft-preview image discovered by the editor backend.
New editor discovery object IDs use `narwal_j1_<client_id>_` followed by
`room_catalog`, `room_editor`, `room_name`, `editor_room` or `room_edit_preview`.
HA normalizes these into entity IDs and may retain an existing name, so copy
the actual IDs shown in HA rather than constructing them from this pattern.

Generate immediately after taking the snapshot:

```sh
deploy/dashboard/generator/target/release/narwal-dashboard-config \
  "$HOME/narwal-dashboard-private/state.private.json" \
  "$HOME/narwal-dashboard-private/entities.private.json" \
  "$HOME/narwal-dashboard-private/dashboard.generated.json"
```

The helper requires connected, fresh state and a current map. It accepts raw
`/api/state` JSON or an object containing it under `local_status`. It requires
all configured room IDs to have valid outlines in the current map. If an
outline is unavailable or ambiguous, it refuses generation instead of
drawing a misleading rectangle across adjacent rooms.

Output is a new mode-0600 file in an existing private mode-0700 directory.
Existing output is never overwritten. On regeneration, choose a new filename
and retain the previous configuration privately for rollback.
Do not commit snapshots or generated dashboards, even to a private repository.

Create an empty dashboard in Home Assistant, take control of its configuration,
and paste the generated JSON into its raw configuration editor. A supported
Lovelace WebSocket save is also suitable; do not edit HA `.storage` files.
If generating from another computer, use the
[documented SSH local forward](../README.md#connect-and-verify) and the exact
address `http://127.0.0.1:35394/` for the loopback endpoint. Transfer private
files over your authenticated connection.

## Use the map

Tap a room's area to select it; tap it again to remove it. Multiple rooms may
be selected. Check all highlighted rooms, then press **Start**. **Clear room
selection** removes every selection. No room-dot buttons are required.

The card uses already-transformed display-pixel outlines and identity
calibration. Do not apply another rotation or reflection. Change the runtime's
`map_display.transform` if needed, then regenerate the dashboard.

- **Vacuum:** choose 1–3 requested passes.
- **Mop:** choose 1–3 requested passes and Normal or Wet dampness.
- **Pause/Resume:** shown for supported mop states.
- **Return to dock:** uses the ordinary local vacuum service.
- **Robot:** red circle. **Dock:** larger white square.
- **Darker regions:** robot-reported cleaned cells, with deeper shades for
  reported passes. The optional blue pose-history line is hidden by default.

Mode, job phase, battery, percentage, area and readable errors appear below
the map. The error card preserves an unknown-code fallback. A fault prevents
the backend from accepting a new cleaning request. A service-call success
alone is not proof of motion; follow the current job and command receipt.

When the robot is connected but its map is unavailable, native HA cards keep
job status, mode, battery, progress, area and dock contact visible. Return to
dock remains available for Mop and Vacuum; Mop Pause and fault-free Resume
appear for their corresponding task states. Starting a new room job still
requires the current interactive map. These fallback cards disappear when
the map returns, and motion controls disappear when the robot is offline.
Both HA `unknown` (no accepted image yet) and `unavailable` map states use
this fallback. Robot availability and current-map availability are separate;
fresh robot feedback can arrive before the first valid map.

The robot's reported progress and cleaned area may reset or retain previous
values after docking. The dashboard does not replace them with an invented
100%. Cleaning shading can remain visible after the task ends.

## Edit rooms

Open the **Rooms** view:

The backend validates each rename, split and merge before offering Save.
Room names are local aliases for the HA map and controls.

1. **Rename:** enter the new room name, choose Rename mode and select one room.
2. **Merge:** choose Merge mode and select two adjacent rooms.
3. **Split:** choose the target in **Room to split**, choose Split mode, then
   place two points defining the dividing line.

Press **▶** to prepare one draft. Review the backend's summary, feedback and
**Draft preview**, then choose **Save** or **Cancel draft**. Selection and
typing do not alter the map. Clear selection removes only the current map
selection; Cancel discards the prepared draft.

Save appears for a ready draft and is inactive if the backend's `can_save`
flag is false. The backend independently rechecks every request. Unsupported
operations, a disconnected robot, a changed map and an unavailable writer
produce visible feedback rather than an apparent successful edit. Saving
stays pending until the backend verifies its result.

Complete or cancel one draft before preparing another. After a confirmed
split, select the newly created room to prepare its name as a separate edit.
See [the editor contract](../../docs/ROOM_EDITOR.md).

## Room changes and cleaning requests

Confirmed room edits automatically update outlines in both views. A changed
catalog, map or display orientation clears old selections; pose and progress
updates preserve them. Regenerate only when entity IDs or dashboard
configuration change.

The generated card publishes to `<topic_prefix>/command/room_clean` with
QoS 0 and `retain: false`. It sends the current catalog's map ID and revision,
selected numeric room IDs, requested pass count and dampness. Explicit map selection does not change
the bridge's saved next-clean selectors. This prevents an unrelated saved
room selection from replacing the rooms selected on the map.

The backend rechecks current mode, map, room catalog, source state, fault
status and durable intent. Retained docked tasks may require the supplied
profile's internal task-clear operation before a new Start. New requests
during washing or another unqualified source state are refused, not queued
for later. See [the control contract](../../docs/CONTROL.md).
