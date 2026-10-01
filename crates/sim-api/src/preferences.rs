//! Host inspection and editing of committed firmware NVS entries.

#[derive(Clone, Debug, Default)]
pub struct PreferencesSnapshot {
    pub entries: Vec<Preference>,
    pub warnings: Vec<String>,
    /// Writes are allowed only during deep sleep.
    pub editable: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Preference {
    pub partition: String,
    pub namespace: String,
    pub key: String,
    pub kind: &'static str,
    pub value: String,
}

/// Set a typed value, or delete the key when `value` is None.
/// Integer values use decimal strings; blobs use hexadecimal bytes.
#[derive(Clone, Debug)]
pub struct PreferenceChange {
    pub partition: String,
    pub namespace: String,
    pub key: String,
    pub value: Option<(String, String)>,
}

impl Preference {
    /// The writable representation (without the blob byte-count annotation).
    pub fn edit_value(&self) -> &str {
        if self.kind == "blob" { self.value.rsplit_once(" (").map_or(&self.value, |(hex, _)| hex) } else { &self.value }
    }
}
