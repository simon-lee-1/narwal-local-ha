# Deploy on a local Linux host

Use a dedicated service host with systemd and a stable IPv4 address reachable
from the robot. Installation needs root; the running processes use a dedicated
unprivileged account. Build with Rust 1.88 or newer using
`cargo build --locked --release`.
On Debian/Ubuntu, runtime installation tools include `socat`, `openssl`,
`jq`, `iproute2`, and the package providing `libcrypto.so.3`
(`libssl3` or `libssl3t64`, depending on the distribution).

## Prepare private configuration

Enable Home Assistant's MQTT integration and a local MQTT broker. Create a
dedicated broker account. The HA Mosquitto add-on can authenticate a dedicated
local Home Assistant user; another broker can use its normal local password
configuration. The bridge requires publish access to its discovery/state/map
topics and subscribe access to its command/settings topics and
`homeassistant/status`. Plain MQTT is confined to the trusted local network by
the generated service policy.

Create a private directory outside this checkout, with mode 0700, and copy
`install.example.json` to `install.json` under `umask 077`. Edit the private copy
in a local editor; never put the MQTT password on the command line.

```sh
umask 077
mkdir -m 700 ~/narwal-private
cp deploy/install.example.json ~/narwal-private/install.json
```

Set `bind_ipv4` to the bridge's **robot-facing address**, `robot_ipv4` to the
robot's reserved address and `mqtt.host` to the broker's IPv4 address. The
installer requires canonical dotted-decimal IPv4 addresses, without leading
zeroes, so the service firewall can allow only those peers.
The robot must actually be using a DNS resolver you control.

Replace the example room catalog with IDs verified from your robot's room map
and app. Record app room names and map references before redirecting its
connection. An ID is a protocol identifier, **not the number in a room's name**.
The example ID and label are placeholders, not a discovered room. Use one
verified room for the first supervised run. Catalogs accept 1–32 unique IDs
from 0 to 255, unique labels up to 128 UTF-8 bytes, and a selected subset.
`passes` is 1–3; `dampness` is exactly `Normal` or `Wet`.

These settings supply the initial catalog. Once a current robot map arrives,
its room IDs and names become authoritative. Use the Rooms editor to save
HA-local names; those aliases persist across map refreshes.

If room IDs are not yet known, the example catalog can be used to establish
the initial connection only. Leave all cleaning controls untouched, inspect
the local map, then stop the service and replace the catalog as described below
before the first Start.

Keep `topic_prefix` and `client_id` unique per robot. Changing a client ID later
creates new HA discovery identities. Never share a MQTT identity with a public
broker or expose the HTTP API to another network.
The prefix accepts 1–256 UTF-8 bytes with nonempty slash-separated levels;
spaces, dots and Unicode are literal data, while control characters and MQTT
wildcards (`+`, `#`) are refused. Credentials are bounded by UTF-8 bytes:
username up to 256 and password up to 8192. The `mqtt` object accepts exactly
the six keys shown in the example; options such as `tls` are unsupported.

## Install without starting

Pass absolute paths. The installer refuses existing installation paths and
accounts rather than overwriting a partial deployment.

```sh
sudo bash deploy/install.sh \
  "$HOME/narwal-private/install.json" \
  "$PWD/target/release/narwal-local-service"
```

It installs the executable and a canonical root-owned copy of socat in
`/opt/narwal-local-ha`, generates a local certificate/key, and creates:

| Path | Purpose |
|---|---|
| `/var/lib/narwal-local-ha/config.json` | Private runtime and MQTT configuration |
| `/var/lib/narwal-local-ha/profile.json` | Explicit source/feedback command contracts |
| `/var/lib/narwal-local-ha/settings.json` | Durable room/pass/dampness selection |
| `/var/lib/narwal-local-ha/intent.json` | Durable command journal; initialized once |
| `/var/lib/narwal-local-ha/room-editor.json` | Private room aliases, draft and map-edit state |
| `/var/lib/narwal-local-ha/key.pem` | Host-local generated TLS private key |
| `/var/lib/narwal-local-ha/certificate.pem` | Host-local generated TLS certificate |
| `/etc/narwal-local-ha/frontend.env` | Source-filtered TLS listener configuration |

Private files are mode 0600 in canonical mode-0700 directories. UID and GID are
looked up independently. The initializer runs under the service account and
does not open MQTT or robot connections. The installer only reloads systemd
metadata; services remain stopped and disabled.

The legacy robot requires TLS 1.0 / AES128-SHA at this frontend. This setting is
scoped to socat, with a single source IPv4 `/32`; it does not weaken system-wide
TLS configuration or install a trusted CA. Both processes have the same UID
and matching `CAP_NET_BIND_SERVICE` sets because Linux process inspection must
allow the backend to verify the peer's `/proc/PID/exe`. Do not replace that
check with a wider privilege such as `SYS_PTRACE`.

## Connect and verify

Once redirected, the official Narwal app may show the robot as offline.
Use fresh HA/local telemetry as the source of status for this deployment.
The app is not assumed to remain connected alongside the local bridge.

1. Start `narwal-local-ha.service` with `sudo systemctl start narwal-local-ha`.
   Its TLS dependency starts after the private socket is ready.
2. In your local DNS resolver, add **only** an exact A-record override for
   `websocket.narwaltech.com` to the robot-facing bridge address. Check the answer
   from the robot's network. Ensure TCP 443 is reachable only from the robot.
   The installer does not change DNS, routing, ARP or firewall rules.
3. The robot must establish a new WebSocket connection using this DNS answer.
   An existing vendor connection may remain until it reconnects. Do not assume
   a DNS change migrates a live connection. Cold startup without any vendor
   bootstrap has not been qualified; this project supplies no blanket DNS
   redirection or credential-capture helper.
4. Read the loopback API: `curl --fail http://127.0.0.1:35394/api/state`.
   Verify a connected session, fresh status, the expected mode, no error and
   actual dock contact. Access remotely through an SSH local forward as below.
5. Check the MQTT-discovered Narwal device in Home Assistant. Verify telemetry,
   Job status and native map-image delivery before the first supervised room
   command. Check whether the robot marker is reported or inferred from dock
   contact. Select the verified room and settings, then Start once; observe
   the physical robot and compare its map region.
   Return once and verify docking. See [control behavior](../docs/CONTROL.md).
6. After acceptance, enable startup with
   `sudo systemctl enable narwal-local-ha.service`.

For remote access, substitute your bridge's SSH destination:

```sh
ssh -N -L 127.0.0.1:35394:127.0.0.1:35394 user@bridge-host
```

Open exactly `http://127.0.0.1:35394/` in the local browser. The HTTP guard
requires that host and port; `localhost`, a different forwarded port or a
proxy-rewritten Host/Origin is refused. The loopback page displays saved room
names, passes and mop dampness. Start uses those saved settings for a new job;
while mop-paused, Resume cleaning continues the existing task.

Changing settings does not move the robot. A missing/unsafe journal, stale
session, unqualified mode/task state or uncertain earlier operation refuses
motion. Inspect `last_command`, `last_refusal` and unresolved-intent status;
do not delete the journal to bypass a refusal.

For the room catalog, `/api/state` includes local `map` metadata when available
and `/map.png` returns the rendered image. Match room IDs to app references
recorded before redirecting. If another app check is needed, use an explicit
maintenance switch back to the vendor connection as described under rollback,
then re-establish and verify the local connection. Stop the service before
manually editing the private settings file, retain a backup, and preserve its
owner/mode.

## Dashboard

Install the included patched Xiaomi map-card resource and Config Template Card
as described in [dashboard/README.md](dashboard/README.md). The Rust helper generates
an installation-specific dashboard from current local room metadata and an
explicit HA entity/topic mapping. Its live room catalog supplies the current
map identifier and outlines. No real map or pre-generated household dashboard
is included in this repository. Keep runtime snapshots and generated files private.

The card supports room-outline selection and Clear selection. Mop and Vacuum
expose 1–3 requested passes. Mop also provides Normal/Wet dampness and
Pause/Resume. Both offer Return to dock. Job phase, mode, battery, percentage,
cleaned area and readable errors remain visible alongside the map.
The Rooms view prepares names, splits and merges, displays the draft preview,
and offers explicit Save/Cancel with backend capability and refusal feedback.

The runtime renders fresh reported moving poses, a white base-station marker,
plain room labels and darker reported cleaning coverage. With no fresh pose,
positive dock contact can explicitly infer the robot at the map's base point.
Do not treat room selection as position evidence. Stale off-dock markers
expire, and offline conditions hide the cached interactive map.

Optional `map_display` installation settings are `transform` (`identity`,
`rotate180`, `flip_horizontal` or `flip_vertical`), `room_labels` and
`show_trail`. Defaults are identity, labels enabled and the blue pose-history
line hidden. The renderer transforms its PNG and exported room outlines
together; the card uses identity calibration. Catalog changes refresh outlines
and clear stale selections automatically. Hiding the trail preserves route telemetry,
cleaning shading and robot/base markers.

For local diagnosis, `/api/state` exposes `pose_diagnostics`, current map and
room outlines, and each position's source/availability. Accepted incoming
poses and drawable current positions are distinct stages. These are read-only
diagnostics; they do not add command permissions or retries.

## Upgrade and rollback

Stop `narwal-local-ha.service` before updating binaries. Back up the installed
binary, both unit files and the complete private state/configuration directory
locally with restrictive permissions. Preserve `intent.json`, settings and the
host's certificate/key. Review profile compatibility before replacing it:
profile changes can invalidate a journal's qualification.

Install a compatible new binary atomically, retaining root ownership and mode
0555, then start the service and verify reconnection, telemetry and no replay.
Do not reinitialize the intent journal on upgrade. If an operation was pending,
follow its resulting robot state and require a fresh explicit command for
reconciliation; restarting never authorizes a delayed cleaning request.

Do not assume a previous installation's profile and journal are interchangeable
with this release. The supplied profile adds guarded `vacuum_end_task` support
for starting a new room job after a retained docked vacuum task. An older
six-operation profile does not enable that preparation. Compare installed
profile and journal compatibility before an upgrade, preserve rollback material,
and keep the service stopped if a migration has not been established. Never
replace or reinitialize an unresolved journal to enable a new command.

The current installer enables a private `room_editor_file`. Add that key to
an existing installation's runtime configuration when enabling the editor;
see [ROOM_EDITOR.md](../docs/ROOM_EDITOR.md). Preserve any existing editor
store and its unresolved state.

Regenerate the dashboard from fresh current-map metadata after the upgrade,
preserving your entity/topic bindings. Verify job status, moving-position
availability, image refresh, readable faults and an explicit room selection
before accepting an upgrade.

To roll back, stop the service and restore the compatible binary/configuration
set without discarding any newer unresolved intent. Keep rollback state private.
To return control to the vendor path, remove the exact DNS override you added
and stop/disable these services; a new robot connection may be required.

The generated certificate lasts 365 days. The installer does not schedule renewal.
Rotate it locally before expiry, preserve permissions, and verify a new connection afterward. A partial install
is intentionally left stopped for inspection; correct only the identified
partial resources rather than rerunning against an existing installation.
