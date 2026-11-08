# Local room editor

The Rooms dashboard prepares one edit at a time: rename one room, merge two
adjacent rooms, or split a selected room using a two-point line. The Rust
backend owns validation, draft geometry, persistence and map-write delivery.
The frontend never treats an overlay change as a saved robot map.

Room names are HA-local aliases stored by the bridge and used on its map and
room controls. A saved alias does not claim to rename the room in the vendor
app. Split and merge change room boundaries only through an available,
validated map writer; a draft preview alone does not enable that writer.

## Enable the editor

The installer enables the editor using
`room_editor_file: "/var/lib/narwal-local-ha/room-editor.json"`.
For an existing installation, add this optional runtime key using its private
state directory. Its parent must be owned
by the service account and mode 0700. The runtime initializes a new mode-0600
store when needed; preserve that store across upgrades. It contains private
names, draft data and map-edit state. Keep it out of the checkout and do not
delete it to clear an unresolved operation.

Restart the bridge with the reviewed configuration, then check its editor,
catalog, text, split-room selector and preview-image MQTT entities. Without
editor configuration or a local connection, the Rooms view reports that the
editor is unavailable and does not enable Save. The committed catalog
continues to serve the cleaning view.

## Prepare and save

Open the Rooms view and prepare one operation:

- **Rename:** enter the proposed name and select one room.
- **Split:** choose the target room, then draw a two-point line whose endpoints
  span that room. Keep both endpoints inside the displayed map.
- **Merge:** select exactly two adjacent rooms.

Prepare the edit, inspect its summary and separate preview, then choose Save
or Cancel. Finish or cancel the current draft before preparing another.
Save requires fresh local feedback and an idle, docked robot without a fault.

Name and split-room controls are scratch settings. Preparing a rename freezes
the current proposed name; preparing a split freezes the chosen room.
Changing those controls afterward does not mutate an existing draft.
Preparing a draft sends no map-write command. Save identifies the exact
displayed draft; the backend checks its map and revision again before applying
it. Cancel discards that draft without writing the map.

The UI shows backend capability and refusal feedback. A draft may be
previewable while its writer is unavailable or unqualified; that does not
enable Save. Reported acknowledgment alone does not establish that the map
was changed. The backend must verify the resulting state before reporting
confirmation. Device split/merge behavior is not established by the
frontend's offline checks.

Before a boundary write, the private store records the original map, exact
candidate and pending operation. The bridge then sends the guarded local
setter and requests the map again. A matching fresh readback confirms the
written geometry and native attributes; an acknowledgment alone does not.
The previous map remains in the private backup state after confirmation.

If delivery or readback is uncertain, the pending operation remains recorded.
The bridge does not resend it after a timeout, reconnect or restart. A later
fresh local map response can reconcile it only when it matches the saved
candidate. Use **Refresh result** while the local connection is fresh to read
the robot's map again. This action never resends the edit. If the returned map
differs from the saved candidate, the operation stays pending. Preserve the
store and inspect the editor's message when it remains unresolved; deleting
the store would discard that evidence.

## Device-verification limits

An earlier vendor-app split supplied a uniquely matched, successful
`/pita/plan_map/set` request and reply. Its ordered operations were
`["map_data", "room_attr"]`; this established the setter shape, not a captured
merge operation. No new split or merge through this editor has yet been
tested on the physical robot.

The current writer accepts the observed native metadata profile:
`origin_angle: null`, room `type` values from -5 through -2, `cType: -2`,
`keepName: false`, and empty native `name` and `cName`. Other profiles are
refused. Existing native attributes are preserved; splitting copies the
selected room's native attributes to the new room ID. Their full semantics,
including whether a new room needs different native metadata, remain
unverified. Matching map readback therefore does not establish subsequent
split/merge cleaning behavior on the device.

Room naming uses local aliases because native custom-name semantics have not
been qualified. The distributed repository excludes the development tests,
recordings and household maps.

## MQTT contract

All commands use QoS 0 and `retain: false`. Replace `<prefix>` with the local
bridge's configured topic prefix.

| Topic | Body |
|---|---|
| `<prefix>/room_editor/name/set` | Proposed name, ordinary text |
| `<prefix>/room_editor/room/set` | Exact split-room option |
| `<prefix>/room_editor/draft` | Rename or merge object below |
| `<prefix>/room_editor/draft` | Split object below |
| `<prefix>/room_editor/save` | `{"draft_id":"<displayed draft>"}` |
| `<prefix>/room_editor/cancel` | `{"draft_id":"<displayed draft>"}` |
| `<prefix>/room_editor/refresh` | `{}` — read the current result without writing |

Rename and merge use `operation`, `map_id`, `revision` and `rooms`. Rename
requires exactly one numeric room ID; merge requires exactly two adjacent
room IDs. The name comes from the backend's proposed-name scratch setting.

Split uses `operation: "split"`, `map_id`, `revision` and `line`, with exactly
two `[x, y]` points in display pixels. The backend uses the split-room setting,
undoes the display transform, and validates the partition. The preview uses
the same display orientation as the current map.

Every draft pins the current map identifier and catalog revision. Invalid,
outdated, retained or duplicate commands must not result in a deferred edit.
A pending Save must be resolved or refused explicitly before another edit.
Refresh result remains available with a pending write, a refusal or no draft.
Its body must be an empty JSON object; it uses the same fresh-connection,
nonretained and nonduplicate delivery checks.
Room-cleaning requests also carry the current map ID and catalog revision.
The bridge checks that binding again before sending a room job, so an old
selection cannot silently target rooms whose boundaries have changed.

## HA entities and dynamic map data

Entity IDs are configurable in the dashboard's entity mapping.

| Mapping key | Domain and purpose |
|---|---|
| `room_catalog_entity` | Sensor: current committed rooms and catalog revision |
| `room_editor_entity` | Sensor: editor lifecycle and readable feedback |
| `room_name_entity` | Text: scratch proposed name |
| `editor_room_entity` | Select: target room for a split |
| `room_preview_entity` | Image: backend-rendered draft preview |

The catalog sensor's state is its revision. Its attributes include `map_id`,
`revision`, `coordinate_space: "display_pixels"` and `rooms`. Each room
contains its numeric `id`, polygon `outline` and unselected default state.
The catalog is published when those data change, independently of robot pose
or cleaning progress.

The editor sensor reports `idle`, `draft_ready`, `draft_blocked`, `saving`,
`confirmed`, `refused` or `unavailable`. It supplies `draft_id`, `operation`,
`summary`, `message`, `can_save` and `capabilities`. Save requires both
`draft_ready` and `can_save: true`; the backend remains authoritative even if
the frontend is stale. Draft identifiers and names are data, never JavaScript
or HA-template source.

The patched Xiaomi card's `room_source` consumes the committed catalog.
It clears selections when geometry or revision changes and continues normal
HA updates for the moving robot, cleaned regions and status. The editor's
preview image is separate from this committed map. Confirmed changes refresh
the cleaning and editor views without an HA API token or dashboard rewrite.

## Privacy

Room names, geometry, draft previews and map identifiers describe the home.
Keep them in the private runtime and MQTT broker. The distributed dashboard
configuration references entities and contains no captured household map.
