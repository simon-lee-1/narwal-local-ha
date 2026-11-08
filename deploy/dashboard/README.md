# Interactive local map dashboard

The dashboard uses the unchanged
[Xiaomi Vacuum Map Card](https://github.com/PiotrMachowski/Lovelace-Xiaomi-Vacuum-Map-card),
tested with **v2.4.1**, and the bridge's local MQTT entities. It needs no Xiaomi,
Dreame or Narwal cloud integration.

Install the card through HACS and register its frontend resource following
the card's instructions. Enable the standard Home Assistant MQTT integration
and first verify the bridge's discovered entities and native map image.

## Generate your private configuration

Room outlines and the map ID belong to your installation. The Rust helper
reads a current `/api/state` snapshot and an entity mapping; it produces a
complete dashboard JSON file. It makes no network requests, changes no
settings and sends no robot commands.

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
The example IDs are placeholders. The initial vacuum object ID may include
the MQTT client ID; copying the example without checking it is insufficient.
The map entity must be the discovered native **image** entity.

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
If generating from another computer, use an SSH local forward for the
loopback HTTP endpoint and transfer private files over your authenticated
connection.

## Use the map

Tap a room's area to select it; tap it again to remove it. Multiple rooms may
be selected. Check all highlighted rooms, then press **Start**. **Clear room
selection** removes every selection. No room-dot buttons are required.

The card uses already-transformed display-pixel outlines and identity
calibration. Do not apply another rotation or reflection. Change the runtime's
`map_display.transform` if needed, then regenerate the dashboard.

- **Vacuum:** one requested pass.
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

The robot's reported progress and cleaned area may reset or retain previous
values after docking. The dashboard does not replace them with an invented
100%. Cleaning shading can remain visible after the task ends.

## Room changes and request contract

Regenerate after splitting, merging or renaming rooms, replacing a map,
changing the configured room catalog, or changing display orientation.
The dashboard pins the current map ID; a replaced ID is rejected. In-place
geometry changes may retain that ID, so the pin alone cannot detect them.

The generated card publishes to `<topic_prefix>/command/room_clean` with
QoS 0 and `retain: false`. It sends the current map ID, selected numeric room
IDs, requested pass count and dampness. Explicit map selection does not change
the bridge's saved next-clean selectors. This prevents an unrelated saved
room selection from replacing the rooms selected on the map.

The backend rechecks current mode, map, room catalog, source state, fault
status and durable intent. Retained docked tasks may require the supplied
profile's internal task-clear operation before a new Start. New requests
during washing or another unqualified source state are refused, not queued
for later. See [the control contract](../../docs/CONTROL.md).
