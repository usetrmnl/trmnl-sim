//! Injected HTTP failures, per route, as in the firmware's `scripts/mock_server.py`: each
//! route has a queue of faults consumed in order (one use per request); once it is empty
//! the route answers normally. A fault with no count stays at the head of its queue.
//!
//! The spec syntax is mock_server.py's: `KIND[=ARG][:COUNT]`, e.g. `500:3`, `timeout=20`,
//! `slow=1024,20`, `redirect=308:1`.

use std::collections::VecDeque;
use std::fmt;

/// Where a fault applies.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Route {
    /// `/api/display`.
    Display,
    /// Image downloads (`/images/...`).
    Image,
}

impl Route {
    pub fn parse(s: &str) -> Option<Route> {
        match s {
            "display" => Some(Route::Display),
            "image" => Some(Route::Image),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Route::Display => "display",
            Route::Image => "image",
        }
    }
}

/// `Content-Length` of a [`HttpFault::TooBig`] answer: over the firmware's non-PSRAM
/// MAX_IMAGE_SIZE (90000, include/config.h).
pub const TOO_BIG_LENGTH: usize = 100_000;

/// One way to answer a request badly.
#[derive(Clone, Debug, PartialEq)]
pub enum HttpFault {
    /// This HTTP status with a short text body.
    Status(u16),
    /// Accept, send nothing for this many seconds, then close.
    Timeout(f32),
    /// TCP RST, nothing sent.
    Reset,
    /// Clean close (FIN), nothing sent.
    Close,
    /// 307 or 308 with a Location back to the same path.
    Redirect(u16),
    /// `/api/display`: 200 with a body that isn't JSON.
    BadJson,
    /// `/api/display`: 200 with JSON `"status": N` (and the refresh rate).
    JsonStatus(i64),
    /// `/api/display`: the normal answer with `"filename": "empty_state"` (the logo screen).
    EmptyState,
    /// Image: the full Content-Length, then only this many bytes (default half) and close.
    Truncate(Option<usize>),
    /// Image: send this many bytes, stall this many seconds, send the rest.
    Slow(usize, f32),
    /// Image: 200 with Content-Length 0.
    Empty,
    /// Image: 200 with [`TOO_BIG_LENGTH`] zero bytes.
    TooBig,
    /// Image: `image/png` of random bytes.
    Garbage,
    /// Image: no Content-Length, the end marked by closing the connection.
    NoLength,
    /// Image: the real image with the other Content-Type (a BMP as `image/png`, a PNG as
    /// `image/bmp`).
    WrongType,
}

impl HttpFault {
    /// One of each kind, with mock_server.py's default arguments.
    pub const ALL: [HttpFault; 15] = [
        HttpFault::Status(500),
        HttpFault::Timeout(20.0),
        HttpFault::Reset,
        HttpFault::Close,
        HttpFault::Redirect(307),
        HttpFault::BadJson,
        HttpFault::JsonStatus(202),
        HttpFault::EmptyState,
        HttpFault::Truncate(None),
        HttpFault::Slow(1024, 20.0),
        HttpFault::Empty,
        HttpFault::TooBig,
        HttpFault::Garbage,
        HttpFault::NoLength,
        HttpFault::WrongType,
    ];

    /// The kinds that apply to `route`, with default arguments.
    pub fn kinds(route: Route) -> impl Iterator<Item = HttpFault> {
        Self::ALL.into_iter().filter(move |f| f.applies_to(route))
    }

    pub fn applies_to(&self, route: Route) -> bool {
        use HttpFault::*;
        match self {
            Status(_) | Timeout(_) | Reset | Close | Redirect(_) => true,
            BadJson | JsonStatus(_) | EmptyState => route == Route::Display,
            Truncate(_) | Slow(..) | Empty | TooBig | Garbage | NoLength | WrongType => route == Route::Image,
        }
    }

    /// The kind's name in a spec (`<code>` for [`HttpFault::Status`]).
    pub fn kind(&self) -> &'static str {
        use HttpFault::*;
        match self {
            Status(_) => "<code>",
            Timeout(_) => "timeout",
            Reset => "reset",
            Close => "close",
            Redirect(_) => "redirect",
            BadJson => "bad-json",
            JsonStatus(_) => "status",
            EmptyState => "empty-state",
            Truncate(_) => "truncate",
            Slow(..) => "slow",
            Empty => "empty",
            TooBig => "too-big",
            Garbage => "garbage",
            NoLength => "no-length",
            WrongType => "wrong-type",
        }
    }

    /// What it does, and what the firmware should make of it (from mock_server.py).
    pub fn help(&self) -> &'static str {
        use HttpFault::*;
        match self {
            Status(_) => {
                "Any HTTP status. /api/display: HTTPS_RESPONSE_CODE_INVALID (retried); \
                 image: HTTPS_IMAGE_DOWNLOAD_FAILED"
            }
            Timeout(_) => "Accept, send nothing for the given seconds, then close",
            Reset => "TCP RST, nothing sent",
            Close => "Clean close, nothing sent",
            Redirect(_) => "307/308 back to the same path: the firmware follows once and gets the next fault",
            BadJson => "200 with a body that isn't JSON: HTTPS_JSON_PARSING_ERR",
            JsonStatus(_) => {
                "200 with JSON \"status\": N. 202: HTTPS_NO_REGISTER (fast 5 s poll); \
                 500: HTTPS_RESET, which WIPES THE DEVICE'S CREDENTIALS"
            }
            EmptyState => "filename \"empty_state\": the logo screen",
            Truncate(_) => "The full Content-Length, then only some bytes (default half) and close: HTTPS_TIMED_OUT",
            Slow(..) => "Send some bytes, then stall: HTTPS_TIMED_OUT (the inactivity timeout is 15 s)",
            Empty => "Content-Length: 0: HTTPS_WRONG_IMAGE_SIZE",
            TooBig => "Content-Length 100000: HTTPS_IMAGE_FILE_TOO_BIG",
            Garbage => "image/png of random bytes: HTTPS_WRONG_IMAGE_FORMAT",
            NoLength => "No Content-Length, closed at the end (the firmware's writeToStream path)",
            WrongType => "The image with the other Content-Type (the firmware should sniff \"BM\")",
        }
    }

    /// The spec without a count (`500`, `timeout=20`, `slow=1024,20`...).
    pub fn spec(&self) -> String {
        use HttpFault::*;
        match self {
            Status(c) => c.to_string(),
            Timeout(s) => format!("timeout={s}"),
            Redirect(c) => format!("redirect={c}"),
            JsonStatus(n) => format!("status={n}"),
            Truncate(Some(n)) => format!("truncate={n}"),
            Slow(b, s) => format!("slow={b},{s}"),
            _ => self.kind().to_string(),
        }
    }

    /// Parse `KIND[=ARG][:COUNT]` for `route`: the fault and its count (None: until cleared).
    pub fn parse(text: &str, route: Route) -> Result<(HttpFault, Option<u32>), String> {
        let (body, count) = match text.split_once(':') {
            Some((b, c)) => {
                let n: u32 = c.parse().map_err(|_| format!("{text:?}: COUNT must be an integer"))?;
                if n < 1 {
                    return Err(format!("{text:?}: COUNT must be >= 1"));
                }
                (b, Some(n))
            }
            None => (text, None),
        };
        let (kind, arg) = match body.split_once('=') {
            Some((k, a)) if !a.is_empty() => (k, Some(a)),
            Some((k, _)) => (k, None),
            None => (body, None),
        };
        let bad = || format!("{text:?}: bad argument for {kind}");
        let num = |a: &str| a.trim().parse::<f32>().ok().filter(|s| s.is_finite() && *s >= 0.0);
        let fault = if !kind.is_empty() && kind.bytes().all(|b| b.is_ascii_digit()) {
            let code: u16 = kind.parse().unwrap_or(0);
            if !(100..=599).contains(&code) {
                return Err(format!("{text:?}: HTTP status must be 100-599"));
            }
            HttpFault::Status(code)
        } else {
            match (kind, arg) {
                ("timeout", a) => HttpFault::Timeout(a.map_or(Some(20.0), num).ok_or_else(bad)?),
                ("reset", None) => HttpFault::Reset,
                ("close", None) => HttpFault::Close,
                ("redirect", a) => match a.map_or(Ok(307), str::parse) {
                    Ok(c @ (307 | 308)) => HttpFault::Redirect(c),
                    _ => return Err(bad()),
                },
                ("bad-json", None) => HttpFault::BadJson,
                ("status", Some(a)) => HttpFault::JsonStatus(a.parse().map_err(|_| bad())?),
                ("empty-state", None) => HttpFault::EmptyState,
                ("truncate", a) => HttpFault::Truncate(a.map(str::parse).transpose().map_err(|_| bad())?),
                ("slow", None) => HttpFault::Slow(1024, 20.0),
                ("slow", Some(a)) => {
                    let (b, s) = a.split_once(',').ok_or_else(bad)?;
                    HttpFault::Slow(b.trim().parse().map_err(|_| bad())?, num(s).ok_or_else(bad)?)
                }
                ("empty", None) => HttpFault::Empty,
                ("too-big", None) => HttpFault::TooBig,
                ("garbage", None) => HttpFault::Garbage,
                ("no-length", None) => HttpFault::NoLength,
                ("wrong-type", None) => HttpFault::WrongType,
                _ if HttpFault::ALL.iter().any(|f| f.kind() == kind) => return Err(bad()),
                _ => {
                    let valid: Vec<&str> = HttpFault::kinds(route).map(|f| f.kind()).collect();
                    return Err(format!("{text:?}: unknown fault {kind:?}; valid: {}", valid.join(", ")));
                }
            }
        };
        if !fault.applies_to(route) {
            return Err(format!("{text:?}: {kind} doesn't apply to the {} route", route.name()));
        }
        Ok((fault, count))
    }
}

impl fmt::Display for HttpFault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.spec())
    }
}

/// A fault in a route's queue.
#[derive(Clone, Debug, PartialEq)]
pub struct QueuedFault {
    pub fault: HttpFault,
    /// Uses left; None: until cleared.
    pub count: Option<u32>,
}

impl fmt::Display for QueuedFault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.count {
            Some(n) => write!(f, "{}:{n}", self.fault),
            None => write!(f, "{}", self.fault),
        }
    }
}

/// Take one use of the fault at the head of `queue`.
pub(crate) fn take(queue: &mut VecDeque<QueuedFault>) -> Option<HttpFault> {
    let head = queue.front_mut()?;
    let fault = head.fault.clone();
    if let Some(n) = &mut head.count {
        *n -= 1;
        if *n == 0 {
            queue.pop_front();
        }
    }
    Some(fault)
}
