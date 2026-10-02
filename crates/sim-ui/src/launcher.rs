//! The window shown when the simulator starts without a board and firmware: pick both, then
//! the simulator window opens.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use egui::{RichText, Ui};

/// A board the simulator can run: its PlatformIO environment and a name to show.
pub struct BoardChoice {
    pub env: String,
    pub name: String,
}

/// What the launcher returns: the chosen environment and merged firmware image.
pub type Launch = (String, PathBuf);

struct Launcher {
    boards: Vec<BoardChoice>,
    board: Option<usize>,
    firmware: Option<PathBuf>,
    error: Option<String>,
    result: Arc<Mutex<Option<Launch>>>,
}

impl eframe::App for Launcher {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        egui::CentralPanel::default().show(ui, |ui| {
            ui.heading("Run firmware");
            ui.add_space(8.0);
            egui::Grid::new("launch").num_columns(2).spacing([12.0, 10.0]).show(ui, |ui| {
                ui.label("Board");
                let selected = self.board.map(|i| label(&self.boards[i])).unwrap_or_else(|| "Choose…".into());
                egui::ComboBox::from_id_salt("board").selected_text(selected).width(360.0).show_ui(ui, |ui| {
                    for (i, b) in self.boards.iter().enumerate() {
                        ui.selectable_value(&mut self.board, Some(i), label(b));
                    }
                });
                ui.end_row();

                ui.label("Firmware");
                ui.horizontal(|ui| {
                    let shown = self.firmware.as_ref().map(|p| p.display().to_string()).unwrap_or("none".into());
                    ui.label(RichText::new(shown).monospace());
                    if ui.button("Choose…").on_hover_text("merged_firmware.bin, with its .elf next to it").clicked()
                        && let Some(p) = crate::pick_firmware()
                    {
                        self.error = (!p.with_extension("elf").is_file())
                            .then(|| format!("{} not found", p.with_extension("elf").display()));
                        self.firmware = Some(p);
                    }
                });
                ui.end_row();
            });
            if let Some(e) = &self.error {
                ui.colored_label(ui.visuals().error_fg_color, e);
            }
            ui.add_space(8.0);
            let ready = self.board.is_some() && self.firmware.is_some() && self.error.is_none();
            if ui.add_enabled(ready, egui::Button::new("Start")).clicked()
                && let (Some(i), Some(fw)) = (self.board, self.firmware.clone())
            {
                *self.result.lock().unwrap() = Some((self.boards[i].env.clone(), fw));
                ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
            }
        });
    }
}

fn label(b: &BoardChoice) -> String {
    format!("{} ({})", b.name, b.env)
}

/// Ask for the board and the firmware image in a small window (`None`: closed without
/// starting). Call it on the main thread, before [`crate::run`].
pub fn launch(boards: Vec<BoardChoice>) -> anyhow::Result<Option<Launch>> {
    let result = Arc::new(Mutex::new(None));
    let native = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("TRMNL Simulator")
            .with_inner_size([620.0, 200.0])
            .with_icon(crate::app_icon()),
        ..Default::default()
    };
    let app = Launcher { boards, board: None, firmware: None, error: None, result: result.clone() };
    eframe::run_native("TRMNL Simulator launcher", native, Box::new(move |_| Ok(Box::new(app))))
        .map_err(|e| anyhow::anyhow!("GUI error: {e}"))?;
    Ok(result.lock().unwrap().take())
}
