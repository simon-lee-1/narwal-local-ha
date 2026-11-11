use crate::{
    control::{CommandInput, CommandProfile, Decision, VerifiedCommand},
    maps::RoomMap,
    session::{CommandObservation, Event, Operation, Request},
    state::ReportedState,
};
use serde_json::{Value, json};
use std::{collections::VecDeque, sync::Arc, time::Instant};
use tokio::sync::{Mutex, mpsc, watch};

#[path = "editor_runtime.rs"]
mod editor_runtime;
pub(crate) use editor_runtime::validate_editor_bytes;
pub use editor_runtime::{DraftEdit, EditorInput};

// Display policy, independent of status freshness or the robot's reporting cadence.
const POSE_DISPLAY_TTL: std::time::Duration = std::time::Duration::from_secs(30);
// Conservative interpolation limits, not validated robot speed/brush width.
const TRAIL_JOIN_TTL: std::time::Duration = std::time::Duration::from_secs(3);
const MAX_TRAIL_POINTS: usize = 8192;
const POSE_IMAGE_INTERVAL: std::time::Duration = std::time::Duration::from_millis(1500);
const COMMAND_ADMISSION_WAIT: std::time::Duration = std::time::Duration::from_secs(3);

#[derive(Default)]
struct CleaningTrail {
    points: VecDeque<crate::maps::TrailPoint>,
    // An observed context, not a device job ID.
    kind: Option<(u16, u16)>,
    ended: bool,
    last_join: Option<(crate::maps::Point, Instant)>,
    last_recorded_at: Option<Instant>,
    truncated: bool,
}
impl CleaningTrail {
    fn clear(&mut self) -> bool {
        let changed = !self.points.is_empty();
        *self = Self::default();
        changed
    }
    fn break_segment(&mut self) {
        self.last_join = None;
    }
    fn append(&mut self, point: crate::maps::Point, now: Instant) {
        let joins = self.last_join.as_ref().is_some_and(|(last, at)| {
            now.checked_duration_since(*at)
                .is_some_and(|age| age < TRAIL_JOIN_TTL)
                && (point.x - last.x).hypot(point.y - last.y) <= 1.0
        });
        self.points.push_back(crate::maps::TrailPoint {
            point: point.clone(),
            starts_segment: !joins,
        });
        if self.points.len() > MAX_TRAIL_POINTS {
            self.points.pop_front();
            if let Some(first) = self.points.front_mut() {
                first.starts_segment = true;
            }
            self.truncated = true;
        }
        self.last_join = Some((point, now));
        self.last_recorded_at = Some(now);
    }
}

type Attachment = Option<(u64, mpsc::Sender<Request>)>;
#[derive(Clone)]
pub struct Engine {
    inner: Arc<Mutex<Inner>>,
    requests: Arc<Mutex<Attachment>>,
    serial: Arc<Mutex<()>>,
    command_admission: Arc<Mutex<()>>,
    pub states: watch::Sender<Value>,
    pub images: watch::Sender<Option<Vec<u8>>>,
    pub edit_images: watch::Sender<Option<Vec<u8>>>,
}
struct Inner {
    state: ReportedState,
    map: Option<RoomMap>,
    raw_map: Option<Value>,
    editor: editor_runtime::EditorState,
    map_generation: Option<u64>,
    rendered_marker: Option<(u64, u64, &'static str)>,
    image_rendered_at: Option<Instant>,
    image_dirty: bool,
    pose: Option<Value>,
    pose_received_at: Option<Instant>,
    pose_diagnostics: Option<(u64, Arc<crate::session::Diagnostics>)>,
    trail: CleaningTrail,
    clean_layer: Option<crate::maps::CleanLayer>,
    clean_layer_received_at: Option<Instant>,
    clean_layer_error: Option<&'static str>,
    map_display: crate::maps::MapDisplay,
    map_error: Option<String>,
    profile: CommandProfile,
    last_command: Option<Value>,
    last_refusal: Option<Value>,
    mop_settings: Option<crate::plan::Store>,
    pending_intent: Option<PendingIntent>,
    intent_store: Option<crate::intent::Store>,
}
#[derive(Clone)]
struct PendingIntent {
    command: String,
    observation: Option<CommandObservation>,
    expected: VerifiedCommand,
    recovery_room: Option<u64>,
}
impl PendingIntent {
    fn accepts(&self, after: &CommandObservation) -> bool {
        self.observation
            .as_ref()
            .is_none_or(|before| after.follows(before))
            && feedback_matches(&self.command, &self.expected, after, self.recovery_room)
    }
}
fn preparation_command(
    profile: &CommandProfile,
    state: &crate::control::StatePredicate,
    context: Option<&crate::control::MopContext>,
) -> Option<&'static str> {
    let context = context?;
    let name = if context.active_vacuum() && crate::control::eligible(profile, "vacuum_start") {
        "vacuum_end_task"
    } else if context.active_mop() {
        "end_task"
    } else {
        return None;
    };
    (crate::control::eligible(profile, name)
        && profile.commands[name].source_states.contains(state))
    .then_some(name)
}
fn feedback_matches(
    command: &str,
    expected: &VerifiedCommand,
    after: &CommandObservation,
    recovery_room: Option<u64>,
) -> bool {
    expected.feedback_states.contains(&after.state)
        && (command != "vacuum_start"
            || after
                .mop_context
                .as_ref()
                .is_some_and(crate::control::MopContext::active_vacuum))
        && (command != "end_task"
            || after
                .mop_context
                .as_ref()
                .is_some_and(crate::control::MopContext::idle_mop))
        && (command != "vacuum_end_task"
            || (after
                .mop_context
                .as_ref()
                .is_some_and(crate::control::MopContext::idle_vacuum)
                && after.clean_order.as_ref().is_some_and(Vec::is_empty)))
        && (command != "vacuum_resume"
            || (after
                .mop_context
                .as_ref()
                .is_some_and(crate::control::MopContext::active_vacuum)
                && recovery_room.is_some_and(|id| after.clean_order.as_deref() == Some(&[id]))))
}
struct RequestFailure {
    message: String,
    before_send: bool,
}
impl Engine {
    pub fn open_persistent(
        profile: CommandProfile,
        path: &std::path::Path,
    ) -> Result<Self, String> {
        let store = crate::intent::Store::open(path, &profile)?;
        let pending = store.pending().map(|command| PendingIntent {
            command: command.into(),
            observation: None,
            expected: profile.commands[command].clone(),
            recovery_room: profile
                .operator_recovery
                .as_ref()
                .map(|admission| admission.room_id),
        });
        Ok(Self::build(profile, Some(store), pending))
    }
    pub fn initialize_intent(
        profile: &CommandProfile,
        path: &std::path::Path,
    ) -> Result<(), String> {
        crate::intent::Store::initialize(path, profile)
    }
    pub fn new(profile: CommandProfile) -> Self {
        Self::build(profile, None, None)
    }

    fn build(
        profile: CommandProfile,
        intent_store: Option<crate::intent::Store>,
        pending_intent: Option<PendingIntent>,
    ) -> Self {
        let (states, _) =
            watch::channel(json!({"online":false,"connected":false,"commands_enabled":false}));
        let (images, _) = watch::channel(None);
        let (edit_images, _) = watch::channel(None);
        Self {
            inner: Arc::new(Mutex::new(Inner {
                state: ReportedState::default(),
                map: None,
                raw_map: None,
                editor: editor_runtime::EditorState::default(),
                map_generation: None,
                rendered_marker: None,
                image_rendered_at: None,
                image_dirty: false,
                pose: None,
                pose_received_at: None,
                pose_diagnostics: None,
                trail: CleaningTrail::default(),
                clean_layer: None,
                clean_layer_received_at: None,
                clean_layer_error: None,
                map_display: crate::maps::MapDisplay::default(),
                map_error: None,
                profile,
                last_command: None,
                last_refusal: None,
                mop_settings: None,
                pending_intent,
                intent_store,
            })),
            requests: Arc::new(Mutex::new(None)),
            serial: Arc::new(Mutex::new(())),
            command_admission: Arc::new(Mutex::new(())),
            states,
            images,
            edit_images,
        }
    }
    pub async fn snapshot(&self) -> Value {
        let inner = self.inner.lock().await;
        let mut value = inner.state.snapshot(Instant::now());
        let enabled: Vec<_> = inner
            .profile
            .commands
            .keys()
            .filter(|name| {
                ![
                    "end_task",
                    "vacuum_end_task",
                    "vacuum_start",
                    "vacuum_resume",
                ]
                .contains(&name.as_str())
            })
            .filter(|name| {
                !(value["status"]["clean_mode"] == 2
                    && ["pause", "resume"].contains(&name.as_str()))
            })
            .filter(|_| inner.motion_ready())
            .filter(|name| name.as_str() != "start" || inner.editor.motion_ready())
            .filter(|name| crate::control::eligible(&inner.profile, name))
            .cloned()
            .collect();
        let room_capable = enabled.iter().any(|name| name == "start")
            && inner
                .profile
                .commands
                .get("start")
                .is_some_and(|command| command.rooms_supported);
        value["commands_enabled"] = json!(!enabled.is_empty());
        value["enabled_commands"] = json!(enabled);
        value["room_selection_enabled"] = json!(room_capable && inner.mop_settings.is_none());
        value["map"] = json!(inner.map);
        value["map_tracking"] = inner.map_tracking(Instant::now());
        value["pose_diagnostics"] = inner.pose_diagnostics(Instant::now());
        value["map_error"] = json!(inner.map_error);
        value["room_catalog"] = inner.editor.catalog.clone();
        value["room_editor"] = inner.editor_snapshot();
        value["last_command"] = json!(inner.last_command);
        value["last_refusal"] = json!(inner.last_refusal);
        value["mop_settings"] = json!(
            inner
                .mop_settings
                .as_ref()
                .filter(|store| store.healthy())
                .map(|store| store.settings())
        );
        value["mop_settings_ready"] = json!(
            inner
                .mop_settings
                .as_ref()
                .is_some_and(|store| store.healthy())
        );
        // This describes the currently usable internal preparation, never an
        // externally callable command or an inferred robot capability.
        value["room_clean_preparation"] = json!(if room_capable
            && inner.pending_intent.is_none()
            && inner.state.fresh(Instant::now())
            && inner
                .mop_settings
                .as_ref()
                .is_some_and(|store| store.healthy())
        {
            crate::control::StatePredicate::from_status(&value["status"])
                .zip(crate::control::MopContext::from_status(&value["status"]))
                .and_then(|(state, context)| {
                    preparation_command(&inner.profile, &state, Some(&context))
                })
        } else {
            None
        });
        value["motion_intent_unresolved"] = json!(inner.pending_intent.is_some());
        value["motion_persistence_ready"] = json!(inner.motion_ready());
        value
    }
    pub async fn configure_mop(&self, path: &std::path::Path) -> Result<(), String> {
        let store = crate::plan::Store::open(path)?;
        let mut inner = self.inner.lock().await;
        let start = inner
            .profile
            .commands
            .get("start")
            .ok_or("mop_profile_required")?;
        if !inner.motion_ready()
            || !crate::control::eligible(&inner.profile, "start")
            || !start.rooms_supported
            || start.params.get("cmd") != Some(&json!(5))
            || start.params.as_object().is_none_or(|fields| {
                fields.len() != 3
                    || !fields.contains_key("clean_order")
                    || !fields.contains_key("floor_type")
            })
        {
            return Err("mop_profile_required".into());
        }
        inner.mop_settings = Some(store);
        drop(inner);
        self.publish().await;
        Ok(())
    }
    pub async fn change_mop(&self, change: crate::plan::Change) -> Result<(), String> {
        let mut inner = self.inner.lock().await;
        let result = inner
            .mop_settings
            .as_mut()
            .ok_or("mop_settings_unavailable")?
            .update(change);
        drop(inner);
        self.publish().await;
        result
    }
    pub async fn publish(&self) {
        self.publish_with_time(None).await;
    }

    async fn publish_with_time(&self, pose_time: Option<Instant>) {
        let mut inner = self.inner.lock().await;
        let now = pose_time.unwrap_or_else(Instant::now);
        let pose_expired = inner.pose.is_some() && inner.fresh_pose(now).is_none();
        if pose_expired {
            inner.clear_pose();
        }
        if !inner.strict_status_fresh(now)
            || inner.trail.last_join.as_ref().is_some_and(|(_, at)| {
                now.checked_duration_since(*at)
                    .is_none_or(|age| age >= TRAIL_JOIN_TTL)
            })
        {
            inner.trail.break_segment();
        }
        let marker = inner.marker_key(now);
        inner.image_dirty |= marker != inner.rendered_marker;
        let invalidated = pose_expired || (marker.is_none() && inner.rendered_marker.is_some());
        let render_due = inner.image_rendered_at.is_none_or(|at| {
            now.checked_duration_since(at)
                .is_some_and(|age| age >= POSE_IMAGE_INTERVAL)
        });
        if inner.image_dirty && (invalidated || render_due) {
            self.images.send_replace(inner.render_image(now));
        }
        drop(inner);
        self.states.send_replace(self.snapshot().await);
    }
    pub async fn event(&self, event: Event) {
        self.event_with_time(event, None).await;
    }

    async fn event_with_time(&self, event: Event, pose_time: Option<Instant>) {
        let mut inner = self.inner.lock().await;
        let now = pose_time.unwrap_or_else(Instant::now);
        let mut image_changed = false;
        match event {
            Event::Connected { generation } if generation > inner.state.generation => {
                inner.state.connect(generation);
                inner.clear_pose();
                inner.clear_cleaning_history();
                image_changed = true;
            }
            Event::Disconnected { generation }
                if inner.state.generation == generation && inner.state.connected =>
            {
                inner.state.disconnect(generation);
                inner.clear_pose();
                inner.clear_cleaning_history();
                image_changed = true;
            }
            Event::Status { generation, result }
                if inner.state.generation == generation
                    && inner.state.connected
                    && result.is_object() =>
            {
                // A missing status interval can hide an entire task boundary.
                if !inner.strict_status_fresh(now) && !inner.trail.ended {
                    image_changed |= inner.clear_cleaning_history();
                }
                inner.state.update(generation, result, now);
                image_changed |= inner.observe_trail_status(now);
            }
            Event::Map { generation, result }
                if inner.state.generation == generation && inner.state.connected =>
            {
                match inner.accept_editor_map(result, true) {
                    Ok(()) => {
                        inner.map_generation = Some(generation);
                        inner.map_error = None;
                        inner.clear_pose();
                        inner.clear_cleaning_history();
                        image_changed = true;
                    }
                    Err(error) => {
                        inner.map_error = Some(error);
                        image_changed = true;
                    }
                }
            }
            Event::Pose { generation, result }
                if inner.state.generation == generation && inner.state.connected =>
            {
                inner.pose = Some(result);
                inner.pose_received_at = Some(now);
                inner.record_trail_pose(now);
                inner.record_cleaning_layer(now);
                inner.image_dirty = true;
                // An invalid latest position must remove or replace its old
                // marker immediately; ordinary movement can be coalesced.
                image_changed = inner.reported_marker(now).is_none();
            }
            _ => {}
        }
        if image_changed {
            // Keep rendering and cache replacement in the state critical section:
            // an older pose render must never overwrite an invalidated image.
            self.images.send_replace(inner.render_image(now));
        }
        drop(inner);
        self.publish_with_time(pose_time).await;
    }
    pub async fn load_map(&self, value: &Value) -> Result<(), String> {
        let mut inner = self.inner.lock().await;
        inner.accept_editor_map(value.clone(), false)?;
        inner.map_generation = None;
        inner.map_error = None;
        inner.clear_pose();
        inner.clear_cleaning_history();
        let image = inner.map_png(Instant::now());
        let result = image.as_ref().map(|_| ()).map_err(Clone::clone);
        self.images.send_replace(image.ok());
        inner.mark_image_rendered(Instant::now());
        drop(inner);
        self.publish().await;
        result
    }
    pub async fn configure_map_display(&self, display: crate::maps::MapDisplay) {
        let mut inner = self.inner.lock().await;
        inner.map_display = display;
        if let Some(raw) = inner.raw_map.clone() {
            if let Err(error) = inner.accept_editor_map(raw, false) {
                inner.map_error = Some(error);
            }
        }
        self.images.send_replace(inner.render_image(Instant::now()));
        drop(inner);
        self.publish().await;
    }
    pub async fn map_png(&self) -> Result<Vec<u8>, String> {
        self.map_png_with_time(None).await
    }

    async fn map_png_with_time(&self, pose_time: Option<Instant>) -> Result<Vec<u8>, String> {
        let inner = self.inner.lock().await;
        inner.map_png(pose_time.unwrap_or_else(Instant::now))
    }
    pub async fn attach(&self, generation: u64, requests: Option<mpsc::Sender<Request>>) {
        *self.requests.lock().await = requests.map(|r| (generation, r));
    }
    pub async fn attach_diagnostics(
        &self,
        generation: u64,
        diagnostics: Arc<crate::session::Diagnostics>,
    ) {
        self.inner.lock().await.pose_diagnostics = Some((generation, diagnostics));
    }
    async fn request_inner(&self, operation: Operation) -> Result<Value, String> {
        self.request_for(None, operation).await
    }
    async fn request_for(
        &self,
        expected: Option<u64>,
        operation: Operation,
    ) -> Result<Value, String> {
        self.request_tracked(expected, operation)
            .await
            .map_err(|failure| failure.message)
    }
    async fn request_tracked(
        &self,
        expected: Option<u64>,
        operation: Operation,
    ) -> Result<Value, RequestFailure> {
        let before = |message: &str| RequestFailure {
            message: message.into(),
            before_send: true,
        };
        let uncertain = |message: &str| RequestFailure {
            message: message.into(),
            before_send: false,
        };
        let (generation, sender) = self
            .requests
            .lock()
            .await
            .clone()
            .ok_or_else(|| before("robot_disconnected"))?;
        if expected.is_some_and(|expected| expected != generation) {
            return Err(before("connection_changed"));
        }
        let (tx, rx) = tokio::sync::oneshot::channel();
        sender
            .try_send(Request {
                operation,
                reply: tx,
            })
            .map_err(|_| before("robot_busy_or_disconnected"))?;
        tokio::time::timeout(std::time::Duration::from_secs(22), rx)
            .await
            .map_err(|_| uncertain("request_timeout"))?
            .map_err(|_| uncertain("robot_disconnected"))?
            .map_err(|message| RequestFailure {
                before_send: message == "session_disconnected_before_send"
                    || message.starts_with("before_send_refused"),
                message,
            })
    }
    async fn command_observation(
        &self,
        expected: Option<u64>,
    ) -> Result<CommandObservation, String> {
        let generation = self
            .requests
            .lock()
            .await
            .as_ref()
            .map(|(generation, _)| *generation)
            .ok_or("robot_disconnected")?;
        if expected.is_some_and(|expected| expected != generation) {
            return Err("connection_changed".into());
        }
        let value = self
            .request_for(Some(generation), Operation::CommandStatus)
            .await?;
        let observation: CommandObservation =
            serde_json::from_value(value).map_err(|_| "command_observation_refused")?;
        if observation.generation != generation || observation.revision == 0 {
            return Err("command_observation_refused".into());
        }
        Ok(observation)
    }
    pub async fn refresh(&self) -> Result<Value, String> {
        let _guard = self.serial.lock().await;
        let reconcile = self
            .inner
            .lock()
            .await
            .pending_intent
            .as_ref()
            .is_some_and(|pending| pending.observation.is_some());
        if !reconcile {
            return self.request_inner(Operation::Status).await;
        }
        // Read in the current session even when a retained intent belongs to
        // an older connection. That intent must not stop ordinary polling.
        let generation = self
            .requests
            .lock()
            .await
            .as_ref()
            .map(|(generation, _)| *generation)
            .ok_or("robot_disconnected")?;
        let result = self
            .request_for(Some(generation), Operation::ObservedStatus)
            .await?;
        let after: CommandObservation = serde_json::from_value(result["observation"].clone())
            .map_err(|_| "command_observation_refused")?;
        if after.generation != generation || after.revision == 0 || !result["status"].is_object() {
            return Err("command_observation_refused".into());
        }
        let mut inner = self.inner.lock().await;
        if inner.state.connected
            && inner.state.generation == generation
            && inner.pending_intent.as_ref().is_some_and(|pending| {
                pending.observation.as_ref().is_some_and(|before| {
                    before.generation == after.generation && after.follows(before)
                }) && pending.accepts(&after)
            })
        {
            // This only reconciles one observed wire leg. It never continues
            // an abandoned task-clear sequence or replays a command. A journal
            // restored without its source observation cannot take this path.
            inner.clear_intent()?;
        }
        drop(inner);
        self.publish().await;
        Ok(result["status"].clone())
    }
    pub async fn refresh_map(&self) -> Result<Value, String> {
        let _guard = self.serial.lock().await;
        self.request_inner(Operation::Map).await
    }
    pub async fn request_positions(&self) -> Result<Value, String> {
        let _guard = self.serial.lock().await;
        let (generation, counters) = {
            let inner = self.inner.lock().await;
            if !inner.state.connected {
                return Err("robot_disconnected".into());
            }
            let generation = inner.state.generation;
            let counters = inner
                .pose_diagnostics
                .as_ref()
                .filter(|(attached, _)| *attached == generation)
                .map(|(_, diagnostics)| diagnostics.snapshot())
                .unwrap_or(Value::Null);
            (generation, counters)
        };
        let mut result = self
            .request_for(Some(generation), Operation::RequestPositions)
            .await?;
        result
            .as_object_mut()
            .ok_or("position_request_receipt_refused")?
            .insert("pose_counters_before".into(), counters);
        Ok(result)
    }
    pub async fn command(&self, input: CommandInput) -> Result<Value, String> {
        self.command_bound(input, None).await
    }
    pub async fn room_command(
        &self,
        input: CommandInput,
        map_id: String,
        revision: String,
    ) -> Result<Value, String> {
        if input.command != "start" || input.mop.is_none() {
            return Err("room_selection_invalid".into());
        }
        self.command_bound(input, Some((map_id, revision))).await
    }
    async fn command_bound(
        &self,
        input: CommandInput,
        binding: Option<(String, String)>,
    ) -> Result<Value, String> {
        // The owner is detached from HTTP/MQTT caller cancellation. Its lock and
        // unresolved intent cannot disappear because a browser stops waiting.
        let admission = match self.command_admission.clone().try_lock_owned() {
            Ok(guard) => guard,
            Err(_) => {
                self.record_refusal(&input.command, "robot_busy").await;
                return Err("robot_busy".into());
            }
        };
        let engine = self.clone();
        let command = input.command.clone();
        tokio::spawn(async move {
            let _admission = admission;
            let result = match tokio::time::timeout(
                COMMAND_ADMISSION_WAIT,
                engine.serial.clone().lock_owned(),
            )
            .await
            {
                Ok(guard) => {
                    let _guard = guard;
                    engine.command_owned(input, binding).await
                }
                Err(_) => Err("robot_busy".into()),
            };
            if let Err(reason) = &result {
                engine.record_refusal(&command, reason).await;
            }
            result
        })
        .await
        .map_err(|_| "command_owner_failed".to_string())?
    }
    async fn record_refusal(&self, command: &str, reason: &str) {
        let command = if ["start", "pause", "stop", "return_to_base", "resume"].contains(&command) {
            command
        } else {
            "unknown"
        };
        self.inner.lock().await.last_refusal = Some(json!({
            "command":command,"outcome":"rejected","reason":reason
        }));
        self.publish().await;
    }
    async fn command_owned(
        &self,
        mut input: CommandInput,
        mut binding: Option<(String, String)>,
    ) -> Result<Value, String> {
        if ["end_task", "vacuum_end_task", "vacuum_start"].contains(&input.command.as_str()) {
            return Err("internal_command_only".into());
        }
        let (profile, rooms, mop_configured) = {
            let inner = self.inner.lock().await;
            if input.command == "start"
                && binding.is_none()
                && inner.map_generation == Some(inner.state.generation)
            {
                binding = inner.editor.catalog["map_id"]
                    .as_str()
                    .zip(inner.editor.catalog["revision"].as_str())
                    .map(|(id, revision)| (id.into(), revision.into()));
            }
            inner.check_room_binding(binding.as_ref())?;
            if !inner.motion_ready() {
                return Err(if inner.intent_store.is_some() {
                    "motion_persistence_failed"
                } else {
                    "motion_persistence_required"
                }
                .into());
            }
            if input.command == "start" && !inner.editor.motion_ready() {
                return Err("room_catalog_unavailable_or_write_pending".into());
            }
            if !crate::control::eligible(&inner.profile, &input.command) {
                return Err("command_guards_unverified".into());
            }
            if input.command == "vacuum_resume"
                && (!input.rooms.is_empty()
                    || input.mop.is_some()
                    || inner
                        .profile
                        .operator_recovery
                        .as_ref()
                        .is_none_or(|admission| !admission.current())
                    || inner.pending_intent.is_some()
                    || inner
                        .intent_store
                        .as_ref()
                        .is_none_or(|store| !store.operator_recovery_available()))
            {
                return Err("operator_recovery_unavailable".into());
            }
            if input.command == "start"
                && crate::control::mop_settings_required(&inner.profile)
                && inner.mop_settings.is_none()
            {
                return Err("mop_settings_unavailable".into());
            }
            let rooms = if let Some(store) = &inner.mop_settings {
                if input.command == "start" {
                    if !store.healthy() {
                        return Err("mop_settings_persistence_failed".into());
                    }
                    if !input.rooms.is_empty() {
                        return Err("room_selection_invalid".into());
                    }
                    let plan = input
                        .mop
                        .get_or_insert_with(|| store.settings().selected.clone());
                    plan.validate(&store.settings().ids())?;
                }
                store.settings().ids()
            } else {
                if input.mop.is_some() {
                    return Err("mop_settings_unavailable".into());
                }
                inner
                    .map
                    .as_ref()
                    .map(|m| m.rooms.iter().map(|r| r.room_id).collect())
                    .unwrap_or_default()
            };
            (inner.profile.clone(), rooms, inner.mop_settings.is_some())
        };
        // Validate the matched reply itself. Publishing Event::Status is not
        // proof that the separate cache consumer has processed that event.
        let mut observation = self.command_observation(None).await?;
        if input.command == "vacuum_resume" {
            let admission = profile
                .operator_recovery
                .as_ref()
                .ok_or("operator_recovery_unavailable")?;
            if !admission.current()
                || !profile.commands["vacuum_resume"]
                    .source_states
                    .contains(&observation.state)
                || !observation
                    .mop_context
                    .as_ref()
                    .is_some_and(crate::control::MopContext::active_vacuum)
                || observation.clean_order.as_deref() != Some(&[admission.room_id])
            {
                return Err("operator_recovery_context_refused".into());
            }
            // Consume durably before creating the motion intent. A crash
            // between those two writes loses permission, never replays it.
            self.inner
                .lock()
                .await
                .intent_store
                .as_mut()
                .ok_or("operator_recovery_unavailable")?
                .consume_operator_recovery()?;
        }
        let vacuum = input.command == "start"
            && observation
                .mop_context
                .as_ref()
                .is_some_and(|context| context.clean_mode == 2);
        if vacuum {
            if !crate::control::eligible(&profile, "vacuum_start") {
                return Err("vacuum_profile_unavailable".into());
            }
            if input.mop.is_none() {
                return Err("mop_settings_unavailable".into());
            }
            input.command = "vacuum_start".into();
        }
        // HA Start resumes an existing paused mop task; selected choices only
        // apply when starting a new task from the qualified idle dock state.
        if input.command == "start"
            && crate::control::eligible(&profile, "resume")
            && profile.commands["resume"]
                .source_states
                .contains(&observation.state)
        {
            input.command = "resume".into();
            input.rooms.clear();
            input.mop = None;
        }
        let mut preparation = None;
        let prepare = (input.mop.is_some()
            && ["start", "vacuum_start"].contains(&input.command.as_str()))
        .then(|| {
            preparation_command(
                &profile,
                &observation.state,
                observation.mop_context.as_ref(),
            )
        })
        .flatten();
        if let Some(prepare) = prepare {
            // This stays inside the detached serial owner. A selector change
            // cannot alter the already validated plan, and a restart never
            // reconstructs this continuation from its per-command journal.
            let generation = observation.generation;
            let revision = observation.revision;
            let (receipt, idle) = self
                .command_step(
                    &profile,
                    &CommandInput {
                        command: prepare.into(),
                        rooms: vec![],
                        mop: None,
                    },
                    &rooms,
                    observation,
                    binding.as_ref(),
                )
                .await?;
            let Some(idle) =
                idle.filter(|idle| idle.generation == generation && idle.revision > revision)
            else {
                return Err(if prepare == "vacuum_end_task" {
                    "vacuum_end_task_not_cleared"
                } else {
                    "mop_end_task_not_cleared"
                }
                .into());
            };
            preparation = Some(receipt);
            observation = idle;
        }
        if input.mop.is_some()
            && (observation.state.robot_status != 100
                || observation.state.work_status != 0
                || !observation.state.station_contact
                || observation.state.error_code != 0
                || !observation.mop_context.as_ref().is_some_and(|context| {
                    if vacuum {
                        context.idle_vacuum()
                    } else {
                        context.idle_mop()
                    }
                }))
        {
            return Err("mop_start_requires_idle_task".into());
        }
        if input.command == "resume"
            && !observation
                .mop_context
                .as_ref()
                .is_some_and(crate::control::MopContext::active_mop)
        {
            return Err("mop_resume_requires_paused_task".into());
        }
        if input.command == "pause"
            && mop_configured
            && !observation
                .mop_context
                .as_ref()
                .is_some_and(crate::control::MopContext::active_mop)
        {
            return Err("mop_pause_requires_active_task".into());
        }
        let (mut result, _) = self
            .command_step(&profile, &input, &rooms, observation, binding.as_ref())
            .await?;
        if vacuum {
            result["command"] = json!("start");
            result["mode"] = json!("Vacuum");
            self.inner.lock().await.last_command = Some(result.clone());
            self.publish().await;
        }
        if let Some(preparation) = preparation {
            result["preparation"] = preparation;
            self.inner.lock().await.last_command = Some(result.clone());
            self.publish().await;
        }
        Ok(result)
    }
    /// One durable wire leg. Only command_owned composes legs; callers cannot
    /// recurse through command() or release the serial owner between them.
    async fn command_step(
        &self,
        profile: &CommandProfile,
        input: &CommandInput,
        rooms: &[u64],
        observation: CommandObservation,
        binding: Option<&(String, String)>,
    ) -> Result<(Value, Option<CommandObservation>), String> {
        {
            let mut inner = self.inner.lock().await;
            inner.check_room_binding(binding)?;
            if let Some(pending) = &inner.pending_intent {
                if !pending.accepts(&observation) {
                    return Err("previous_motion_intent_unresolved".into());
                }
                inner.clear_intent()?;
            }
        }
        let decision = crate::control::decide(profile, input, &observation.state, rooms)?;
        let expected = profile.commands[&input.command].clone();
        let generation = observation.generation;
        if matches!(decision, Decision::NoOp) {
            let receipt = self
                .request_for(
                    Some(generation),
                    Operation::GuardedCommand {
                        observation,
                        params: None,
                    },
                )
                .await?;
            let no_op = receipt.get("no_op").and_then(Value::as_bool) == Some(true);
            let result = json!({
                "command":input.command,
                "outcome":if no_op { "no_action_needed" } else { "before_send_refused" },
                "no_op":no_op,"transport_attempted":false,"write_completed":false,
                "acknowledged":false,"verified":false
            });
            self.inner.lock().await.last_command = Some(result.clone());
            self.publish().await;
            return Ok((result, None));
        }
        let Decision::Send(params) = decision else {
            unreachable!()
        };
        // Audit the validated, frozen request and its rendered wire order.
        // These describe intent, not room coverage or transport success.
        let requested = input
            .mop
            .as_ref()
            .filter(|_| ["start", "vacuum_start"].contains(&input.command.as_str()))
            .map(|plan| (json!(plan.rooms), params["clean_order"].clone()));
        {
            let mut inner = self.inner.lock().await;
            inner.check_room_binding(binding)?;
            inner.pending_intent = Some(PendingIntent {
                command: input.command.clone(),
                observation: Some(observation.clone()),
                expected: expected.clone(),
                recovery_room: profile
                    .operator_recovery
                    .as_ref()
                    .map(|admission| admission.room_id),
            });
            if let Some(store) = &mut inner.intent_store {
                store.set_pending(Some(&input.command))?;
            }
        }
        let mut sending = json!({"command":input.command,"outcome":"sending"});
        if let Some((rooms, order)) = &requested {
            sending["requested_rooms"] = rooms.clone();
            sending["requested_clean_order"] = order.clone();
        }
        self.inner.lock().await.last_command = Some(sending);
        // Session dispatch rechecks this exact revision immediately before the
        // write. No publication or cache-read await is used as a substitute.
        let receipt = self
            .request_tracked(
                Some(generation),
                Operation::GuardedCommand {
                    observation: observation.clone(),
                    params: Some(params),
                },
            )
            .await;
        let receipt_flag = |name: &str| {
            receipt
                .as_ref()
                .ok()
                .and_then(|value| value.get(name))
                .and_then(Value::as_bool)
                .unwrap_or(false)
        };
        let before_send = match &receipt {
            Ok(value) => value.get("transport_attempted") == Some(&json!(false)),
            Err(failure) => failure.before_send,
        };
        let attempted = if before_send {
            json!(false)
        } else {
            receipt
                .as_ref()
                .ok()
                .and_then(|value| value.get("transport_attempted"))
                .cloned()
                .unwrap_or(Value::Null)
        };
        let mut result = json!({
            "command":input.command,
            "outcome":if before_send { "before_send_refused" } else { "delivery_uncertain" },
            "no_op":false,
            "transport_attempted":attempted,
            "acknowledged":receipt_flag("acknowledged"),
            "write_completed":receipt_flag("transport_written"),
            "verified":false
        });
        if let Some((rooms, order)) = requested {
            result["requested_rooms"] = rooms;
            result["requested_clean_order"] = order;
        }
        let mut verified_observation = None;
        // Never resend a motion request, even if its acknowledgement is missing.
        if before_send {
            self.inner.lock().await.clear_intent()?;
        } else {
            // Ending a retained task can finish a several-minute dock wash.
            // Only its observation window is longer: never repeat the wire
            // command, extend ordinary command feedback, or outlive this
            // absolute deadline while sleeping or awaiting a status reply.
            let end_deadline = (["end_task", "vacuum_end_task"].contains(&input.command.as_str()))
                .then(|| tokio::time::Instant::now() + std::time::Duration::from_secs(300));
            for _ in 0..if end_deadline.is_some() { 150 } else { 3 } {
                let after = if let Some(deadline) = end_deadline {
                    let next = (tokio::time::Instant::now() + std::time::Duration::from_secs(2))
                        .min(deadline);
                    tokio::time::sleep_until(next).await;
                    if tokio::time::Instant::now() >= deadline {
                        break;
                    }
                    let Ok(Ok(after)) = tokio::time::timeout_at(
                        deadline,
                        self.command_observation(Some(generation)),
                    )
                    .await
                    else {
                        break;
                    };
                    if tokio::time::Instant::now() >= deadline || after.state.error_code != 0 {
                        break;
                    }
                    if input.command == "vacuum_end_task"
                        && after
                            .mop_context
                            .as_ref()
                            .is_none_or(|context| context.clean_mode != 2)
                    {
                        break;
                    }
                    after
                } else {
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                    let Ok(after) = self.command_observation(Some(generation)).await else {
                        break;
                    };
                    after
                };
                if after.generation == generation
                    && after.follows(&observation)
                    && feedback_matches(
                        &input.command,
                        &expected,
                        &after,
                        profile
                            .operator_recovery
                            .as_ref()
                            .map(|admission| admission.room_id),
                    )
                {
                    self.inner.lock().await.clear_intent()?;
                    result["outcome"] = json!("observed_expected_state");
                    result["verified"] = json!(true);
                    if ["end_task", "vacuum_end_task"].contains(&input.command.as_str()) {
                        result["task_clear_proof"] = json!({
                            "source":observation.state,
                            "source_mop_context":observation.mop_context,
                            "target":after.state,
                            "target_mop_context":after.mop_context,
                            "same_generation":true,"newer_revision":true
                        });
                        if input.command == "vacuum_end_task" {
                            result["task_clear_proof"]["source_order"] =
                                json!(observation.clean_order);
                            result["task_clear_proof"]["target_order"] = json!(after.clean_order);
                        }
                    }
                    if input.command == "vacuum_resume" {
                        result["continuation_proof"] = json!({
                            "source":observation.state,"source_context":observation.mop_context,
                            "source_order":observation.clean_order,
                            "target":after.state,"target_context":after.mop_context,
                            "target_order":after.clean_order,
                            "same_generation":true,"newer_revision":true,
                            "operator_admission_consumed":true
                        });
                    }
                    verified_observation = Some(after);
                    break;
                }
            }
        }
        self.inner.lock().await.last_command = Some(result.clone());
        self.publish().await;
        Ok((result, verified_observation))
    }
}
impl Inner {
    fn clear_cleaning_history(&mut self) -> bool {
        let changed =
            self.trail.clear() || self.clean_layer.is_some() || self.clean_layer_error.is_some();
        self.clean_layer = None;
        self.clean_layer_received_at = None;
        self.clean_layer_error = None;
        changed
    }
    fn record_cleaning_layer(&mut self, now: Instant) {
        if !self.current_map() || !self.strict_status_fresh(now) || self.trail.ended {
            return;
        }
        // The layer may arrive during a wash or pause in the same observed job.
        // Initial dock telemetry cannot seed a new job's coverage.
        let context = self
            .state
            .status
            .as_ref()
            .and_then(crate::control::MopContext::from_status);
        if !context.is_some_and(|context| {
            self.trail.kind == Some((context.clean_mode, context.main_task))
                && matches!(context.main_task, 1 | 2)
        }) {
            return;
        }
        let Some(pose) = self.fresh_pose(now) else {
            return;
        };
        if pose.get("show_layer").is_none() {
            return;
        }
        match crate::maps::parse_reported_clean_layer(pose) {
            Ok(layer)
                if self
                    .map
                    .as_ref()
                    .is_some_and(|map| layer.matches_wire_map(map)) =>
            {
                self.clean_layer = Some(layer);
                self.clean_layer_received_at = Some(now);
                self.clean_layer_error = None;
            }
            result => {
                self.clean_layer_error = Some(if result.is_ok() {
                    "different_map"
                } else {
                    "invalid_reported_layer"
                });
                self.clean_layer = None;
                self.clean_layer_received_at = None;
            }
        }
    }
    fn cleaning_layer_tracking(&self, now: Instant) -> Value {
        let available = self.current_map() && self.clean_layer.is_some();
        let state = if !available {
            "empty"
        } else if self.trail.ended {
            "ended"
        } else if !self.strict_status_fresh(now) {
            "stale"
        } else if self.floor_kind(now).is_some() {
            "collecting"
        } else {
            "paused"
        };
        json!({
            "available":available,
            "source":"reported_show_layer",
            "state":state,
            "last_layer_age_seconds":self.clean_layer_received_at
                .and_then(|at| now.checked_duration_since(at)).map(|age| age.as_secs()),
            "error":self.clean_layer_error,
            "layer":self.clean_layer.as_ref().map(|layer| layer.summary()),
        })
    }
    fn strict_status_fresh(&self, now: Instant) -> bool {
        self.state.connected
            && self.state.updated.is_some_and(|at| {
                now.checked_duration_since(at)
                    .is_some_and(|age| age < POSE_DISPLAY_TTL)
            })
    }
    fn floor_kind(&self, now: Instant) -> Option<(u16, u16)> {
        if !self.strict_status_fresh(now) {
            return None;
        }
        let status = self.state.status.as_ref()?;
        let tuple = crate::control::StatePredicate::from_status(status)?;
        let context = crate::control::MopContext::from_status(status)?;
        (tuple.work_status == 1
            && !tuple.station_contact
            && tuple.error_code == 0
            && matches!(context.main_task, 1 | 2)
            && status.get("task_name").and_then(Value::as_str) == Some("RoomClean")
            && matches!(
                (tuple.robot_status, context.clean_mode),
                (300, 2) | (302, 3)
            ))
        .then_some((context.clean_mode, context.main_task))
    }
    fn observe_trail_status(&mut self, now: Instant) -> bool {
        if let Some(kind) = self.floor_kind(now) {
            let changed = if self.trail.ended || self.trail.kind.is_some_and(|old| old != kind) {
                self.clear_cleaning_history()
            } else {
                false
            };
            self.trail.kind = Some(kind);
            self.trail.ended = false;
            return changed;
        }
        self.trail.break_segment();
        let Some(status) = self.state.status.as_ref() else {
            return false;
        };
        if status.get("task_name").and_then(Value::as_str) == Some("SysStartOperation") {
            return self.clear_cleaning_history();
        }
        // A new dock preparation phase must not retain the previous job's layer.
        if self.trail.ended
            && status.get("task_name").and_then(Value::as_str) == Some("RoomClean")
            && crate::control::StatePredicate::from_status(status).is_some_and(|tuple| {
                tuple.robot_status == 406
                    && tuple.work_status == 1
                    && tuple.station_contact
                    && tuple.error_code == 0
            })
            && crate::control::MopContext::from_status(status)
                .is_some_and(|context| context.main_task == 1)
        {
            return self.clear_cleaning_history();
        }
        if let (Some(tuple), Some(context)) = (
            crate::control::StatePredicate::from_status(status),
            crate::control::MopContext::from_status(status),
        ) && tuple.robot_status == 100
            && tuple.work_status == 0
            && tuple.station_contact
            && tuple.error_code == 0
            && context.main_task == 0
            && context.task_queue_empty
        {
            // The same cleared state can follow completion or cancellation.
            self.trail.ended = true;
        }
        false
    }
    fn record_trail_pose(&mut self, now: Instant) {
        let point = self
            .floor_kind(now)
            .zip(self.current_map().then_some(()))
            .and_then(|(kind, ())| {
                let pose = self.fresh_pose(now)?;
                let point = crate::maps::Point {
                    x: pose.get("x")?.as_f64()?,
                    y: pose.get("y")?.as_f64()?,
                };
                let pixel = self.map.as_ref()?.world_pixel(point.x, point.y)?;
                Some((kind, point, pixel))
            });
        let Some((kind, point, pixel)) = point else {
            self.trail.break_segment();
            return;
        };
        if self.trail.ended || self.trail.kind != Some(kind) {
            self.clear_cleaning_history();
        }
        self.trail.kind = Some(kind);
        self.trail.ended = false;
        if self.trail.last_join.as_ref().is_some_and(|(previous, _)| {
            self.map
                .as_ref()
                .and_then(|map| map.world_pixel(previous.x, previous.y))
                .is_none_or(|(x, y)| {
                    (pixel.0 - x).abs().max((pixel.1 - y).abs())
                        > crate::maps::MAX_TRAIL_JOIN_PIXELS
                })
        }) {
            self.trail.break_segment();
        }
        self.trail.append(point, now);
    }
    fn trail_tracking(&self, now: Instant) -> Value {
        let state = if self.trail.points.is_empty() {
            "empty"
        } else if self.trail.ended {
            "ended"
        } else if !self.strict_status_fresh(now) {
            "stale"
        } else if self.floor_kind(now).is_some()
            && self.trail.last_join.as_ref().is_some_and(|(_, at)| {
                now.checked_duration_since(*at)
                    .is_some_and(|age| age < TRAIL_JOIN_TTL)
            })
        {
            "collecting"
        } else {
            "paused"
        };
        json!({
            "available": self.current_map() && !self.trail.points.is_empty(),
            "source": "reported_pose_history",
            "state": state,
            "points": self.trail.points.len(),
            "segments": self.trail.points.iter().filter(|point| point.starts_segment).count(),
            "last_point_age_seconds": self.trail.last_recorded_at.and_then(|at| now.checked_duration_since(at)).map(|age| age.as_secs()),
            "max_points": MAX_TRAIL_POINTS,
            "truncated": self.trail.truncated,
            "cleaned_coverage": false,
        })
    }
    fn pose_diagnostics(&self, now: Instant) -> Value {
        let counters = self
            .pose_diagnostics
            .as_ref()
            .filter(|(generation, _)| *generation == self.state.generation)
            .map(|(_, diagnostics)| diagnostics.snapshot());
        let render_reason = if !self.state.connected {
            "disconnected"
        } else if !self.current_map() {
            "map_not_current"
        } else if self.pose.is_none() {
            "no_retained_pose"
        } else if self.fresh_pose(now).is_none() {
            "pose_expired"
        } else if let Some((x, y)) = self
            .pose
            .as_ref()
            .and_then(|pose| Some((pose.get("x")?.as_f64()?, pose.get("y")?.as_f64()?)))
            .filter(|(x, y)| x.is_finite() && y.is_finite())
        {
            if self
                .map
                .as_ref()
                .is_some_and(|map| map.world_pixel(x, y).is_some())
            {
                "eligible"
            } else {
                "out_of_bounds"
            }
        } else {
            "invalid_coordinates"
        };
        json!({
            "connection_current": self.state.connected && counters.is_some(),
            "counters": counters,
            "retained_pose": self.pose.is_some(),
            "fresh_pose": self.fresh_pose(now).is_some(),
            "reported_pose_render_eligible": render_reason == "eligible",
            "render_reason": render_reason,
        })
    }
    fn current_map(&self) -> bool {
        self.state.connected
            && self.map_generation == Some(self.state.generation)
            && self.map.is_some()
    }
    fn robot_marker(&self, now: Instant) -> Option<(crate::maps::Point, &'static str, u64)> {
        if !self.current_map() {
            return None;
        }
        let map = self.map.as_ref()?;
        if let Some(marker) = self.reported_marker(now) {
            return Some(marker);
        }
        if self.state.fresh(now)
            && self.state.updated.is_some_and(|updated| {
                now.checked_duration_since(updated)
                    .is_some_and(|age| age < POSE_DISPLAY_TTL)
            })
            && self
                .state
                .status
                .as_ref()
                .is_some_and(|status| status.get("station_contact") == Some(&json!(true)))
        {
            let point = map.dock.as_ref()?.clone();
            map.world_pixel(point.x, point.y)?;
            return Some((
                point,
                "dock_contact",
                now.saturating_duration_since(self.state.updated?).as_secs(),
            ));
        }
        None
    }
    fn reported_marker(&self, now: Instant) -> Option<(crate::maps::Point, &'static str, u64)> {
        if !self.current_map() {
            return None;
        }
        let pose = self.fresh_pose(now)?;
        let point = crate::maps::Point {
            x: pose.get("x")?.as_f64()?,
            y: pose.get("y")?.as_f64()?,
        };
        self.map.as_ref()?.world_pixel(point.x, point.y)?;
        let age = now
            .checked_duration_since(self.pose_received_at?)?
            .as_secs();
        Some((point, "reported_pose", age))
    }
    fn mark_image_rendered(&mut self, now: Instant) {
        self.rendered_marker = self.marker_key(now);
        self.image_rendered_at = Some(now);
        self.image_dirty = false;
    }
    fn render_image(&mut self, now: Instant) -> Option<Vec<u8>> {
        let image = self.map_png(now).ok();
        self.mark_image_rendered(now);
        image
    }
    fn marker_key(&self, now: Instant) -> Option<(u64, u64, &'static str)> {
        self.robot_marker(now)
            .map(|(point, source, _)| (point.x.to_bits(), point.y.to_bits(), source))
    }
    fn map_tracking(&self, now: Instant) -> Value {
        let robot = match self.robot_marker(now) {
            Some((point, source, age)) => {
                json!({"available":true,"source":source,"x":point.x,"y":point.y,"age_seconds":age})
            }
            None => json!({"available":false,"source":"unavailable"}),
        };
        let dock = self.map.as_ref().and_then(|map| {
            map.dock
                .as_ref()
                .filter(|point| map.world_pixel(point.x, point.y).is_some())
        });
        let dock = match dock {
            Some(point) => {
                json!({"available":true,"source":"map_base_station","x":point.x,"y":point.y})
            }
            None => json!({"available":false,"source":"unavailable"}),
        };
        json!({"map_current":self.current_map(),"robot":robot,"dock":dock,"trail":self.trail_tracking(now),
            "coverage":self.cleaning_layer_tracking(now),
            "display":{"transform":self.map_display.transform,"room_labels":self.map_display.room_labels,
                "show_trail":self.map_display.show_trail,
                "coordinate_space":"display_pixels",
                "labels":self.map.as_ref().map(|map|map.display_labels(self.map_display.transform)).unwrap_or_default(),
                "rooms":self.map.as_ref().map(|map|map.display_room_outlines(self.map_display.transform)).unwrap_or_default()}})
    }
    fn clear_pose(&mut self) {
        self.pose = None;
        self.pose_received_at = None;
        self.trail.break_segment();
    }
    fn fresh_pose(&self, now: Instant) -> Option<&Value> {
        if self.state.connected
            && self.pose_received_at.is_some_and(|received| {
                now.checked_duration_since(received)
                    .is_some_and(|age| age < POSE_DISPLAY_TTL)
            })
        {
            self.pose.as_ref()
        } else {
            None
        }
    }
    fn map_png(&self, now: Instant) -> Result<Vec<u8>, String> {
        let marker = self
            .robot_marker(now)
            .map(|(point, _, _)| json!({"x":point.x,"y":point.y}));
        self.map
            .as_ref()
            .ok_or("map_unavailable")?
            .png_display_with_cleaning(
                marker.as_ref(),
                self.trail.points.iter().filter(|_| self.current_map()),
                self.map_display,
                self.clean_layer.as_ref().filter(|_| self.current_map()),
            )
    }
    fn motion_ready(&self) -> bool {
        self.intent_store
            .as_ref()
            .is_some_and(crate::intent::Store::healthy)
    }
    fn clear_intent(&mut self) -> Result<(), String> {
        if let Some(store) = &mut self.intent_store {
            store.set_pending(None)?;
        }
        self.pending_intent = None;
        Ok(())
    }
}
