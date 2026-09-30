//! Read-only view of committed firmware NVS entries.

#[derive(Clone, Debug, Default)]
pub struct PreferencesSnapshot {
    pub entries: Vec<Preference>,
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Preference {
    pub partition: String,
    pub namespace: String,
    pub key: String,
    pub kind: &'static str,
    pub value: String,
}
