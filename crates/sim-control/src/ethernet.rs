//! JSON form of the USB Ethernet adapter, for `POST /ethernet` and the `--ethernet` CLI flag:
//!
//! ```json
//! {"plugged": true, "link": true, "dhcp": true}
//! ```
//!
//! Keys left out keep their current value (`POST /ethernet`) or their default (`--ethernet`:
//! plugged in, with link and DHCP).

use serde_json::Value;
use sim_api::EthernetAdapter;

/// Parse an `--ethernet` argument: the adapter is plugged in unless it says otherwise.
pub fn parse_str(s: &str) -> Result<EthernetAdapter, String> {
    let v: Value = serde_json::from_str(s).map_err(|e| format!("bad ethernet JSON: {e}"))?;
    merge(EthernetAdapter { plugged: true, ..Default::default() }, &v)
}

/// `cur` with the keys of `v` applied.
pub fn merge(mut cur: EthernetAdapter, v: &Value) -> Result<EthernetAdapter, String> {
    let obj = v.as_object().ok_or("ethernet: need a JSON object")?;
    for (k, v) in obj {
        let b = v.as_bool().ok_or(format!("ethernet: \"{k}\" must be true or false"))?;
        match k.as_str() {
            "plugged" => cur.plugged = b,
            "link" => cur.link = b,
            "dhcp" => cur.dhcp = b,
            _ => return Err(format!("ethernet: unknown key \"{k}\" (plugged, link, dhcp)")),
        }
    }
    Ok(cur)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_left_out_keep_their_value() {
        let a = parse_str(r#"{"dhcp": false}"#).unwrap();
        assert_eq!(a, EthernetAdapter { plugged: true, link: true, dhcp: false });
        let b = merge(a, &serde_json::json!({"link": false})).unwrap();
        assert_eq!(b, EthernetAdapter { plugged: true, link: false, dhcp: false });
        assert!(merge(a, &serde_json::json!({"speed": 100})).is_err());
        assert!(merge(a, &serde_json::json!({"link": 1})).is_err());
    }
}
