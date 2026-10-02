//! The control API's endpoints, the one list both front ends are built from: the HTTP server
//! answers only the routes listed here (anything else is a 404), and the MCP server offers
//! one tool per row, with its input schema generated from the row's parameters and its call
//! dispatched through the same router as the HTTP request. A new endpoint therefore needs a
//! row here, which makes it an MCP tool too; the tests check every row reaches a handler and
//! every route the handlers match is listed.

use tiny_http::Method;

pub struct Endpoint {
    pub method: Method,
    /// Absolute path; at most one `{name}` segment, which may span several path segments.
    pub path: &'static str,
    /// MCP tool name.
    pub tool: &'static str,
    pub doc: &'static str,
    pub params: &'static [Param],
    /// The request body is raw bytes (an image, a file) rather than JSON built from `params`.
    pub raw_body: Option<&'static str>,
    /// The success answer is a PNG rather than JSON.
    pub png: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Loc {
    /// `{name}` in the path.
    Path,
    /// `?name=` (booleans become `1`/`0`).
    Query,
    /// A key of the JSON body.
    Body,
    /// The whole JSON body.
    WholeBody,
}

#[derive(Clone, Copy)]
pub enum Ty {
    String,
    Integer,
    Number,
    Boolean,
    Object,
    /// Array of anything.
    Array,
    Strings,
    /// Array of integers 0..255.
    Bytes,
    Enum(&'static [&'static str]),
}

#[derive(Clone, Copy)]
pub struct Param {
    pub name: &'static str,
    pub loc: Loc,
    pub ty: Ty,
    pub required: bool,
    /// `null` is accepted (and resets the value).
    pub nullable: bool,
    pub doc: &'static str,
}

const fn param(name: &'static str, loc: Loc, ty: Ty, doc: &'static str) -> Param {
    Param { name, loc, ty, required: false, nullable: false, doc }
}
const fn body(name: &'static str, ty: Ty, doc: &'static str) -> Param {
    param(name, Loc::Body, ty, doc)
}
const fn query(name: &'static str, ty: Ty, doc: &'static str) -> Param {
    param(name, Loc::Query, ty, doc)
}

impl Param {
    const fn required(mut self) -> Self {
        self.required = true;
        self
    }
    const fn nullable(mut self) -> Self {
        self.nullable = true;
        self
    }
}

const fn ep(method: Method, path: &'static str, tool: &'static str, doc: &'static str) -> Endpoint {
    Endpoint { method, path, tool, doc, params: &[], raw_body: None, png: false }
}

impl Endpoint {
    const fn params(mut self, params: &'static [Param]) -> Self {
        self.params = params;
        self
    }
    const fn raw_body(mut self, what: &'static str) -> Self {
        self.raw_body = Some(what);
        self
    }
    const fn png(mut self) -> Self {
        self.png = true;
        self
    }

    /// The `{name}` value if `path` is this endpoint's.
    pub fn match_path<'a>(&self, path: &'a str) -> Option<&'a str> {
        match self.path.split_once('{') {
            None => (self.path == path).then_some(""),
            Some((prefix, rest)) => {
                let suffix = &rest[rest.find('}')? + 1..];
                let middle = path.strip_prefix(prefix)?.strip_suffix(suffix)?;
                (!middle.is_empty()).then_some(middle)
            }
        }
    }

    /// The path with `{name}` replaced.
    pub fn fill_path(&self, value: &str) -> String {
        match (self.path.find('{'), self.path.find('}')) {
            (Some(a), Some(b)) => format!("{}{value}{}", &self.path[..a], &self.path[b + 1..]),
            _ => self.path.to_string(),
        }
    }
}

/// The endpoint serving `method` on `path`.
pub fn find(method: &Method, path: &str) -> Option<&'static Endpoint> {
    ENDPOINTS.iter().find(|e| e.method == *method && e.match_path(path).is_some())
}

const CROP: [Param; 4] = [
    query("x", Ty::Integer, "crop: left edge"),
    query("y", Ty::Integer, "crop: top edge"),
    query("w", Ty::Integer, "crop: width (all four of x, y, w, h or none)"),
    query("h", Ty::Integer, "crop: height"),
];
const STATES: &[&str] = &["running", "paused", "idle", "light_sleep", "deep_sleep", "halted"];
const CONNECTION: Param = body("connection", Ty::Integer, "token from bluetooth_connect").required();
const PREF_KEY: [Param; 3] = [
    body("partition", Ty::String, "NVS partition label, e.g. \"nvs\"").required(),
    body("namespace", Ty::String, "NVS namespace").required(),
    body("key", Ty::String, "NVS key").required(),
];

pub static ENDPOINTS: &[Endpoint] = &[
    ep(Method::Get, "/status", "status", "Simulator status: run state, virtual time, board capabilities, battery, WiFi/IP, portal URL, display busy and refresh count, boot count, firmware, faults, console line total."),
    ep(Method::Post, "/button", "button", "Hold (down: true) or release (down: false) the device button.")
        .params(&[body("down", Ty::Boolean, "true holds, false releases").required()]),
    ep(Method::Post, "/press", "press", "Press the button for ms of virtual time and return once released; count > 1 repeats it (a double click is count 2).")
        .params(&[
            body("ms", Ty::Integer, "hold time in virtual ms (default 100)"),
            body("count", Ty::Integer, "number of presses (default 1)"),
            body("gap_ms", Ty::Integer, "virtual ms between presses (default 150)"),
        ]),
    ep(Method::Post, "/touch", "touch", "TRMNL X touch bar: tap a zone for ms of virtual time (returns after the finger lifts), or with down hold (true) / lift (false) a finger.")
        .params(&[
            body("zone", Ty::Enum(&["left", "center", "right"]), "touch bar zone").required(),
            body("ms", Ty::Integer, "tap length in virtual ms (default 120)"),
            body("down", Ty::Boolean, "hold (true) or lift (false) instead of tapping"),
        ]),
    ep(Method::Post, "/gesture", "gesture", "TRMNL X touch bar slide (slide mode only).")
        .params(&[body("gesture", Ty::Enum(&["swipe_next", "swipe_back", "flick_next", "flick_back"]), "the slide").required()]),
    ep(Method::Post, "/dock", "dock", "Put the TRMNL X on (docked: true) or off its magnetic dock.")
        .params(&[body("docked", Ty::Boolean, "on the dock").required()]),
    ep(Method::Post, "/reset", "reset", "Reset the chip (as the reset line does)."),
    ep(Method::Post, "/power-cycle", "power_cycle", "Remove and restore power."),
    ep(Method::Post, "/wake", "wake", "Wake the device from deep or light sleep now."),
    ep(Method::Post, "/wifi", "wifi", "Change the WiFi environment: networks in or out of range, the access points, whether a captive portal client is attached. Give at least one.")
        .params(&[
            body("available", Ty::Boolean, "networks in range"),
            body("networks", Ty::Array, "access points replacing the current ones: [{\"ssid\", \"password\" (null = any but \"fail\"), \"rssi\", \"channel\" (1-14 or 32-177), \"open\", \"internet\"}]; only ssid is required"),
            body("portal_client", Ty::Boolean, "false lets an unattended captive portal run ahead in turbo"),
        ]),
    ep(Method::Post, "/battery", "battery", "Set the battery voltage.")
        .params(&[body("mv", Ty::Integer, "millivolts, e.g. 3300").required()]),
    ep(Method::Post, "/turbo", "turbo", "Run as fast as possible instead of pacing to wall-clock time.")
        .params(&[body("on", Ty::Boolean, "default true")]),
    ep(Method::Post, "/pause", "pause", "Pause or resume the CPU.")
        .params(&[body("on", Ty::Boolean, "default true")]),
    ep(Method::Post, "/debug", "debug", "Dump CPU state, backtrace, FreeRTOS tasks and board diagnostics to the console."),
    ep(Method::Post, "/quit", "quit", "Quit the simulator."),
    ep(Method::Get, "/console", "console", "Serial console lines with absolute indices, and the total so far.")
        .params(&[query("since", Ty::Integer, "first line index to return (default 0)")]),
    ep(Method::Post, "/wait", "wait", "Block until all the given conditions hold at once. Fails on timeout (HTTP 408) or if the CPU halts (409, unless waiting for state halted). Returns the matching console line and the status.")
        .params(&[
            body("console", Ty::String, "regex a console line must match"),
            body("since", Ty::Integer, "first console line index to search (default 0)"),
            body("state", Ty::Enum(STATES), "run state"),
            body("min_refreshes", Ty::Integer, "at least this many display refreshes"),
            body("min_boots", Ty::Integer, "at least this many boots"),
            body("display_idle", Ty::Boolean, "the panel is not busy"),
            body("wifi_connected", Ty::Boolean, "WiFi connected (or not)"),
            body("portal", Ty::Boolean, "captive portal up (or not)"),
            body("timeout_s", Ty::Number, "wall-clock seconds (default 60)"),
            body("settle_ms", Ty::Integer, "once satisfied, wait this long (wall ms) and check again"),
        ]),
    ep(Method::Get, "/screenshot", "screenshot", "The screen as a PNG: grayscale with 0 = ink, 255 = paper; RGB on color panels.")
        .params(&CROP)
        .png(),
    ep(Method::Post, "/screenshot/compare", "screenshot_compare", "Compare the screen (or a crop of it) with a reference PNG of the same size.")
        .params(&[
            CROP[0], CROP[1], CROP[2], CROP[3],
            query("tolerance", Ty::Integer, "per-channel difference a pixel may have (default 48)"),
            query("max_ratio", Ty::Number, "fraction of differing pixels that still matches (default 0.001)"),
        ])
        .raw_body("the reference PNG"),
    ep(Method::Post, "/savepoint", "savepoint", "Take a save point (full machine state). Refused (409) when not possible right now.")
        .params(&[
            body("path", Ty::String, "also write it to this absolute .trmnlsave file"),
            body("label", Ty::String, "label for the in-memory slot"),
        ]),
    ep(Method::Post, "/restore", "restore", "Restore a save point from a file or an in-memory slot.")
        .params(&[
            body("path", Ty::String, "a .trmnlsave file"),
            body("id", Ty::Integer, "an in-memory slot id (see savepoints)"),
        ]),
    ep(Method::Get, "/savepoints", "savepoints", "The in-memory save points."),
    ep(Method::Post, "/coverage", "coverage", "With --coverage: write the lcov tracefile now, optionally starting the counts over.")
        .params(&[
            body("path", Ty::String, "tracefile path (default: the --coverage one)"),
            body("reset", Ty::Boolean, "start over after writing"),
        ]),
    ep(Method::Get, "/preferences", "preferences", "The NVS values, warnings, and whether editing is allowed now (only in deep sleep)."),
    ep(Method::Put, "/preferences", "preferences_set", "Create or replace a typed NVS value (deep sleep only). Returns the updated snapshot.")
        .params(&[
            PREF_KEY[0], PREF_KEY[1], PREF_KEY[2],
            body("type", Ty::Enum(&["string", "blob", "u8", "u16", "u32", "u64", "i8", "i16", "i32", "i64"]), "NVS type (Arduino booleans are u8)").required(),
            body("value", Ty::String, "decimal integer, literal text, or hex blob bytes (spaces optional)").required(),
        ]),
    ep(Method::Delete, "/preferences", "preferences_delete", "Delete an NVS key (deep sleep only). Returns the updated snapshot.")
        .params(&PREF_KEY),
    ep(Method::Get, "/memcheck", "memcheck", "With --memcheck: violations, suppressed ones, heap statistics, stack marks."),
    ep(Method::Get, "/faults", "faults", "Injected faults, power losses, flash program/erase counts and the partition table."),
    ep(Method::Post, "/faults", "faults_set", "Merge faults into the current ones: keys left out keep their value, null resets one. Returns the new state.")
        .params(&[
            body("net", Ty::Object, "{\"latency_ms\", \"loss\" (0..1), \"bandwidth_bps\", \"dns\": \"servfail\"|\"nxdomain\"|\"empty\"|\"timeout\", \"no_internet\", \"offline\", \"tcp_cut\": {\"after_bytes\", \"stall\", \"port\"}}").nullable(),
            body("power_loss", Ty::Object, "{\"op\": \"any\"|\"program\"|\"erase\", \"partition\", \"range\": [start, end], \"nth\", \"cut\": \"before\"|\"torn\"|\"after\"}").nullable(),
            body("i2c_absent", Ty::Array, "I2C addresses that don't answer, e.g. [85, \"0x55\"]").nullable(),
            body("panel_busy_stuck", Ty::Boolean, "the panel's BUSY line never releases").nullable(),
            body("modem_unresponsive", Ty::Boolean, "the TRMNL X's ESP-AT modem doesn't answer").nullable(),
            body("modem_at_errors", Ty::Strings, "AT command prefixes the modem answers with ERROR").nullable(),
            body("touch_bar", Ty::Enum(&["reset", "lockup", "ati_error"]), "touch bar controller fault").nullable(),
            body("gauge_reset", Ty::Boolean, "the fuel gauge lost its state").nullable(),
            body("chip_temp_c", Ty::Number, "chip temperature sensor reading (°C)").nullable(),
        ]),
    ep(Method::Delete, "/faults", "faults_clear", "Clear all injected faults."),
    ep(Method::Post, "/bluetooth/connect", "bluetooth_connect", "Connect a BLE central to the advertising firmware; returns a connection token."),
    ep(Method::Post, "/bluetooth/disconnect", "bluetooth_disconnect", "Disconnect the BLE central.")
        .params(&[CONNECTION]),
    ep(Method::Post, "/bluetooth/att", "bluetooth_att", "Send an ATT PDU and return the ATT reply bytes (ATT errors come back as ATT bytes).")
        .params(&[CONNECTION, body("data", Ty::Bytes, "the ATT PDU, 1..517 bytes").required()]),
    ep(Method::Post, "/bluetooth/receive", "bluetooth_receive", "The next queued notification/indication, or [] if none.")
        .params(&[CONNECTION]),
    ep(Method::Get, "/mock", "mock_state", "The built-in mock TRMNL server's state: port, images, current image, playlist, answers, queue, faults, files."),
    ep(Method::Post, "/mock/start", "mock_start", "Start the built-in mock server; returns its port and the URL the device should use.")
        .params(&[body("port", Ty::Integer, "host port (0 = any free one)")]),
    ep(Method::Post, "/mock/stop", "mock_stop", "Stop the built-in mock server."),
    ep(Method::Post, "/mock/images", "mock_add_image", "Add an image (PNG/JPEG/BMP/GIF), converted for the panel unless raw.")
        .params(&[
            query("name", Ty::String, "image name").required(),
            query("current", Ty::Boolean, "make it the image /api/display serves"),
            query("dither", Ty::Boolean, "dither when converting (default true)"),
            query("fit", Ty::Enum(&["contain", "cover", "stretch"]), "how to fit the panel (default contain)"),
            query("raw", Ty::Boolean, "serve the bytes as they are"),
        ])
        .raw_body("the image file"),
    ep(Method::Get, "/mock/images/{name}/expected", "mock_expected_image", "The PNG the screen should show for a mock server image.")
        .params(&[param("name", Loc::Path, Ty::String, "image name").required()])
        .png(),
    ep(Method::Delete, "/mock/images/{name}", "mock_remove_image", "Remove a mock server image.")
        .params(&[param("name", Loc::Path, Ty::String, "image name").required()]),
    ep(Method::Post, "/mock/display", "mock_display", "Change what /api/display answers. Returns the server state.")
        .params(&[
            body("image", Ty::String, "current image name"),
            body("refresh_rate", Ty::Integer, "seconds"),
            body("special_function", Ty::Enum(&mock_trmnl::SPECIAL_FUNCTIONS), "what a double-click does"),
            body("playlist", Ty::Strings, "image names to rotate through"),
            body("auto_advance", Ty::Boolean, "advance the playlist on each request"),
            body("registered", Ty::Boolean, "the device is registered (setup succeeds)"),
            body("friendly_id", Ty::String, "the device's friendly id"),
            body("api_key", Ty::String, "the device's API key"),
            body("extra", Ty::Object, "extra answer fields; a null value removes one"),
        ]),
    ep(Method::Post, "/mock/queue", "mock_queue", "Raw /api/display fields (plus image) for the next answer only.")
        .params(&[param("fields", Loc::WholeBody, Ty::Object, "the answer fields").required()]),
    ep(Method::Delete, "/mock/queue", "mock_queue_clear", "Drop the queued answers."),
    ep(Method::Post, "/mock/faults", "mock_faults", "Append HTTP and connection failures to each route's queue, in mock_server.py syntax KIND[=ARG][:COUNT], e.g. \"503:2\", \"timeout=20\", \"slow=1024,20\", \"truncate:1\".")
        .params(&[
            body("display", Ty::Strings, "faults for /api/display"),
            body("image", Ty::Strings, "faults for image downloads"),
        ]),
    ep(Method::Delete, "/mock/faults", "mock_faults_clear", "Clear the queued mock server faults.")
        .params(&[query("route", Ty::Enum(&["display", "image"]), "only this route's (default both)")]),
    ep(Method::Post, "/mock/files", "mock_add_file", "Serve a file at a path on the mock server; returns its URL.")
        .params(&[query("path", Ty::String, "e.g. /firmware.bin").required()])
        .raw_body("the file"),
    ep(Method::Get, "/mock/requests", "mock_requests", "Requests the device made to the mock server.")
        .params(&[query("since", Ty::Integer, "first request index (default 0)")]),
];

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn rows_are_unique_and_well_formed() {
        let mut routes = HashSet::new();
        let mut tools = HashSet::new();
        for e in ENDPOINTS {
            assert!(routes.insert((e.method.to_string(), e.path)), "{} {} listed twice", e.method, e.path);
            assert!(tools.insert(e.tool), "tool {} listed twice", e.tool);
            assert!(e.tool.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'), "tool name {}", e.tool);
            assert_eq!(e.path.contains('{'), e.params.iter().any(|p| p.loc == Loc::Path), "{}", e.path);
            assert!(e.params.iter().filter(|p| p.loc == Loc::WholeBody).count() <= 1);
            assert!(e.raw_body.is_none() || e.params.iter().all(|p| matches!(p.loc, Loc::Path | Loc::Query)));
            assert_eq!(find(&e.method, &e.fill_path("x")).map(|f| f.tool), Some(e.tool));
        }
    }

    /// Every row reaches its handler (no "no route" answer), so every MCP tool works.
    #[test]
    fn every_row_reaches_a_handler() {
        // No emulator: commands that expect an answer fail at once instead of waiting.
        let (h, ports) = sim_api::channel(std::sync::Arc::new(sim_api::Frame::new(4, 2).into()));
        drop(ports);
        let mock = mock_trmnl::MockServer::new(mock_trmnl::Panel::Og);
        for e in ENDPOINTS {
            let q: Vec<(String, String)> = e
                .params
                .iter()
                .filter(|p| p.loc == Loc::Query)
                .map(|p| (p.name.to_string(), if let Ty::Enum(v) = p.ty { v[0] } else { "1" }.to_string()))
                .collect();
            // A malformed body: handlers that read one fail before doing anything.
            let reply = crate::dispatch(&h, Some(&mock), &e.method, &e.fill_path("x"), &q, b"{");
            let mut body = Vec::new();
            std::io::Read::read_to_end(&mut reply.into_reader(), &mut body).unwrap();
            let body = String::from_utf8_lossy(&body);
            assert!(!body.contains("no route"), "{} {} has no handler: {body}", e.method, e.path);
        }
    }

    /// Every literal route the routers match is listed, so none is missing from MCP.
    #[test]
    fn every_handled_route_is_listed() {
        let arm =
            regex::Regex::new(r#"\(((?:Method::\w+\s*\|\s*)*Method::\w+),\s*((?:"[^"]*"\s*\|\s*)*"[^"]*")\)"#).unwrap();
        let word = regex::Regex::new(r#"Method::(\w+)|"([^"]*)""#).unwrap();
        let mut n = 0;
        for (src, prefix) in [(include_str!("lib.rs"), ""), (include_str!("mock.rs"), "/mock/")] {
            for c in arm.captures_iter(src) {
                let methods = word.captures_iter(&c[1]).map(|m| m[1].to_uppercase());
                let paths: Vec<String> = word
                    .captures_iter(&c[2])
                    .map(|m| match &m[2] {
                        "" if prefix == "/mock/" => "/mock".to_string(),
                        p => format!("{prefix}{p}"),
                    })
                    .collect();
                for m in methods {
                    let method: Method = m.parse().unwrap();
                    for p in &paths {
                        assert!(find(&method, p).is_some(), "{m} {p} is handled but not in ENDPOINTS");
                        n += 1;
                    }
                }
            }
        }
        assert!(n > 30, "found only {n} routes; did the routers change shape?");
    }
}
