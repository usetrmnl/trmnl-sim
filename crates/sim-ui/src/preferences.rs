use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, TryRecvError};
use sim_api::{Command, PreferencesSnapshot, SimHandle};

pub struct PreferencesPanel {
    snapshot: Option<PreferencesSnapshot>,
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
            && (!self.was_open
                || (self.auto_refresh
                    && self.error.is_none()
                    && self.updated.is_none_or(|t| t.elapsed() >= Duration::from_secs(1))))
        {
            self.refresh(handle);
        }
        self.was_open = true;
        egui::Window::new("Firmware preferences").open(open).default_size([760.0, 440.0]).min_width(440.0).show(
            ctx,
            |ui| {
                ui.weak("Read-only · saved values from live NVS flash");
                ui.horizontal(|ui| {
                    if ui.add_enabled(self.pending.is_none(), egui::Button::new("Refresh")).clicked() {
                        self.refresh(handle);
                    }
                    ui.checkbox(&mut self.auto_refresh, "Auto-refresh");
                    if self.pending.is_some() {
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
                                    if ui.small_button("Copy").on_hover_text("Copy value").clicked() {
                                        ui.ctx().copy_text(entry.value.clone());
                                    }
                                    ui.end_row();
                                }
                            });
                    }
                });
            },
        );
        ctx.request_repaint_after(Duration::from_millis(100));
    }
}
