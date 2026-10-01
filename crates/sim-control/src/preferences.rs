use super::*;

pub fn route(h: &SimHandle, method: &Method, body: &[u8]) -> Result<Reply, String> {
    if *method == Method::Get {
        let (reply, rx) = crossbeam_channel::bounded(1);
        h.send(Command::ReadPreferences(reply));
        return Ok(match rx.recv_timeout(Duration::from_secs(5)) {
            Ok(snapshot) => snapshot_reply(snapshot),
            Err(_) => err(504, "no preferences snapshot from the emulator"),
        });
    }
    let b = body_json(body)?;
    let field = |key: &str| b[key].as_str().map(str::to_owned).ok_or_else(|| format!("need {key} as a string"));
    let change = sim_api::PreferenceChange {
        partition: field("partition")?,
        namespace: field("namespace")?,
        key: field("key")?,
        value: if *method == Method::Put { Some((field("type")?, field("value")?)) } else { None },
    };
    let (reply, rx) = crossbeam_channel::bounded(1);
    h.send(Command::ChangePreference { change, reply });
    Ok(match rx.recv_timeout(Duration::from_secs(5)) {
        Ok(Ok(snapshot)) => snapshot_reply(snapshot),
        Ok(Err(message)) => err(409, message),
        Err(_) => err(504, "no preferences edit result from the emulator"),
    })
}

fn snapshot_reply(snapshot: sim_api::PreferencesSnapshot) -> Reply {
    let entries: Vec<_> = snapshot
        .entries
        .iter()
        .map(|e| {
            json!({
                "partition": e.partition, "namespace": e.namespace, "key": e.key,
                "type": e.kind, "value": e.edit_value(),
            })
        })
        .collect();
    json_reply(
        200,
        json!({"ok": true, "editable": snapshot.editable, "entries": entries, "warnings": snapshot.warnings}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use sim_api::{Frame, Preference, PreferencesSnapshot};

    #[test]
    fn get_returns_live_values_and_editability() {
        let (h, p) = sim_api::channel(std::sync::Arc::new(Frame::new(1, 1).into()));
        let worker = std::thread::spawn(move || {
            let Command::ReadPreferences(reply) = p.commands.recv().unwrap() else { panic!("read command") };
            reply
                .send(PreferencesSnapshot {
                    editable: true,
                    entries: vec![Preference {
                        partition: "nvs".into(),
                        namespace: "data".into(),
                        key: "password".into(),
                        kind: "string",
                        value: "unmasked".into(),
                    }],
                    warnings: vec![],
                })
                .unwrap();
        });
        let r = super::super::route(&h, &Method::Get, "/preferences", &[], b"").unwrap();
        assert_eq!(r.status_code().0, 200);
        let v: Value = serde_json::from_reader(r.into_reader()).unwrap();
        assert_eq!(v["editable"], true);
        assert_eq!(v["entries"][0]["value"], "unmasked");
        assert_eq!(v["entries"][0]["type"], "string");
        worker.join().unwrap();
    }

    #[test]
    fn put_and_delete_send_changes_and_report_rejection() {
        for method in [Method::Put, Method::Delete] {
            let (h, p) = sim_api::channel(std::sync::Arc::new(Frame::new(1, 1).into()));
            let deleting = method == Method::Delete;
            let worker = std::thread::spawn(move || {
                let Command::ChangePreference { change, reply } = p.commands.recv().unwrap() else {
                    panic!("write command")
                };
                assert_eq!(change.key, "count");
                assert_eq!(change.value.is_none(), deleting);
                reply.send(Err("deep sleep required".into())).unwrap();
            });
            let r = super::super::route(
                &h,
                &method,
                "/preferences",
                &[],
                br#"{"partition":"nvs","namespace":"data","key":"count","type":"u64","value":"18446744073709551615"}"#,
            )
            .unwrap();
            assert_eq!(r.status_code().0, 409);
            worker.join().unwrap();
        }
    }

    #[test]
    fn malformed_changes_never_reach_emulator() {
        let (h, p) = sim_api::channel(std::sync::Arc::new(Frame::new(1, 1).into()));
        assert!(route(&h, &Method::Put, br#"{"key":"x"}"#).is_err());
        assert!(
            route(&h, &Method::Put, br#"{"partition":"nvs","namespace":"data","key":"x","type":"u64","value":1}"#)
                .is_err()
        );
        assert!(p.commands.try_recv().is_err());
    }
}
