//! Loopback dashboard for the same command/state owner used by MQTT.
use crate::engine::Engine;
use axum::{
    Json, Router,
    body::{Body, to_bytes},
    extract::State,
    http::{HeaderMap, HeaderValue, Method, Request, StatusCode},
    middleware::{self, Next},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
};
use ring::rand::{SecureRandom, SystemRandom};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{net::SocketAddr, sync::Arc, time::Duration};
use tokio::{net::TcpListener, time::timeout};

struct App {
    engine: Engine,
    host: String,
    csrf: String,
}
fn local_address(address: SocketAddr) -> Result<(), String> {
    if !address.ip().is_loopback() {
        return Err("dashboard_requires_loopback".into());
    }
    Ok(())
}
fn one_header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    let mut values = headers.get_all(name).iter();
    let first = values.next()?.to_str().ok()?;
    values.next().is_none().then_some(first)
}
fn private_response(mut response: Response, token: &str) -> Response {
    let headers = response.headers_mut();
    headers.insert("cache-control", HeaderValue::from_static("no-store"));
    headers.insert("referrer-policy", HeaderValue::from_static("no-referrer"));
    headers.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        "cross-origin-resource-policy",
        HeaderValue::from_static("same-origin"),
    );
    headers.insert("content-security-policy", HeaderValue::from_str(&format!(
        "default-src 'none'; script-src 'nonce-{token}'; style-src 'nonce-{token}'; img-src 'self'; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'none'"
    )).expect("generated nonce contains only hexadecimal"));
    response
}
async fn guard(State(app): State<Arc<App>>, request: Request<Body>, next: Next) -> Response {
    let headers = request.headers();
    let origin = format!("http://{}", app.host);
    let forbidden = || private_response(StatusCode::FORBIDDEN.into_response(), &app.csrf);
    if one_header(headers, "host") != Some(app.host.as_str()) {
        return forbidden();
    }
    if headers.contains_key("origin") && one_header(headers, "origin") != Some(origin.as_str()) {
        return forbidden();
    }
    if headers.contains_key("sec-fetch-site")
        && !matches!(
            one_header(headers, "sec-fetch-site"),
            Some("same-origin" | "none")
        )
    {
        return forbidden();
    }
    if request.method() == Method::POST {
        if one_header(headers, "origin") != Some(origin.as_str())
            || one_header(headers, "x-narwal-csrf") != Some(app.csrf.as_str())
        {
            return forbidden();
        }
        if one_header(headers, "content-type")
            .and_then(|s| s.split(';').next())
            .map(str::trim)
            != Some("application/json")
        {
            return private_response(
                StatusCode::UNSUPPORTED_MEDIA_TYPE.into_response(),
                &app.csrf,
            );
        }
        let (parts, body) = request.into_parts();
        let bytes = match timeout(Duration::from_secs(5), to_bytes(body, 4096)).await {
            Ok(Ok(bytes)) => bytes,
            Ok(Err(_)) => {
                return private_response(StatusCode::PAYLOAD_TOO_LARGE.into_response(), &app.csrf);
            }
            Err(_) => {
                return private_response(StatusCode::REQUEST_TIMEOUT.into_response(), &app.csrf);
            }
        };
        return private_response(
            next.run(Request::from_parts(parts, Body::from(bytes)))
                .await,
            &app.csrf,
        );
    }
    private_response(next.run(request).await, &app.csrf)
}
fn router(app: Arc<App>) -> Router {
    Router::new()
        .route("/", get(page))
        .route("/api/state", get(state))
        .route("/map.png", get(image))
        .route("/api/refresh", post(refresh))
        .route("/api/map/refresh", post(refresh_map))
        .route("/api/telemetry/request_positions", post(request_positions))
        .route("/api/command", post(command))
        .route("/api/room-editor", post(room_editor))
        .route("/api/room-editor/name", post(editor_name))
        .route("/api/room-editor/room", post(editor_room))
        .route("/room-edit-preview.png", get(editor_image))
        .layer(middleware::from_fn_with_state(app.clone(), guard))
        .with_state(app)
}
async fn page(State(app): State<Arc<App>>) -> Html<String> {
    Html(PAGE.replace("__TOKEN__", &app.csrf))
}
async fn state(State(app): State<Arc<App>>) -> Json<Value> {
    Json(app.engine.snapshot().await)
}
async fn image(State(app): State<Arc<App>>) -> Response {
    match app.engine.map_png().await {
        Ok(bytes) => ([(axum::http::header::CONTENT_TYPE, "image/png")], bytes).into_response(),
        Err(_) => (
            StatusCode::NOT_FOUND,
            Json(json!({"error":"map_unavailable"})),
        )
            .into_response(),
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Empty {}
fn operation_response(result: Result<Value, String>) -> Response {
    match result {
        Ok(value) => Json(value).into_response(),
        Err(error) => (StatusCode::CONFLICT, Json(json!({"error":error}))).into_response(),
    }
}
async fn refresh(State(app): State<Arc<App>>, Json(_): Json<Empty>) -> Response {
    operation_response(app.engine.refresh().await)
}
async fn refresh_map(State(app): State<Arc<App>>, Json(_): Json<Empty>) -> Response {
    operation_response(app.engine.refresh_map().await)
}
async fn request_positions(State(app): State<Arc<App>>, Json(input): Json<Value>) -> Response {
    if !input.as_object().is_some_and(serde_json::Map::is_empty) {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({"error":"empty_object_required"})),
        )
            .into_response();
    }
    operation_response(app.engine.request_positions().await)
}
async fn command(
    State(app): State<Arc<App>>,
    Json(input): Json<crate::control::CommandInput>,
) -> Response {
    operation_response(app.engine.command(input).await)
}
async fn room_editor(
    State(app): State<Arc<App>>,
    Json(input): Json<crate::engine::EditorInput>,
) -> Response {
    operation_response(app.engine.room_editor(input).await)
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EditorSetting {
    value: String,
}
async fn editor_name(State(app): State<Arc<App>>, Json(input): Json<EditorSetting>) -> Response {
    operation_response(
        app.engine
            .editor_name(input.value)
            .await
            .map(|_| json!({"saved":false})),
    )
}
async fn editor_room(State(app): State<Arc<App>>, Json(input): Json<EditorSetting>) -> Response {
    operation_response(
        app.engine
            .editor_room(input.value)
            .await
            .map(|_| json!({"saved":false})),
    )
}
async fn editor_image(State(app): State<Arc<App>>) -> Response {
    match app.engine.room_edit_preview().await {
        Ok(bytes) => ([(axum::http::header::CONTENT_TYPE, "image/png")], bytes).into_response(),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}
pub async fn run(engine: Engine, address: SocketAddr) -> Result<(), String> {
    local_address(address)?;
    let mut random = [0u8; 32];
    SystemRandom::new()
        .fill(&mut random)
        .map_err(|_| "dashboard_randomness_failed")?;
    let csrf = random.iter().map(|byte| format!("{byte:02x}")).collect();
    let listener = TcpListener::bind(address)
        .await
        .map_err(|_| "dashboard_bind_failed")?;
    let host = listener
        .local_addr()
        .map_err(|_| "dashboard_address_failed")?
        .to_string();
    let app = Arc::new(App { engine, host, csrf });
    axum::serve(listener, router(app))
        .await
        .map_err(|_| "dashboard_serve_failed".into())
}

const PAGE: &str = r##"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>Narwal J1 · Local</title>
<style nonce="__TOKEN__">
:root{font:16px/1.5 system-ui,-apple-system,BlinkMacSystemFont,"Segoe UI",sans-serif;color:#193c36;background:#f1f5f2;color-scheme:light}
*{box-sizing:border-box}body{margin:0}button,input{font:inherit}button{border:1px solid #c8d8ce;border-radius:10px;background:white;color:#193c36;padding:10px 17px;cursor:pointer;font-weight:600}button:hover:enabled{background:#e7f3ec}button:focus-visible,input:focus-visible{outline:3px solid #80b49e;outline-offset:3px}button:disabled{opacity:.42;cursor:default}button.primary{background:#176b52;color:white;border-color:#176b52}button.primary:hover:enabled{background:#125841}main{max-width:1180px;margin:auto;padding:35px 28px 44px}
.top{display:flex;align-items:center;justify-content:space-between;gap:24px;margin-bottom:27px}.eyebrow{font-size:12px;letter-spacing:2px;font-weight:750;color:#638174;margin-bottom:5px}h1{font-size:38px;line-height:1.1;letter-spacing:-1.4px;margin:0}h2{font-size:20px;margin:0}p{margin:0}.muted{color:#688074;font-size:14px}.badge{display:inline-flex;align-items:center;gap:8px;border:1px solid #cddbd3;background:#fff;border-radius:30px;padding:7px 13px;font-size:13px;font-weight:650}.badge::before{content:"";width:8px;height:8px;background:#9ca9a0;border-radius:100%}.badge.online::before{background:#26966f}.top-status{text-align:right}.fresh{font-size:12px;color:#688074;margin-top:6px}
.metrics{display:grid;grid-template-columns:repeat(4,1fr);gap:14px;margin-bottom:20px}.metric{background:#fff;border:1px solid #dce6df;border-radius:15px;padding:18px 21px}.metric-label{font-size:12px;text-transform:uppercase;letter-spacing:1px;color:#688074}.metric-value{font-size:28px;line-height:1.25;font-weight:650;margin-top:8px;letter-spacing:-.5px}.metric-note{font-size:12px;color:#688074;margin-top:4px}
.panel{background:#fff;border:1px solid #dce6df;border-radius:18px;overflow:hidden}.panel-head{padding:22px 24px;display:flex;align-items:center;justify-content:space-between;gap:16px;border-bottom:1px solid #e6ece7}.actions{display:flex;gap:8px;flex-wrap:wrap}.map-layout{display:grid;grid-template-columns:minmax(0,1fr) 250px;min-height:360px}.map-area{background:#eaf0ec;padding:20px;display:flex;align-items:center;justify-content:center;min-height:360px}.map-stage{position:relative;width:100%;max-width:720px;line-height:0}.map-stage img{display:block;width:100%;max-height:600px;object-fit:contain;image-rendering:pixelated}.map-stage svg{position:absolute;inset:0;width:100%;height:100%;pointer-events:none}.room-label{font-weight:700;fill:#193c36;stroke:#fff;paint-order:stroke;stroke-linejoin:round;text-anchor:middle;dominant-baseline:middle}.empty{max-width:320px;text-align:center;color:#688074;font-size:14px}.empty-mark{width:55px;height:55px;border:2px dashed #a9c1b0;border-radius:15px;margin:0 auto 15px;display:grid;place-items:center;font-size:25px}
.rooms{padding:22px;border-left:1px solid #e6ece7}.rooms h3{font-size:15px;margin:0 0 5px}.room-list{display:grid;gap:9px;margin-top:17px}.room{display:flex;align-items:center;gap:10px;border:1px solid #e0e8e2;border-radius:9px;padding:10px;font-size:14px}.room input{accent-color:#176b52;width:16px;height:16px}.room-title{overflow-wrap:anywhere}.room-id{font-size:11px;color:#7c9084}.room-selection{font-size:12px;color:#688074;margin-top:14px}.controls{display:flex;justify-content:space-between;align-items:center;gap:18px;padding:20px 24px;border-top:1px solid #e6ece7}.controls-note{max-width:390px}.message{padding:14px 0 0;font-size:14px;color:#516f60;min-height:36px}.message.error{color:#a04232}.footer{display:flex;justify-content:space-between;gap:15px;padding-top:13px;font-size:12px;color:#809084}[hidden]{display:none!important}
@media(max-width:800px){main{padding:24px 16px}.metrics{grid-template-columns:repeat(2,1fr)}.map-layout{grid-template-columns:1fr}.rooms{border-left:0;border-top:1px solid #e6ece7}.room-list{grid-template-columns:repeat(2,1fr)}.map-area{min-height:300px}.controls{align-items:flex-start;flex-direction:column}.panel-head{align-items:flex-start;flex-direction:column}.top{gap:12px}h1{font-size:32px}.metric-value{font-size:25px}.footer{flex-direction:column;gap:3px}}
</style>
</head>
<body><main>
<header class="top"><div><div class="eyebrow">NARWAL / LOCAL</div><h1>Your J1</h1></div>
<div class="top-status"><div id="online" class="badge">Connecting to service</div><div id="fresh" class="fresh">Waiting for feedback</div></div></header>
<section class="metrics" aria-label="Robot feedback">
<div class="metric"><div class="metric-label">Battery</div><div id="battery" class="metric-value">—</div><div id="charging" class="metric-note">Not reported</div></div>
<div class="metric"><div class="metric-label">Dock</div><div id="dock" class="metric-value">—</div><div class="metric-note">Reported dock contact</div></div>
<div class="metric"><div class="metric-label">Robot state</div><div id="robot-state" class="metric-value">—</div><div id="work-state" class="metric-note">Work state not reported</div></div>
<div class="metric"><div class="metric-label">Error</div><div id="error-code" class="metric-value">—</div><div id="error-note" class="metric-note">Not reported</div></div>
</section>
<section class="panel">
<div class="panel-head"><div><h2>Room map</h2><p id="map-caption" class="muted">Waiting for a reported map</p></div><div class="actions"><button id="refresh">Read status</button><button id="read-map">Read map</button></div></div>
<div class="map-layout"><div class="map-area">
<div id="map-empty" class="empty"><div class="empty-mark" aria-hidden="true">⌗</div><p>No room map received yet.</p><p class="muted">A map will appear after the J1 supplies it.</p></div>
<div id="map-stage" class="map-stage" hidden><img id="map-image" alt="Reported Narwal J1 room map"><svg id="map-labels" aria-hidden="true"></svg></div></div>
<aside class="rooms"><h3>Reported rooms</h3><p class="muted">Names and IDs from your map</p><div id="room-list" class="room-list"></div><p id="room-selection" class="room-selection">No rooms received.</p></aside></div>
<div class="controls"><div class="actions"><button class="primary" id="start" disabled>Start cleaning</button><button id="pause" disabled>Pause</button><button id="stop" disabled>Stop</button><button id="return_to_base" disabled>Return to dock</button></div>
<p id="controls-note" class="muted controls-note">Controls unavailable until verified for this J1.</p></div>
</section>
<p id="message" class="message" role="status" aria-live="polite"></p>
<footer class="footer"><span>Local connection · feedback comes from the robot</span><span id="last-command"></span></footer>
</main>
<script nonce="__TOKEN__">
"use strict";
const token="__TOKEN__", $=id=>document.getElementById(id);
let current=null,busy=false,loading=false,selected=new Set(),lastImageKey="";
const commands=["start","pause","stop","return_to_base"];
const labelRoom=r=>typeof r.name==="string"&&r.name.trim()?r.name:"Room "+r.room_id;
function message(text,error=false){$("message").textContent=text;$("message").classList.toggle("error",error);}
function shown(value){return value===undefined||value===null?"—":String(value);}
function render(s){
 current=s;const status=s.status||{},enabled=new Set(Array.isArray(s.enabled_commands)?s.enabled_commands:[]),online=s.online===true;
 $("online").textContent=online?"Online":s.connected?"Feedback stale":"Robot offline";$("online").classList.toggle("online",online);
 $("fresh").textContent=typeof s.age_seconds==="number"?"Last feedback "+Math.floor(s.age_seconds)+"s ago":"Waiting for robot feedback";
 $("battery").textContent=typeof status.battery_per==="number"?status.battery_per+"%":"—";
 $("charging").textContent=status.charge_status===true?"Charging":status.charge_status===false?"Not charging":"Not reported";
 $("dock").textContent=status.station_contact===true?"Docked":status.station_contact===false?"Away":"—";
 $("robot-state").textContent=shown(status.robot_status);$("work-state").textContent="Reported work state "+shown(status.work_status);
 $("error-code").textContent=shown(status.error_code);$("error-note").textContent=status.error_code===0?"No error reported":"Robot error code";
 for(const command of commands)$(command).disabled=busy||!online||!enabled.has(command);
 $("return_to_base").disabled=$("return_to_base").disabled||status.station_contact!==false;
 $("refresh").disabled=busy||!s.connected;$("read-map").disabled=busy||!s.connected;
 $("controls-note").textContent=!s.commands_enabled?"Controls unavailable until verified for this J1.":!online?"Fresh robot feedback is required before sending a command.":"Commands are checked against the robot’s reported response.";
 const map=s.map,rooms=map&&Array.isArray(map.rooms)?map.rooms:[],canSelect=online&&enabled.has("start")&&s.room_selection_enabled===true&&!busy;
 const actual=new Set(rooms.map(r=>r.room_id));for(const id of selected)if(!actual.has(id))selected.delete(id);
 if(!canSelect)selected.clear();
 $("room-list").replaceChildren();
 for(const room of rooms){
  const label=document.createElement("label");label.className="room";
  const box=document.createElement("input");box.type="checkbox";box.value=String(room.room_id);box.checked=selected.has(room.room_id);box.disabled=!canSelect;
  box.addEventListener("change",()=>{if(box.checked)selected.add(room.room_id);else selected.delete(room.room_id);selection(rooms,canSelect);});
  const title=document.createElement("span");title.className="room-title";title.textContent=labelRoom(room);
  const id=document.createElement("div");id.className="room-id";id.textContent="ID "+room.room_id;title.appendChild(id);label.append(box,title);$("room-list").appendChild(label);
 }
 selection(rooms,canSelect);
 $("map-stage").hidden=!map;$("map-empty").hidden=!!map;
 if(map){
  $("map-caption").textContent=online?"Reported map · "+map.width+" × "+map.height:"Stored reported map · robot offline or feedback stale";
  const key=JSON.stringify([map.map_id,map.width,map.height,online?Math.floor(Date.now()/3000):"stored"]);
  if(key!==lastImageKey){$("map-image").src="/map.png?v="+Date.now();lastImageKey=key;}
  const svg=$("map-labels");svg.setAttribute("viewBox","0 0 "+map.width+" "+map.height);svg.replaceChildren();
  for(const room of rooms)if(Number.isFinite(room.label_x)&&Number.isFinite(room.label_y)){
   const text=document.createElementNS("http://www.w3.org/2000/svg","text");text.setAttribute("x",String(room.label_x));text.setAttribute("y",String(room.label_y));text.setAttribute("class","room-label");text.setAttribute("font-size",String(Math.max(1,map.width/40)));text.setAttribute("stroke-width",String(Math.max(.15,map.width/200)));text.textContent=labelRoom(room);svg.appendChild(text);
  }
 }else{$("map-caption").textContent=s.map_error?"Map unavailable: "+s.map_error:"Waiting for a reported map";}
 const last=s.last_command;$("last-command").textContent=last?"Last command: "+last.command+" · "+(last.verified?"expected state observed":last.outcome||"awaiting feedback"):"";
}
function selection(rooms,allowed){$("room-selection").textContent=!rooms.length?"No rooms received.":!allowed?"Room selection is unavailable until verified and online.":selected.size?selected.size+" room"+(selected.size===1?"":"s")+" selected":"No selection: clean the configured full area.";}
async function load(){if(loading)return;loading=true;try{const response=await fetch("/api/state",{cache:"no-store",signal:AbortSignal.timeout(5000)});if(!response.ok)throw new Error("Cannot read local service");render(await response.json());}catch(error){$("online").textContent="Service unavailable";$("online").classList.remove("online");for(const command of commands)$(command).disabled=true;$("refresh").disabled=true;$("read-map").disabled=true;message(error.message,true);}finally{loading=false;}}
async function send(path,body){
 if(busy)return;busy=true;if(current)render(current);message("Waiting for the robot…");
 try{const response=await fetch(path,{method:"POST",headers:{"Content-Type":"application/json","X-Narwal-CSRF":token},body:JSON.stringify(body),cache:"no-store",signal:AbortSignal.timeout(90000)});
  const value=await response.json();if(!response.ok)throw new Error(value.error||"Request refused");
  message(path==="/api/command"?(value.verified?"Expected robot state observed.":"Request finished; check the reported state above."):"Robot feedback received.");
 }catch(error){message(error.message,true);}finally{busy=false;await load();}
}
for(const command of commands)$(command).addEventListener("click",()=>send("/api/command",{command,rooms:command==="start"?Array.from(selected):[]}));
$("refresh").addEventListener("click",()=>send("/api/refresh",{}));$("read-map").addEventListener("click",()=>send("/api/map/refresh",{}));
$("map-image").addEventListener("error",()=>{$("map-stage").hidden=true;$("map-empty").hidden=false;$("map-caption").textContent="Map image unavailable";lastImageKey="";});
load();setInterval(load,3000);
</script></body></html>"##;
