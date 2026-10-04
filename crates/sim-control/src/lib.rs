//! HTTP/JSON control interface for driving a running simulator from integration
//! tests (or scripts). Everything the GUI can do is available here, plus
//! screenshots, screen comparison and blocking waits on console/state/display.
//!
//! All endpoints are on a local address (e.g. `--control 127.0.0.1:7878`):
//!
//! | method | path                   | body / query                                   | result |
//! |--------|------------------------|------------------------------------------------|--------|
//! | GET    | `/status`              |                                                | status JSON |
//! | POST   | `/button`              | `{"down": true}`                               | |
//! | POST   | `/press`               | `{"ms": 1200}` hold for virtual ms, then release; `"count": 2, "gap_ms": 150` repeats it | |
//! | POST   | `/touch`               | `{"zone": "left"\|"center"\|"right", "ms": 120}` tap, returns after lift; `"down": bool` holds / lifts instead | |
//! | POST   | `/gesture`             | `{"gesture": "swipe_next"\|"swipe_back"\|"flick_next"\|"flick_back"}` | |
//! | POST   | `/dock`                | `{"docked": true}`                             | |
//! | POST   | `/reset`               |                                                | |
//! | POST   | `/power-cycle`         |                                                | |
//! | POST   | `/wake`                |                                                | |
//! | POST   | `/wifi`                | `{"available": false}`, `{"networks": [...]}` (see [`wifi`]) | |
//! | POST   | `/battery`             | `{"mv": 3300}`                                 | |
//! | POST   | `/turbo`               | `{"on": true}`                                 | |
//! | POST   | `/pause`               | `{"on": true}`                                 | |
//! | POST   | `/quit`                |                                                | |
//! | GET    | `/console`             | `?since=N`                                     | `{"total", "lines":[{"i","text"}]}` |
//! | POST   | `/wait`                | see [`WaitSpec`]                               | `{"ok", ...}` or 408 |
//! | GET    | `/screenshot`          | `?x=&y=&w=&h=` (optional crop)                 | `image/png` (gray; RGB on color panels) |
//! | POST   | `/screenshot/compare`  | PNG body; `?x=&y=&w=&h=&tolerance=&max_ratio=` | `{"match", "diff_pixels", "diff_ratio"}` |
//! | POST   | `/savepoint`           | `{"path": "/abs/file.trmnlsave", "label": "..."}` (both optional) | `{"ok", "savepoint"}`; 409 if not possible |
//! | POST   | `/restore`             | `{"path": "..."}` or `{"id": 3}` (in-memory slot) | `{"ok", "savepoint"}`; 409 on failure |
//! | GET    | `/savepoints`          |                                                | `{"savepoints": [...]}` (in-memory slots) |
//! | POST   | `/coverage`            | `{"path": "out.info", "reset": false}` (both optional) write lcov now | `{"ok", "path", "lines_found", "lines_hit", ...}` |
//! | GET    | `/preferences`         |                                                | NVS snapshot and editability |
//! | PUT    | `/preferences`         | `{"partition","namespace","key","type","value"}` (all strings) | updated snapshot; deep sleep only |
//! | DELETE | `/preferences`         | `{"partition","namespace","key"}`             | updated snapshot; deep sleep only |
//! | GET    | `/memcheck`            |                                                | `--memcheck` report: violations, heap stats, stack marks |
//! | *      | `/mock/...`            | the built-in mock TRMNL server, see [`mock`]   | |
//! | GET    | `/faults`              |                                                | faults, partitions, flash counters |
//! | POST   | `/faults`              | faults JSON, merged into the current ones (see [`faults`]) | as GET |
//! | DELETE | `/faults`              | clear all faults                               | as GET |
//! | POST   | `/mcp`                 | MCP JSON-RPC: every endpoint as a tool (see [`mcp`]) | |
//!
//! Screens are grayscale with 0 = black ink and 255 = paper.
//!
//! The routes are listed in [`endpoints::ENDPOINTS`]; the HTTP server answers only those, and
//! the MCP server offers each as a tool, so a new endpoint is added there (and here).

pub mod endpoints;
pub mod faults;
mod mcp;
mod mock;
mod preferences;
pub mod wifi;

use std::net::SocketAddr;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use sim_api::{Command, RunState, SavePointInfo, SavePointSource, SimHandle, SliderGesture, Status, TouchZone};
use tiny_http::{Header, Method, Request, Response, Server};

pub fn serve(handle: SimHandle, addr: SocketAddr) -> std::io::Result<(SocketAddr, JoinHandle<()>)> {
    serve_with_mock(handle, addr, None)
}

/// [`serve`], plus the `/mock/...` endpoints driving the built-in mock TRMNL server.
pub fn serve_with_mock(
    handle: SimHandle,
    addr: SocketAddr,
    mock: Option<mock_trmnl::MockServer>,
) -> std::io::Result<(SocketAddr, JoinHandle<()>)> {
    let server = Server::http(addr).map_err(std::io::Error::other)?;
    let bound = server.server_addr().to_ip().unwrap_or(addr);
    let t = std::thread::Builder::new().name("control".into()).spawn(move || {
        // One thread per request so a long /wait doesn't block other calls.
        for req in server.incoming_requests() {
            let h = handle.clone();
            let m = mock.clone();
            std::thread::spawn(move || handle_request(&h, m.as_ref(), req));
        }
    })?;
    Ok((bound, t))
}

type Reply = Response<std::io::Cursor<Vec<u8>>>;

fn json_reply(code: u16, v: Value) -> Reply {
    Response::from_data(serde_json::to_vec_pretty(&v).unwrap())
        .with_status_code(code)
        .with_header(Header::from_bytes("Content-Type", "application/json").unwrap())
}

fn err(code: u16, msg: impl Into<String>) -> Reply {
    json_reply(code, json!({ "ok": false, "error": msg.into() }))
}

fn handle_request(h: &SimHandle, mock: Option<&mock_trmnl::MockServer>, mut req: Request) {
    let method = req.method().clone();
    let url = req.url().to_string();
    let (path, query) = url.split_once('?').unwrap_or((&url, ""));
    let path = path.to_string();
    let q = parse_query(query);
    let origin = req.headers().iter().find(|h| h.field.equiv("Origin")).map(|h| h.value.to_string());
    let mut body = Vec::new();
    let _ = std::io::Read::read_to_end(req.as_reader(), &mut body);
    let reply = if path == "/mcp" {
        mcp::handle(h, mock, &method, origin.as_deref(), &body)
    } else if endpoints::find(&method, &path).is_none() {
        err(404, format!("no route {method} {path}"))
    } else {
        dispatch(h, mock, &method, &path, &q, &body)
    };
    let _ = req.respond(reply);
}

/// Answer a control API request (from HTTP or an MCP tool call).
fn dispatch(
    h: &SimHandle,
    mock: Option<&mock_trmnl::MockServer>,
    method: &Method,
    path: &str,
    q: &[(String, String)],
    body: &[u8],
) -> Reply {
    let routed = if path == "/mock" || path.starts_with("/mock/") {
        match mock {
            Some(m) => mock::route(m, method, path, q, body),
            None => Ok(err(404, "no mock server in this simulator")),
        }
    } else {
        route(h, method, path, q, body)
    };
    routed.unwrap_or_else(|e| err(400, e))
}

fn parse_query(q: &str) -> Vec<(String, String)> {
    q.split('&')
        .filter(|s| !s.is_empty())
        .map(|kv| {
            let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
            (k.to_string(), v.to_string())
        })
        .collect()
}

fn qget<T: std::str::FromStr>(q: &[(String, String)], k: &str) -> Option<T> {
    q.iter().find(|(kk, _)| kk == k).and_then(|(_, v)| v.parse().ok())
}

fn body_json(body: &[u8]) -> Result<Value, String> {
    if body.iter().all(|b| b.is_ascii_whitespace()) {
        return Ok(json!({}));
    }
    serde_json::from_slice(body).map_err(|e| format!("bad JSON body: {e}"))
}

fn route(h: &SimHandle, method: &Method, path: &str, q: &[(String, String)], body: &[u8]) -> Result<Reply, String> {
    let ok = || Ok(json_reply(200, json!({ "ok": true })));
    match (method, path) {
        (Method::Get | Method::Put | Method::Delete, "/preferences") => preferences::route(h, method, body),
        (Method::Get, "/status") => Ok(json_reply(200, status_json(h))),
        (Method::Post, "/bluetooth/connect" | "/bluetooth/disconnect" | "/bluetooth/att" | "/bluetooth/receive") => {
            let b = body_json(body)?;
            let connection = || b["connection"].as_u64().ok_or("need connection token");
            let operation = match path {
                "/bluetooth/connect" => sim_api::BluetoothOperation::Connect,
                "/bluetooth/disconnect" => sim_api::BluetoothOperation::Disconnect { connection: connection()? },
                "/bluetooth/receive" => sim_api::BluetoothOperation::Receive { connection: connection()? },
                _ => {
                    let values = b["data"].as_array().ok_or("need data byte array")?;
                    if values.is_empty() || values.len() > 517 {
                        return Err("ATT data must contain 1..517 bytes".into());
                    }
                    let data = values
                        .iter()
                        .map(|v| {
                            v.as_u64()
                                .filter(|n| *n <= 255)
                                .map(|n| n as u8)
                                .ok_or_else(|| "data must contain integers 0..255".to_string())
                        })
                        .collect::<Result<Vec<_>, _>>()?;
                    sim_api::BluetoothOperation::Exchange { connection: connection()?, data }
                }
            };
            let (reply, result) = crossbeam_channel::bounded(1);
            h.send(Command::Bluetooth { operation, reply });
            Ok(match result.recv_timeout(Duration::from_secs(10)) {
                Ok(Ok(r)) => json_reply(200, json!({"ok": true, "connection": r.connection, "data": r.data})),
                Ok(Err(e)) => err(409, e),
                Err(_) => err(504, "Bluetooth command timed out"),
            })
        }
        (Method::Post, "/button") => {
            let down = body_json(body)?["down"].as_bool().ok_or("need {\"down\": bool}")?;
            h.send(Command::Button(down));
            ok()
        }
        (Method::Post, "/press") => {
            let b = body_json(body)?;
            let ms = b["ms"].as_u64().unwrap_or(100);
            let count = b["count"].as_u64().unwrap_or(1) as u32;
            let gap_ms = b["gap_ms"].as_u64().unwrap_or(150);
            press(h, ms, count, gap_ms)?;
            ok()
        }
        (Method::Post, "/touch") => {
            let b = body_json(body)?;
            let zone = b["zone"]
                .as_str()
                .and_then(TouchZone::parse)
                .ok_or("need {\"zone\": \"left\"|\"center\"|\"right\"}")?;
            match b["down"].as_bool() {
                Some(true) => h.send(Command::TouchDown(zone)),
                Some(false) => h.send(Command::TouchUp(zone)),
                None => touch(h, zone, b["ms"].as_u64().unwrap_or(120))?,
            }
            ok()
        }
        (Method::Post, "/gesture") => {
            let g = body_json(body)?["gesture"]
                .as_str()
                .and_then(SliderGesture::parse)
                .ok_or("need {\"gesture\": \"swipe_next\"|\"swipe_back\"|\"flick_next\"|\"flick_back\"}")?;
            h.send(Command::Gesture(g));
            ok()
        }
        (Method::Post, "/dock") => {
            let docked = body_json(body)?["docked"].as_bool().ok_or("need {\"docked\": bool}")?;
            h.send(Command::SetDocked(docked));
            ok()
        }
        (Method::Post, "/reset") => {
            h.send(Command::Reset);
            ok()
        }
        (Method::Post, "/power-cycle") => {
            h.send(Command::PowerCycle);
            ok()
        }
        (Method::Post, "/wake") => {
            h.send(Command::WakeFromSleep);
            ok()
        }
        (Method::Post, "/wifi") => {
            let b = body_json(body)?;
            let (on, nets, client) = (b["available"].as_bool(), b.get("networks"), b["portal_client"].as_bool());
            if on.is_none() && nets.is_none() && client.is_none() {
                return Err("need {\"available\": bool}, {\"networks\": [...]} and/or {\"portal_client\": bool}".into());
            }
            if let Some(client) = client {
                h.send(Command::SetPortalClient(client));
            }
            if let Some(nets) = nets {
                h.send(Command::SetWifiNetworks(wifi::parse_networks(nets)?));
            }
            if let Some(on) = on {
                h.send(Command::SetWifiAvailable(on));
            }
            ok()
        }
        (Method::Post, "/battery") => {
            let mv = body_json(body)?["mv"].as_u64().ok_or("need {\"mv\": number}")?;
            h.send(Command::SetBatteryMv(mv as u32));
            ok()
        }
        (Method::Post, "/turbo") => {
            let on = body_json(body)?["on"].as_bool().unwrap_or(true);
            h.send(Command::SetTurbo(on));
            ok()
        }
        (Method::Post, "/pause") => {
            let on = body_json(body)?["on"].as_bool().unwrap_or(true);
            h.send(Command::Pause(on));
            ok()
        }
        (Method::Post, "/debug") => {
            h.send(Command::DumpDebug);
            ok()
        }
        (Method::Post, "/coverage") => {
            let b = body_json(body)?;
            let (reply, rx) = crossbeam_channel::bounded(1);
            let path = b["path"].as_str().map(std::path::PathBuf::from);
            h.send(Command::WriteCoverage { path, reset: b["reset"].as_bool().unwrap_or(false), reply });
            // Reading the ELF's line tables the first time takes a moment.
            match rx.recv_timeout(Duration::from_secs(120)) {
                Ok(Ok(s)) => Ok(json_reply(
                    200,
                    json!({
                        "ok": true,
                        "path": s.path,
                        "files": s.files,
                        "lines_found": s.lines_found,
                        "lines_hit": s.lines_hit,
                        "functions_found": s.functions_found,
                        "functions_hit": s.functions_hit,
                    }),
                )),
                Ok(Err(e)) => Ok(err(409, e)),
                Err(_) => Ok(err(504, "no coverage report from the emulator")),
            }
        }
        (Method::Get, "/memcheck") => {
            let (tx, rx) = crossbeam_channel::bounded(1);
            h.send(Command::Memcheck(tx));
            let report = rx.recv_timeout(Duration::from_secs(30)).map_err(|_| "the emulator did not answer")?;
            let v: Value = serde_json::from_str(&report).map_err(|e| format!("bad report: {e}"))?;
            Ok(json_reply(200, v))
        }
        (Method::Post, "/quit") => {
            h.send(Command::Quit);
            ok()
        }
        (Method::Post, "/savepoint") => {
            let b = body_json(body)?;
            let path = b["path"].as_str().map(std::path::PathBuf::from);
            let label = b["label"].as_str().map(str::to_string);
            Ok(savepoint_call(h, |reply| Command::SavePoint { label, path, reply: Some(reply) }))
        }
        (Method::Post, "/restore") => {
            let b = body_json(body)?;
            let from = match (b["path"].as_str(), b["id"].as_u64()) {
                (Some(p), _) => SavePointSource::File(p.into()),
                (None, Some(id)) => SavePointSource::Slot(id as u32),
                _ => return Err("need {\"path\": str} or {\"id\": n}".into()),
            };
            Ok(savepoint_call(h, |reply| Command::RestoreSavePoint { from, reply: Some(reply) }))
        }
        (Method::Get, "/savepoints") => {
            let list: Vec<Value> = h.status.lock().savepoints.iter().map(savepoint_json).collect();
            Ok(json_reply(200, json!({ "savepoints": list })))
        }
        (Method::Get, "/faults") => Ok(json_reply(200, faults_status(h))),
        (Method::Post, "/faults") => {
            let current = h.status.lock().faults.clone();
            let f = faults::merge_faults(&current, &body_json(body)?)?;
            faults::validate(&f, &h.status.lock().partitions)?;
            set_faults(h, f)?;
            Ok(json_reply(200, faults_status(h)))
        }
        (Method::Delete, "/faults") => {
            set_faults(h, sim_api::Faults::default())?;
            Ok(json_reply(200, faults_status(h)))
        }
        (Method::Get, "/console") => {
            let since = qget(q, "since").unwrap_or(0u64);
            let c = h.console.lock();
            let lines: Vec<Value> = c.lines_since(since).into_iter().map(|(i, t)| json!({"i": i, "text": t})).collect();
            Ok(json_reply(200, json!({ "total": c.total, "lines": lines })))
        }
        (Method::Post, "/wait") => {
            let spec = WaitSpec::from_json(&body_json(body)?)?;
            Ok(wait(h, &spec))
        }
        (Method::Get, "/screenshot") => {
            let (w, hgt, ch, px) = h.frame.lock().viewer(crop(q));
            let png = encode_png_channels(w, hgt, ch, &px);
            Ok(Response::from_data(png).with_header(Header::from_bytes("Content-Type", "image/png").unwrap()))
        }
        (Method::Post, "/screenshot/compare") => {
            let (w, hgt, ch, px) = h.frame.lock().viewer(crop(q));
            let (rw, rh, reference) = decode_png(body, ch)?;
            if (rw, rh) != (w, hgt) {
                return Ok(err(422, format!("reference is {rw}x{rh}, screen region is {w}x{hgt}")));
            }
            let tol: i32 = qget(q, "tolerance").unwrap_or(48);
            let max_ratio: f64 = qget(q, "max_ratio").unwrap_or(0.001);
            // A pixel differs if any channel is off by more than the tolerance.
            let diff = px
                .chunks(ch)
                .zip(reference.chunks(ch))
                .filter(|(a, b)| a.iter().zip(b.iter()).any(|(a, b)| (*a as i32 - *b as i32).abs() > tol))
                .count();
            let ratio = diff as f64 / (w * hgt).max(1) as f64;
            Ok(json_reply(
                200,
                json!({ "match": ratio <= max_ratio, "diff_pixels": diff, "diff_ratio": ratio, "width": w, "height": hgt }),
            ))
        }
        _ => Ok(err(404, format!("no route {method} {path}"))),
    }
}

// ---- status ------------------------------------------------------------------------------------

fn state_name(s: &RunState) -> &'static str {
    match s {
        RunState::Running => "running",
        RunState::Paused => "paused",
        RunState::Debugger => "debugger",
        RunState::Idle => "idle",
        RunState::LightSleep { .. } => "light_sleep",
        RunState::DeepSleep { .. } => "deep_sleep",
        RunState::Halted(_) => "halted",
    }
}

fn status_json(h: &SimHandle) -> Value {
    let st: Status = h.status.lock().clone();
    let generation = h.frame.lock().generation;
    let total = h.console.lock().total;
    let wake = match &st.state {
        RunState::DeepSleep { wake_at_ns } | RunState::LightSleep { wake_at_ns } => *wake_at_ns,
        _ => None,
    };
    json!({
        "state": state_name(&st.state),
        "halted_reason": match &st.state { RunState::Halted(m) => Some(m.clone()), _ => None },
        "wake_at_s": wake.map(|n| n as f64 / 1e9),
        "sim_time_s": st.sim_time_ns as f64 / 1e9,
        "mips": st.mips,
        "speed_ratio": st.speed_ratio,
        "board": {
            "name": st.board.name,
            "has_button": st.board.has_button,
            "has_touchbar": st.board.has_touchbar,
            "has_dock": st.board.has_dock,
            "has_5ghz": st.board.has_5ghz,
            "has_refresh_flashing": st.board.has_refresh_flashing,
        },
        "docked": st.docked,
        "charging": st.charging,
        "touching": st.touching.map(|z| z.name()),
        "battery_mv": st.battery_mv,
        "button_down": st.button_down,
        "bluetooth": {
            "active": st.bluetooth.active,
            "initialized": st.bluetooth.initialized,
            "advertising": st.bluetooth.advertising,
            "connection": st.bluetooth.connection,
            "advertisement": st.bluetooth.advertisement,
            "scan_response": st.bluetooth.scan_response,
        },
        "wifi_available": st.wifi_available,
        "wifi_connected": st.wifi_connected,
        "ip": st.ip,
        "portal_url": st.portal_url,
        "display_busy": st.display_busy,
        "display_refreshes": st.display_refreshes,
        "display_generation": generation,
        "boot_count": st.boot_count,
        "firmware": st.firmware,
        "turbo": st.turbo,
        "faults": st.faults.summary(),
        "power_losses": st.power_losses,
        "console_total": total,
    })
}

// ---- save points -------------------------------------------------------------------------------

fn savepoint_json(i: &SavePointInfo) -> Value {
    json!({
        "id": i.id,
        "label": i.label,
        "deep_sleep": i.deep_sleep,
        "sim_time_s": i.sim_time_ns as f64 / 1e9,
        "wake_at_s": i.wake_at_ns.map(|n| n as f64 / 1e9),
        "path": i.path.as_ref().map(|p| p.display().to_string()),
        "bytes": i.bytes,
    })
}

/// Send a save point command and wait for the emulator's answer.
fn savepoint_call(h: &SimHandle, cmd: impl FnOnce(sim_api::SavePointReply) -> Command) -> Reply {
    let (tx, rx) = crossbeam_channel::bounded(1);
    h.send(cmd(tx));
    match rx.recv_timeout(Duration::from_secs(120)) {
        Ok(Ok(info)) => json_reply(200, json!({ "ok": true, "savepoint": savepoint_json(&info) })),
        Ok(Err(e)) => err(409, e),
        Err(_) => err(500, "the emulator did not answer"),
    }
}

// ---- faults --------------------------------------------------------------------------------------

fn faults_status(h: &SimHandle) -> Value {
    let st = h.status.lock();
    let parts: Vec<Value> = st
        .partitions
        .iter()
        .map(|p| json!({"label": p.label, "type": p.kind, "subtype": p.subtype, "offset": p.offset, "size": p.size}))
        .collect();
    json!({
        "ok": true,
        "faults": faults::faults_json(&st.faults),
        "summary": st.faults.summary(),
        "power_losses": st.power_losses,
        "flash": {"programs": st.flash_programs, "erases": st.flash_erases},
        "partitions": parts,
    })
}

/// Send new faults and wait until the emulator applied them (or a power loss it arms
/// already fired).
fn set_faults(h: &SimHandle, f: sim_api::Faults) -> Result<(), String> {
    let losses = h.status.lock().power_losses;
    h.send(Command::SetFaults(f.clone()));
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        {
            let st = h.status.lock();
            if st.faults == f || st.power_losses > losses {
                return Ok(());
            }
        }
        if Instant::now() > deadline {
            return Err("timed out waiting for the faults to apply".into());
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

// ---- button ---------------------------------------------------------------------------------------

/// Hold the button for exactly `ms` of *virtual* time (timed by the emulator),
/// returning once it has been released.
fn press(h: &SimHandle, ms: u64, count: u32, gap_ms: u64) -> Result<(), String> {
    let before = h.status.lock().presses_done;
    h.send(if count > 1 { Command::PressRepeat { ms, gap_ms, count } } else { Command::Press { ms } });
    let total = (ms + gap_ms) * count.max(1) as u64;
    let deadline = Instant::now() + Duration::from_millis(total * 20 + 30_000);
    while h.status.lock().presses_done <= before {
        if Instant::now() > deadline {
            return Err("timed out waiting for the press to complete (is the simulator paused?)".into());
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    Ok(())
}

/// Tap a touch bar zone for `ms` of virtual time, returning once the finger lifted.
fn touch(h: &SimHandle, zone: TouchZone, ms: u64) -> Result<(), String> {
    let before = h.status.lock().touches_done;
    h.send(Command::Touch { zone, ms });
    let deadline = Instant::now() + Duration::from_millis(ms * 20 + 30_000);
    while h.status.lock().touches_done <= before {
        if Instant::now() > deadline {
            return Err("timed out waiting for the touch to complete (is the simulator paused?)".into());
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    Ok(())
}

// ---- waits -------------------------------------------------------------------------------------------

/// Conditions for `POST /wait`. All given conditions must hold at once.
///
/// ```json
/// {"console": "regex", "since": 120, "state": "deep_sleep", "min_refreshes": 3,
///  "display_idle": true, "wifi_connected": true, "timeout_s": 60, "settle_ms": 0}
/// ```
pub struct WaitSpec {
    console: Option<regex::Regex>,
    since: u64,
    state: Option<String>,
    min_refreshes: Option<u64>,
    min_boots: Option<u64>,
    display_idle: bool,
    wifi_connected: Option<bool>,
    portal: Option<bool>,
    timeout: Duration,
    /// Once satisfied, keep waiting this long (wall ms) and re-check; e.g. to
    /// let a refresh animation finish.
    settle: Duration,
}

impl WaitSpec {
    fn from_json(v: &Value) -> Result<Self, String> {
        let console = match v["console"].as_str() {
            Some(r) => Some(regex::Regex::new(r).map_err(|e| format!("bad regex: {e}"))?),
            None => None,
        };
        let state = v["state"].as_str().map(str::to_string);
        if let Some(s) = &state
            && !["running", "paused", "debugger", "idle", "light_sleep", "deep_sleep", "halted"].contains(&s.as_str())
        {
            return Err(format!("unknown state {s}"));
        }
        Ok(WaitSpec {
            console,
            since: v["since"].as_u64().unwrap_or(0),
            state,
            min_refreshes: v["min_refreshes"].as_u64(),
            min_boots: v["min_boots"].as_u64(),
            display_idle: v["display_idle"].as_bool().unwrap_or(false),
            wifi_connected: v["wifi_connected"].as_bool(),
            portal: v["portal"].as_bool(),
            timeout: Duration::from_secs_f64(v["timeout_s"].as_f64().unwrap_or(60.0)),
            settle: Duration::from_millis(v["settle_ms"].as_u64().unwrap_or(0)),
        })
    }
}

fn check(h: &SimHandle, s: &WaitSpec) -> Option<Value> {
    let mut found = Value::Null;
    if let Some(re) = &s.console {
        let c = h.console.lock();
        let hit = c.lines_since(s.since).into_iter().find(|(_, l)| re.is_match(l))?;
        found = json!({"i": hit.0, "text": hit.1});
    }
    let st = h.status.lock().clone();
    if let Some(want) = &s.state
        && state_name(&st.state) != want
    {
        return None;
    }
    if s.min_refreshes.is_some_and(|n| st.display_refreshes < n)
        || s.min_boots.is_some_and(|n| (st.boot_count as u64) < n)
        || (s.display_idle && st.display_busy)
        || s.wifi_connected.is_some_and(|w| st.wifi_connected != w)
        || s.portal.is_some_and(|p| st.portal_url.is_some() != p)
    {
        return None;
    }
    Some(found)
}

fn wait(h: &SimHandle, s: &WaitSpec) -> Reply {
    let start = Instant::now();
    loop {
        if let Some(line) = check(h, s) {
            if s.settle.is_zero() {
                return json_reply(
                    200,
                    json!({"ok": true, "elapsed_s": start.elapsed().as_secs_f64(), "line": line, "status": status_json(h)}),
                );
            }
            std::thread::sleep(s.settle);
            if let Some(line) = check(h, s) {
                return json_reply(
                    200,
                    json!({"ok": true, "elapsed_s": start.elapsed().as_secs_f64(), "line": line, "status": status_json(h)}),
                );
            }
        }
        if let RunState::Halted(m) = &h.status.lock().state
            && s.state.as_deref() != Some("halted")
        {
            return json_reply(409, json!({"ok": false, "error": format!("simulator halted: {m}")}));
        }
        if start.elapsed() > s.timeout {
            return json_reply(408, json!({"ok": false, "error": "timeout", "status": status_json(h)}));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

// ---- screen ---------------------------------------------------------------------------------------

fn crop(q: &[(String, String)]) -> Option<(usize, usize, usize, usize)> {
    Some((qget(q, "x")?, qget(q, "y")?, qget(q, "w")?, qget(q, "h")?))
}

/// 8-bit grayscale PNG.
pub fn encode_png(w: usize, h: usize, px: &[u8]) -> Vec<u8> {
    encode_png_channels(w, h, 1, px)
}

/// 8-bit PNG: 1 channel = grayscale, 3 = RGB.
pub fn encode_png_channels(w: usize, h: usize, channels: usize, px: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, w as u32, h as u32);
        enc.set_color(if channels == 3 { png::ColorType::Rgb } else { png::ColorType::Grayscale });
        enc.set_depth(png::BitDepth::Eight);
        let mut wr = enc.write_header().unwrap();
        wr.write_image_data(px).unwrap();
    }
    out
}

/// Decode any common PNG into 8-bit gray (`channels` = 1) or RGB (3).
fn decode_png(data: &[u8], channels: usize) -> Result<(usize, usize, Vec<u8>), String> {
    let mut dec = png::Decoder::new(std::io::Cursor::new(data));
    dec.set_transformations(png::Transformations::normalize_to_color8());
    let mut r = dec.read_info().map_err(|e| format!("bad PNG: {e}"))?;
    let mut buf = vec![0; r.output_buffer_size().ok_or("PNG too large")?];
    let info = r.next_frame(&mut buf).map_err(|e| format!("bad PNG: {e}"))?;
    let (w, h) = (info.width as usize, info.height as usize);
    let ch = info.color_type.samples();
    let px = buf[..info.buffer_size()].chunks(ch);
    let out = if channels == 3 {
        px.flat_map(|p| if ch <= 2 { [p[0]; 3] } else { [p[0], p[1], p[2]] }).collect()
    } else {
        px.map(|p| match ch {
            1 | 2 => p[0],
            _ => ((p[0] as u32 * 30 + p[1] as u32 * 59 + p[2] as u32 * 11) / 100) as u8,
        })
        .collect()
    };
    Ok((w, h, out))
}
