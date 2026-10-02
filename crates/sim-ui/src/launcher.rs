//! The window shown when the simulator starts without a board and firmware: pick both, then
//! the simulator window opens. The last choice is remembered for next time.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use egui::{RichText, Ui};
use serde_json::{Value, json};

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

impl Launcher {
    fn set_firmware(&mut self, p: PathBuf) {
        let elf = p.with_extension("elf");
        self.error = if !p.is_file() {
            Some(format!("{} not found", p.display()))
        } else if !elf.is_file() {
            Some(format!("{} not found", elf.display()))
        } else {
            None
        };
        self.firmware = Some(p);
    }
}

impl eframe::App for Launcher {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        egui::CentralPanel::default().show(ui, |ui| {
            ui.heading("Run firmware");
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.add_sized([64.0, 20.0], egui::Label::new("Board"));
                let selected = self.board.map(|i| label(&self.boards[i])).unwrap_or_else(|| "Choose…".into());
                egui::ComboBox::from_id_salt("board").selected_text(selected).width(360.0).show_ui(ui, |ui| {
                    for (i, b) in self.boards.iter().enumerate() {
                        ui.selectable_value(&mut self.board, Some(i), label(b));
                    }
                });
            });
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.add_sized([64.0, 20.0], egui::Label::new("Firmware"));
                // The button first: a long path is cut short to fit the window, not the button.
                if ui.button("Choose…").on_hover_text("merged_firmware.bin, with its .elf next to it").clicked()
                    && let Some(p) = crate::pick_firmware(self.firmware.as_deref().and_then(|f| f.parent()))
                {
                    self.set_firmware(p);
                }
                let shown = self.firmware.as_ref().map(|p| p.display().to_string());
                let font = egui::TextStyle::Monospace.resolve(ui.style());
                let char_w = ui.fonts_mut(|f| f.glyph_width(&font, 'x')).max(1.0);
                let fits = (ui.available_width() / char_w) as usize;
                let text = elide_start(shown.as_deref().unwrap_or("none"), fits);
                ui.add(egui::Label::new(RichText::new(text).monospace()).truncate())
                    .on_hover_text(shown.unwrap_or_default());
            });
            if let Some(e) = &self.error {
                ui.add(egui::Label::new(RichText::new(e).color(ui.visuals().error_fg_color)).wrap());
            }
            ui.add_space(8.0);
            let ready = self.board.is_some() && self.firmware.is_some() && self.error.is_none();
            if ui.add_enabled(ready, egui::Button::new("Start")).clicked()
                && let (Some(i), Some(fw)) = (self.board, self.firmware.clone())
            {
                let env = self.boards[i].env.clone();
                save_last(&env, &fw);
                *self.result.lock().unwrap() = Some((env, fw));
                ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
            }
        });
    }
}

/// `s` cut to `max` characters from its start ("…/TRMNL_X/merged_firmware.bin"): the end of a
/// path says which build it is.
fn elide_start(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max {
        return s.to_string();
    }
    let keep = max.saturating_sub(1);
    std::iter::once('…').chain(s.chars().skip(n - keep)).collect()
}

fn label(b: &BoardChoice) -> String {
    format!("{} ({})", b.name, b.env)
}

/// Where the last choice is kept: the user's config directory (`~/Library/Application Support`
/// on macOS, `%APPDATA%` on Windows, else `$XDG_CONFIG_HOME` or `~/.config`).
fn settings_path() -> Option<PathBuf> {
    let home = || std::env::var_os("HOME").map(PathBuf::from);
    let dir = if cfg!(target_os = "macos") {
        home()?.join("Library/Application Support")
    } else if cfg!(windows) {
        PathBuf::from(std::env::var_os("APPDATA")?)
    } else {
        std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).or_else(|| Some(home()?.join(".config")))?
    };
    Some(dir.join("trmnl-sim").join("preferences.json"))
}

/// The board and firmware chosen last time, if any.
fn load_last() -> (Option<String>, Option<PathBuf>) {
    let v: Value = settings_path()
        .and_then(|p| std::fs::read(p).ok())
        .and_then(|d| serde_json::from_slice(&d).ok())
        .unwrap_or_default();
    (v["env"].as_str().map(Into::into), v["firmware"].as_str().map(PathBuf::from))
}

fn save_last(env: &str, firmware: &std::path::Path) {
    let Some(path) = settings_path() else { return };
    let v = json!({ "env": env, "firmware": firmware.to_string_lossy() });
    // Remembering is a convenience: a read-only config directory isn't worth an error.
    let _ = path.parent().map(std::fs::create_dir_all);
    let _ = std::fs::write(path, serde_json::to_vec_pretty(&v).unwrap_or_default());
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
    let (env, firmware) = load_last();
    let board = env.and_then(|e| boards.iter().position(|b| b.env == e));
    let mut app = Launcher { boards, board, firmware: None, error: None, result: result.clone() };
    if let Some(f) = firmware {
        app.set_firmware(f);
    }
    eframe::run_native("TRMNL Simulator launcher", native, Box::new(move |_| Ok(Box::new(app))))
        .map_err(|e| anyhow::anyhow!("GUI error: {e}"))?;
    Ok(result.lock().unwrap().take())
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui_kittest::kittest::Queryable;

    #[test]
    fn elides_the_start() {
        assert_eq!(elide_start("/a/b/c.bin", 20), "/a/b/c.bin");
        assert_eq!(elide_start("/a/b/c.bin", 6), "…c.bin");
    }

    /// A long firmware path is cut short at its start; Choose… and Start stay in the window.
    #[test]
    fn long_path_fits() {
        let boards = vec![BoardChoice { env: "trmnl".into(), name: "TRMNL OG".into() }];
        let long = PathBuf::from(format!("/{}/TRMNL_X/merged_firmware.bin", "very-long-directory-name".repeat(8)));
        let app = Launcher { boards, board: Some(0), firmware: Some(long), error: None, result: Default::default() };
        let mut h = egui_kittest::Harness::builder()
            .with_size([620.0, 200.0])
            .with_pixels_per_point(2.0)
            .build_eframe(move |_| app);
        h.run();
        let window = h.ctx.content_rect();
        for name in ["Choose…", "Start"] {
            let r = h.get_by_label(name).rect();
            assert!(window.contains_rect(r), "{name} at {r:?} is outside {window:?}");
        }
        assert!(h.query_by_label_contains("TRMNL_X/merged_firmware.bin").is_some(), "the path's end is shown");
        if let Ok(dir) = std::env::var("SIM_UI_RENDER_DIR") {
            h.render().expect("render").save(format!("{dir}/launcher.png")).expect("save");
        }
    }
}
