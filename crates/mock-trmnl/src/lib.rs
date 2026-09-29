//! A mock TRMNL server built into the simulator: `/api/setup`, `/api/display`, `/api/log`,
//! image downloads and firmware files for OTA, with images you add at runtime (from the
//! GUI, the control API or code). It mirrors `python/trmnl_mock.py`.
//!
//! The device reaches the host at 10.0.2.2, so the server URL to give the device is
//! [`MockServer::device_url`], e.g. `http://10.0.2.2:8090`.
//!
//! State lives behind [`MockServer::state`]; front-ends change it directly and the next
//! device request sees the change. Every request is recorded in [`State::requests`].
//!
//! HTTP and connection failures (the firmware's `scripts/mock_server.py` set) are queued
//! per route with [`State::add_fault`]; see [`fault`].

mod art;
pub mod convert;
pub mod fault;
mod http;
mod portal;

use std::collections::{HashMap, VecDeque};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use parking_lot::{Mutex, MutexGuard};
use serde_json::{Map, Value, json};

pub use convert::{ConvertOptions, Converted, Fit, Inks, Panel, Preview};
pub use fault::{HttpFault, QueuedFault, Route};
use http::{Action, Delivery, Wire};
pub use portal::portal_connect;

/// The port the server listens on unless told otherwise. Fixed, because a device keeps
/// the server URL it was onboarded with across simulator runs.
pub const DEFAULT_PORT: u16 = 8090;

/// The host as seen from the simulated device.
pub const DEVICE_HOST: &str = "10.0.2.2";

/// The double-click actions the firmware knows (`special_function` in `/api/display`).
pub const SPECIAL_FUNCTIONS: [&str; 8] =
    ["none", "identify", "sleep", "add_wifi", "restart_playlist", "rewind", "send_to_me", "guest_mode"];

/// Name of the built-in image served until others are added.
pub const DEFAULT_IMAGE: &str = "default";

/// An image the server can hand to the device.
#[derive(Clone)]
pub struct Image {
    /// Unique name; also the URL path: `/images/<name>.<ext>`.
    pub name: String,
    /// Server-style `plugin-<6 hex>-<epoch>` name the firmware caches the image under.
    pub filename: String,
    pub data: Arc<Vec<u8>>,
    pub ext: &'static str,
    /// What the panel should show (a screenshot of the device should match it).
    pub preview: Arc<Preview>,
    /// Bumped on every change to any image (lets viewers cache thumbnails).
    pub version: u64,
}

impl Image {
    pub fn content_type(&self) -> &'static str {
        if self.ext == "png" { "image/png" } else { "image/bmp" }
    }

    pub fn path(&self) -> String {
        format!("/images/{}.{}", self.name, self.ext)
    }
}

/// A file served verbatim, e.g. a `firmware.bin` for OTA updates.
#[derive(Clone, Debug)]
pub enum FileSource {
    Bytes(Arc<Vec<u8>>),
    /// Read on every request (so a rebuilt firmware is picked up).
    Path(PathBuf),
}

/// One request the device made.
#[derive(Clone, Debug)]
pub struct Request {
    /// Absolute index (like console lines).
    pub index: u64,
    pub method: String,
    /// Without the query string.
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    pub at: SystemTime,
    /// The simulator's virtual time when it arrived, if a clock is attached.
    pub sim_time_ns: Option<u64>,
    /// The response status code.
    pub status: u16,
    /// A short description of the response, e.g. `image hello, refresh 900 s`.
    pub summary: String,
}

impl Request {
    /// Header value by case-insensitive name (the TRMNL X modem path lowercases them).
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
}

/// Everything the server serves. Change fields directly (under [`MockServer::state`]);
/// mutating helpers bump [`State::version`] so viewers notice.
pub struct State {
    pub panel: Panel,
    pub images: Vec<Image>,
    /// The image `/api/display` points at (a name in `images`).
    pub current: String,
    /// Image names in playlist order.
    pub playlist: Vec<String>,
    /// Step through the playlist on every `/api/display`.
    pub auto_advance: bool,
    pub refresh_rate: u32,
    /// What a double-click does on the device (one of [`SPECIAL_FUNCTIONS`]).
    pub special_function: String,
    /// Raw fields merged into every `/api/display` answer (e.g. `maximum_compatibility`,
    /// `touchbar_mode`, `status`).
    pub extra: Map<String, Value>,
    /// One-shot `/api/display` answers, consumed first, one per request. Fields as in
    /// `extra`, plus optional `image` (a name).
    pub queue: VecDeque<Map<String, Value>>,
    /// `/api/setup` registers the device (otherwise `"status": 404`, "not registered").
    pub registered: bool,
    /// Faults for `/api/display` requests, consumed first (see [`fault`]).
    pub display_faults: VecDeque<QueuedFault>,
    /// Faults for image downloads.
    pub image_faults: VecDeque<QueuedFault>,
    pub api_key: String,
    pub friendly_id: String,
    /// Extra files by URL path.
    pub files: HashMap<String, FileSource>,
    /// Recent requests, oldest first.
    pub requests: VecDeque<Request>,
    /// Requests ever recorded.
    pub total_requests: u64,
    /// Bumped on every change (by the helpers and by requests).
    pub version: u64,
    /// The port while the server runs.
    pub port: Option<u16>,
    identify: Option<Image>,
    stamps: HashMap<String, u64>,
}

impl State {
    const MAX_REQUESTS: usize = 2000;

    fn new(panel: Panel) -> Self {
        let mut s = State {
            panel,
            images: Vec::new(),
            current: DEFAULT_IMAGE.into(),
            playlist: Vec::new(),
            auto_advance: false,
            refresh_rate: 900,
            special_function: "sleep".into(),
            extra: Map::new(),
            queue: VecDeque::new(),
            registered: true,
            display_faults: VecDeque::new(),
            image_faults: VecDeque::new(),
            api_key: "sim-test-api-key".into(),
            friendly_id: "SIMTST".into(),
            files: HashMap::new(),
            requests: VecDeque::new(),
            total_requests: 0,
            version: 0,
            port: None,
            identify: None,
            stamps: HashMap::new(),
        };
        let img = convert::convert_image(&art::default_screen(panel), panel, ConvertOptions::default());
        s.put_image(DEFAULT_IMAGE, img);
        s
    }

    /// The server as seen from the device, while it runs.
    pub fn device_url(&self) -> Option<String> {
        self.port.map(|p| format!("http://{DEVICE_HOST}:{p}"))
    }

    pub fn image(&self, name: &str) -> Option<&Image> {
        self.images.iter().find(|i| i.name == name)
    }

    /// Add or replace an image. Replacing gives it a new cache filename, so the device
    /// downloads it again.
    pub fn put_image(&mut self, name: &str, img: Converted) -> &Image {
        let name = sanitize(name);
        self.version += 1;
        let filename = self.stamp(&name);
        let image = Image {
            name: name.clone(),
            filename,
            data: Arc::new(img.data),
            ext: img.ext,
            preview: Arc::new(img.preview),
            version: self.version,
        };
        match self.images.iter().position(|i| i.name == name) {
            Some(i) => {
                self.images[i] = image;
                &self.images[i]
            }
            None => {
                self.images.push(image);
                self.images.last().unwrap()
            }
        }
    }

    /// Remove an image (the built-in default stays).
    pub fn remove_image(&mut self, name: &str) {
        if name == DEFAULT_IMAGE {
            return;
        }
        self.images.retain(|i| i.name != name);
        self.playlist.retain(|n| n != name);
        if self.current == name {
            self.current = DEFAULT_IMAGE.into();
        }
        self.version += 1;
    }

    /// Serve this image from now on (and continue the playlist from it, if it's in there).
    pub fn set_current(&mut self, name: &str) -> Result<(), String> {
        if self.image(name).is_none() {
            return Err(format!("no image named {name:?}"));
        }
        self.current = name.to_string();
        self.version += 1;
        Ok(())
    }

    /// Move to the next (or previous, `step` = -1) playlist entry after the current image.
    pub fn advance(&mut self, step: i32) {
        self.playlist.retain(|n| self.images.iter().any(|i| &i.name == n));
        let n = self.playlist.len() as i32;
        if n == 0 {
            return;
        }
        let next = match self.playlist.iter().position(|p| *p == self.current) {
            Some(i) => (i as i32 + step).rem_euclid(n),
            None => 0,
        };
        self.current = self.playlist[next as usize].clone();
        self.version += 1;
    }

    /// Queue a one-shot `/api/display` answer.
    pub fn enqueue(&mut self, fields: Map<String, Value>) {
        self.queue.push_back(fields);
        self.version += 1;
    }

    /// Queue a fault for `route`, `count` times (None: until cleared).
    pub fn add_fault(&mut self, route: Route, fault: HttpFault, count: Option<u32>) -> Result<(), String> {
        if !fault.applies_to(route) {
            return Err(format!("{} doesn't apply to the {} route", fault.kind(), route.name()));
        }
        self.faults_mut(route).push_back(QueuedFault { fault, count });
        self.version += 1;
        Ok(())
    }

    /// The fault queue of `route`.
    pub fn faults(&self, route: Route) -> &VecDeque<QueuedFault> {
        match route {
            Route::Display => &self.display_faults,
            Route::Image => &self.image_faults,
        }
    }

    /// The fault queue of `route`, to change (bump [`State::version`] after).
    pub fn faults_mut(&mut self, route: Route) -> &mut VecDeque<QueuedFault> {
        match route {
            Route::Display => &mut self.display_faults,
            Route::Image => &mut self.image_faults,
        }
    }

    /// Empty both fault queues.
    pub fn clear_faults(&mut self) {
        self.display_faults.clear();
        self.image_faults.clear();
        self.version += 1;
    }

    /// Requests with absolute index >= `since`.
    pub fn requests_since(&self, since: u64) -> impl Iterator<Item = &Request> {
        let first = self.total_requests - self.requests.len() as u64;
        self.requests.iter().skip(since.saturating_sub(first) as usize)
    }

    /// `plugin-<6 hex>-<epoch>`: the TRMNL X caches images under this name. The first 14
    /// characters identify the plugin (a new version replaces the old one) and files whose
    /// timestamp is over 24 h old are purged. Each version of a name gets a later stamp.
    fn stamp(&mut self, name: &str) -> String {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let last = self.stamps.get(name).copied().unwrap_or(0);
        let t = now.max(last + 1);
        self.stamps.insert(name.to_string(), t);
        // FNV-1a: a stable per-name plugin id.
        let id = name.bytes().fold(0x811c_9dc5u32, |h, b| (h ^ b as u32).wrapping_mul(0x0100_0193)) & 0xff_ffff;
        format!("plugin-{id:06x}-{t}")
    }

    fn identify_image(&mut self) -> Image {
        if self.identify.as_ref().is_none_or(|i| !i.filename.contains(&self.friendly_id)) {
            let img = convert::convert_image(
                &art::identify_screen(self.panel, &self.friendly_id),
                self.panel,
                ConvertOptions::default(),
            );
            self.identify = Some(Image {
                // Leading underscores can't come from `sanitize`: no clash with added images.
                name: "_identify".into(),
                filename: format!("identify-{}", self.friendly_id),
                data: Arc::new(img.data),
                ext: img.ext,
                preview: Arc::new(img.preview),
                version: 0,
            });
        }
        self.identify.clone().unwrap()
    }

    /// Answer one request, or fail it with the next queued fault of its route: what to do
    /// with the connection, the status sent (0: none) and a summary for the log.
    fn answer(&mut self, method: &str, target: &str, headers: &[(String, String)]) -> (Action, u16, String) {
        let path = target.split('?').next().unwrap_or("");
        let route = match path {
            "/api/display" => Some(Route::Display),
            _ if matches!(method, "GET" | "HEAD") && path.starts_with("/images/") => Some(Route::Image),
            _ => None,
        };
        let Some(f) = route.and_then(|r| fault::take(self.faults_mut(r))) else {
            let r = self.respond(method, path, headers);
            return (Action::Send(Wire::new(r.status, r.content_type, r.body)), r.status, r.summary);
        };
        use HttpFault::*;
        let text =
            |status: u16| Wire::new(status, "text/plain", Arc::new(format!("mock failure {status}\n").into_bytes()));
        let (action, status, what) = match f {
            Status(c) => (Action::Send(text(c)), c, format!("HTTP {c}")),
            Timeout(s) => (Action::Hold(Duration::from_secs_f32(s)), 0, format!("sent nothing for {s} s, then closed")),
            Reset => (Action::Reset, 0, "TCP RST, nothing sent".into()),
            Close => (Action::Close, 0, "closed, nothing sent".into()),
            Redirect(c) => {
                let w = Wire { location: Some(target.to_string()), ..Wire::new(c, "text/plain", Arc::default()) };
                (Action::Send(w), c, format!("HTTP {c} Location {target}"))
            }
            BadJson => {
                let w = Wire::new(200, "application/json", Arc::new(b"this is not json {{{".to_vec()));
                (Action::Send(w), 200, "200, not JSON".into())
            }
            JsonStatus(n) => {
                let body = json!({"status": n, "refresh_rate": self.refresh_rate});
                let w = Wire::new(200, "application/json", Arc::new(body.to_string().into_bytes()));
                (Action::Send(w), 200, format!("JSON status {n}"))
            }
            EmptyState => {
                let r = self.respond(method, path, headers);
                let mut body: Value = serde_json::from_slice(&r.body).unwrap_or_default();
                body["filename"] = json!("empty_state");
                let w = Wire::new(200, "application/json", Arc::new(body.to_string().into_bytes()));
                (Action::Send(w), 200, "filename empty_state".into())
            }
            // The image faults change the normal answer.
            _ => {
                let r = self.respond(method, path, headers);
                let len = r.body.len();
                let mut w = Wire::new(r.status, r.content_type, r.body);
                let what = match f {
                    Truncate(n) => {
                        let n = n.unwrap_or(len / 2).min(len);
                        w.delivery = Delivery::Cut(n);
                        format!("sent {n} of {len} bytes, then closed")
                    }
                    Slow(n, s) => {
                        w.delivery = Delivery::Stall(n, Duration::from_secs_f32(s));
                        format!("sent {} bytes, stalls {s} s, then the rest", n.min(len))
                    }
                    Empty => {
                        w.body = Arc::default();
                        "Content-Length 0".into()
                    }
                    TooBig => {
                        w.body = Arc::new(vec![0; fault::TOO_BIG_LENGTH]);
                        format!("{} zero bytes", fault::TOO_BIG_LENGTH)
                    }
                    Garbage => {
                        w.content_type = "image/png";
                        w.body = Arc::new(random_bytes(len.max(4096)));
                        format!("image/png, {} random bytes", w.body.len())
                    }
                    NoLength => {
                        w.content_length = false;
                        format!("{len} bytes without Content-Length")
                    }
                    WrongType => {
                        let really = w.content_type;
                        w.content_type = if really == "image/png" { "image/bmp" } else { "image/png" };
                        format!("{len} bytes as {} (really {really})", w.content_type)
                    }
                    _ => unreachable!("not an image fault: {f:?}"),
                };
                (Action::Send(w), r.status, what)
            }
        };
        self.version += 1;
        (action, status, format!("fault {}: {what}", f.spec()))
    }

    /// Answer one request: (status, content type, body, summary for the log).
    fn respond(&mut self, method: &str, path: &str, req_headers: &[(String, String)]) -> Reply {
        let base = self.device_url().unwrap_or_default();
        let header =
            |name: &str| req_headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str());
        match path {
            "/api/setup" => {
                if !self.registered {
                    // as trmnl.app answers an unknown MAC: HTTP 200, "status": 404 in the body (the
                    // firmware only reads the message from an HTTP 200, src/api-client/setup.cpp)
                    let mac = header("ID").unwrap_or_default();
                    let body = json!({"status": 404, "api_key": null, "friendly_id": null, "image_url": null,
                                      "message": format!("MAC {mac} not registered - send to support@trmnl.com to activate your TRMNL")});
                    return Reply::json(200, &body, "not registered".into());
                }
                let default = self.image(DEFAULT_IMAGE).map(|i| i.path()).unwrap_or_default();
                let body = json!({
                    "status": 200, "api_key": self.api_key, "friendly_id": self.friendly_id,
                    "image_url": format!("{base}{default}"),
                    "message": format!("Register at usetrmnl.com/signup with Device ID '{}'", self.friendly_id),
                });
                Reply::json(200, &body, format!("registered as {}", self.friendly_id))
            }
            "/api/display" => self.display(&base, header("special_function").is_some()),
            "/api/log" => Reply::json(200, &json!({"status": 200}), String::new()),
            _ if method == "GET" && path.starts_with("/images/") => {
                let file = &path["/images/".len()..];
                let img = self.images.iter().find(|i| format!("{}.{}", i.name, i.ext) == file).cloned();
                let img = img.or_else(|| self.identify.clone().filter(|i| format!("{}.{}", i.name, i.ext) == file));
                match img {
                    Some(i) => Reply::bytes(i.content_type(), i.data.clone(), format!("{} bytes", i.data.len())),
                    None => Reply::text(404, "no such image"),
                }
            }
            _ => match self.files.get(path) {
                Some(FileSource::Bytes(b)) => {
                    Reply::bytes("application/octet-stream", b.clone(), format!("{} bytes", b.len()))
                }
                Some(FileSource::Path(p)) => match std::fs::read(p) {
                    Ok(b) => {
                        let summary = format!("{} bytes from {}", b.len(), p.display());
                        Reply::bytes("application/octet-stream", Arc::new(b), summary)
                    }
                    Err(e) => Reply::text(500, &format!("{}: {e}", p.display())),
                },
                None => Reply::text(404, "not found"),
            },
        }
    }

    fn display(&mut self, base: &str, special: bool) -> Reply {
        let queued = self.queue.pop_front();
        let mut fields = self.extra.clone();
        let mut image = None;
        let mut action = None;
        if let Some(q) = &queued {
            for (k, v) in q {
                if k == "image" {
                    image = v.as_str().map(str::to_string);
                } else {
                    fields.insert(k.clone(), v.clone());
                }
            }
        } else if special {
            // A double-click: the device sends the `special_function` header and expects
            // the action it has stored (the one we sent last time) back.
            let sf = self.special_function.clone();
            match sf.as_str() {
                "rewind" => self.advance(-1),
                "restart_playlist" => {
                    if let Some(first) = self.playlist.first().cloned() {
                        self.current = first;
                    }
                }
                _ => {}
            }
            action = Some(sf);
        } else if self.auto_advance {
            self.advance(1);
        }
        let img = if action.as_deref() == Some("identify") {
            Some(self.identify_image())
        } else {
            let name = image.unwrap_or_else(|| self.current.clone());
            self.image(&name).or_else(|| self.image(DEFAULT_IMAGE)).cloned()
        };
        let mut body = json!({
            "status": 0,
            "image_url": img.as_ref().map(|i| format!("{base}{}", i.path())),
            "filename": img.as_ref().map(|i| i.filename.clone()),
            "refresh_rate": self.refresh_rate,
            "update_firmware": false,
            "firmware_url": null,
            "reset_firmware": false,
            "special_function": self.special_function,
        });
        if let Some(a) = &action {
            body["action"] = json!(a);
        }
        let map = body.as_object_mut().unwrap();
        for (k, v) in fields {
            map.insert(k, v);
        }
        let mut summary = match &img {
            Some(i) => format!("image {}", i.name),
            None => "no image".to_string(),
        };
        summary += &format!(", refresh {} s", body["refresh_rate"]);
        if let Some(a) = &action {
            summary += &format!(", action {a}");
        }
        if body["update_firmware"] == json!(true) {
            summary += ", update_firmware";
        }
        if body["reset_firmware"] == json!(true) {
            summary += ", reset_firmware";
        }
        if queued.is_some() {
            summary += " (queued)";
        }
        Reply::json(200, &body, summary)
    }

    fn record(&mut self, mut req: Request) {
        req.index = self.total_requests;
        self.total_requests += 1;
        self.version += 1;
        self.requests.push_back(req);
        while self.requests.len() > Self::MAX_REQUESTS {
            self.requests.pop_front();
        }
    }
}

/// Image names become URL paths: keep them tame.
fn sanitize(name: &str) -> String {
    let s: String =
        name.trim().chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect();
    let s = s.trim_start_matches('_');
    if s.is_empty() { "image".into() } else { s.to_string() }
}

/// A unique image name based on a file name (`photo.jpg` -> `photo`, `photo_2`, ...).
pub fn name_from_file(state: &State, file_name: &str) -> String {
    let stem = file_name.rsplit(['/', '\\']).next().unwrap_or(file_name);
    let stem = sanitize(stem.rsplit_once('.').map_or(stem, |(s, _)| s));
    let mut name = stem.clone();
    let mut n = 2;
    while state.image(&name).is_some() {
        name = format!("{stem}_{n}");
        n += 1;
    }
    name
}

struct Reply {
    status: u16,
    content_type: &'static str,
    body: Arc<Vec<u8>>,
    summary: String,
}

impl Reply {
    fn json(status: u16, v: &Value, summary: String) -> Reply {
        Reply { status, content_type: "application/json", body: Arc::new(v.to_string().into_bytes()), summary }
    }

    fn bytes(content_type: &'static str, body: Arc<Vec<u8>>, summary: String) -> Reply {
        Reply { status: 200, content_type, body, summary }
    }

    fn text(status: u16, msg: &str) -> Reply {
        Reply { status, content_type: "text/plain", body: Arc::new(msg.as_bytes().to_vec()), summary: msg.to_string() }
    }
}

type Clock = Box<dyn Fn() -> u64 + Send + Sync>;

struct Running {
    addr: SocketAddr,
    /// Set by stop(): ends the accept loop and cuts faults' waits short.
    stopping: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

struct Inner {
    state: Mutex<State>,
    running: Mutex<Option<Running>>,
    clock: Mutex<Option<Clock>>,
}

/// The built-in server. Cheap to clone; all clones share one state. It exists (with the
/// default image) before it is started, so images can be prepared first.
#[derive(Clone)]
pub struct MockServer {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for MockServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MockServer").field("port", &self.port()).finish()
    }
}

impl MockServer {
    pub fn new(panel: Panel) -> Self {
        MockServer {
            inner: Arc::new(Inner {
                state: Mutex::new(State::new(panel)),
                running: Mutex::new(None),
                clock: Mutex::new(None),
            }),
        }
    }

    pub fn state(&self) -> MutexGuard<'_, State> {
        self.inner.state.lock()
    }

    pub fn panel(&self) -> Panel {
        self.state().panel
    }

    /// Stamp requests with this virtual time (nanoseconds), e.g. the simulator's clock.
    pub fn set_clock(&self, clock: impl Fn() -> u64 + Send + Sync + 'static) {
        *self.inner.clock.lock() = Some(Box::new(clock));
    }

    /// Listen on 127.0.0.1:`port` (0 = any free port). Returns the bound address. If it
    /// already runs, returns its address.
    pub fn start(&self, port: u16) -> std::io::Result<SocketAddr> {
        let mut running = self.inner.running.lock();
        if let (Some(_), Some(p)) = (running.as_ref(), self.port()) {
            return Ok(SocketAddr::from(([127, 0, 0, 1], p)));
        }
        let listener = TcpListener::bind(("127.0.0.1", port))?;
        let bound = listener.local_addr()?;
        self.state().port = Some(bound.port());
        let stopping = Arc::new(AtomicBool::new(false));
        let (stop, me) = (stopping.clone(), self.clone());
        let thread = std::thread::Builder::new().name("mock-trmnl".into()).spawn(move || {
            for conn in listener.incoming() {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                match conn {
                    Ok(stream) => {
                        let (me, stop) = (me.clone(), stop.clone());
                        std::thread::spawn(move || me.handle(stream, &stop));
                    }
                    Err(e) => log::debug!("mock-trmnl: accept failed: {e}"),
                }
            }
        })?;
        *running = Some(Running { addr: bound, stopping, thread: Some(thread) });
        Ok(bound)
    }

    /// Stop listening (state and images are kept).
    pub fn stop(&self) {
        if let Some(mut r) = self.inner.running.lock().take() {
            r.stopping.store(true, Ordering::Relaxed);
            // Wake the accept loop so it sees `stopping` (and drops the listener).
            let _ = TcpStream::connect_timeout(&r.addr, Duration::from_secs(1));
            if let Some(t) = r.thread.take() {
                let _ = t.join();
            }
        }
        let mut st = self.state();
        st.port = None;
        st.version += 1;
    }

    pub fn port(&self) -> Option<u16> {
        self.state().port
    }

    /// The URL to give the device (`http://10.0.2.2:<port>`), while running.
    pub fn device_url(&self) -> Option<String> {
        self.state().device_url()
    }

    /// The URL from this machine, while running.
    pub fn host_url(&self) -> Option<String> {
        self.port().map(|p| format!("http://127.0.0.1:{p}"))
    }

    /// Decode, convert for the panel and add (or replace) an image.
    pub fn add_image(&self, name: &str, bytes: &[u8], opts: ConvertOptions) -> Result<Image, String> {
        let panel = self.panel();
        // Conversion is the slow part: don't hold the lock for it.
        let img = convert::convert(bytes, panel, opts)?;
        Ok(self.state().put_image(name, img).clone())
    }

    /// Add an image served exactly as given (PNG or BMP).
    pub fn add_raw_image(&self, name: &str, bytes: &[u8]) -> Result<Image, String> {
        let img = convert::passthrough(bytes, self.panel())?;
        Ok(self.state().put_image(name, img).clone())
    }

    /// Serve `source` at `path`; returns the device URL (if running) or the path.
    pub fn set_file(&self, path: &str, source: FileSource) -> String {
        let path = if path.starts_with('/') { path.to_string() } else { format!("/{path}") };
        let mut st = self.state();
        st.files.insert(path.clone(), source);
        st.version += 1;
        format!("{}{path}", st.device_url().unwrap_or_default())
    }

    fn handle(&self, stream: TcpStream, stopping: &AtomicBool) {
        let _ = stream.set_nodelay(true);
        let Some(req) = http::read_request(&stream) else { return };
        let sim_time_ns = self.inner.clock.lock().as_ref().map(|c| c());
        let action = {
            let mut st = self.state();
            let (action, status, summary) = st.answer(&req.method, &req.target, &req.headers);
            st.record(Request {
                index: 0,
                path: req.path().to_string(),
                method: req.method.clone(),
                headers: req.headers,
                body: req.body,
                at: SystemTime::now(),
                sim_time_ns,
                status,
                summary,
            });
            action
        };
        action.perform(stream, req.method == "HEAD", stopping);
    }
}

/// `n` bytes of noise (xorshift; nothing depends on its quality).
fn random_bytes(n: usize) -> Vec<u8> {
    let mut x = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(1) | 1;
    (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect()
}

#[cfg(test)]
mod tests;
