//! `/mock/...`: the built-in mock TRMNL server (crate `mock-trmnl`), for tests that don't
//! want to run their own.
//!
//! | method | path                        | body / query                                   | result |
//! |--------|-----------------------------|------------------------------------------------|--------|
//! | GET    | `/mock`                     |                                                | server state |
//! | POST   | `/mock/start`               | `{"port": 0}` (0 = any free port)              | `{"port", "device_url", "host_url"}` |
//! | POST   | `/mock/stop`                |                                                | |
//! | POST   | `/mock/images`              | image file (PNG/JPEG/BMP/GIF); `?name=&current=1&dither=0&fit=contain\|cover\|stretch&raw=1` | image |
//! | GET    | `/mock/images/NAME/expected`|                                                | PNG the screen should show |
//! | DELETE | `/mock/images/NAME`         |                                                | |
//! | POST   | `/mock/display`             | `{"image", "refresh_rate", "special_function", "playlist", "auto_advance", "registered", "friendly_id", "api_key", "extra"}` (all optional) | server state |
//! | POST   | `/mock/queue`               | raw `/api/display` fields for the next answer only (plus `image`) | |
//! | DELETE | `/mock/queue`               |                                                | |
//! | POST   | `/mock/faults`              | `{"display": ["503:2", "timeout"], "image": ["truncate:1"]}`: HTTP and connection failures appended to each route's queue, in the firmware's `scripts/mock_server.py` syntax `KIND[=ARG][:COUNT]` (see `mock_trmnl::fault`) | `{"display": [...], "image": [...]}` |
//! | DELETE | `/mock/faults`              | `?route=display\|image` (default both)          | |
//! | POST   | `/mock/files`               | file body; `?path=/firmware.bin`               | `{"url"}` |
//! | GET    | `/mock/requests`            | `?since=N`                                     | `{"total", "requests": [...]}` |

use std::time::UNIX_EPOCH;

use mock_trmnl::{ConvertOptions, Fit, HttpFault, MockServer, Route, State};
use serde_json::{Value, json};
use tiny_http::{Header, Method, Response};

use crate::{Reply, body_json, err, json_reply, qget};

pub(crate) fn route(
    m: &MockServer,
    method: &Method,
    path: &str,
    q: &[(String, String)],
    body: &[u8],
) -> Result<Reply, String> {
    let ok = || Ok(json_reply(200, json!({ "ok": true })));
    let rest = path.trim_start_matches("/mock").trim_start_matches('/');
    match (method, rest) {
        (Method::Get, "") => Ok(json_reply(200, state_json(m))),
        (Method::Post, "start") => {
            let port = body_json(body)?["port"].as_u64().unwrap_or(0) as u16;
            let bound = m.start(port).map_err(|e| format!("can't start the mock server on port {port}: {e}"))?;
            Ok(json_reply(
                200,
                json!({ "ok": true, "port": bound.port(), "device_url": m.device_url(), "host_url": m.host_url() }),
            ))
        }
        (Method::Post, "stop") => {
            m.stop();
            ok()
        }
        (Method::Post, "images") => {
            let name = unescape(&qget::<String>(q, "name").ok_or("need ?name=")?);
            let img = if qget(q, "raw").unwrap_or(0) == 1 {
                m.add_raw_image(&name, body)?
            } else {
                let fit = match qget::<String>(q, "fit") {
                    Some(f) => Fit::parse(&f).ok_or("fit must be contain, cover or stretch")?,
                    None => Fit::default(),
                };
                m.add_image(&name, body, ConvertOptions { dither: qget(q, "dither").unwrap_or(1) != 0, fit })?
            };
            if qget(q, "current").unwrap_or(0) == 1 {
                m.state().set_current(&img.name)?;
            }
            Ok(json_reply(200, image_json(m, &img)))
        }
        (Method::Get, r) if r.starts_with("images/") && r.ends_with("/expected") => {
            let name = &r["images/".len()..r.len() - "/expected".len()];
            let preview = m.state().image(name).map(|i| i.preview.clone());
            match preview {
                Some(p) => Ok(Response::from_data(p.to_png())
                    .with_header(Header::from_bytes("Content-Type", "image/png").unwrap())),
                None => Ok(err(404, format!("no image named {name:?}"))),
            }
        }
        (Method::Delete, r) if r.starts_with("images/") => {
            m.state().remove_image(&r["images/".len()..]);
            ok()
        }
        (Method::Post, "display") => {
            let b = body_json(body)?;
            let mut st = m.state();
            if let Some(name) = b["image"].as_str() {
                st.set_current(name)?;
            }
            if let Some(r) = b["refresh_rate"].as_u64() {
                st.refresh_rate = r as u32;
            }
            if let Some(sf) = b["special_function"].as_str() {
                if !mock_trmnl::SPECIAL_FUNCTIONS.contains(&sf) {
                    return Err(format!("unknown special_function {sf}"));
                }
                st.special_function = sf.to_string();
            }
            if let Some(list) = b["playlist"].as_array() {
                st.playlist = list.iter().filter_map(|v| v.as_str().map(str::to_string)).collect();
            }
            if let Some(a) = b["auto_advance"].as_bool() {
                st.auto_advance = a;
            }
            if let Some(r) = b["registered"].as_bool() {
                st.registered = r;
            }
            if let Some(f) = b["friendly_id"].as_str() {
                st.friendly_id = f.to_string();
            }
            if let Some(k) = b["api_key"].as_str() {
                st.api_key = k.to_string();
            }
            if let Some(extra) = b["extra"].as_object() {
                for (k, v) in extra {
                    if v.is_null() {
                        st.extra.remove(k);
                    } else {
                        st.extra.insert(k.clone(), v.clone());
                    }
                }
            }
            st.version += 1;
            drop(st);
            Ok(json_reply(200, state_json(m)))
        }
        (Method::Post, "queue") => {
            let fields = body_json(body)?.as_object().cloned().ok_or("need a JSON object")?;
            m.state().enqueue(fields);
            ok()
        }
        (Method::Delete, "queue") => {
            m.state().queue.clear();
            ok()
        }
        (Method::Post, "faults") => {
            let b = body_json(body)?;
            let obj = b.as_object().ok_or("need a JSON object")?;
            // Parse everything first: a bad spec adds nothing.
            let mut add = Vec::new();
            for (k, v) in obj {
                let route = Route::parse(k).ok_or_else(|| format!("unknown route {k:?} (display, image)"))?;
                let specs = v.as_array().ok_or_else(|| format!("{k}: a list of fault specs"))?;
                for spec in specs {
                    let spec = spec.as_str().ok_or_else(|| format!("{k}: fault specs are strings"))?;
                    let (fault, count) = HttpFault::parse(spec, route)?;
                    add.push((route, fault, count));
                }
            }
            let mut st = m.state();
            for (route, fault, count) in add {
                st.add_fault(route, fault, count)?;
            }
            Ok(json_reply(200, faults_json(&st)))
        }
        (Method::Delete, "faults") => {
            let mut st = m.state();
            match qget::<String>(q, "route") {
                None => st.clear_faults(),
                Some(r) => {
                    let route = Route::parse(&r).ok_or_else(|| format!("unknown route {r:?} (display, image)"))?;
                    st.faults_mut(route).clear();
                    st.version += 1;
                }
            }
            ok()
        }
        (Method::Post, "files") => {
            let path = unescape(&qget::<String>(q, "path").ok_or("need ?path=")?);
            let url = m.set_file(&path, mock_trmnl::FileSource::Bytes(body.to_vec().into()));
            Ok(json_reply(200, json!({ "ok": true, "url": url })))
        }
        (Method::Get, "requests") => {
            let since = qget(q, "since").unwrap_or(0u64);
            let st = m.state();
            let reqs: Vec<Value> = st.requests_since(since).map(request_json).collect();
            Ok(json_reply(200, json!({ "total": st.total_requests, "requests": reqs })))
        }
        _ => Ok(err(404, format!("no route {method} {path}"))),
    }
}

/// Decode `%XX` escapes and `+` in a query value.
fn unescape(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => {
                match u8::from_str_radix(std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or("?"), 16) {
                    Ok(v) => {
                        out.push(v);
                        i += 3;
                        continue;
                    }
                    Err(_) => out.push(b'%'),
                }
            }
            b'+' => out.push(b' '),
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn image_json(m: &MockServer, img: &mock_trmnl::Image) -> Value {
    json!({
        "name": img.name,
        "filename": img.filename,
        "path": img.path(),
        "url": m.device_url().map(|u| format!("{u}{}", img.path())),
        "bytes": img.data.len(),
        "width": img.preview.width,
        "height": img.preview.height,
    })
}

fn state_json(m: &MockServer) -> Value {
    let images = m.state().images.clone();
    let images: Vec<Value> = images.iter().map(|i| image_json(m, i)).collect();
    let st = m.state();
    json!({
        "running": st.port.is_some(),
        "port": st.port,
        "device_url": st.device_url(),
        "panel": st.panel.name(),
        "images": images,
        "current": st.current,
        "playlist": st.playlist,
        "auto_advance": st.auto_advance,
        "refresh_rate": st.refresh_rate,
        "special_function": st.special_function,
        "registered": st.registered,
        "friendly_id": st.friendly_id,
        "api_key": st.api_key,
        "extra": st.extra,
        "queue": st.queue.iter().cloned().collect::<Vec<_>>(),
        "faults": faults_json(&st),
        "files": st.files.keys().collect::<Vec<_>>(),
        "total_requests": st.total_requests,
    })
}

/// Each route's fault queue as specs (`KIND[=ARG][:COUNT]`, the count being what is left).
fn faults_json(st: &State) -> Value {
    let specs = |r| st.faults(r).iter().map(|f| f.to_string()).collect::<Vec<_>>();
    json!({ "display": specs(Route::Display), "image": specs(Route::Image) })
}

fn request_json(r: &mock_trmnl::Request) -> Value {
    let headers: serde_json::Map<String, Value> = r.headers.iter().map(|(k, v)| (k.clone(), json!(v))).collect();
    json!({
        "i": r.index,
        "method": r.method,
        "path": r.path,
        "headers": headers,
        "body": String::from_utf8_lossy(&r.body[..r.body.len().min(64 * 1024)]),
        "at": r.at.duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0),
        "sim_time_s": r.sim_time_ns.map(|n| n as f64 / 1e9),
        "status": r.status,
        "summary": r.summary,
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn unescape_query_values() {
        assert_eq!(super::unescape("%2Ffirmware.bin"), "/firmware.bin");
        assert_eq!(super::unescape("a+b%20c%"), "a b c%");
        assert_eq!(super::unescape("%zz"), "%zz");
    }
}
