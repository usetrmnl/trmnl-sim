//! JSON form of the access points in range, for `POST /wifi {"networks": [...]}` and the
//! `--wifi-networks` CLI flag:
//!
//! ```json
//! [{"ssid": "TRMNL-Sim", "password": null, "rssi": -54, "channel": 6, "open": false, "internet": true}]
//! ```
//!
//! Only `ssid` is required; the rest default to the values above (`password: null` accepts
//! any password but `fail`). `channel` is 1-14 (2.4 GHz) or 32-177 (5 GHz: seen by dual-band radios,
//! the ESP32-C5 and the TRMNL X's modem).
//!
//! `"enterprise": {"username": "alice", "password": "s3cret"}` makes it a WPA2-Enterprise
//! network whose server accepts those EAP credentials (either may be `null` or left out:
//! anything but `fail`); `"enterprise": true` accepts any. It takes no `password` and is not
//! `open`.

use serde_json::Value;
use sim_api::{Enterprise, WifiNetwork};

/// Parse a `--wifi-networks` argument (a JSON array).
pub fn parse_networks_str(s: &str) -> Result<Vec<WifiNetwork>, String> {
    let v: Value = serde_json::from_str(s).map_err(|e| format!("bad networks JSON: {e}"))?;
    parse_networks(&v)
}

pub fn parse_networks(v: &Value) -> Result<Vec<WifiNetwork>, String> {
    let list = v.as_array().ok_or("networks: need a JSON array")?;
    list.iter().map(network).collect()
}

fn network(v: &Value) -> Result<WifiNetwork, String> {
    let obj = v.as_object().ok_or("network: need a JSON object")?;
    let ssid = obj.get("ssid").and_then(Value::as_str).ok_or("network: need \"ssid\"")?;
    let mut n = WifiNetwork::new(ssid);
    for (k, v) in obj {
        match k.as_str() {
            "ssid" => {}
            "password" => n.password = if v.is_null() { None } else { Some(str_of(k, v)?.into()) },
            "rssi" => n.rssi = int_of(k, v, -127, 0)? as i8,
            "channel" => {
                n.channel = int_of(k, v, 1, 177)
                    .ok()
                    .filter(|c| !(15..32).contains(c))
                    .ok_or(format!("network: \"{k}\" must be 1..14 (2.4 GHz) or 32..177 (5 GHz)"))?
                    as u8
            }
            "open" => n.open = bool_of(k, v)?,
            "internet" => n.internet = bool_of(k, v)?,
            "enterprise" => n.enterprise = enterprise(v)?,
            _ => return Err(format!("network: unknown key \"{k}\"")),
        }
    }
    if n.enterprise.is_some() && (n.open || n.password.is_some()) {
        return Err("network: an \"enterprise\" network has no \"password\" and is not \"open\"".into());
    }
    Ok(n)
}

fn enterprise(v: &Value) -> Result<Option<Enterprise>, String> {
    let credential = |k: &str| -> Result<Option<String>, String> {
        match v.get(k) {
            None | Some(Value::Null) => Ok(None),
            Some(s) => Ok(Some(s.as_str().ok_or(format!("network: \"enterprise.{k}\" must be a string"))?.to_string())),
        }
    };
    match v {
        Value::Null | Value::Bool(false) => Ok(None),
        Value::Bool(true) => Ok(Some(Enterprise::default())),
        Value::Object(obj) => {
            if let Some(k) = obj.keys().find(|k| !["username", "password"].contains(&k.as_str())) {
                return Err(format!("network: unknown key \"enterprise.{k}\""));
            }
            Ok(Some(Enterprise { username: credential("username")?, password: credential("password")? }))
        }
        _ => Err("network: \"enterprise\" must be true or {\"username\", \"password\"}".into()),
    }
}

fn str_of<'a>(k: &str, v: &'a Value) -> Result<&'a str, String> {
    v.as_str().ok_or(format!("network: \"{k}\" must be a string"))
}

fn bool_of(k: &str, v: &Value) -> Result<bool, String> {
    v.as_bool().ok_or(format!("network: \"{k}\" must be true or false"))
}

fn int_of(k: &str, v: &Value, min: i64, max: i64) -> Result<i64, String> {
    v.as_i64().filter(|n| (min..=max).contains(n)).ok_or(format!("network: \"{k}\" must be {min}..{max}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_and_overrides() {
        let n = parse_networks_str(r#"[{"ssid": "A"}, {"ssid": "B", "password": "pw", "rssi": -80, "open": true, "internet": false, "channel": 11}]"#)
            .unwrap();
        assert_eq!(n[0], WifiNetwork::new("A"));
        assert_eq!(parse_networks_str(r#"[{"ssid": "5G", "channel": 36}]"#).unwrap()[0].channel, 36);
        assert_eq!(
            n[1],
            WifiNetwork {
                ssid: "B".into(),
                password: Some("pw".into()),
                rssi: -80,
                channel: 11,
                open: true,
                internet: false,
                enterprise: None,
            }
        );
    }

    #[test]
    fn enterprise_networks() {
        let n = parse_networks_str(
            r#"[{"ssid": "Corp", "enterprise": {"username": "alice", "password": "s3cret"}},
                {"ssid": "Any", "enterprise": true}, {"ssid": "Half", "enterprise": {"username": "bob"}}]"#,
        )
        .unwrap();
        assert_eq!(
            n[0].enterprise,
            Some(Enterprise { username: Some("alice".into()), password: Some("s3cret".into()) })
        );
        assert_eq!(n[1].enterprise, Some(Enterprise::default()));
        assert_eq!(n[2].enterprise, Some(Enterprise { username: Some("bob".into()), password: None }));
    }

    #[test]
    fn rejects_bad_input() {
        for bad in [
            r#"{"ssid": "A"}"#,
            r#"[{"rssi": -50}]"#,
            r#"[{"ssid": "A", "rssi": 5}]"#,
            r#"[{"ssid": "A", "x": 1}]"#,
            r#"[{"ssid": "A", "channel": 20}]"#,
            r#"[{"ssid": "A", "channel": 200}]"#,
            r#"[{"ssid": "A", "enterprise": "yes"}]"#,
            r#"[{"ssid": "A", "enterprise": {"user": "x"}}]"#,
            r#"[{"ssid": "A", "enterprise": {"username": 1}}]"#,
            r#"[{"ssid": "A", "enterprise": true, "password": "pw"}]"#,
            r#"[{"ssid": "A", "enterprise": true, "open": true}]"#,
        ] {
            assert!(parse_networks_str(bad).is_err(), "{bad}");
        }
    }
}
