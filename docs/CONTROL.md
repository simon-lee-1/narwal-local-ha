# Command and settings contract

The command profile supplies explicit source, no-op and feedback tuples.
The engine additionally checks fresh connection generation, status revision,
mode, main task, queue and cleaning order. A wire ACK and fresh resulting
state are separate evidence; neither alone proves physical coverage.

| Action | Local behavior |
|---|---|
| Start in idle Mop mode | Freeze selected rooms, passes and dampness; send `cmd:5` once after idle checks |
| Start in idle Vacuum mode | Freeze selected rooms with one pass; send `cmd:5` with no mop dampness values |
| New job after a retained docked task | Use the mode-specific internal `cmd:4` task clearance, then require fresh idle feedback before sending the frozen plan |
| Pause while mopping | Bare `cmd:3`, guarded by the mopping source state |
| Start while mop-paused | Bare `cmd:3` with reverse guards; resume the existing task |
| Return to dock | `cmd:1`; returning and dock contact are reported separately |

The supplied profile contains seven guarded operations. Internal `end_task`,
`vacuum_end_task`, `resume` and `vacuum_start` entries cannot be invoked
directly through ordinary HA command input. The bridge does not expose a
general Stop/cancel button. Task clearance and Return have different commands
and behavior.

Mop mode is `clean_mode:3`; Vacuum is `clean_mode:2`. A fresh idle Start requires
status 100, work 0, dock contact, no error, main task 0 and an empty task queue.
Floor mopping feedback is 302/work 1/off-dock; mop pause is 303/work 0/off-dock.
Vacuum floor feedback is 300/work 1/off-dock/main task 1.

A retained task at the dock is not fully idle. Mop status 303 can use internal
`end_task`; Vacuum status 301 can use `vacuum_end_task`. Bounded preparation
requires newer same-connection idle feedback before the new plan is sent.
Vacuum preparation additionally requires mode 2, main task 0, an empty queue
and an explicitly empty cleaning order. Fault, timeout, disconnect or
incomplete feedback prevents Start. Washing and active dock cycles do not
qualify, and restarting never arranges delayed motion.

Room plans repeat each selected protocol room ID for the requested pass
count. Normal uses `floor_type:0`; Wet uses `2`. Vacuum uses an empty
`floor_type` array. These are decoded requested settings, not measurements
of physical passes or water output. Mop supports 1–3 passes and Normal/Wet.
Vacuum currently admits one pass; repeated vacuum passes and Vacuum
Pause/Resume are unqualified. Dry is unsupported.

The runtime retains guarded operator-recovery support, but the supplied
profile does not enable it or contain a `vacuum_resume` command. Ordinary HA
controls cannot invoke it. It is not automatic fault recovery.

## MQTT interface

With the configured prefix represented by `<prefix>`:

| Topic | Input or output |
|---|---|
| `<prefix>/command` | Non-retained `start`, `pause`, `return_to_base`, when supported |
| `<prefix>/command/room_clean` | Current map ID and explicit room/pass/dampness plan |
| `<prefix>/room/set` | Exact configured room label |
| `<prefix>/passes/set` | 1–3 in Mop mode; one in Vacuum mode |
| `<prefix>/dampness/set` | Exact `Normal` or `Wet`, applicable to Mop |
| `<prefix>/state` | Retained observed state, job status, tracking, settings and command diagnostics |
| `<prefix>/availability` | Retained availability and offline last will |
| `<prefix>/map/image` | Retained local map PNG for the image and camera entities |

`room_clean` accepts a JSON object with exactly `map_id`, `rooms`, `passes`
and `dampness`. The map ID must match the current map. Rooms must be 1–32
unique numeric IDs from the configured catalog. Passes must fit the current
mode, and dampness must be Normal or Wet even though the Vacuum wire command
omits it. The payload is limited to 2048 bytes. Use QoS 0 and `retain: false`.
Unknown/duplicate fields, malformed values, retained messages and duplicate
MQTT deliveries are rejected.

Explicit map requests freeze their own plan without changing saved selectors.
Ordinary `vacuum.start` uses the saved plan for a new task, or resumes the
existing paused Mop task without replacing its rooms. Changing a setting
persists it without motion.

The engine owns serial delivery and writes durable intent before motion.
Uncertain sends remain unresolved across restart. Reconnection republishes
state and discovery without replaying commands. A new explicit request may
reconcile an earlier preparation only against fresh qualifying state.
Do not delete a journal to bypass uncertainty.

Check `last_command` and `last_refusal` along with current native state.
A room Start receipt records `requested_rooms` and `requested_clean_order`,
including repeats. These preserve the request, not proof of completed
physical passes. MQTT-admission refusals are discarded rather than queued
for a later dock cycle.

## Status and map

Availability requires a connected session and status no more than 30 seconds
old. Job labels distinguish supported preparation, mop washing, mopping,
vacuuming, returning, leaving dock, paused and idle states. Faults take
precedence. An unqualified active dock phase is **Dock cycle**, not an inferred
Drying phase. Dock contact remains separate from paused or retained tasks.

Progress and area come from the robot. A task can finish and return to the
dock without a final 100% progress report, then reset the counters. The bridge
does not manufacture completion percentages from docking.

Fresh reported poses have been observed moving during local room jobs.
Map tracking identifies `reported_pose` separately from `dock_contact`, the
fallback inference placing a docked robot at the map's base station. A pose
requires a current-session map, valid coordinates and source age under
30 seconds. Stale off-dock markers disappear. Selected rooms are not used as
position evidence.

The PNG combines the room map, reported cleaning layer, plain room labels
and robot/base markers. Cleaning-layer states 1–3 shade/count reported passes;
unclassified states do not invent cleaned regions. Coverage can persist after
the job ends. `map_display.show_trail` defaults to false; enabling it draws
reported pose history without changing the cleaned layer.

The loopback API exposes `pose_diagnostics` for delivery counts and rendering
eligibility, and current display-pixel room outlines for the dashboard
generator. These read-only diagnostics do not permit commands or bypass
intent guards.
