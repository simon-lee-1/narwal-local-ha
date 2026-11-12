//! Persistent robot session. The caller owns network admission and operation policy.
use base64::{Engine, engine::general_purpose::STANDARD};
use futures_util::{SinkExt, StreamExt};
use ring::rand::{SecureRandom, SystemRandom};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    io,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
    time::Duration,
};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::{mpsc, oneshot};
use tokio::time::{Instant, sleep_until, timeout};
use tokio_tungstenite::{
    WebSocketStream, accept_hdr_async_with_config,
    tungstenite::{
        handshake::server::{ErrorResponse, Request as UpgradeRequest, Response},
        protocol::{Message, WebSocketConfig},
    },
};

const INFO: &str = "/pita/info/all";
const MAP: &str = "/pita/plan_map/get";
const POSE: &str = "/pita/map";
const APP_ONLINE: &str = "app_net_flag";
const COMMAND: &str = "/pita_clean_system/command";
const MAP_SET: &str = "/pita/plan_map/set";
const MAP_SET_PARAMS_CAP: usize = 2 * 1024 * 1024;
const MESSAGE_CAP: usize = crate::codec::MESSAGE_CAP;
const WRITE_DEADLINE: Duration = Duration::from_secs(5);
const REPLY_DEADLINE: Duration = Duration::from_secs(15);
const COMMAND_ACK_DEADLINE: Duration = Duration::from_secs(5);
const IDLE_DEADLINE: Duration = Duration::from_secs(90);
const STATUS_FIELDS: [&str; 28] = [
    "battery_per",
    "charge_status",
    "clean_bucket",
    "clean_mode",
    "clean_order",
    "cleaned_area",
    "cliff_shield_mode",
    "cur_task_type",
    "dust_box",
    "error_code",
    "fan_status",
    "flume_status",
    "humidity",
    "language",
    "main_task",
    "mode_strength",
    "ovf_status",
    "robot_mode",
    "robot_status",
    "slop_bucket",
    "station_contact",
    "station_sub_mode",
    "task_name",
    "task_queue",
    "task_st_time",
    "volume",
    "work_status",
    "working_progress",
];

#[derive(Debug)]
pub enum Event {
    Connected { generation: u64 },
    Disconnected { generation: u64 },
    Status { generation: u64, result: Value },
    Map { generation: u64, result: Value },
    Pose { generation: u64, result: Value },
}

#[derive(Debug)]
pub enum Operation {
    Status,
    Map,
    /// Fixed captured app-online frame; write completion is not a robot ACK.
    RequestPositions,
    /// Legacy entry point retained only to return an explicit refusal.
    Command {
        params: Value,
    },
    CommandStatus,
    /// Matched status and local revision metadata, without a command permit.
    ObservedStatus,
    GuardedCommand {
        observation: CommandObservation,
        params: Option<Value>,
    },
    /// A single map replacement, bound to a fresh, matched idle-status read.
    GuardedMapSet {
        observation: CommandObservation,
        params: Value,
    },
}

/// Internal metadata issued for a matched read; never accepted from robot JSON.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CommandObservation {
    pub(crate) generation: u64,
    pub(crate) revision: u64,
    pub(crate) state: crate::control::StatePredicate,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) mop_context: Option<crate::control::MopContext>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) clean_order: Option<Vec<u64>>,
}
impl CommandObservation {
    pub(crate) fn follows(&self, before: &Self) -> bool {
        self.generation > before.generation
            || (self.generation == before.generation && self.revision > before.revision)
    }

    pub(crate) fn map_edit_idle(&self) -> bool {
        self.state
            == (crate::control::StatePredicate {
                robot_status: 100,
                work_status: 0,
                station_contact: true,
                error_code: 0,
            })
            && self
                .mop_context
                .as_ref()
                .is_some_and(|context| context.idle_mop() || context.idle_vacuum())
            && self.clean_order.as_ref().is_some_and(Vec::is_empty)
    }
}

/// Bounded, identity-free counters for one connection. These do not participate
/// in message admission or command policy.
#[derive(Default)]
pub struct Diagnostics(Mutex<DiagnosticCounts>);
#[derive(Default, Serialize)]
struct DiagnosticCounts {
    decoded_objects: u64,
    pose_envelopes: u64,
    pose_accepted: u64,
    pose_rejected: PoseRejections,
}
#[derive(Default, Serialize)]
struct PoseRejections {
    identity: u64,
    request_id: u64,
    pending_id_collision: u64,
    code: u64,
    result: u64,
    coordinates: u64,
    delivery_failed: u64,
}
enum PoseRejection {
    Identity,
    RequestId,
    PendingIdCollision,
    Code,
    Result,
    Coordinates,
    DeliveryFailed,
}
impl Diagnostics {
    pub fn snapshot(&self) -> Value {
        json!(*self.0.lock().unwrap_or_else(|error| error.into_inner()))
    }
    fn received(&self, pose: bool) {
        let mut counts = self.0.lock().unwrap_or_else(|error| error.into_inner());
        counts.decoded_objects = counts.decoded_objects.saturating_add(1);
        if pose {
            counts.pose_envelopes = counts.pose_envelopes.saturating_add(1);
        }
    }
    fn rejected(&self, reason: PoseRejection) {
        let mut counts = self.0.lock().unwrap_or_else(|error| error.into_inner());
        let count = match reason {
            PoseRejection::Identity => &mut counts.pose_rejected.identity,
            PoseRejection::RequestId => &mut counts.pose_rejected.request_id,
            PoseRejection::PendingIdCollision => &mut counts.pose_rejected.pending_id_collision,
            PoseRejection::Code => &mut counts.pose_rejected.code,
            PoseRejection::Result => &mut counts.pose_rejected.result,
            PoseRejection::Coordinates => &mut counts.pose_rejected.coordinates,
            PoseRejection::DeliveryFailed => &mut counts.pose_rejected.delivery_failed,
        };
        *count = count.saturating_add(1);
    }
    fn accepted(&self) {
        let mut counts = self.0.lock().unwrap_or_else(|error| error.into_inner());
        counts.pose_accepted = counts.pose_accepted.saturating_add(1);
    }
}

#[derive(Default)]
struct SessionStatus {
    latest: Option<(CommandObservation, Instant)>,
    revision: u64,
    consumed_revision: u64,
    issued_revision: Option<u64>,
    diagnostics: Arc<Diagnostics>,
}
impl SessionStatus {
    fn revoke(&mut self) {
        self.issued_revision = None;
    }
    fn observe(&mut self, generation: u64, value: &Value) -> Result<CommandObservation, String> {
        self.issued_revision = None;
        let state =
            crate::control::StatePredicate::from_status(value).ok_or("command_state_missing")?;
        self.revision = self
            .revision
            .checked_add(1)
            .ok_or("status_revision_exhausted")?;
        let observation = CommandObservation {
            generation,
            revision: self.revision,
            state,
            mop_context: crate::control::MopContext::from_status(value),
            clean_order: value
                .get("clean_order")
                .and_then(Value::as_array)
                .and_then(|order| {
                    (order.len() <= 64)
                        .then(|| {
                            order
                                .iter()
                                .map(|id| id.as_u64().filter(|id| *id <= 255))
                                .collect::<Option<Vec<_>>>()
                        })
                        .flatten()
                }),
        };
        self.latest = Some((observation.clone(), Instant::now()));
        Ok(observation)
    }
    fn consume(&mut self, observation: &CommandObservation, generation: u64) -> Result<(), String> {
        let (latest, updated) = self.latest.as_ref().ok_or("command_state_missing")?;
        if observation.generation != generation
            || latest.generation != generation
            || observation.revision != latest.revision
            || observation.state != latest.state
            || observation.mop_context != latest.mop_context
            || observation.clean_order != latest.clean_order
            || observation.revision <= self.consumed_revision
            || self.issued_revision != Some(observation.revision)
            || updated.elapsed() > WRITE_DEADLINE
        {
            return Err("command_observation_superseded_or_expired".into());
        }
        self.consumed_revision = observation.revision;
        self.issued_revision = None;
        Ok(())
    }
}

pub struct Request {
    pub operation: Operation,
    pub reply: oneshot::Sender<Result<Value, String>>,
}

struct Pending {
    request_id: String,
    service: &'static str,
    reply: oneshot::Sender<Result<Value, String>>,
    deadline: Instant,
    command_status: bool,
    observed_status: bool,
    transport_written: bool,
}

fn completed_command_receipt(acknowledged: bool) -> Value {
    json!({"transport_attempted":true,"transport_written":true,"acknowledged":acknowledged})
}

fn guarded_write(service: &str) -> bool {
    matches!(service, COMMAND | MAP_SET)
}

// Tungstenite bounds WebSocket messages but not the total HTTP header bytes.
// This adapter bounds only the initial header; subsequent traffic is streamed.
struct HeaderBounded<S> {
    inner: S,
    bytes: usize,
    tail: [u8; 4],
    complete: bool,
}
impl<S: AsyncRead + Unpin> AsyncRead for HeaderBounded<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        out: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if this.complete {
            return Pin::new(&mut this.inner).poll_read(cx, out);
        }
        if out.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        let mut buffer = [0u8; 1024];
        let capacity = out.remaining().min(buffer.len());
        let mut incoming = ReadBuf::new(&mut buffer[..capacity]);
        match Pin::new(&mut this.inner).poll_read(cx, &mut incoming) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Err(error)) => Poll::Ready(Err(error)),
            Poll::Ready(Ok(())) => {
                for byte in incoming.filled() {
                    this.bytes += 1;
                    this.tail.rotate_left(1);
                    this.tail[3] = *byte;
                    if this.bytes > 8192 {
                        return Poll::Ready(Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "HTTP header limit",
                        )));
                    }
                    if this.tail == *b"\r\n\r\n" {
                        this.complete = true;
                        break;
                    }
                }
                out.put_slice(incoming.filled());
                Poll::Ready(Ok(()))
            }
        }
    }
}
impl<S: AsyncWrite + Unpin> AsyncWrite for HeaderBounded<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write(cx, bytes)
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

// Tungstenite's Callback trait requires this concrete HTTP error response.
#[allow(clippy::result_large_err)]
fn upgrade(request: &UpgradeRequest, response: Response) -> Result<Response, ErrorResponse> {
    let headers = request.headers();
    let valid_nonce = headers
        .get("sec-websocket-key")
        .and_then(|s| s.to_str().ok())
        .and_then(|s| STANDARD.decode(s).ok().map(|decoded| (s, decoded)))
        .is_some_and(|(s, decoded)| decoded.len() == 16 && STANDARD.encode(decoded) == s);
    let singles = [
        "host",
        "connection",
        "upgrade",
        "sec-websocket-key",
        "sec-websocket-version",
    ]
    .iter()
    .all(|name| headers.get_all(*name).iter().count() == 1);
    if request.uri().path() != "/robot_websocket"
        || request.uri().query().is_some()
        || !singles
        || !valid_nonce
        || headers.contains_key("sec-websocket-extensions")
        || headers.contains_key("sec-websocket-protocol")
        || headers.contains_key("transfer-encoding")
        || headers.get("content-length").is_some_and(|v| v != "0")
    {
        let mut rejected = ErrorResponse::new(Some("WebSocket request refused".into()));
        *rejected.status_mut() = tokio_tungstenite::tungstenite::http::StatusCode::BAD_REQUEST;
        return Err(rejected);
    }
    Ok(response)
}

fn identity(value: &Value) -> Option<&str> {
    value
        .get("machine_id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty() && s.len() <= 256 && !s.chars().any(char::is_control))
}

fn heartbeat(raw: &[u8]) -> bool {
    // Evidence from V16. This application text is not a WebSocket Ping frame.
    // No application heartbeat or unknown response is generated.
    raw.len() == 16
        && raw.is_ascii()
        && raw.windows(4).any(|s| s == b"ping")
        && raw.iter().filter(|b| b.is_ascii_lowercase()).count() == 5
        && raw.iter().filter(|b| b.is_ascii_digit()).count() == 7
        && raw.iter().filter(|b| b.is_ascii_punctuation()).count() == 4
}

fn status_result(result: &Value) -> Option<Value> {
    let r = result.as_object()?;
    if !r
        .get("battery_per")
        .and_then(Value::as_u64)
        .is_some_and(|n| n <= 100)
        || !["error_code", "robot_status", "work_status"]
            .iter()
            .all(|key| {
                r.get(*key)
                    .and_then(Value::as_u64)
                    .is_some_and(|n| n <= 65535)
            })
        || !["charge_status", "station_contact"]
            .iter()
            .all(|key| r.get(*key).and_then(Value::as_bool).is_some())
    {
        return None;
    }
    // The six core types are proven. Other known fields retain their reported
    // JSON types; the old inspector observed kinds but did not validate them.
    Some(Value::Object(
        STATUS_FIELDS
            .iter()
            .filter_map(|key| r.get(*key).map(|value| ((*key).into(), value.clone())))
            .collect(),
    ))
}

fn fresh_id() -> Result<String, String> {
    let mut bytes = [0u8; 16];
    SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| "request_randomness_failed")?;
    bytes[6] = (bytes[6] & 15) | 64;
    bytes[8] = (bytes[8] & 63) | 128;
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    Ok(format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    ))
}

fn operation(operation: Operation) -> Result<(&'static str, Value), String> {
    match operation {
        Operation::Status | Operation::CommandStatus | Operation::ObservedStatus => {
            Ok((INFO, json!("{}")))
        }
        Operation::Map => Ok((
            MAP,
            json!({"operations":["room_attr","clean_order","map_data"]}),
        )),
        Operation::RequestPositions => Err("position_request_requires_one_way_dispatch".into()),
        Operation::GuardedCommand {
            params: Some(params),
            ..
        } => {
            if !params.is_object()
                || serde_json::to_vec(&params)
                    .map_err(|_| "command_encoding_failed")?
                    .len()
                    > 4096
            {
                return Err("command_parameters_refused".into());
            }
            Ok((COMMAND, params))
        }
        Operation::GuardedMapSet {
            observation,
            params,
        } => {
            if !observation.map_edit_idle() {
                return Err("map_set_requires_idle_docked_robot".into());
            }
            // Reconstruct the captured setter fields rather than accepting an
            // arbitrary route, operation token, or getter-only field.
            let validated = crate::room_editor::validated_set_params(&params)?;
            if validated != params
                || serde_json::to_vec(&params)
                    .map_err(|_| "map_set_encoding_failed")?
                    .len()
                    > MAP_SET_PARAMS_CAP
            {
                return Err("map_set_parameters_refused".into());
            }
            Ok((MAP_SET, params))
        }
        Operation::Command { .. } | Operation::GuardedCommand { params: None, .. } => {
            Err("unguarded_motion_refused".into())
        }
    }
}

fn before_send_refused(reason: &str) -> Value {
    json!({
        "outcome":"before_send_refused","reason":reason,
        "transport_attempted":false,"transport_written":false,"acknowledged":false
    })
}

async fn publish(events: &mpsc::Sender<Event>, event: Event) -> Result<(), String> {
    timeout(Duration::from_secs(2), events.send(event))
        .await
        .map_err(|_| "event_consumer_timeout")?
        .map_err(|_| "event_consumer_closed".into())
}

async fn application<S>(socket: &mut WebSocketStream<S>) -> Result<Option<Value>, String>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    match socket.next().await {
        Some(Ok(Message::Text(text))) => {
            if heartbeat(text.as_bytes()) {
                return Ok(None);
            }
            crate::codec::decode_object(text.as_bytes())
                .map(Some)
                .ok_or_else(|| "application_format_refused".into())
        }
        Some(Ok(Message::Ping(_))) => {
            // Tungstenite queues the standard control Pong automatically.
            // This does not echo the robot's opaque application text.
            timeout(WRITE_DEADLINE, socket.flush())
                .await
                .map_err(|_| "control_write_timeout")?
                .map_err(|_| "control_write_failed")?;
            Ok(None)
        }
        Some(Ok(Message::Pong(_))) => Ok(None),
        Some(Ok(Message::Close(_))) => Err("peer_closed_normally".into()),
        Some(Ok(_)) => Err("websocket_message_refused".into()),
        Some(Err(_)) => Err("websocket_read_failed".into()),
        None => Err("peer_disconnected".into()),
    }
}

async fn deliver(
    object: Value,
    machine: &str,
    generation: u64,
    pending: &mut Option<Pending>,
    events: &mpsc::Sender<Event>,
    status: &mut SessionStatus,
) -> Result<(), String> {
    let pose = object.get("service").and_then(Value::as_str) == Some(POSE);
    status.diagnostics.received(pose);
    if identity(&object) != Some(machine) {
        if pose {
            status.diagnostics.rejected(PoseRejection::Identity);
        }
        return Err("robot_identity_changed".into());
    }
    // Incoming traffic can win select! after the deadline. It must not make
    // an expired command acknowledgement eligible or extend its grace.
    if pending.as_ref().is_some_and(|waiting| {
        guarded_write(waiting.service)
            && waiting.transport_written
            && Instant::now() >= waiting.deadline
    }) && let Some(expired) = pending.take()
    {
        let _ = expired.reply.send(Ok(completed_command_receipt(false)));
    }
    let service = object.get("service").and_then(Value::as_str);
    let id = object.get("request_id").and_then(Value::as_str);
    if let Some(waiting) = pending.as_ref()
        && id == Some(waiting.request_id.as_str())
        && service == Some(waiting.service)
    {
        let waiting = pending.take().expect("pending response");
        if guarded_write(waiting.service) {
            let acknowledged = object.get("code").and_then(Value::as_i64) == Some(0)
                && object.get("result").is_some_and(Value::is_object);
            let result = if waiting.transport_written {
                Ok(completed_command_receipt(acknowledged))
            } else {
                Err("command_write_completion_unconfirmed".into())
            };
            let _ = waiting.reply.send(result);
            return Ok(());
        }
        if waiting.service == INFO {
            // Failed or incomplete current status must also revoke old permits.
            status.revoke();
        }
        let result = if object.get("code").and_then(Value::as_i64) != Some(0) {
            Err("robot_returned_error".into())
        } else {
            object
                .get("result")
                .filter(|r| r.is_object())
                .and_then(|result| {
                    if waiting.service == INFO {
                        status_result(result)
                    } else {
                        Some(result.clone())
                    }
                })
                .ok_or_else(|| "robot_result_refused".into())
        };
        let observation = if waiting.service == INFO {
            match &result {
                Ok(value) => {
                    let observation = status.observe(generation, value)?;
                    if waiting.command_status {
                        status.issued_revision = Some(observation.revision);
                    }
                    Some(observation)
                }
                Err(_) => None,
            }
        } else {
            None
        };
        if let Ok(value) = &result {
            let event = match waiting.service {
                INFO => Some(Event::Status {
                    generation,
                    result: value.clone(),
                }),
                MAP => Some(Event::Map {
                    generation,
                    result: value.clone(),
                }),
                _ => None,
            };
            if let Some(event) = event
                && let Err(error) = publish(events, event).await
            {
                let _ = waiting.reply.send(Err(error.clone()));
                return Err(error);
            }
        }
        let result = if waiting.observed_status {
            result.and_then(|value| {
                Ok(json!({
                    "status": value,
                    "observation": observation.ok_or("command_state_missing")?
                }))
            })
        } else if waiting.command_status {
            result.and_then(|_| {
                serde_json::to_value(observation.ok_or("command_state_missing")?)
                    .map_err(|_| "command_observation_encoding_failed".into())
            })
        } else {
            result
        };
        let _ = waiting.reply.send(result);
        return Ok(());
    }
    // J1 pose events carry their own IDs without corresponding requests.
    // They must not consume or collide with a pending response.
    if service == Some(POSE)
        && id.is_some_and(|id| {
            !id.is_empty() && id.len() <= 256 && !id.chars().any(char::is_control)
        })
        && pending
            .as_ref()
            .is_none_or(|waiting| id != Some(waiting.request_id.as_str()))
        && object.get("code").and_then(Value::as_i64) == Some(0)
        && let Some(result) = object.get("result").filter(|r| r.is_object())
        && ["x", "y"].iter().all(|field| {
            result
                .get(*field)
                .and_then(Value::as_f64)
                .is_some_and(f64::is_finite)
        })
    {
        let delivered = publish(
            events,
            Event::Pose {
                generation,
                result: result.clone(),
            },
        )
        .await;
        match delivered {
            Ok(()) => status.diagnostics.accepted(),
            Err(error) => {
                status.diagnostics.rejected(PoseRejection::DeliveryFailed);
                return Err(error);
            }
        }
        return Ok(());
    }
    // Other responses to previous/unknown requests cannot become telemetry.
    if object.get("request_id").is_some_and(|id| !id.is_null()) {
        if pose {
            let reason = if !id.is_some_and(|id| {
                !id.is_empty() && id.len() <= 256 && !id.chars().any(char::is_control)
            }) {
                PoseRejection::RequestId
            } else if pending
                .as_ref()
                .is_some_and(|waiting| id == Some(waiting.request_id.as_str()))
            {
                PoseRejection::PendingIdCollision
            } else if object.get("code").and_then(Value::as_i64) != Some(0) {
                PoseRejection::Code
            } else if !object.get("result").is_some_and(Value::is_object) {
                PoseRejection::Result
            } else {
                PoseRejection::Coordinates
            };
            status.diagnostics.rejected(reason);
        }
        return Ok(());
    }
    if service == Some(INFO) {
        status.revoke();
    }
    if object
        .get("code")
        .is_some_and(|code| code.as_i64() != Some(0))
    {
        if pose {
            status.diagnostics.rejected(PoseRejection::Code);
        }
        return Ok(());
    }
    let Some(result) = object.get("result").filter(|r| r.is_object()) else {
        if pose {
            status.diagnostics.rejected(PoseRejection::Result);
        }
        return Ok(());
    };
    if pose
        && !["x", "y"].iter().all(|field| {
            result
                .get(*field)
                .and_then(Value::as_f64)
                .is_some_and(f64::is_finite)
        })
    {
        status.diagnostics.rejected(PoseRejection::Coordinates);
        return Ok(());
    }
    let event = match service {
        Some(INFO) => status_result(result).map(|result| Event::Status { generation, result }),
        Some(POSE) => Some(Event::Pose {
            generation,
            result: result.clone(),
        }),
        Some(MAP) => Some(Event::Map {
            generation,
            result: result.clone(),
        }),
        _ => None,
    };
    if let Some(event) = event {
        if let Event::Status { result, .. } = &event {
            status.observe(generation, result)?;
        }
        if let Err(error) = publish(events, event).await {
            if pose {
                status.diagnostics.rejected(PoseRejection::DeliveryFailed);
            }
            return Err(error);
        }
        if pose {
            status.diagnostics.accepted();
        }
    }
    Ok(())
}

async fn run<S>(
    stream: S,
    generation: u64,
    requests: &mut mpsc::Receiver<Request>,
    events: &mpsc::Sender<Event>,
    pending: &mut Option<Pending>,
    connected: &mut bool,
    diagnostics: Arc<Diagnostics>,
) -> Result<(), String>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let config = WebSocketConfig::default()
        .read_buffer_size(16 * 1024)
        .write_buffer_size(0)
        .max_write_buffer_size(MESSAGE_CAP + 1024)
        .max_message_size(Some(MESSAGE_CAP))
        .max_frame_size(Some(MESSAGE_CAP))
        .accept_unmasked_frames(false);
    let bounded = HeaderBounded {
        inner: stream,
        bytes: 0,
        tail: [0; 4],
        complete: false,
    };
    let mut socket = timeout(
        WRITE_DEADLINE,
        accept_hdr_async_with_config(bounded, upgrade, Some(config)),
    )
    .await
    .map_err(|_| "http_upgrade_timeout")?
    .map_err(|_| "http_upgrade_refused")?;
    let startup = Instant::now() + Duration::from_secs(35);
    let announcement = loop {
        if let Some(object) = tokio::time::timeout_at(startup, application(&mut socket))
            .await
            .map_err(|_| "robot_announcement_timeout")??
        {
            break object;
        }
    };
    let machine = identity(&announcement)
        .ok_or("robot_announcement_refused")?
        .to_owned();
    publish(events, Event::Connected { generation }).await?;
    *connected = true;
    let mut status = SessionStatus {
        diagnostics,
        ..SessionStatus::default()
    };
    deliver(
        announcement,
        &machine,
        generation,
        pending,
        events,
        &mut status,
    )
    .await?;
    let mut requests_open = true;
    let mut idle = Instant::now() + IDLE_DEADLINE;
    loop {
        let reply_deadline = pending.as_ref().map(|p| p.deadline).unwrap_or(idle);
        tokio::select! {
            incoming = application(&mut socket) => {
                match incoming {
                    Ok(object) => {
                        idle = Instant::now() + IDLE_DEADLINE;
                        if let Some(object) = object {
                            deliver(object, &machine, generation, pending, events, &mut status).await?;
                        }
                    }
                    Err(error) if error == "peer_closed_normally" => return Ok(()),
                    Err(error) => return Err(error),
                }
            }
            request = requests.recv(), if pending.is_none() && requests_open => {
                let Some(request) = request else { requests_open = false; continue; };
                if request.reply.is_closed() { continue; }
                if matches!(request.operation, Operation::RequestPositions) {
                    let object = json!({
                        "service":APP_ONLINE,"machine_id":machine,
                        "device_id":"","request_id":"","result_type":"",
                        "params":{"state":true}
                    });
                    let Some(encoded) = crate::codec::encode_object(&object)
                        .and_then(|bytes| String::from_utf8(bytes).ok()) else
                    {
                        let _ = request.reply.send(Err("position_request_encoding_failed".into()));
                        continue;
                    };
                    // The captured request has no correlation ID. Do not create
                    // a Pending reply, retry it, or claim a robot acknowledgement.
                    let result = timeout(WRITE_DEADLINE, socket.send(Message::Text(encoded.into())))
                        .await
                        .map_err(|_| "position_request_write_timeout_result_unknown")
                        .and_then(|result| result.map_err(|_| "position_request_write_failed_result_unknown"));
                    match result {
                        Ok(()) => {
                            let _ = request.reply.send(Ok(json!({
                                "operation":"request_positions",
                                "transport_attempted":true,"transport_written":true,
                                "acknowledged":false,"position_reporting_confirmed":false
                            })));
                        }
                        Err(error) => {
                            let _ = request.reply.send(Err(error.into()));
                            return Err(error.into());
                        }
                    }
                    continue;
                }
                if let Operation::GuardedCommand { observation, params } = &request.operation {
                    if let Err(reason) = status.consume(observation, generation) {
                        let _ = request.reply.send(Ok(before_send_refused(&reason)));
                        continue;
                    }
                    if params.is_none() {
                        let _ = request.reply.send(Ok(json!({
                            "outcome":"no_action_needed","no_op":true,
                            "transport_attempted":false,"transport_written":false,
                            "acknowledged":false
                        })));
                        continue;
                    }
                }
                if let Operation::GuardedMapSet { observation, .. } = &request.operation
                    && let Err(reason) = status.consume(observation, generation)
                {
                    let _ = request.reply.send(Ok(before_send_refused(&reason)));
                    continue;
                }
                let command_status = matches!(request.operation, Operation::CommandStatus);
                let observed_status = matches!(request.operation, Operation::ObservedStatus);
                let guarded_motion = matches!(request.operation,
                    Operation::GuardedCommand { .. } | Operation::GuardedMapSet { .. });
                let refused = |error: String| {
                    if guarded_motion { Ok(before_send_refused(&error)) } else { Err(error) }
                };
                let (service, params) = match operation(request.operation) {
                    Ok(operation) => operation,
                    Err(error) => { let _ = request.reply.send(refused(error)); continue; }
                };
                let request_id = match fresh_id() {
                    Ok(id) => id,
                    Err(error) => { let _ = request.reply.send(refused(error.clone())); return Err(error); }
                };
                let object = json!({"machine_id":machine,"request_id":request_id,
                    "service":service,"params":params});
                let Some(encoded) = crate::codec::encode_object(&object)
                    .and_then(|bytes| String::from_utf8(bytes).ok()) else
                {
                    let _ = request.reply.send(refused("request_encoding_failed".into()));
                    continue;
                };
                // Consume this intent before the first write. On ambiguous failure
                // the caller receives an error; this frame is never replayed.
                *pending = Some(Pending { request_id, service, reply: request.reply,
                    deadline: Instant::now() + REPLY_DEADLINE, command_status, observed_status,
                    transport_written: false });
                timeout(WRITE_DEADLINE, socket.send(Message::Text(encoded.into()))).await
                    .map_err(|_| "request_write_timeout")?
                    .map_err(|_| "request_write_failed")?;
                if let Some(written) = pending.as_mut() {
                    written.transport_written = true;
                    if guarded_write(service) {
                        written.deadline = Instant::now() + COMMAND_ACK_DEADLINE;
                    }
                }
            }
            _ = sleep_until(reply_deadline), if pending.is_some() => {
                if let Some(waiting) = pending.take() {
                    if guarded_write(waiting.service) && waiting.transport_written {
                        // A missing optional ACK does not end the connection.
                        // The Engine can now request fresh, separate feedback.
                        let _ = waiting.reply.send(Ok(completed_command_receipt(false)));
                        continue;
                    }
                    let _ = waiting.reply.send(Err("reply_timeout_result_unknown".into()));
                }
                // A missing read response ends this session; the old query is
                // never replayed or accepted as fresh on a later connection.
                return Err("reply_timeout_result_unknown".into());
            }
            _ = sleep_until(idle) => return Err("robot_idle_timeout".into()),
        }
    }
}

pub async fn serve<S>(
    stream: S,
    generation: u64,
    requests: mpsc::Receiver<Request>,
    events: mpsc::Sender<Event>,
) -> Result<(), String>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    serve_observed(
        stream,
        generation,
        requests,
        events,
        Arc::new(Diagnostics::default()),
    )
    .await
}

pub async fn serve_observed<S>(
    stream: S,
    generation: u64,
    mut requests: mpsc::Receiver<Request>,
    events: mpsc::Sender<Event>,
    diagnostics: Arc<Diagnostics>,
) -> Result<(), String>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let mut pending = None;
    let mut connected = false;
    let result = run(
        stream,
        generation,
        &mut requests,
        &events,
        &mut pending,
        &mut connected,
        diagnostics,
    )
    .await;
    if let Some(waiting) = pending.take() {
        let reply = if guarded_write(waiting.service) && waiting.transport_written {
            Ok(completed_command_receipt(false))
        } else {
            Err("session_disconnected_result_unknown".into())
        };
        let _ = waiting.reply.send(reply);
    }
    requests.close();
    while let Ok(request) = requests.try_recv() {
        let _ = request
            .reply
            .send(Err("session_disconnected_before_send".into()));
    }
    if connected {
        // State consumers receive a final invalidation even after protocol errors.
        let _ = publish(&events, Event::Disconnected { generation }).await;
    }
    result
}
