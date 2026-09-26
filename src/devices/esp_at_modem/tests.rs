//! Byte-stream tests in virtual time, written against what trmnl-firmware's `modem.cpp`,
//! `modem_scan_parse.cpp` and esp-serial-flasher actually do with the bytes.

use std::io::{Read, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::rom::{self, Slip};
use super::*;

// ---------------------------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------------------------

struct H {
    m: EspAtModem,
    now: u64,
}

impl H {
    fn new(cfg: ModemConfig) -> Self {
        H { m: EspAtModem::new(cfg), now: 0 }
    }

    fn power(&mut self, on: bool, strap: bool) {
        self.m.set_power(self.now, on, strap);
    }

    fn send(&mut self, b: &[u8]) {
        self.m.host_tx(self.now, b);
    }

    /// `ModemSerial.println(cmd)`.
    fn cmd(&mut self, s: &str) {
        self.send(format!("{s}\r\n").as_bytes());
    }

    /// Advance to the next thing that can happen, but not past `deadline`.
    fn step(&mut self, deadline: u64) {
        if self.m.busy() {
            // Waiting on real network I/O: pace virtual time to wall time like the simulator.
            std::thread::sleep(Duration::from_millis(1));
            self.now = (self.now + MS).min(deadline.max(self.now));
        } else {
            match self.m.next_event_ns() {
                Some(t) if t <= deadline => self.now = self.now.max(t),
                _ => self.now = deadline.max(self.now),
            }
        }
    }

    /// Everything the modem says within `ms` of virtual time.
    fn run_for(&mut self, ms: u64) -> Vec<u8> {
        let deadline = self.now + ms * MS;
        let mut acc = self.m.poll(self.now);
        while self.now < deadline {
            self.step(deadline);
            acc.extend(self.m.poll(self.now));
        }
        acc
    }

    /// `Modem::waitForResponse(marker, timeout)`: None on timeout, else everything read.
    fn wait_for(&mut self, marker: &str, timeout_ms: u64) -> Option<String> {
        let deadline = self.now + timeout_ms * MS;
        let mut acc = Vec::new();
        loop {
            acc.extend(self.m.poll(self.now));
            if find(&acc, marker.as_bytes()).is_some() {
                return Some(String::from_utf8_lossy(&acc).into_owned());
            }
            if self.now >= deadline {
                return None;
            }
            self.step(deadline);
        }
    }

    fn flush(&mut self) {
        self.m.poll(self.now);
    }

    /// The `Modem(115200)` constructor after `modem_reset_target()`.
    fn firmware_init(&mut self) {
        self.power(false, false);
        self.now += 50 * MS;
        self.power(true, false);
        self.now += 100 * MS; // delay(100)
        self.flush();
        self.cmd("AT");
        self.wait_for("OK", 5000).expect("AT");
        for c in ["AT+CWMODE=1", "AT+CWAUTOCONN=0", "AT+UART_CUR=5000000,8,1,0,3"] {
            self.cmd(c);
            self.wait_for("OK", 5000).unwrap_or_else(|| panic!("{c}"));
        }
        self.now += 100 * MS;
        self.flush();
        self.cmd("AT");
        self.wait_for("OK", 5000).expect("AT @5M");
    }

    /// `Modem::connectToNetwork()`.
    fn connect(&mut self, ssid: &str, pwd: &str, hostname: &str) -> bool {
        self.flush();
        self.cmd("AT+CWMODE=1");
        self.wait_for("OK", 3000).unwrap();
        if !hostname.is_empty() {
            self.cmd(&format!("AT+CWHOSTNAME=\"{}\"", esc(hostname)));
            self.wait_for("OK", 3000);
        }
        self.cmd(&format!("AT+CWJAP=\"{}\",\"{}\"", esc(ssid), esc(pwd)));
        self.wait_for("WIFI GOT IP", 20000).is_some()
    }

    /// `Modem::httpGet()` (the chunk-callback flavour's parser), returning (ok, body).
    fn http_get(&mut self, url: &str, headers: &str) -> (bool, Vec<u8>) {
        self.flush();
        if !headers.is_empty() {
            self.cmd("AT+HTTPCHEAD=0");
            assert!(self.wait_for("OK", 3000).is_some());
            for h in headers.split('\n').map(str::trim).filter(|h| !h.is_empty()) {
                self.cmd(&format!("AT+HTTPCHEAD={}", h.len()));
                assert!(self.wait_for(">", 3000).is_some(), "no > prompt");
                self.send(h.as_bytes());
                assert!(self.wait_for("OK", 3000).is_some(), "no OK for header");
            }
        }
        let param = esc(url);
        if param.len() + 24 > 256 {
            self.cmd(&format!("AT+HTTPURLCFG={}", url.len()));
            assert!(self.wait_for(">", 5000).is_some());
            self.send(url.as_bytes());
            assert!(self.wait_for("SET OK", 5000).is_some());
            self.flush();
            self.cmd("AT+HTTPCLIENT=2,1,\"\",,,2");
        } else {
            self.cmd(&format!("AT+HTTPCLIENT=2,1,\"{param}\",,,2"));
        }
        let mut p = FwHttpParser::default();
        let deadline = self.now + 60_000 * MS;
        let res = 'outer: loop {
            for b in self.m.poll(self.now) {
                if let Some(r) = p.feed(b) {
                    break 'outer Some(r);
                }
            }
            if self.now >= deadline {
                break None;
            }
            self.step(deadline);
        };
        if !headers.is_empty() {
            self.cmd("AT+HTTPCHEAD=0");
            self.wait_for("OK", 2000);
        }
        (res == Some(true) && !p.body.is_empty(), p.body)
    }
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// `escape_modem_param()`.
fn esc(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"").replace(',', "\\,")
}

/// The byte-by-byte SCAN/SIZE/DATA state machine from `Modem::httpGet()`.
#[derive(Default)]
struct FwHttpParser {
    state: u8, // 0 SCAN, 1 SIZE, 2 DATA
    match_pos: usize,
    tail: Vec<u8>,
    size: String,
    remaining: u32,
    body: Vec<u8>,
}

impl FwHttpParser {
    /// Some(true) = done ("\nOK\r"), Some(false) = "ERROR".
    fn feed(&mut self, b: u8) -> Option<bool> {
        const MARKER: &[u8] = b"+HTTPCLIENT:";
        match self.state {
            0 => {
                if self.tail.len() == 8 {
                    self.tail.remove(0);
                }
                self.tail.push(b);
                let n = self.tail.len();
                if n >= 4 && &self.tail[n - 4..n - 1] == b"\nOK" && (b == b'\r' || b == b'\n') {
                    return Some(true);
                }
                if n >= 5 && &self.tail[n - 5..] == b"ERROR" {
                    return Some(false);
                }
                if b == MARKER[self.match_pos] {
                    self.match_pos += 1;
                    if self.match_pos == MARKER.len() {
                        self.state = 1;
                        self.size.clear();
                        self.match_pos = 0;
                        self.tail.clear();
                    }
                } else {
                    self.match_pos = usize::from(b == MARKER[0]);
                }
            }
            1 => {
                if b == b',' {
                    self.remaining = self.size.parse().unwrap_or(0);
                    self.state = 2;
                } else if b.is_ascii_digit() {
                    self.size.push(b as char);
                }
            }
            _ => {
                self.body.push(b);
                self.remaining -= 1;
                if self.remaining == 0 {
                    self.state = 0;
                    self.tail.clear();
                    self.match_pos = 0;
                }
            }
        }
        None
    }
}

// Arduino String helpers for the parseCwlapResponse() port.
fn index_of(s: &str, pat: &str, from: i32) -> i32 {
    if from < 0 || from as usize > s.len() {
        return -1;
    }
    s[from as usize..].find(pat).map_or(-1, |i| i as i32 + from)
}

fn substring(s: &str, a: i32, b: i32) -> String {
    let (mut a, mut b) = (a.max(0) as usize, b.max(0) as usize);
    if a > b {
        std::mem::swap(&mut a, &mut b);
    }
    let b = b.min(s.len());
    if a >= b { String::new() } else { s[a..b].to_string() }
}

fn to_int(s: &str) -> i32 {
    let s = s.trim_start();
    let (neg, digits) = match s.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, s.strip_prefix('+').unwrap_or(s)),
    };
    let v: i32 = digits.chars().take_while(char::is_ascii_digit).fold(0, |a, c| a * 10 + (c as i32 - 48));
    if neg { -v } else { v }
}

#[derive(Debug, PartialEq)]
struct FwNet {
    ssid: String,
    rssi: i32,
    open: bool,
    is5: bool,
    enterprise: bool,
}

/// Port of `parseCwlapResponse()` (lib/trmnl/src/modem_scan_parse.cpp).
fn parse_cwlap(raw: &str) -> Vec<FwNet> {
    let mut results: Vec<FwNet> = Vec::new();
    let mut pos = 0;
    loop {
        pos = index_of(raw, "+CWLAP:", pos);
        if pos < 0 {
            break;
        }
        let paren = index_of(raw, "(", pos);
        let close = index_of(raw, ")", paren);
        if paren < 0 || close < 0 {
            pos += 1;
            continue;
        }
        let entry = substring(raw, paren + 1, close);
        let c1 = index_of(&entry, ",", 0);
        let ecn = to_int(&substring(&entry, 0, c1));
        let q1 = index_of(&entry, "\"", c1);
        let q2 = index_of(&entry, "\"", q1 + 1);
        let ssid = substring(&entry, q1 + 1, q2);
        let c2 = index_of(&entry, ",", q2 + 1);
        let c3 = index_of(&entry, ",", c2 + 1);
        let rssi = to_int(&substring(&entry, c2 + 1, c3));
        let q3 = index_of(&entry, "\"", c3);
        let q4 = index_of(&entry, "\"", q3 + 1);
        let c4 = index_of(&entry, ",", q4 + 1);
        let c5 = index_of(&entry, ",", c4 + 1);
        let channel = to_int(&substring(&entry, c4 + 1, if c5 >= 0 { c5 } else { entry.len() as i32 }));
        if ssid.is_empty() || ssid == "TRMNL" {
            pos = close;
            continue;
        }
        let is5 = channel >= 36;
        if let Some(n) = results.iter_mut().find(|n| n.ssid == ssid && n.is5 == is5) {
            n.rssi = n.rssi.max(rssi);
        } else {
            results.push(FwNet { ssid, rssi, open: ecn == 0, is5, enterprise: ecn == 5 });
        }
        pos = close;
    }
    results
}

fn ap(ssid: &str, pwd: Option<&str>, rssi: i8, channel: u8, ecn: u8, last: u8) -> ModemAp {
    ModemAp {
        ssid: ssid.into(),
        password: pwd.map(Into::into),
        rssi,
        channel,
        ecn,
        bssid: [0x24, 0x0a, 0xc4, 0x00, 0x00, last],
    }
}

fn home_cfg() -> ModemConfig {
    ModemConfig {
        mac: [0xdc, 0x54, 0x75, 0xaa, 0xbb, 0xcc],
        networks: vec![
            ap("Home", Some("hunter22"), -48, 36, 3, 1),
            ap("Home", Some("hunter22"), -61, 6, 3, 2),
            ap("Home", Some("hunter22"), -70, 149, 3, 3),
            ap("Cafe", None, -75, 11, 0, 4),
            ap("Corp", None, -80, 44, 5, 5),
            ap("TRMNL", None, -30, 1, 0, 6),
            ap("my \"net\",\\x", Some("p,w\"d"), -55, 40, 3, 7),
        ],
        ..ModemConfig::default()
    }
}

fn connected(cfg: ModemConfig) -> H {
    let mut h = H::new(cfg);
    h.firmware_init();
    assert!(h.connect("Home", "hunter22", "TRMNL-X"));
    h
}

// ---------------------------------------------------------------------------------------------
// AT basics
// ---------------------------------------------------------------------------------------------

#[test]
fn boot_prints_ready_and_buffers_early_input() {
    let mut h = H::new(ModemConfig::default());
    h.power(true, false);
    assert!(h.m.poll(h.now).is_empty(), "nothing at t=0");
    // The firmware sends AT 100 ms after releasing EN, long before ESP-AT is up.
    h.now = 100 * MS;
    let early = h.m.poll(h.now);
    assert!(String::from_utf8_lossy(&early).contains("ESP-ROM:esp32c5"), "ROM banner");
    h.cmd("AT");
    assert!(h.m.poll(h.now).is_empty(), "no answer while booting");
    let r = h.wait_for("OK", 5000).unwrap();
    assert!(h.now >= 300 * MS && h.now < 320 * MS, "answered right after boot, t={}", h.now);
    let ready = r.find("\r\nready\r\n").expect("ready");
    let echo = r.find("AT\r\n").expect("echo");
    assert!(ready < echo && echo < r.find("\r\nOK\r\n").unwrap(), "{r:?}");
    assert_eq!(h.m.stats().boots, 1);
}

#[test]
fn firmware_init_sequence() {
    let mut h = H::new(ModemConfig::default());
    h.firmware_init();
    assert_eq!(h.m.uart_baud(), 5_000_000);
    assert!(h.now < 700 * MS, "init took {} ms", h.now / MS);
    // Echo is on and every command gets its own OK.
    h.cmd("AT+CWMODE?");
    assert_eq!(h.wait_for("OK", 100).unwrap(), "AT+CWMODE?\r\n+CWMODE:1\r\n\r\nOK\r\n");
}

#[test]
fn mac_and_unknown_and_garbage() {
    let mut h = H::new(home_cfg());
    h.firmware_init();
    h.cmd("AT+CIPSTAMAC?");
    let r = h.wait_for("OK", 3000).unwrap();
    // getMacAddress() parsing.
    let i = r.find("+CIPSTAMAC:\"").unwrap() + 12;
    assert_eq!(&r[i..i + r[i..].find('"').unwrap()], "dc:54:75:aa:bb:cc");

    h.cmd("AT+BOGUS=1");
    assert!(h.wait_for("ERROR", 100).is_some());
    assert_eq!(h.m.stats().last_unknown_command.as_deref(), Some("AT+BOGUS=1"));

    // Boot-banner style junk without "AT" is ignored, and junk before AT on a line is skipped.
    h.send(b"\x00\xffESP-ROM:esp32s3-20210327\r\nrst:0x1 (POWERON)\r\n");
    assert!(!String::from_utf8_lossy(&h.run_for(50)).contains("ERROR"));
    h.send(b"\xfe\xfeAT\r\n");
    assert!(h.wait_for("OK", 100).is_some());
}

// ---------------------------------------------------------------------------------------------
// Scan / join
// ---------------------------------------------------------------------------------------------

#[test]
fn cwlap_parses_back_through_firmware_parser() {
    let mut h = H::new(home_cfg());
    h.firmware_init();
    h.flush();
    let t0 = h.now;
    h.cmd("AT+CWLAP");
    // scanNetworks(): wait for "\nOK" or "ERROR", 15 s.
    let mut acc = String::new();
    while !acc.contains("\nOK") && !acc.contains("ERROR") {
        assert!(h.now - t0 < 15_000 * MS);
        h.step(t0 + 15_000 * MS);
        acc.push_str(&String::from_utf8_lossy(&h.m.poll(h.now)));
    }
    let dt = (h.now - t0) / MS;
    assert!((1900..2500).contains(&dt), "scan took {dt} ms");
    assert!(acc.contains("+CWLAP:(3,\"Home\",-48,\"24:0a:c4:00:00:01\",36,"), "{acc}");
    let nets = parse_cwlap(&acc);
    let expect = [
        ("Home", -48, false, true, false),
        ("Home", -61, false, false, false),
        ("Cafe", -75, true, false, false),
        ("Corp", -80, false, true, true),
        ("my \"net\",\\x", 0, false, false, false), // raw quote in SSID confuses the parser, as on hardware
    ];
    assert_eq!(nets.len(), expect.len(), "{nets:?}");
    for (n, (ssid, rssi, open, is5, ent)) in nets.iter().zip(expect).take(4) {
        assert_eq!(*n, FwNet { ssid: ssid.into(), rssi, open, is5, enterprise: ent });
    }
    assert_eq!(h.m.stats().scans, 1);

    // Out of range: just OK.
    h.m.set_wifi_available(false);
    h.cmd("AT+CWLAP");
    let r = h.wait_for("\nOK", 15000).unwrap();
    assert!(!r.contains("+CWLAP:"));
}

#[test]
fn cwjap_success_and_query() {
    let mut h = H::new(home_cfg());
    h.firmware_init();
    let t0 = h.now;
    assert!(h.connect("Home", "hunter22", "TRMNL-X"));
    let dt = (h.now - t0) / MS;
    assert!((1500..1600).contains(&dt), "join took {dt} ms");
    assert_eq!(h.m.connected_ssid().as_deref(), Some("Home"));
    assert_eq!(h.m.stats().hostname.as_deref(), Some("TRMNL-X"));

    // getSignalRssi(): strongest "Home" is the 5 GHz one.
    h.flush();
    h.cmd("AT+CWJAP?");
    let r = h.wait_for("OK", 3000).unwrap();
    assert!(r.contains("+CWJAP:\"Home\",\"24:0a:c4:00:00:01\",36,-48,"), "{r}");

    // Escaped SSID / password round trip.
    assert!(h.connect("my \"net\",\\x", "p,w\"d", ""));
    assert_eq!(h.m.connected_ssid().as_deref(), Some("my \"net\",\\x"));
}

#[test]
fn cwjap_failures() {
    let mut h = H::new(home_cfg());
    h.firmware_init();
    // Wrong password: +CWJAP:2 / FAIL, host times out waiting for WIFI GOT IP.
    h.flush();
    h.cmd("AT+CWJAP=\"Home\",\"wrong\"");
    let r = String::from_utf8_lossy(&h.run_for(20_000)).into_owned();
    assert!(r.contains("+CWJAP:2\r\n\r\nFAIL\r\n") && !r.contains("WIFI GOT IP"), "{r}");
    assert_eq!(h.m.connected_ssid(), None);

    // Unknown SSID.
    assert!(!h.connect("Nope", "x", ""));
    // Busy while joining.
    h.cmd("AT+CWJAP=\"Home\",\"hunter22\"");
    h.now += 100 * MS;
    h.cmd("AT");
    assert!(h.wait_for("busy p...", 100).is_some());
    assert!(h.wait_for("WIFI GOT IP", 20000).is_some());
    assert_eq!(h.m.stats().busy_rejections, 1);

    // Networks out of range.
    h.m.set_wifi_available(false);
    assert_eq!(h.m.connected_ssid(), None);
    h.flush();
    h.cmd("AT+CWJAP=\"Home\",\"hunter22\"");
    let r = h.wait_for("FAIL", 20000).unwrap();
    assert!(r.contains("+CWJAP:3"));

    // AP vanishes mid-join.
    h.m.set_wifi_available(true);
    h.run_for(5000);
    h.flush();
    h.cmd("AT+CWJAP=\"Cafe\",\"\"");
    h.now += 500 * MS;
    h.m.poll(h.now);
    h.m.set_wifi_available(false);
    let r = h.wait_for("FAIL", 20000).unwrap();
    assert!(!r.contains("WIFI GOT IP"), "{r}");
}

#[test]
fn disconnect_reconnect_and_cwqap() {
    let mut h = connected(home_cfg());
    h.flush();
    h.m.set_wifi_available(false);
    assert!(h.wait_for("WIFI DISCONNECT", 10).is_some());
    h.cmd("AT+CWJAP?");
    assert!(h.wait_for("OK", 100).unwrap().contains("No AP"));
    h.m.set_wifi_available(true);
    let r = h.wait_for("WIFI GOT IP", 10_000).unwrap();
    assert!(r.contains("WIFI CONNECTED"));
    assert_eq!(h.m.connected_ssid().as_deref(), Some("Home"));

    h.cmd("AT+CWQAP");
    let r = h.wait_for("WIFI DISCONNECT", 5000).unwrap();
    assert!(r.find("OK").unwrap() < r.find("WIFI DISCONNECT").unwrap());
    h.m.set_wifi_available(false);
    h.m.set_wifi_available(true);
    assert!(!String::from_utf8_lossy(&h.run_for(10_000)).contains("WIFI"), "no reconnect after CWQAP");
}

// ---------------------------------------------------------------------------------------------
// SNTP
// ---------------------------------------------------------------------------------------------

#[test]
fn sntp_time_updated_and_time() {
    let mut h = H::new(home_cfg());
    h.firmware_init();
    // Configured before joining: syncs only once connected.
    h.cmd("AT+CIPSNTPCFG=1,0,\"time.google.com\",\"time.cloudflare.com\"");
    assert!(h.wait_for("OK", 3000).is_some());
    assert!(!String::from_utf8_lossy(&h.run_for(3000)).contains("+TIME_UPDATED"));
    assert!(h.connect("Home", "hunter22", ""));
    let t0 = h.now;
    assert!(h.wait_for("+TIME_UPDATED", 30_000).is_some());
    assert!((900..1100).contains(&((h.now - t0) / MS)));

    // getSntpTime() again while connected (the usual path).
    h.flush();
    h.cmd("AT+CIPSNTPCFG=1,0,\"time.google.com\",\"time.cloudflare.com\"");
    assert!(h.wait_for("OK", 3000).is_some());
    assert!(h.wait_for("+TIME_UPDATED", 30_000).is_some());
    h.flush();
    h.cmd("AT+CIPSNTPTIME?");
    let r = h.wait_for("OK", 3000).unwrap();
    let i = r.find("+CIPSNTPTIME:").unwrap() + 13;
    let line = r[i..].lines().next().unwrap().trim();
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64;
    let ok = (now - 2..=now + 2).any(|t| asctime(t) == line);
    assert!(ok, "{line:?} vs {:?}", asctime(now));
    assert_eq!(h.m.stats().sntp_syncs, 2);
}

// ---------------------------------------------------------------------------------------------
// HTTP
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct SeenReq {
    path: String,
    headers: Vec<(String, String)>,
}

struct TestServer {
    port: u16,
    seen: Arc<Mutex<Vec<SeenReq>>>,
}

fn big_body(n: usize) -> Vec<u8> {
    (0..n).map(|i| (i.wrapping_mul(31) ^ (i >> 9)) as u8).collect()
}

fn serve(mut s: TcpStream, port: u16, seen: Arc<Mutex<Vec<SeenReq>>>) {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 1024];
    while find(&buf, b"\r\n\r\n").is_none() {
        match s.read(&mut tmp) {
            Ok(0) | Err(_) => return,
            Ok(n) => buf.extend_from_slice(&tmp[..n]),
        }
    }
    let head = String::from_utf8_lossy(&buf[..find(&buf, b"\r\n\r\n").unwrap()]).into_owned();
    let mut lines = head.split("\r\n");
    let path = lines.next().unwrap().split(' ').nth(1).unwrap().to_string();
    let headers = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    seen.lock().unwrap().push(SeenReq { path: path.clone(), headers });
    let reply = |s: &mut TcpStream, status: &str, extra: &str, body: &[u8]| {
        let _ = write!(s, "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n{extra}\r\n", body.len());
        let _ = s.write_all(body);
    };
    match path.as_str() {
        "/big" => reply(&mut s, "200 OK", "Content-Type: application/octet-stream\r\n", &big_body(300_000)),
        "/chunked" => {
            let body = big_body(100_000);
            let _ = write!(s, "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n");
            for c in body.chunks(4093) {
                let _ = write!(s, "{:x}\r\n", c.len());
                let _ = s.write_all(c);
                let _ = s.write_all(b"\r\n");
                let _ = s.flush();
            }
            let _ = s.write_all(b"0\r\n\r\n");
        }
        "/redirect" => reply(&mut s, "302 Found", &format!("Location: http://10.0.2.2:{port}/small\r\n"), b""),
        "/slow" => {
            std::thread::sleep(Duration::from_millis(150));
            reply(&mut s, "200 OK", "", b"finally");
        }
        "/404" => reply(&mut s, "404 Not Found", "", b"no such thing"),
        p if p.starts_with("/long/") => reply(&mut s, "200 OK", "", p.as_bytes()),
        _ => reply(&mut s, "200 OK", "", b"hello world"),
    }
}

fn start_server() -> TestServer {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let s2 = seen.clone();
    std::thread::spawn(move || {
        for s in l.incoming().flatten() {
            let seen = s2.clone();
            std::thread::spawn(move || serve(s, port, seen));
        }
    });
    TestServer { port, seen }
}

#[test]
fn http_get_with_headers_and_large_body() {
    let srv = start_server();
    let mut h = connected(home_cfg());
    let url = format!("http://10.0.2.2:{}/big", srv.port);
    let (ok, body) = h.http_get(&url, "ID: DC:54:75:AA:BB:CC\nAccess-Token: abc123\nFW-Version: 1.8.14");
    assert!(ok);
    assert_eq!(body.len(), 300_000);
    assert!(body == big_body(300_000), "body mismatch");
    let seen = srv.seen.lock().unwrap().last().unwrap().clone();
    let get = |k: &str| seen.headers.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
    assert_eq!(seen.path, "/big");
    assert_eq!(get("id"), Some("DC:54:75:AA:BB:CC"));
    assert_eq!(get("access-token"), Some("abc123"));
    assert_eq!(get("fw-version"), Some("1.8.14"));
    assert_eq!(get("user-agent"), Some("ESP32 HTTP Client/1.0"));
    assert_eq!(get("host"), Some(format!("10.0.2.2:{}", srv.port).as_str()));
    let st = h.m.stats();
    assert_eq!((st.http_requests, st.http_errors, st.http_bytes), (1, 0, 300_000));
    assert_eq!(st.last_http_status, Some(200));
    assert_eq!(st.last_request_headers.len(), 3);
    assert!(!h.m.busy());

    // Headers were cleared afterwards: the next request carries none.
    let (ok, body) = h.http_get(&format!("http://10.0.2.2:{}/small", srv.port), "");
    assert!(ok && body == b"hello world");
    let seen = srv.seen.lock().unwrap().last().unwrap().clone();
    assert!(!seen.headers.iter().any(|(k, _)| k == "access-token"));
}

#[test]
fn http_chunk_framing_is_raw() {
    // Chunks are "+HTTPCLIENT:<n>,<n raw bytes>\r\n" and at most 2048 bytes.
    let srv = start_server();
    let mut h = connected(home_cfg());
    h.flush();
    h.cmd(&format!("AT+HTTPCLIENT=2,1,\"http://10.0.2.2:{}/chunked\",,,2", srv.port));
    let mut raw = Vec::new();
    let deadline = h.now + 30_000 * MS;
    while !raw.ends_with(b"\r\nOK\r\n") {
        assert!(h.now < deadline);
        h.step(deadline);
        raw.extend(h.m.poll(h.now));
    }
    let mut i = find(&raw, b"+HTTPCLIENT:").unwrap();
    let mut body = Vec::new();
    while raw[i..].starts_with(b"+HTTPCLIENT:") {
        let comma = i + raw[i..].iter().position(|&b| b == b',').unwrap();
        let n: usize = std::str::from_utf8(&raw[i + 12..comma]).unwrap().parse().unwrap();
        assert!(n > 0 && n <= 2048);
        body.extend_from_slice(&raw[comma + 1..comma + 1 + n]);
        i = comma + 1 + n;
        assert_eq!(&raw[i..i + 2], b"\r\n");
        i += 2;
    }
    assert_eq!(&raw[i..], b"\r\nOK\r\n");
    assert!(body == big_body(100_000));
}

#[test]
fn http_long_url_via_urlcfg_and_redirect() {
    let srv = start_server();
    let mut h = connected(home_cfg());
    let path = format!("/long/{}", "x".repeat(300));
    let url = format!("http://10.0.2.2:{}{path}", srv.port);
    let (ok, body) = h.http_get(&url, "");
    assert!(ok);
    assert_eq!(body, path.as_bytes());
    assert_eq!(h.m.stats().last_url, Some(url));

    let (ok, body) = h.http_get(&format!("http://10.0.2.2:{}/redirect", srv.port), "");
    assert!(ok && body == b"hello world");
    let seen = srv.seen.lock().unwrap();
    assert_eq!(seen[seen.len() - 1].path, "/small");
    assert_eq!(seen[seen.len() - 2].path, "/redirect");
}

#[test]
fn http_404_is_error() {
    let srv = start_server();
    let mut h = connected(home_cfg());
    let (ok, body) = h.http_get(&format!("http://10.0.2.2:{}/404", srv.port), "");
    assert!(!ok);
    assert_eq!(body, b"no such thing", "error body is still relayed, like ESP-AT");
    let st = h.m.stats();
    assert_eq!((st.last_http_status, st.http_errors), (Some(404), 1));
    // Modem is usable afterwards.
    h.cmd("AT");
    assert!(h.wait_for("OK", 100).is_some());
}

#[test]
fn http_offline_and_dns_overrides() {
    let srv = start_server();
    let mut cfg = home_cfg();
    cfg.offline = true;
    cfg.dns_overrides = vec![("api.trmnl.test".into(), Ipv4Addr::new(10, 0, 2, 2))];
    let mut h = connected(cfg);
    for url in ["http://example.com/", "https://usetrmnl.com/api/display", "http://93.184.216.34/", "http://127.0.0.1/"]
    {
        let (ok, _) = h.http_get(url, "");
        assert!(!ok, "{url} should be refused offline");
        assert_eq!(h.m.stats().last_http_status, None);
    }
    let (ok, body) = h.http_get(&format!("http://api.trmnl.test:{}/small", srv.port), "");
    assert!(ok && body == b"hello world");
    let seen = srv.seen.lock().unwrap().last().unwrap().clone();
    let host = seen.headers.iter().find(|(k, _)| k == "host").unwrap().1.clone();
    assert_eq!(host, format!("api.trmnl.test:{}", srv.port));
}

#[test]
fn http_requires_connection_and_reports_busy() {
    let srv = start_server();
    let mut h = H::new(home_cfg());
    h.firmware_init();
    let (ok, _) = h.http_get(&format!("http://10.0.2.2:{}/small", srv.port), "");
    assert!(!ok, "not connected");
    assert!(h.connect("Home", "hunter22", ""));
    h.flush();
    h.cmd(&format!("AT+HTTPCLIENT=2,1,\"http://10.0.2.2:{}/slow\",,,2", srv.port));
    h.now += 3 * MS;
    h.m.poll(h.now);
    assert!(h.m.busy());
    assert_eq!(h.m.next_event_ns(), None, "waiting on the network only");
    h.cmd("AT");
    let r = h.wait_for("\r\nOK\r\n", 10_000).unwrap();
    assert!(r.contains("busy p...") && r.contains("+HTTPCLIENT:7,finally"), "{r}");
    assert!(!h.m.busy());
}

// ---------------------------------------------------------------------------------------------
// Power
// ---------------------------------------------------------------------------------------------

#[test]
fn power_cycle_loses_state() {
    let srv = start_server();
    let mut h = connected(home_cfg());
    h.cmd("AT+HTTPCHEAD=5");
    h.wait_for(">", 100).unwrap();
    h.send(b"A: bc");
    h.wait_for("OK", 100).unwrap();
    // Power off mid-request: silent, input ignored, request cancelled.
    h.cmd(&format!("AT+HTTPCLIENT=2,1,\"http://10.0.2.2:{}/slow\",,,2", srv.port));
    h.now += 2 * MS;
    h.flush();
    h.power(false, false);
    assert!(!h.m.busy() && !h.m.powered());
    assert_eq!(h.m.connected_ssid(), None);
    h.cmd("AT");
    assert!(h.run_for(1000).is_empty(), "silent while off");
    assert_eq!(h.m.next_event_ns(), None);

    h.power(true, false);
    assert!(h.wait_for("ready", 1000).is_some());
    h.cmd("AT+CWJAP?");
    assert!(h.wait_for("OK", 100).unwrap().contains("No AP"));
    h.cmd("AT+HTTPCHEAD?");
    assert!(!h.wait_for("OK", 100).unwrap().contains("+HTTPCHEAD:"));
    assert_eq!(h.m.uart_baud(), 115_200);
    assert_eq!(h.m.stats().boots, 2);
    // Stays powered when EN is re-asserted.
    h.power(true, true);
    assert!(!h.m.in_download_mode());

    // AT+RST reboots too.
    h.cmd("AT+RST");
    assert!(h.wait_for("ready", 1000).is_some());
    assert_eq!(h.m.stats().boots, 3);
}

// ---------------------------------------------------------------------------------------------
// ROM download mode
// ---------------------------------------------------------------------------------------------

fn rom_req(cmd: u8, data: &[u8], chk: u32) -> Vec<u8> {
    let mut p = vec![0x00, cmd];
    p.extend_from_slice(&(data.len() as u16).to_le_bytes());
    p.extend_from_slice(&chk.to_le_bytes());
    p.extend_from_slice(data);
    rom::slip_encode(&p)
}

fn words(w: &[u32]) -> Vec<u8> {
    w.iter().flat_map(|x| x.to_le_bytes()).collect()
}

/// A decoded response the way esp-serial-flasher's `check_response()` sees it.
#[derive(Debug)]
struct Resp {
    cmd: u8,
    value: u32,
    data: Vec<u8>,
    failed: u8,
    error: u8,
}

impl H {
    /// Send one request and collect `n` responses (like send_cmd()).
    fn rom(&mut self, cmd: u8, data: &[u8], chk: u32, resp_data_size: usize, n: usize) -> Vec<Resp> {
        self.send(&rom_req(cmd, data, chk));
        let mut slip = Slip::default();
        let mut frames = Vec::new();
        let deadline = self.now + 5_000 * MS;
        while frames.len() < n {
            assert!(self.now < deadline, "timeout waiting for response to {cmd:#x}");
            self.step(deadline);
            frames.extend(slip.feed(&self.m.poll(self.now)));
        }
        frames
            .into_iter()
            .map(|f| {
                assert_eq!(f[0], 0x01);
                // SLIP_receive_packet truncates at header + status(2) + resp_data_size.
                let len = f.len().min(8 + 2 + resp_data_size);
                let st = &f[len - 2..len];
                Resp {
                    cmd: f[1],
                    value: u32::from_le_bytes(f[4..8].try_into().unwrap()),
                    data: f[8..len - 2].to_vec(),
                    failed: st[0],
                    error: st[1],
                }
            })
            .collect()
    }

    fn read_reg(&mut self, addr: u32) -> u32 {
        let r = self.rom(rom::READ_REG, &words(&[addr]), 0, 0, 1);
        assert_eq!(r[0].failed, 0);
        r[0].value
    }

    fn write_reg(&mut self, addr: u32, v: u32) {
        let r = self.rom(rom::WRITE_REG, &words(&[addr, v, 0xffff_ffff, 0]), 0, 0, 1);
        assert_eq!(r[0].failed, 0);
    }

    /// modem_enter_bootloader(): EN low 100 ms, straps, EN high, 300 ms.
    fn enter_bootloader(&mut self) {
        self.power(false, false);
        self.now += 150 * MS;
        self.power(true, true);
        self.now += 300 * MS;
    }
}

#[test]
fn rom_loader_flash_sequence() {
    let mut h = H::new(home_cfg());
    h.firmware_init();
    let data_before = h.m.stats().rom_packets;
    assert_eq!(data_before, 0);
    h.enter_bootloader();
    assert!(h.m.in_download_mode());
    let banner = String::from_utf8_lossy(&h.m.poll(h.now)).into_owned();
    assert!(banner.contains("waiting for download"), "{banner}");
    let mut sync = vec![0x07, 0x07, 0x12, 0x20];
    sync.extend_from_slice(&[0x55; 32]);

    // SYNC -> 8 responses.
    let r = h.rom(rom::SYNC, &sync, 0, 0, 8);
    assert_eq!(r.len(), 8);
    assert!(r.iter().all(|r| r.cmd == rom::SYNC && r.failed == 0));
    assert!(h.run_for(20).is_empty(), "exactly 8");

    // loader_detect_chip(): GET_SECURITY_INFO, 20 bytes, chip id 23.
    let r = h.rom(rom::GET_SECURITY_INFO, &[], 0, 20, 1);
    assert_eq!((r[0].failed, r[0].data.len()), (0, 20));
    assert_eq!(u32::from_le_bytes(r[0].data[12..16].try_into().unwrap()), 23);
    assert_eq!(h.read_reg(rom::CHIP_DETECT_MAGIC_REG), 0x1101_406f);

    // SPI_ATTACH (connect), then esp_loader_flash_detect_size()'s READ_ID dance.
    let r = h.rom(rom::SPI_ATTACH, &words(&[0, 0]), 0, 0, 1);
    assert_eq!(r[0].failed, 0);
    let (usr, usr2) = (h.read_reg(rom::SPI_BASE + 0x18), h.read_reg(rom::SPI_USR2));
    h.write_reg(rom::SPI_BASE + 0x28, 23); // miso_dlen
    h.write_reg(rom::SPI_BASE + 0x18, 1 << 31 | 1 << 28);
    h.write_reg(rom::SPI_USR2, 7 << 28 | 0x9f);
    h.write_reg(rom::SPI_W0, 0);
    h.write_reg(rom::SPI_CMD, 1 << 18);
    assert_eq!(h.read_reg(rom::SPI_CMD) & (1 << 18), 0, "USR bit self-clears");
    let id = h.read_reg(rom::SPI_W0);
    assert_eq!((id >> 16) & 0xff, 0x16);
    h.write_reg(rom::SPI_BASE + 0x18, usr);
    h.write_reg(rom::SPI_USR2, usr2);
    // eFuse MAC as loader_read_mac() assembles it.
    let (p1, p2) = (h.read_reg(rom::EFUSE_MAC0), h.read_reg(rom::EFUSE_MAC0 + 4));
    assert_eq!(
        [(p2 >> 8) as u8, p2 as u8, (p1 >> 24) as u8, (p1 >> 16) as u8, (p1 >> 8) as u8, p1 as u8],
        home_cfg().mac
    );

    let r = h.rom(rom::SPI_SET_PARAMS, &words(&[0, 4 << 20, 64 << 10, 4 << 10, 256, 0xffff]), 0, 0, 1);
    assert_eq!(r[0].failed, 0);

    // FLASH_BEGIN (with the C5's encryption word) + 1024 x 1 KiB FLASH_DATA + FLASH_END.
    const BLK: usize = 1024;
    const N: usize = 1024;
    let image = big_body(BLK * N);
    let t0 = h.now;
    let r = h.rom(rom::FLASH_BEGIN, &words(&[(BLK * N) as u32, N as u32, BLK as u32, 0x1000, 0]), 0, 0, 1);
    assert_eq!(r[0].failed, 0);
    let erase_ms = (h.now - t0) / MS;
    assert!((2000..3000).contains(&erase_ms), "1 MiB erase took {erase_ms} ms");
    for (seq, blk) in image.chunks(BLK).enumerate() {
        let mut d = words(&[BLK as u32, seq as u32, 0, 0]);
        d.extend_from_slice(blk);
        if seq == 500 {
            // Corrupted checksum: INVALID_CRC, nothing written; the flasher retries.
            let r = h.rom(rom::FLASH_DATA, &d, rom::checksum(blk) ^ 0x01, 0, 1);
            assert_eq!((r[0].failed, r[0].error), (1, rom::ERR_INVALID_CRC));
        }
        let r = h.rom(rom::FLASH_DATA, &d, rom::checksum(blk), 0, 1);
        assert_eq!((r[0].failed, r[0].error), (0, 0), "block {seq}");
    }
    let r = h.rom(rom::FLASH_END, &words(&[0]), 0, 0, 1); // reboot = true
    assert_eq!(r[0].failed, 0);
    let flash = h.m.flash_image().unwrap();
    assert_eq!(flash.len(), 4 << 20);
    assert!(flash[0x1000..0x1000 + BLK * N] == image[..]);
    assert!(flash[..0x1000].iter().all(|&b| b == 0xff));
    let st = h.m.stats();
    assert_eq!((st.flash_data_packets, st.rom_checksum_errors, st.flash_ends), (N as u64, 1, 1));
    assert_eq!(st.flash_bytes_written, (BLK * N) as u64);

    // FLASH_END(run) boots the AT firmware; then modem_reset_target() power cycles anyway.
    assert!(h.wait_for("ready", 1000).is_some());
    assert!(!h.m.in_download_mode());
    h.power(false, false);
    h.now += 100 * MS;
    h.power(true, false);
    h.cmd("AT");
    assert!(h.wait_for("OK", 1000).is_some());
    assert!(h.m.flash_image().is_some(), "flash survives power cycles");
}

#[test]
fn rom_loader_edge_cases() {
    let mut h = H::new(ModemConfig { flash_size_id: 0x17, ..ModemConfig::default() });
    h.power(true, true);
    // Too early: the ROM isn't listening yet, the flasher's SYNC just times out.
    h.send(&rom_req(rom::SYNC, &[0x07, 0x07, 0x12, 0x20], 0));
    h.run_for(100);
    assert_eq!(h.m.stats().rom_packets, 0);
    // Text/garbage between frames is ignored; unknown command -> INVALID_COMMAND.
    h.send(b"garbage\r\n");
    let r = h.rom(0xd0, &[], 0, 0, 1); // ERASE_FLASH is stub-only
    assert_eq!((r[0].failed, r[0].error), (1, rom::ERR_INVALID_COMMAND));
    // FLASH_DATA without FLASH_BEGIN.
    let mut d = words(&[4, 0, 0, 0]);
    d.extend_from_slice(&[1, 2, 3, 4]);
    let r = h.rom(rom::FLASH_DATA, &d, rom::checksum(&[1, 2, 3, 4]), 0, 1);
    assert_eq!((r[0].failed, r[0].error), (1, rom::ERR_COMMAND_FAILED));
    // Escaped bytes survive SLIP both ways; 8 MB flash id.
    h.write_reg(0x600c_0000, 0xc0db_c0db);
    assert_eq!(h.read_reg(0x600c_0000), 0xc0db_c0db);
    h.write_reg(rom::SPI_USR2, 0x9f);
    h.write_reg(rom::SPI_CMD, 1 << 18);
    assert_eq!(h.read_reg(rom::SPI_W0) >> 16, 0x17);
    // AT commands mean nothing here.
    h.cmd("AT");
    assert!(!String::from_utf8_lossy(&h.run_for(100)).contains("OK"));
    // FLASH_END(stay in loader) doesn't reboot.
    h.rom(rom::FLASH_END, &words(&[1]), 0, 0, 1);
    h.run_for(1000);
    assert!(h.m.in_download_mode());
}
