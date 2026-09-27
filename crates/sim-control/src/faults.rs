//! JSON form of [`Faults`] for `GET/POST/DELETE /faults` and the `--faults` CLI flag.
//!
//! ```json
//! {"net": {"latency_ms": 300, "loss": 0.1, "bandwidth_bps": 20000,
//!          "dns": "servfail" | "nxdomain" | "empty" | "timeout",
//!          "no_internet": true, "offline": true,
//!          "tcp_cut": {"after_bytes": 10000, "stall": false, "port": 8080}},
//!  "power_loss": {"op": "any" | "program" | "erase", "partition": "nvs",
//!                 "range": [36864, "0xe000"], "nth": 1, "cut": "before" | "torn" | "after"},
//!  "i2c_absent": [85, "0x55"],
//!  "panel_busy_stuck": true,
//!  "modem_unresponsive": true,
//!  "modem_at_errors": ["AT+CWMODE", "AT+HTTPCHEAD"],
//!  "touch_bar": "reset" | "lockup" | "ati_error",
//!  "gauge_reset": true,
//!  "chip_temp_c": 60}
//! ```
//!
//! A POSTed object is merged into the current faults: keys that are left out keep their
//! value, `null` resets one (inside `net` too). Unknown keys are errors.

use serde_json::{Map, Value, json};
use sim_api::{CutPoint, DnsFault, Faults, FlashOp, NetFaults, PartitionInfo, PowerLoss, TcpCut, TouchBarFault};

/// Parse a `--faults` argument (a JSON object, as for `POST /faults`).
pub fn parse_faults(s: &str) -> Result<Faults, String> {
    merge_faults_str(&Faults::default(), s)
}

/// `base` with the faults in the JSON text `s` applied on top.
pub fn merge_faults_str(base: &Faults, s: &str) -> Result<Faults, String> {
    let v: Value = serde_json::from_str(s).map_err(|e| format!("bad faults JSON: {e}"))?;
    merge_faults(base, &v)
}

/// `base` with the faults in `v` applied on top.
pub fn merge_faults(base: &Faults, v: &Value) -> Result<Faults, String> {
    let obj = object(v, "faults")?;
    let mut f = base.clone();
    for (k, v) in obj {
        match k.as_str() {
            "net" if v.is_null() => f.net = NetFaults::default(),
            "net" => f.net = merge_net(&f.net, v)?,
            "power_loss" => f.power_loss = if v.is_null() { None } else { Some(power_loss(v)?) },
            "i2c_absent" => f.i2c_absent = if v.is_null() { Vec::new() } else { i2c_addrs(v)? },
            "panel_busy_stuck" => f.panel_busy_stuck = flag(v, k)?,
            "modem_unresponsive" => f.modem_unresponsive = flag(v, k)?,
            "modem_at_errors" => {
                f.modem_at_errors = match v {
                    Value::Null => Vec::new(),
                    Value::Array(a) => a
                        .iter()
                        .map(|p| p.as_str().map(String::from).ok_or("modem_at_errors: strings"))
                        .collect::<Result<_, _>>()?,
                    _ => return Err("modem_at_errors: a list of command prefixes".into()),
                }
            }
            "touch_bar" => {
                f.touch_bar = match v {
                    Value::Null => None,
                    Value::String(s) => {
                        Some(TouchBarFault::parse(s).ok_or("touch_bar: \"reset\", \"lockup\" or \"ati_error\"")?)
                    }
                    _ => return Err("touch_bar: a string".into()),
                }
            }
            "gauge_reset" => f.gauge_reset = flag(v, k)?,
            "chip_temp_c" => {
                f.chip_temp_c = match v {
                    Value::Null => None,
                    _ => Some(v.as_f64().ok_or("chip_temp_c: a number (°C)")? as f32),
                }
            }
            _ => return Err(format!("unknown fault {k:?}")),
        }
    }
    Ok(f)
}

fn merge_net(base: &NetFaults, v: &Value) -> Result<NetFaults, String> {
    let mut n = base.clone();
    for (k, v) in object(v, "net")? {
        let null = v.is_null();
        match k.as_str() {
            "latency_ms" => n.latency_ms = if null { 0 } else { uint(v, k)? as u32 },
            "loss" => {
                n.loss = if null { 0.0 } else { v.as_f64().filter(|l| (0.0..=1.0).contains(l)).ok_or("loss: 0..1")? }
            }
            "bandwidth_bps" => n.bandwidth_bps = if null { None } else { Some(uint(v, k)?.max(1)) },
            "dns" => {
                n.dns = match v {
                    Value::Null => None,
                    Value::String(s) => Some(DnsFault::parse(s).ok_or_else(|| {
                        let names: Vec<_> = DnsFault::ALL.iter().map(|d| d.name()).collect();
                        format!("dns: one of {}", names.join(", "))
                    })?),
                    _ => return Err("dns: a string".into()),
                }
            }
            "no_internet" => n.no_internet = flag(v, k)?,
            "offline" => n.offline = flag(v, k)?,
            "tcp_cut" => n.tcp_cut = if null { None } else { Some(tcp_cut(v)?) },
            _ => return Err(format!("unknown network fault {k:?}")),
        }
    }
    Ok(n)
}

fn tcp_cut(v: &Value) -> Result<TcpCut, String> {
    let mut c = TcpCut { after_bytes: 0, stall: false, port: None };
    for (k, v) in object(v, "tcp_cut")? {
        match k.as_str() {
            "after_bytes" => c.after_bytes = uint(v, k)?,
            "stall" => c.stall = flag(v, k)?,
            "port" => c.port = if v.is_null() { None } else { Some(uint(v, k)? as u16) },
            _ => return Err(format!("unknown tcp_cut field {k:?}")),
        }
    }
    Ok(c)
}

fn power_loss(v: &Value) -> Result<PowerLoss, String> {
    let mut p = PowerLoss::default();
    for (k, v) in object(v, "power_loss")? {
        match k.as_str() {
            "op" => p.op = v.as_str().and_then(FlashOp::parse).ok_or("op: any, program or erase")?,
            "partition" => p.partition = v.as_str().map(str::to_string),
            "range" => {
                p.range = match v.as_array().map(Vec::as_slice) {
                    None if v.is_null() => None,
                    Some([a, b]) => Some((uint(a, "range")? as u32, uint(b, "range")? as u32)),
                    _ => return Err("range: [start, end]".into()),
                }
            }
            "nth" => p.nth = uint(v, k)?.max(1) as u32,
            "cut" => p.cut = v.as_str().and_then(CutPoint::parse).ok_or("cut: before, torn or after")?,
            _ => return Err(format!("unknown power_loss field {k:?}")),
        }
    }
    Ok(p)
}

fn i2c_addrs(v: &Value) -> Result<Vec<u8>, String> {
    let list = v.as_array().ok_or("i2c_absent: a list of 7-bit addresses")?;
    list.iter()
        .map(|a| {
            uint(a, "i2c_absent").and_then(|a| u8::try_from(a).ok().filter(|&a| a < 0x80).ok_or("bad address".into()))
        })
        .collect()
}

fn object<'a>(v: &'a Value, what: &str) -> Result<&'a Map<String, Value>, String> {
    v.as_object().ok_or_else(|| format!("{what}: expected a JSON object"))
}

fn flag(v: &Value, k: &str) -> Result<bool, String> {
    if v.is_null() { Ok(false) } else { v.as_bool().ok_or_else(|| format!("{k}: true or false")) }
}

/// A non-negative integer, as a number or a string like "0x9000".
fn uint(v: &Value, k: &str) -> Result<u64, String> {
    let parsed = match v {
        Value::String(s) => match s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
            Some(hex) => u64::from_str_radix(hex, 16).ok(),
            None => s.parse().ok(),
        },
        _ => v.as_u64(),
    };
    parsed.ok_or_else(|| format!("{k}: expected a non-negative integer"))
}

/// The JSON form of `f` (every key present, `null`/false when off).
pub fn faults_json(f: &Faults) -> Value {
    let n = &f.net;
    json!({
        "net": {
            "latency_ms": n.latency_ms,
            "loss": n.loss,
            "bandwidth_bps": n.bandwidth_bps,
            "dns": n.dns.map(|d| d.name()),
            "no_internet": n.no_internet,
            "offline": n.offline,
            "tcp_cut": n.tcp_cut.map(|c| json!({"after_bytes": c.after_bytes, "stall": c.stall, "port": c.port})),
        },
        "power_loss": f.power_loss.as_ref().map(|p| json!({
            "op": p.op.name(),
            "partition": p.partition,
            "range": p.range.map(|(a, b)| [a, b]),
            "nth": p.nth,
            "cut": p.cut.name(),
        })),
        "i2c_absent": f.i2c_absent,
        "panel_busy_stuck": f.panel_busy_stuck,
        "modem_unresponsive": f.modem_unresponsive,
        "modem_at_errors": f.modem_at_errors,
        "touch_bar": f.touch_bar.map(|t| t.name()),
        "gauge_reset": f.gauge_reset,
        "chip_temp_c": f.chip_temp_c,
    })
}

/// Check what can be checked before the emulator sees the faults (partition names).
pub fn validate(f: &Faults, parts: &[PartitionInfo]) -> Result<(), String> {
    if let Some(name) = f.power_loss.as_ref().and_then(|p| p.partition.as_ref())
        && !parts.is_empty()
        && PartitionInfo::find(parts, name).is_none()
    {
        let names: Vec<&str> = parts.iter().map(|p| p.label.as_str()).collect();
        return Err(format!("no partition named {name:?} (the table has {})", names.join(", ")));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_and_round_trip() {
        let f = parse_faults(r#"{"net": {"latency_ms": 300, "dns": "servfail"}, "i2c_absent": ["0x55"]}"#).unwrap();
        assert_eq!(f.net.latency_ms, 300);
        assert_eq!(f.net.dns, Some(DnsFault::ServFail));
        assert_eq!(f.i2c_absent, vec![0x55]);
        // Merging keeps what isn't mentioned; null resets.
        let g = merge_faults(&f, &json!({"net": {"dns": null, "loss": 0.5}, "panel_busy_stuck": true})).unwrap();
        assert_eq!((g.net.latency_ms, g.net.dns, g.net.loss), (300, None, 0.5));
        assert!(g.panel_busy_stuck);
        assert_eq!(merge_faults(&Faults::default(), &faults_json(&g)).unwrap(), g);
    }

    #[test]
    fn power_loss_fields() {
        let f = parse_faults(r#"{"power_loss": {"op": "erase", "range": [4096, "0x2000"], "nth": 3, "cut": "torn"}}"#)
            .unwrap();
        let p = f.power_loss.unwrap();
        assert_eq!((p.op, p.range, p.nth, p.cut), (FlashOp::Erase, Some((0x1000, 0x2000)), 3, CutPoint::Torn));
        let d = parse_faults(r#"{"power_loss": {"partition": "nvs"}}"#).unwrap().power_loss.unwrap();
        assert_eq!((d.op, d.nth, d.cut), (FlashOp::Any, 1, CutPoint::Before));
    }

    #[test]
    fn rejects_mistakes() {
        for bad in [
            r#"{"latency_ms": 3}"#,
            r#"{"net": {"dns": "nope"}}"#,
            r#"{"net": {"loss": 2}}"#,
            r#"{"power_loss": {"cut": "sideways"}}"#,
            r#"{"i2c_absent": [300]}"#,
            r#"[]"#,
        ] {
            assert!(parse_faults(bad).is_err(), "{bad}");
        }
        let parts = [PartitionInfo { label: "nvs".into(), kind: 1, subtype: 2, offset: 0x9000, size: 0x5000 }];
        let f = parse_faults(r#"{"power_loss": {"partition": "otadata"}}"#).unwrap();
        assert!(validate(&f, &parts).is_err());
    }
}
