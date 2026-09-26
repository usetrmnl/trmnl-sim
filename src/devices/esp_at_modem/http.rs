//! The modem's HTTP(S) client: real requests made from the machine running the simulator, on
//! a worker thread so `EspAtModem::poll()` never blocks.
//!
//! Addressing follows the rest of the simulator (see the `vnet` crate): `dns_overrides` first,
//! then `10.0.2.2` means host `127.0.0.1`. Other addresses in 10.0.2.0/24 and loopback are
//! unreachable; in `offline` mode everything except 10.0.2.2 is (unknown names don't resolve).

use std::io::Read;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4, ToSocketAddrs};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::time::Duration;

use ureq::config::Config;
use ureq::http::Uri;
use ureq::tls::TlsConfig;
use ureq::unversioned::resolver::{ResolvedSocketAddrs, Resolver};
use ureq::unversioned::transport::{DefaultConnector, NextTimeout};

/// ESP-AT's HTTP client (esp_http_client) default User-Agent.
const USER_AGENT: &str = "ESP32 HTTP Client/1.0";
/// Largest `+HTTPCLIENT:<n>,` chunk we emit.
pub(super) const CHUNK: usize = 2048;
const HOST_GATEWAY: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 2);

#[derive(Clone, Debug)]
pub(super) struct HttpJob {
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub head_only: bool,
    pub offline: bool,
    pub dns_overrides: Vec<(String, Ipv4Addr)>,
}

pub(super) enum HttpMsg {
    /// Final status line (after redirects).
    Status(u16),
    Data(Vec<u8>),
    /// Ok = 2xx and body complete; Err = anything else (ESP-AT then answers ERROR).
    Done(Result<(), String>),
}

/// Start the request. Dropping the receiver cancels it (the worker exits at its next send).
pub(super) fn spawn(job: HttpJob) -> Receiver<HttpMsg> {
    // Bounded, so a slow guest applies back-pressure instead of buffering a whole OTA image.
    let (tx, rx) = sync_channel(64);
    std::thread::Builder::new()
        .name("esp-at-http".into())
        .spawn(move || {
            let res = run(&job, &tx);
            if let Err(e) = &res {
                log::debug!("esp-at modem: GET {} failed: {e}", job.url);
            }
            let _ = tx.send(HttpMsg::Done(res));
        })
        .expect("spawn esp-at http worker");
    rx
}

fn run(job: &HttpJob, tx: &SyncSender<HttpMsg>) -> Result<(), String> {
    let uri: Uri = job.url.parse().map_err(|e| format!("bad url: {e}"))?;
    // ESP-AT does not verify server certificates unless a CA is configured (it isn't, on the
    // TRMNL X). We keep verification for the real internet, but a local dev server reached
    // through 10.0.2.2 usually has a self-signed certificate, so skip it there.
    let local = uri
        .host()
        .and_then(|h| lookup_override(h, &job.dns_overrides).or_else(|| h.parse().ok()))
        .is_some_and(|ip| ip == HOST_GATEWAY);
    let config = Config::builder()
        .http_status_as_error(false)
        .max_redirects(10)
        .user_agent(USER_AGENT)
        .proxy(None)
        .timeout_connect(Some(Duration::from_secs(10)))
        .timeout_recv_response(Some(Duration::from_secs(30)))
        .tls_config(TlsConfig::builder().disable_verification(local).build())
        .build();
    let resolver = SimResolver { overrides: job.dns_overrides.clone(), offline: job.offline };
    let agent = ureq::Agent::with_parts(config, DefaultConnector::default(), resolver);

    let resp = if job.head_only {
        let mut r = agent.head(&job.url);
        for (k, v) in &job.headers {
            r = r.header(k, v);
        }
        r.call()
    } else {
        let mut r = agent.get(&job.url);
        for (k, v) in &job.headers {
            r = r.header(k, v);
        }
        r.call()
    }
    .map_err(|e| e.to_string())?;

    let status = resp.status().as_u16();
    tx.send(HttpMsg::Status(status)).map_err(|_| "cancelled")?;
    // Like ESP-AT, the body is printed whatever the status; the final result code reflects it.
    let mut reader = resp.into_body().into_reader();
    loop {
        let mut buf = vec![0u8; CHUNK];
        let n = reader.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        buf.truncate(n);
        tx.send(HttpMsg::Data(buf)).map_err(|_| "cancelled")?;
    }
    if (200..300).contains(&status) { Ok(()) } else { Err(format!("HTTP status {status}")) }
}

fn lookup_override(host: &str, overrides: &[(String, Ipv4Addr)]) -> Option<Ipv4Addr> {
    let host = host.trim_end_matches('.');
    overrides.iter().find(|(h, _)| h.trim_end_matches('.').eq_ignore_ascii_case(host)).map(|(_, ip)| *ip)
}

/// Device-visible IPv4 destination -> host destination (None = unreachable).
pub(super) fn host_target(ip: Ipv4Addr, offline: bool) -> Option<Ipv4Addr> {
    if ip == HOST_GATEWAY {
        Some(Ipv4Addr::LOCALHOST)
    } else if offline
        || (u32::from(ip) & 0xffff_ff00) == u32::from(Ipv4Addr::new(10, 0, 2, 0))
        || ip.is_loopback()
        || ip.is_unspecified()
        || ip.is_broadcast()
        || ip.is_multicast()
    {
        None
    } else {
        Some(ip)
    }
}

#[derive(Debug)]
struct SimResolver {
    overrides: Vec<(String, Ipv4Addr)>,
    offline: bool,
}

impl Resolver for SimResolver {
    fn resolve(&self, uri: &Uri, _config: &Config, _timeout: NextTimeout) -> Result<ResolvedSocketAddrs, ureq::Error> {
        let host = uri.host().ok_or(ureq::Error::HostNotFound)?;
        let port = uri.port_u16().unwrap_or(if uri.scheme_str() == Some("https") { 443 } else { 80 });
        let host = host.trim_start_matches('[').trim_end_matches(']');
        let candidates: Vec<Ipv4Addr> = if let Some(ip) = lookup_override(host, &self.overrides) {
            vec![ip]
        } else if let Ok(ip) = host.parse::<Ipv4Addr>() {
            vec![ip]
        } else if self.offline || host.parse::<IpAddr>().is_ok() {
            // Offline: no DNS. IPv6 literals: the modem's lwIP config is IPv4 only.
            return Err(ureq::Error::HostNotFound);
        } else {
            (host, port)
                .to_socket_addrs()
                .map_err(|_| ureq::Error::HostNotFound)?
                .filter_map(|a| match a {
                    SocketAddr::V4(v4) => Some(*v4.ip()),
                    SocketAddr::V6(_) => None,
                })
                .collect()
        };
        if candidates.is_empty() {
            return Err(ureq::Error::HostNotFound);
        }
        let mut out = self.empty();
        for ip in candidates.into_iter().filter_map(|ip| host_target(ip, self.offline)).take(16) {
            out.push(SocketAddr::V4(SocketAddrV4::new(ip, port)));
        }
        if out.is_empty() { Err(ureq::Error::ConnectionFailed) } else { Ok(out) }
    }
}
