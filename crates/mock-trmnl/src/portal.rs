//! Onboarding through the device's captive portal, like a phone would (what
//! `Simulator.portal_connect` does in the Python client).

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant};

/// POST `{"ssid", "pswd", "server"}` to `<portal_url>/connect` (e.g. the simulator's
/// `http://127.0.0.1:8080/`). Connection failures are retried for up to 30 s, since the
/// device may still be starting its access point. Returns the response body.
pub fn portal_connect(portal_url: &str, ssid: &str, password: &str, server: &str) -> Result<String, String> {
    let rest = portal_url.strip_prefix("http://").ok_or_else(|| format!("not an http:// URL: {portal_url}"))?;
    let host = rest.split('/').next().unwrap_or(rest);
    let body = serde_json::json!({ "ssid": ssid, "pswd": password, "server": server }).to_string();
    let request = format!(
        "POST /connect HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match exchange(host, &request) {
            Ok(resp) => return parse(&resp),
            Err(e) if Instant::now() > deadline => return Err(format!("portal at {portal_url}: {e}")),
            Err(_) => std::thread::sleep(Duration::from_millis(500)),
        }
    }
}

fn exchange(host: &str, request: &str) -> std::io::Result<Vec<u8>> {
    let addr = host.to_socket_addrs()?.next().ok_or_else(|| std::io::Error::other("no address"))?;
    let mut s = TcpStream::connect_timeout(&addr, Duration::from_secs(5))?;
    s.set_read_timeout(Some(Duration::from_secs(30)))?;
    s.write_all(request.as_bytes())?;
    let mut resp = Vec::new();
    s.read_to_end(&mut resp)?;
    if resp.is_empty() {
        return Err(std::io::Error::other("empty response"));
    }
    Ok(resp)
}

fn parse(resp: &[u8]) -> Result<String, String> {
    let text = String::from_utf8_lossy(resp);
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let status = head.split_whitespace().nth(1).unwrap_or("?");
    if status != "200" {
        return Err(format!("portal answered {status}: {}", body.chars().take(200).collect::<String>()));
    }
    Ok(body.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn posts_credentials_as_json() {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/", l.local_addr().unwrap());
        let t = std::thread::spawn(move || {
            let (mut c, _) = l.accept().unwrap();
            let mut buf = vec![0; 4096];
            let n = c.read(&mut buf).unwrap();
            c.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}").unwrap();
            String::from_utf8_lossy(&buf[..n]).to_string()
        });
        assert_eq!(portal_connect(&url, "TRMNL-Sim", "pw", "http://10.0.2.2:8090").unwrap(), "{}");
        let req = t.join().unwrap();
        assert!(req.starts_with("POST /connect HTTP/1.1\r\n"), "{req}");
        let body = req.split("\r\n\r\n").nth(1).unwrap();
        let v: serde_json::Value = serde_json::from_str(body).unwrap();
        assert_eq!(v["ssid"], "TRMNL-Sim");
        assert_eq!(v["server"], "http://10.0.2.2:8090");
    }
}
