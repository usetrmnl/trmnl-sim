//! `/mcp`: the control API as a Model Context Protocol server (Streamable HTTP transport,
//! JSON answers, no sessions), for AI agents:
//! `claude mcp add --transport http trmnl-sim http://127.0.0.1:7878/mcp`.
//!
//! Every [`ENDPOINTS`] row is a tool whose arguments are the row's parameters. A call is
//! turned into the HTTP request it stands for and goes through the HTTP router, so both
//! front ends behave identically; the answer comes back as JSON text (and structured
//! content), or as an image for PNG endpoints. Raw request bodies (reference screenshots,
//! mock server images and files) are given as `file` (a path on this machine) or
//! `data_base64`; PNG answers can also be written to `save_to`. An HTTP error status makes
//! the result an error, with the same JSON the HTTP client would get.

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use serde_json::{Map, Value, json};
use sim_api::SimHandle;
use tiny_http::{Header, Method, Response};

use crate::endpoints::{ENDPOINTS, Endpoint, Loc, Param, Ty};
use crate::{Reply, json_reply};

const PROTOCOL_VERSIONS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];

pub(crate) fn handle(
    h: &SimHandle,
    mock: Option<&mock_trmnl::MockServer>,
    method: &Method,
    origin: Option<&str>,
    body: &[u8],
) -> Reply {
    // The transport spec asks servers to refuse other sites' pages (DNS rebinding).
    if let Some(o) = origin
        && !is_local_origin(o)
    {
        return json_reply(403, rpc_error(Value::Null, -32600, format!("origin {o} not allowed")));
    }
    if *method != Method::Post {
        // No server-initiated stream (GET) and no sessions to end (DELETE).
        return Response::from_data(Vec::new())
            .with_status_code(405)
            .with_header(Header::from_bytes("Allow", "POST").unwrap());
    }
    let msg: Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(e) => return json_reply(400, rpc_error(Value::Null, -32700, format!("parse error: {e}"))),
    };
    let answer = match &msg {
        Value::Array(batch) => {
            let out: Vec<Value> = batch.iter().filter_map(|m| message(h, mock, m)).collect();
            (!out.is_empty()).then_some(Value::Array(out))
        }
        m => message(h, mock, m),
    };
    match answer {
        Some(v) => json_reply(200, v),
        // Only notifications (or responses): accepted, nothing to say.
        None => Response::from_data(Vec::new()).with_status_code(202),
    }
}

fn is_local_origin(o: &str) -> bool {
    let host = o.split_once("://").map_or(o, |(_, rest)| rest);
    let host = host.split('/').next().unwrap_or("");
    let host = host.strip_prefix('[').and_then(|h| h.split_once(']')).map_or_else(
        || host.rsplit_once(':').map_or(host, |(h, port)| if port.parse::<u16>().is_ok() { h } else { host }),
        |(h, _)| h,
    );
    matches!(host, "localhost" | "127.0.0.1" | "::1") || o == "null"
}

fn rpc_error(id: Value, code: i64, message: impl Into<String>) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message.into() } })
}

/// The answer to one JSON-RPC message; `None` for notifications and responses.
fn message(h: &SimHandle, mock: Option<&mock_trmnl::MockServer>, m: &Value) -> Option<Value> {
    let Some(method) = m["method"].as_str() else {
        // A response to a request of ours (we send none), or junk.
        return m.get("id").is_none().then(|| rpc_error(Value::Null, -32600, "invalid request"));
    };
    let id = m.get("id")?.clone();
    let params = &m["params"];
    let result = match method {
        "initialize" => {
            let asked = params["protocolVersion"].as_str().unwrap_or("");
            let version = PROTOCOL_VERSIONS.iter().find(|v| **v == asked).unwrap_or(&PROTOCOL_VERSIONS[0]);
            Ok(json!({
                "protocolVersion": version,
                "capabilities": { "tools": { "listChanged": false } },
                "serverInfo": { "name": "trmnl-sim", "version": env!("CARGO_PKG_VERSION") },
                "instructions": INSTRUCTIONS,
            }))
        }
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": ENDPOINTS.iter().map(tool_json).collect::<Vec<_>>() })),
        "tools/call" => match params["name"].as_str().and_then(|n| ENDPOINTS.iter().find(|e| e.tool == n)) {
            Some(e) => {
                let empty = Map::new();
                Ok(call(h, mock, e, params["arguments"].as_object().unwrap_or(&empty)))
            }
            None => Err((-32602, format!("unknown tool {}", params["name"]))),
        },
        _ => Err((-32601, format!("method {method} not supported"))),
    };
    Some(match result {
        Ok(r) => json!({ "jsonrpc": "2.0", "id": id, "result": r }),
        Err((code, msg)) => rpc_error(id, code, msg),
    })
}

const INSTRUCTIONS: &str = "Drives a running trmnl-sim: a TRMNL e-paper device emulated with its real firmware. \
Each tool is one endpoint of the simulator's HTTP control API (named in its description). \
Typical loop: status, then act (press, touch, wifi, wake, mock_display...), then wait for a console regex, \
state or refresh count, then screenshot. Times in ms arguments are virtual (emulated) time.";

fn tool_json(e: &Endpoint) -> Value {
    let mut props = Map::new();
    let mut required = Vec::new();
    for p in e.params {
        props.insert(p.name.into(), param_schema(p));
        if p.required {
            required.push(p.name);
        }
    }
    if let Some(what) = e.raw_body {
        props.insert(
            "file".into(),
            json!({ "type": "string", "description": format!("{what}: path of a file on the simulator's machine") }),
        );
        props.insert(
            "data_base64".into(),
            json!({ "type": "string", "description": format!("{what}: base64 (instead of file)") }),
        );
    }
    if e.png {
        props.insert("save_to".into(), json!({ "type": "string", "description": "also write the PNG to this path" }));
    }
    let mut schema = json!({ "type": "object", "properties": props, "additionalProperties": false });
    if !required.is_empty() {
        schema["required"] = json!(required);
    }
    json!({
        "name": e.tool,
        "title": e.tool.replace('_', " "),
        "description": format!("{} (HTTP: {} {})", e.doc, e.method, e.path),
        "inputSchema": schema,
        "annotations": { "readOnlyHint": e.method == Method::Get },
    })
}

fn param_schema(p: &Param) -> Value {
    let mut s = match p.ty {
        Ty::String => json!({ "type": "string" }),
        Ty::Integer => json!({ "type": "integer" }),
        Ty::Number => json!({ "type": "number" }),
        Ty::Boolean => json!({ "type": "boolean" }),
        Ty::Object => json!({ "type": "object" }),
        Ty::Array => json!({ "type": "array" }),
        Ty::Strings => json!({ "type": "array", "items": { "type": "string" } }),
        Ty::Bytes => json!({ "type": "array", "items": { "type": "integer", "minimum": 0, "maximum": 255 } }),
        Ty::Enum(values) => json!({ "type": "string", "enum": values }),
    };
    if p.nullable {
        s["type"] = json!([s["type"].clone(), "null"]);
        if let Some(values) = s.get_mut("enum").and_then(Value::as_array_mut) {
            values.push(Value::Null);
        }
    }
    s["description"] = json!(p.doc);
    s
}

/// Run a tool: build the HTTP request, route it, and turn the answer into a tool result.
fn call(h: &SimHandle, mock: Option<&mock_trmnl::MockServer>, e: &Endpoint, args: &Map<String, Value>) -> Value {
    let (path, query, body, save_to) = match request(e, args) {
        Ok(r) => r,
        Err(msg) => return tool_error(json!({ "ok": false, "error": msg })),
    };
    let reply = crate::dispatch(h, mock, &e.method, &path, &query, &body);
    let status = reply.status_code().0;
    let png = reply.headers().iter().any(|h| h.field.equiv("Content-Type") && h.value.as_str() == "image/png");
    let mut data = Vec::new();
    let _ = std::io::Read::read_to_end(&mut reply.into_reader(), &mut data);
    if png && status < 400 {
        let mut content = vec![json!({ "type": "image", "data": B64.encode(&data), "mimeType": "image/png" })];
        if let Some(path) = save_to {
            if let Err(err) = std::fs::write(&path, &data) {
                return tool_error(json!({ "ok": false, "error": format!("can't write {path}: {err}") }));
            }
            content.push(json!({ "type": "text", "text": format!("saved {} bytes to {path}", data.len()) }));
        }
        return json!({ "content": content, "isError": false });
    }
    let v: Value = serde_json::from_slice(&data).unwrap_or_else(|_| json!(String::from_utf8_lossy(&data)));
    let mut result = json!({
        "content": [{ "type": "text", "text": serde_json::to_string_pretty(&v).unwrap() }],
        "isError": status >= 400,
    });
    if v.is_object() {
        result["structuredContent"] = v;
    }
    result
}

fn tool_error(v: Value) -> Value {
    json!({ "content": [{ "type": "text", "text": v.to_string() }], "structuredContent": v, "isError": true })
}

type Request = (String, Vec<(String, String)>, Vec<u8>, Option<String>);

/// The HTTP request a tool call stands for: path, query, body, and where to save a PNG answer.
fn request(e: &Endpoint, args: &Map<String, Value>) -> Result<Request, String> {
    let mut path_value = String::new();
    let mut query = Vec::new();
    let mut body = Map::new();
    let mut whole: Option<Value> = None;
    let mut raw: Option<Vec<u8>> = None;
    let mut save_to = None;
    for (k, v) in args {
        let Some(p) = e.params.iter().find(|p| p.name == k) else {
            match k.as_str() {
                "file" if e.raw_body.is_some() => {
                    let f = v.as_str().ok_or("file: a path")?;
                    raw = Some(std::fs::read(f).map_err(|err| format!("can't read {f}: {err}"))?);
                }
                "data_base64" if e.raw_body.is_some() => {
                    let s = v.as_str().ok_or("data_base64: a string")?;
                    raw = Some(B64.decode(s.trim()).map_err(|err| format!("data_base64: {err}"))?);
                }
                "save_to" if e.png => save_to = Some(v.as_str().ok_or("save_to: a path")?.to_string()),
                _ => return Err(format!("unknown argument {k:?} for {}; expected {}", e.tool, arg_names(e))),
            }
            continue;
        };
        match p.loc {
            Loc::Path => path_value = scalar(k, v)?,
            Loc::Query if v.is_null() => {}
            Loc::Query => query.push((k.clone(), percent_encode(&scalar(k, v)?))),
            Loc::Body => {
                body.insert(k.clone(), v.clone());
            }
            Loc::WholeBody => whole = Some(v.clone()),
        }
    }
    if let Some(p) = e.params.iter().find(|p| p.required && !args.contains_key(p.name)) {
        return Err(format!("{} needs {:?}", e.tool, p.name));
    }
    let body = match (e.raw_body, raw, whole) {
        (Some(what), None, _) => return Err(format!("{} needs {what} as file or data_base64", e.tool)),
        (_, Some(raw), _) => raw,
        (_, None, Some(whole)) => serde_json::to_vec(&whole).unwrap(),
        (_, None, None) => serde_json::to_vec(&Value::Object(body)).unwrap(),
    };
    Ok((e.fill_path(&path_value), query, body, save_to))
}

fn arg_names(e: &Endpoint) -> String {
    let mut names: Vec<&str> = e.params.iter().map(|p| p.name).collect();
    if e.raw_body.is_some() {
        names.extend(["file", "data_base64"]);
    }
    if e.png {
        names.push("save_to");
    }
    if names.is_empty() { "none".into() } else { names.join(", ") }
}

/// A query or path value as the HTTP client would send it (booleans are `1`/`0`).
fn scalar(k: &str, v: &Value) -> Result<String, String> {
    match v {
        Value::String(s) => Ok(s.clone()),
        Value::Number(n) => Ok(n.to_string()),
        Value::Bool(b) => Ok(if *b { "1" } else { "0" }.into()),
        _ => Err(format!("{k}: a string, number or boolean")),
    }
}

/// Escape a query value so the router's unescaping gives it back unchanged.
fn percent_encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use sim_api::Frame;
    use std::sync::Arc;

    fn rpc(h: &SimHandle, mock: Option<&mock_trmnl::MockServer>, body: Value) -> (u16, Value) {
        let r = handle(h, mock, &Method::Post, None, body.to_string().as_bytes());
        let code = r.status_code().0;
        let mut data = Vec::new();
        std::io::Read::read_to_end(&mut r.into_reader(), &mut data).unwrap();
        (code, serde_json::from_slice(&data).unwrap_or(Value::Null))
    }

    fn call_tool(h: &SimHandle, mock: Option<&mock_trmnl::MockServer>, name: &str, args: Value) -> Value {
        let (code, v) = rpc(
            h,
            mock,
            json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": name, "arguments": args}}),
        );
        assert_eq!(code, 200);
        v["result"].clone()
    }

    #[test]
    fn initialize_and_list_every_endpoint() {
        let (h, _p) = sim_api::channel(Arc::new(Frame::new(4, 2).into()));
        let (code, v) = rpc(
            &h,
            None,
            json!({"jsonrpc": "2.0", "id": 7, "method": "initialize", "params": {"protocolVersion": "2025-03-26"}}),
        );
        assert_eq!(code, 200);
        assert_eq!(v["id"], 7);
        assert_eq!(v["result"]["protocolVersion"], "2025-03-26");
        let r = handle(&h, None, &Method::Post, None, br#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);
        assert_eq!(r.status_code().0, 202);
        let (_, v) = rpc(&h, None, json!({"jsonrpc": "2.0", "id": 8, "method": "tools/list"}));
        let tools = v["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), ENDPOINTS.len());
        let wait = tools.iter().find(|t| t["name"] == "wait").unwrap();
        assert_eq!(wait["inputSchema"]["properties"]["state"]["enum"][4], "deep_sleep");
        let (_, v) = rpc(&h, None, json!({"jsonrpc": "2.0", "id": 9, "method": "resources/list"}));
        assert_eq!(v["error"]["code"], -32601);
    }

    #[test]
    fn calls_go_through_the_http_router() {
        let (h, p) = sim_api::channel(Arc::new(Frame::new(4, 2).into()));
        let r = call_tool(&h, None, "status", json!({}));
        assert_eq!(r["isError"], false);
        assert_eq!(r["structuredContent"]["state"], "running");

        let r = call_tool(&h, None, "battery", json!({"mv": 3300}));
        assert_eq!(r["isError"], false);
        assert!(matches!(p.commands.try_recv(), Ok(sim_api::Command::SetBatteryMv(3300))));

        // Validation happens in the router, as for HTTP.
        let r = call_tool(&h, None, "touch", json!({"zone": "top"}));
        assert_eq!(r["isError"], true);
        let r = call_tool(&h, None, "battery", json!({"volts": 3}));
        assert_eq!(r["isError"], true);
        assert!(r["structuredContent"]["error"].as_str().unwrap().contains("unknown argument"));
        let r = call_tool(&h, None, "mock_state", json!({}));
        assert_eq!(r["isError"], true);
    }

    #[test]
    fn screenshots_are_images_and_compare_takes_base64() {
        let (h, _p) = sim_api::channel(Arc::new(Frame::new(4, 2).into()));
        let r = call_tool(&h, None, "screenshot", json!({"x": 0, "y": 0, "w": 2, "h": 2}));
        assert_eq!(r["content"][0]["type"], "image");
        let png = B64.decode(r["content"][0]["data"].as_str().unwrap()).unwrap();
        let r = call_tool(
            &h,
            None,
            "screenshot_compare",
            json!({"x": 0, "y": 0, "w": 2, "h": 2, "data_base64": B64.encode(&png)}),
        );
        assert_eq!(r["structuredContent"]["match"], true);
        let r = call_tool(&h, None, "screenshot_compare", json!({}));
        assert_eq!(r["isError"], true);
    }

    #[test]
    fn mock_paths_and_queries_round_trip() {
        let (h, _p) = sim_api::channel(Arc::new(Frame::new(4, 2).into()));
        let m = mock_trmnl::MockServer::new(mock_trmnl::Panel::Og);
        let r = call_tool(&h, Some(&m), "mock_add_file", json!({"path": "/a b+c.bin", "data_base64": "AAEC"}));
        assert_eq!(r["isError"], false);
        let r = call_tool(&h, Some(&m), "mock_state", json!({}));
        assert_eq!(r["structuredContent"]["files"][0], "/a b+c.bin");
        let r = call_tool(&h, Some(&m), "mock_expected_image", json!({"name": "missing"}));
        assert_eq!(r["isError"], true);
    }

    #[test]
    fn foreign_origins_and_gets_are_refused() {
        let (h, _p) = sim_api::channel(Arc::new(Frame::new(4, 2).into()));
        let ping = br#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#;
        assert_eq!(handle(&h, None, &Method::Post, Some("http://evil.example"), ping).status_code().0, 403);
        assert_eq!(handle(&h, None, &Method::Post, Some("http://localhost:3000"), ping).status_code().0, 200);
        assert_eq!(handle(&h, None, &Method::Post, Some("http://[::1]:3000"), ping).status_code().0, 200);
        assert_eq!(handle(&h, None, &Method::Get, None, b"").status_code().0, 405);
    }
}
