# Narwal J1 local Home Assistant bridge

A Rust service that gives a legacy Narwal J1 local Home Assistant controls,
room selection, telemetry and a live cleaning map through MQTT discovery.
The robot connects to a local TLS/WebSocket listener; the bridge talks to
your local MQTT broker. The running bridge uses no Narwal account credentials
or cloud client.

This is a Linux service, not a HACS integration. Home Assistant needs its
standard MQTT integration. The interactive frontend uses the unchanged
[Xiaomi Vacuum Map Card](https://github.com/PiotrMachowski/Lovelace-Xiaomi-Vacuum-Map-card),
tested with version 2.4.1.

## Features

- Click room outlines on the map, review the highlighted selection, then Start.
- Local room vacuuming with one pass; room mopping with 1–3 requested passes
  and Normal or Wet dampness.
- Mop Pause/Resume and Return to dock.
- Live reported robot position, base station position, unboxed room labels
  and darker cleaned-area shading for reported passes.
- Mode, job phase, battery, progress, cleaned area, readable errors and dock contact.
- Durable settings and command journals, with no automatic replay after an
  uncertain send, disconnect or restart.

Local room targeting, moving map markers, cleaning shading and natural
return to dock have been exercised on a J1. Requested passes and dampness are
decoded command settings; physical pass count and water delivery have not
been measured. The robot's progress percentage can reset when a task ends
without a final 100% report. Map coverage, task state and dock contact are
separate observations.

## Install

Use a Linux host with systemd, OpenSSL 3 and a route from the robot's network.
The TLS frontend occupies a dedicated IPv4 address on TCP 443.

```sh
cargo build --locked --release
```

The runtime is `target/release/narwal-local-service`. It requires
`libcrypto.so.3`; build for a compatible libc/OpenSSL environment.
The source uses Rust edition 2024 and the dependency versions in `Cargo.lock`.

Follow the [deployment guide](deploy/README.md), then
[generate your dashboard](deploy/dashboard/README.md) from your own current
local room metadata. The supplied generator is also Rust. No real room
polygons, map identifiers or generated household dashboard are shipped.

## Controls and limits

The bridge selects behavior from the robot's **reported** physical mode.
It does not remotely change the installed vacuum/mop module. Vacuum pass
selection is currently limited to one because repeated vacuum passes have
not been qualified; this is not a returned robot capability limit.
Mop dampness offers Normal and Wet. Dry, zone cleaning, spot cleaning,
go-to commands and a general Stop/cancel action are not exposed.

Returning to the dock can retain a paused task. The supplied profile supports
bounded internal task clearance before a new room job, with fresh idle
feedback required before Start. Washing and other active dock cycles do not
qualify. See the [control contract](docs/CONTROL.md).

This repository targets the **legacy J1 protocol**. Compatibility with other
Narwal models or firmware is not established. It is not the port-9002 local
protocol used by newer models.

A local connection, direct DNS routing and service restart have been
demonstrated. A fully cloud-independent **cold robot startup remains
unqualified**: separate bootstrap traffic may still use `robot.narwaltech.com`.
The deployment redirects only `websocket.narwaltech.com`.

After that redirect, the official Narwal app may show the robot as offline.
Use fresh HA/local telemetry for this deployment's status. Record room
references before redirecting, or use the documented maintenance rollback
when app access is needed.

## Maps and privacy

Robot markers require a fresh current-session reported pose. When such a pose
is unavailable, positive dock contact can explicitly infer the robot at the
map's base station. Stale off-dock markers disappear; a selected room is not
position evidence. The optional blue pose-history trail is hidden by default;
reported cleaning shading and robot/base markers remain visible.

The map and generated dashboard contain your floorplan and room identifiers.
Keep snapshots, generated dashboards, configuration, credentials and journal
files outside the checkout in private directories. MQTT map data is also
private household data. This distribution excludes tests, fixtures, captured
traffic, real maps, device identities and operational credentials.

## Prior work and license

The implementation builds on protocol and HA conventions researched in
[ha-narwal-t10](https://github.com/nitinkr0411/ha-narwal-t10) and
[matterbridge-narwal](https://github.com/redelva/matterbridge-narwal).
[NarwalIntegration](https://github.com/sjmotew/NarwalIntegration) informed the
comparison with newer local-protocol devices; its documentation excludes J1/T10.
[Architecture and decoding](docs/ARCHITECTURE.md) explains these roles.

The Rust bridge and dashboard generator are MIT licensed. Upstream revisions
and MIT/Apache notices are retained in
[THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md). Cargo dependencies retain
their respective licenses. The unchanged Xiaomi frontend is installed
separately through HACS; its source and compiled bundle are not vendored here.
