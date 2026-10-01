use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, TryRecvError};
use sim_api::{Command, Preference, PreferenceChange, PreferencesSnapshot, SimHandle};

type EditResult = Result<PreferencesSnapshot, String>;

pub struct PreferencesPanel {
    snapshot: Option<PreferencesSnapshot>,
    editing: Option<Editor>,
    saving: Option<(Receiver<EditResult>, Instant)>,
    notice: Option<String>,
    pending: Option<(Receiver<PreferencesSnapshot>, Instant)>,
    updated: Option<Instant>,
    error: Option<String>,
    filter: String,
    auto_refresh: bool,
    was_open: bool,
}

impl Default for PreferencesPanel {
    fn default() -> Self {
        Self {
            snapshot: None,
            editing: None,
            saving: None,
            notice: None,
            pending: None,
            updated: None,
            error: None,
            filter: String::new(),
            auto_refresh: true,
            was_open: false,
        }
    }
}

impl PreferencesPanel {
    fn refresh(&mut self, handle: &SimHandle) {
        let (tx, rx) = crossbeam_channel::bounded(1);
        handle.send(Command::ReadPreferences(tx));
        self.pending = Some((rx, Instant::now()));
        self.error = None;
    }

    pub fn show(&mut self, ctx: &egui::Context, handle: &SimHandle, open: &mut bool) {
        if !*open {
            self.was_open = false;
            self.pending = None;
            return;
        }
        if let Some((rx, sent)) = &self.saving {
            let result = match rx.try_recv() {
                Ok(result) => Some(result),
                Err(e) if e == TryRecvError::Disconnected || sent.elapsed() > Duration::from_secs(5) => {
                    Some(Err("No edit result received. Refresh to check whether the change was saved.".into()))
                }
                Err(_) => None,
            };
            if let Some(result) = result {
                self.saving = None;
                match result {
                    Ok(snapshot) => {
                        self.snapshot = Some(snapshot);
                        self.pending = None;
                        self.updated = Some(Instant::now());
                        self.editing = None;
                        self.error = None;
                        self.notice = Some("Saved to flash. The firmware will use the change on its next wake.".into());
                    }
                    Err(error) => self.error = Some(error),
                }
            }
        }
        if let Some((rx, sent)) = &self.pending {
            match rx.try_recv() {
                Ok(snapshot) => {
                    self.snapshot = Some(snapshot);
                    self.updated = Some(Instant::now());
                    self.pending = None;
                    self.error = None;
                }
                Err(e) if e == TryRecvError::Disconnected || sent.elapsed() > Duration::from_secs(3) => {
                    self.pending = None;
                    self.error = Some("The simulator did not answer. Try Refresh.".into());
                }
                Err(_) => {}
            }
        }
        if self.pending.is_none()
            && self.saving.is_none()
            && (!self.was_open
                || (self.auto_refresh
                    && self.error.is_none()
                    && self.updated.is_none_or(|t| t.elapsed() >= Duration::from_secs(1))))
        {
            self.refresh(handle);
        }
        self.was_open = true;
        let editable = self.snapshot.as_ref().is_some_and(|s| s.editable) && self.saving.is_none();
        egui::Window::new("Firmware preferences").open(open).default_size([760.0, 440.0]).min_width(440.0).show(
            ctx,
            |ui| {
                ui.weak(if editable {
                    "Deep sleep · edits are saved immediately and take effect on the next wake"
                } else {
                    "Saved values from live NVS flash · editing requires deep sleep"
                });
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(self.pending.is_none() && self.saving.is_none(), egui::Button::new("Refresh"))
                        .clicked()
                    {
                        self.refresh(handle);
                    }
                    ui.checkbox(&mut self.auto_refresh, "Auto-refresh");
                    if ui.add_enabled(editable, egui::Button::new("Add preference")).clicked() {
                        self.editing = Some(Editor::new());
                        self.error = None;
                        self.notice = None;
                    }
                    if self.pending.is_some() || self.saving.is_some() {
                        ui.spinner();
                    } else if let Some(updated) = self.updated {
                        ui.weak(format!("Updated {}s ago", updated.elapsed().as_secs()));
                    }
                });
                ui.add(egui::TextEdit::singleline(&mut self.filter).hint_text("Search namespace, key, type, or value"));
                ui.separator();
                if let Some(error) = &self.error {
                    ui.colored_label(ui.visuals().error_fg_color, error);
                }
                if let Some(notice) = &self.notice {
                    ui.label(notice);
                }
                let Some(snapshot) = &self.snapshot else {
                    ui.label("Loading preferences…");
                    return;
                };
                for warning in &snapshot.warnings {
                    ui.colored_label(ui.visuals().warn_fg_color, warning);
                }
                let filter = self.filter.to_lowercase();
                let entries: Vec<_> = snapshot
                    .entries
                    .iter()
                    .filter(|entry| {
                        [&entry.partition, &entry.namespace, &entry.key, entry.kind, &entry.value]
                            .iter()
                            .any(|text| text.to_lowercase().contains(&filter))
                    })
                    .collect();
                ui.weak(format!("{} of {} preferences", entries.len(), snapshot.entries.len()));
                if entries.is_empty() {
                    ui.label(if snapshot.entries.is_empty() {
                        "No saved preferences found."
                    } else {
                        "No matching preferences."
                    });
                }
                egui::ScrollArea::vertical().id_salt("preferences_rows").show(ui, |ui| {
                    for entries in entries.chunk_by(|a, b| a.partition == b.partition && a.namespace == b.namespace) {
                        let first = entries[0];
                        ui.add_space(10.0);
                        ui.strong(format!("{} / {}", first.partition, first.namespace));
                        egui::Grid::new((&first.partition, &first.namespace))
                            .num_columns(4)
                            .striped(true)
                            .min_col_width(65.0)
                            .max_col_width(450.0)
                            .show(ui, |ui| {
                                ui.weak("Key");
                                ui.weak("Type");
                                ui.weak("Value");
                                ui.label("");
                                ui.end_row();
                                for entry in entries {
                                    ui.monospace(&entry.key);
                                    ui.weak(entry.kind);
                                    ui.add(egui::Label::new(egui::RichText::new(&entry.value).monospace()).wrap());
                                    ui.horizontal(|ui| {
                                        if ui.small_button("Copy").on_hover_text("Copy value").clicked() {
                                            ui.ctx().copy_text(entry.edit_value().into());
                                        }
                                        if ui.add_enabled(editable, egui::Button::new("Edit").small()).clicked() {
                                            self.editing = Some(Editor::from(*entry));
                                            self.error = None;
                                            self.notice = None;
                                        }
                                    });
                                    ui.end_row();
                                }
                            });
                    }
                });
            },
        );
        if let Some(editor) = &mut self.editing {
            match editor.show(ctx, editable, self.saving.is_some(), self.error.as_deref()) {
                Some(EditAction::Save(change)) => {
                    let (reply, rx) = crossbeam_channel::bounded(1);
                    handle.send(Command::ChangePreference { change, reply });
                    self.pending = None;
                    self.saving = Some((rx, Instant::now()));
                    self.error = None;
                }
                Some(EditAction::Cancel) => {
                    self.editing = None;
                }
                None => {}
            }
        }
        ctx.request_repaint_after(Duration::from_millis(100));
    }
}

enum EditAction {
    Save(PreferenceChange),
    Cancel,
}

struct Editor {
    partition: String,
    namespace: String,
    key: String,
    kind: String,
    value: String,
    existing: bool,
    deleting: bool,
}

impl From<&Preference> for Editor {
    fn from(e: &Preference) -> Self {
        Self {
            partition: e.partition.clone(),
            namespace: e.namespace.clone(),
            key: e.key.clone(),
            kind: e.kind.into(),
            value: e.edit_value().into(),
            existing: true,
            deleting: false,
        }
    }
}

impl Editor {
    fn new() -> Self {
        Self {
            partition: "nvs".into(),
            namespace: "data".into(),
            key: String::new(),
            kind: "string".into(),
            value: String::new(),
            existing: false,
            deleting: false,
        }
    }

    fn show(&mut self, ctx: &egui::Context, editable: bool, saving: bool, error: Option<&str>) -> Option<EditAction> {
        let mut action = None;
        egui::Window::new(if self.existing { "Edit preference" } else { "New preference" })
            .id(egui::Id::new("preference_editor"))
            .collapsible(false)
            .default_width(480.0)
            .show(ctx, |ui| {
                ui.add_enabled_ui(!saving, |ui| {
                    egui::Grid::new("preference_fields").num_columns(2).show(ui, |ui| {
                        for (label, text) in [
                            ("Partition", &mut self.partition),
                            ("Namespace", &mut self.namespace),
                            ("Key", &mut self.key),
                        ] {
                            let label = ui.label(label);
                            ui.add_enabled(!self.existing, egui::TextEdit::singleline(text)).labelled_by(label.id);
                            ui.end_row();
                        }
                        ui.label("Type");
                        egui::ComboBox::from_id_salt("preference_type").selected_text(&self.kind).show_ui(ui, |ui| {
                            for kind in ["string", "u8", "u16", "u32", "u64", "i8", "i16", "i32", "i64", "blob"] {
                                ui.selectable_value(&mut self.kind, kind.to_owned(), kind);
                            }
                        });
                        ui.end_row();
                    });
                    ui.weak(match self.kind.as_str() {
                        "blob" => "Value: hexadecimal bytes (spaces optional)",
                        "string" => "Value: text (no masking)",
                        _ => "Value: decimal integer",
                    });
                    let label = ui.label("Value to save");
                    ui.add(egui::TextEdit::multiline(&mut self.value).desired_rows(3).desired_width(f32::INFINITY))
                        .labelled_by(label.id);
                    if let Some(error) = error {
                        ui.colored_label(ui.visuals().error_fg_color, error);
                    }
                    if !editable {
                        ui.weak("Wait for deep sleep before saving or deleting.");
                    }
                    if self.deleting {
                        ui.label(format!("Delete {}/{} from {}?", self.namespace, self.key, self.partition));
                    }
                    ui.horizontal(|ui| {
                        let label = if self.deleting { "Confirm delete" } else { "Save preference" };
                        if ui.add_enabled(editable, egui::Button::new(label)).clicked() {
                            action = Some(EditAction::Save(PreferenceChange {
                                partition: self.partition.clone(),
                                namespace: self.namespace.clone(),
                                key: self.key.clone(),
                                value: (!self.deleting).then(|| (self.kind.clone(), self.value.clone())),
                            }));
                        }
                        if self.existing
                            && !self.deleting
                            && ui.add_enabled(editable, egui::Button::new("Delete preference")).clicked()
                        {
                            self.deleting = true;
                        }
                        if ui.button("Cancel edit").clicked() {
                            action = Some(EditAction::Cancel);
                        }
                    });
                });
                if saving {
                    ui.spinner();
                }
            });
        action
    }
}
