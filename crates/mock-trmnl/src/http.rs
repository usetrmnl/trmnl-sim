//! The server's HTTP/1.1, by hand: one request per connection (`Connection: close`, like
//! the firmware's `scripts/mock_server.py`), so a fault can do anything to the socket: send
//! nothing, reset it, lie about the length, stall halfway.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Longest request head (request line and headers) accepted.
const MAX_HEAD: usize = 64 * 1024;
/// A client that sends nothing for this long is dropped.
const READ_TIMEOUT: Duration = Duration::from_secs(60);

pub(crate) struct HttpRequest {
    pub method: String,
    /// Path and query string, as sent.
    pub target: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl HttpRequest {
    pub fn path(&self) -> &str {
        self.target.split('?').next().unwrap_or("")
    }
}

/// Read one request; None if the client closed or sent something that isn't HTTP.
pub(crate) fn read_request(stream: &TcpStream) -> Option<HttpRequest> {
    let _ = stream.set_read_timeout(Some(READ_TIMEOUT));
    let mut r = BufReader::new(stream);
    let mut line = String::new();
    let mut head_len = 0;
    let mut next_line = |r: &mut BufReader<&TcpStream>, line: &mut String| -> Option<()> {
        line.clear();
        let n = r.by_ref().take((MAX_HEAD - head_len) as u64).read_line(line).ok()?;
        head_len += n;
        (n > 0 && line.ends_with('\n')).then_some(())
    };
    next_line(&mut r, &mut line)?;
    let mut parts = line.split_whitespace();
    let (method, target) = (parts.next()?.to_string(), parts.next()?.to_string());
    let mut headers = Vec::new();
    loop {
        next_line(&mut r, &mut line)?;
        let l = line.trim_end_matches(['\r', '\n']);
        if l.is_empty() {
            break;
        }
        let (k, v) = l.split_once(':')?;
        headers.push((k.trim().to_string(), v.trim().to_string()));
    }
    let len = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("Content-Length"))
        .and_then(|(_, v)| v.parse::<usize>().ok())
        .unwrap_or(0);
    let mut body = vec![0; len];
    r.read_exact(&mut body).ok()?;
    Some(HttpRequest { method, target, headers, body })
}

/// How the body goes out.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Delivery {
    All,
    /// Only this many bytes, then close.
    Cut(usize),
    /// This many bytes, a stall, then the rest.
    Stall(usize, Duration),
}

/// A response as it goes on the wire.
pub(crate) struct Wire {
    pub status: u16,
    pub content_type: &'static str,
    pub location: Option<String>,
    pub body: Arc<Vec<u8>>,
    /// Send `Content-Length` (otherwise closing the connection ends the body).
    pub content_length: bool,
    pub delivery: Delivery,
}

impl Wire {
    pub fn new(status: u16, content_type: &'static str, body: Arc<Vec<u8>>) -> Wire {
        Wire { status, content_type, location: None, body, content_length: true, delivery: Delivery::All }
    }
}

/// What to do with the connection.
pub(crate) enum Action {
    Send(Wire),
    /// Send nothing for this long, then close.
    Hold(Duration),
    /// TCP RST.
    Reset,
    /// Clean close without a byte.
    Close,
}

impl Action {
    /// Carry it out (`head_only` for HEAD requests). `stopping` cuts waits short.
    pub fn perform(self, mut stream: TcpStream, head_only: bool, stopping: &AtomicBool) {
        match self {
            Action::Send(w) => {
                let mut head =
                    format!("HTTP/1.1 {} {}\r\nContent-Type: {}\r\n", w.status, reason(w.status), w.content_type);
                if w.content_length {
                    head += &format!("Content-Length: {}\r\n", w.body.len());
                }
                if let Some(l) = &w.location {
                    head += &format!("Location: {l}\r\n");
                }
                head += "Connection: close\r\n\r\n";
                let body: &[u8] = if head_only { &[] } else { &w.body };
                let res = (|| {
                    stream.write_all(head.as_bytes())?;
                    match w.delivery {
                        Delivery::All => stream.write_all(body)?,
                        Delivery::Cut(n) => stream.write_all(&body[..n.min(body.len())])?,
                        Delivery::Stall(n, wait) => {
                            let n = n.min(body.len());
                            stream.write_all(&body[..n])?;
                            stream.flush()?;
                            sleep(wait, stopping);
                            stream.write_all(&body[n..])?;
                        }
                    }
                    stream.flush()
                })();
                if let Err(e) = res {
                    log::debug!("mock-trmnl: response cut short: {e}");
                }
                // Our FIN only after the peer has what we sent.
                let _ = stream.shutdown(std::net::Shutdown::Write);
            }
            Action::Hold(wait) => sleep(wait, stopping),
            Action::Reset => {
                // SO_LINGER 0: close() sends RST instead of FIN.
                let _ = socket2::SockRef::from(&stream).set_linger(Some(Duration::ZERO));
            }
            Action::Close => {}
        }
    }
}

/// Sleep `wait`, or until the server stops.
fn sleep(wait: Duration, stopping: &AtomicBool) {
    let end = Instant::now() + wait;
    while !stopping.load(Ordering::Relaxed) {
        let left = end.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        std::thread::sleep(left.min(Duration::from_millis(50)));
    }
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        204 => "No Content",
        301 => "Moved Permanently",
        302 => "Found",
        304 => "Not Modified",
        307 => "Temporary Redirect",
        308 => "Permanent Redirect",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        408 => "Request Timeout",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "Status",
    }
}
