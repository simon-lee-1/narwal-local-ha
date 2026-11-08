# Architecture and decoding

```text
Narwal J1
  local DNS: websocket.narwaltech.com -> bridge IPv4
  TLS/WebSocket :443
       |
source-filtered socat, host-local generated certificate
       |
private Unix socket, same-UID and executable verification
       |
Rust session / state / command engine / map renderer
       |
local MQTT broker -> Home Assistant MQTT discovery
                         |
                  Xiaomi Vacuum Map Card
```

The service terminates legacy messages, correlates replies, publishes
observed state and applies guarded command contracts. It does not proxy the
robot connection to the cloud. Its HTTP interface is loopback-only. The
production deployment does not need ARP interception, packet capture, a cloud
account or the development reference tools.

The codec uses the AES/base64 convention published by upstream
implementations. Its fixed key is public protocol data, not a device password,
MQTT credential, generated TLS key or captured session secret.
Operational credentials and identity remain on the installation host.

## Establishing command behavior

Reference app operations were compared with upstream conventions for
room selection, passes, dampness, pause and Return. Local requests were
then exercised individually, matching command receipts with fresh robot
state and keeping physical observations separate from ACKs.

`cmd:5` carries `clean_order` and `floor_type`. Repeated room IDs encode
requested passes; aligned floor-type values distinguish Normal and Wet.
Pause and Resume both use bare `cmd:3`, with different guarded source states.
Return is `cmd:1`; `cmd:4` is task clearance and does not substitute for Return.
The engine can use mode-specific task clearance for a retained docked task
before a new frozen room plan, only after fresh fully-idle feedback.

The production profile contains explicit state transitions. It is not a
generic command console, and unsupported model behavior is not inferred
from a successful transport write.

## Map decoding and rendering

The decoder handles compressed grid data and room membership. Reported
cleaning-layer values are interpreted separately from the static map; only
classified pass values shade/count as cleaned. Percentages do not create
paths or coverage.

Robot and dock points use the upstream direct world-to-grid projection:
`(x - origin_x) / resolution`, `(y - origin_y) / resolution`.
`origin_angle` remains metadata rather than an additional point rotation.
Invalid coordinates are rejected. A configured display transform is applied
once to the rendered layers and exported room outlines.

Room outlines trace the largest connected component of each room mask.
Detached cells are reported as omitted; holes or ambiguous boundaries produce
an outline error. The generator requires usable current outlines rather
than enclosing adjacent rooms in fabricated rectangles.

Dark plain room labels are drawn after the transform, so text stays upright.
A white square with a dark outline marks the base station; a smaller red
circle with a white rim marks the robot. A current reported pose expires
after 30 seconds. Fresh positive dock contact can separately infer a docked
robot at the base station. A selected room is never used to infer position.

Disconnects, changed maps/sessions and expiry remove the live robot marker.
Static map and ended coverage may remain available. The optional blue
pose-history line is hidden by default; hiding it does not discard telemetry
or change reported cleaning shading.

One canonical PNG topic feeds both native MQTT image and compatible camera
entities. The interactive frontend uses the image entity's update timestamp
for refresh. Its room polygons use the bridge's transformed display pixels
with identity calibration, so no second flip or rotation belongs in the card.

## Portable dashboard generation

The Rust dashboard helper reads a private current-state snapshot and an
operator-supplied entity/topic mapping. It emits a private JSON dashboard
containing that installation's map ID and room geometry. These generated
files are not distribution material.

The unchanged Xiaomi card handles polygon selection, zoom and pass controls.
Its service schema publishes nonretained `room_clean` messages through Home
Assistant's local MQTT integration. The backend validates the explicit plan
again against current state and does not overwrite it with saved selectors.
Regenerate after any map, room catalog or orientation change.

## Upstream roles

| Project | Inspected revision | Role |
|---|---|---|
| [ha-narwal-t10](https://github.com/nitinkr0411/ha-narwal-t10) | `5ffdbc931140ac22f2d511350e5a2f7cb4a9cae5` | Legacy message framing/crypto, reply correlation, status/commands, map conventions and HA presentation |
| [matterbridge-narwal](https://github.com/redelva/matterbridge-narwal) | `9baa889f01f94391580565af14f0116ecbba7a92` | China/J1 discovery conventions, room/map fields and cleaning-order representation |
| [NarwalIntegration](https://github.com/sjmotew/NarwalIntegration) | `90a4e8455fd21828ab20a422439f5dcd4202509a` | Comparison with newer local WebSocket devices; its documentation excludes legacy J1/T10 |
| [Xiaomi Vacuum Map Card](https://github.com/PiotrMachowski/Lovelace-Xiaomi-Vacuum-Map-card) | Frontend tested with v2.4.1 | Unchanged interactive room-map frontend and service-call configuration interface |

These projects do not all implement the same robot protocol. In particular,
NarwalIntegration's newer port-9002 transport is not the J1 transport.
Upstream cloud clients used during reference work are not runtime
dependencies. Attribution and license texts are preserved in
[THIRD_PARTY_NOTICES.md](../THIRD_PARTY_NOTICES.md).
