//! The window shown when the simulator starts without a board and firmware: pick both (and
//! optionally a few command line options), then the simulator window opens. The last choice is
//! remembered for next time, except erasing the flash.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use egui::{RichText, Ui};
use serde_json::{Value, json};

/// A board the simulator can run: its PlatformIO environment and a name to show.
pub struct BoardChoice {
    pub env: String,
    pub name: String,
}

/// What the launcher returns: the board and firmware, and the options it sets (unset ones keep
/// their command line value).
#[derive(Clone, Debug, Default)]
pub struct Launch {
    pub env: String,
    pub firmware: PathBuf,
    /// `--mac`
    pub mac: Option<[u8; 6]>,
    /// `--flash`
    pub flash: Option<PathBuf>,
    /// `--ota-firmware`
    pub ota_firmware: Option<PathBuf>,
    /// `--erase`
    pub erase: bool,
    /// `--turbo`
    pub turbo: bool,
    /// `--fast-sleep`
    pub fast_sleep: bool,
    /// `--offline`
    pub offline: bool,
}

#[derive(Default)]
struct Launcher {
    boards: Vec<BoardChoice>,
    board: Option<usize>,
    firmware: Option<PathBuf>,
    mac: String,
    flash: Option<PathBuf>,
    ota_firmware: Option<PathBuf>,
    erase: bool,
    turbo: bool,
    fast_sleep: bool,
    offline: bool,
    result: Arc<Mutex<Option<Launch>>>,
}

/// Width of the labels in front of each row.
const LABEL_W: f32 = 96.0;

impl Launcher {
    /// What is wrong with the choices, if anything (Start stays disabled).
    fn problems(&self) -> Vec<String> {
        let mut v = Vec::new();
        if let Some(p) = &self.firmware {
            for f in [p.clone(), p.with_extension("elf")] {
                if !f.is_file() {
                    v.push(format!("{} not found", f.display()));
                }
            }
        }
        if let Some(p) = &self.ota_firmware {
            for f in [p.clone(), p.with_extension("elf")] {
                if !f.is_file() {
                    v.push(format!("OTA firmware: {} not found", f.display()));
                }
            }
        }
        if !self.mac.trim().is_empty() && parse_mac(&self.mac).is_none() {
            v.push("MAC address: expected 6 hex bytes like 7C:DF:A1:12:34:56".into());
        }
        v
    }

    fn launch(&self) -> Option<Launch> {
        Some(Launch {
            env: self.boards.get(self.board?)?.env.clone(),
            firmware: self.firmware.clone()?,
            mac: parse_mac(&self.mac),
            flash: self.flash.clone(),
            ota_firmware: self.ota_firmware.clone(),
            erase: self.erase,
            turbo: self.turbo,
            fast_sleep: self.fast_sleep,
            offline: self.offline,
        })
    }
}

impl eframe::App for Launcher {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        egui::CentralPanel::default().show(ui, |ui| {
            ui.heading("Run firmware");
            ui.add_space(8.0);
            row(ui, "Board", |ui| {
                let selected = self.board.map(|i| label(&self.boards[i])).unwrap_or_else(|| "Choose…".into());
                egui::ComboBox::from_id_salt("board").selected_text(selected).width(360.0).show_ui(ui, |ui| {
                    for (i, b) in self.boards.iter().enumerate() {
                        ui.selectable_value(&mut self.board, Some(i), label(b));
                    }
                });
            });
            row(ui, "Firmware", |ui| {
                let start = self.firmware.as_deref().and_then(Path::parent);
                if ui.button("Choose…").on_hover_text("merged_firmware.bin, with its .elf next to it").clicked()
                    && let Some(p) = crate::pick_firmware(start)
                {
                    self.firmware = Some(p);
                }
                path_label(ui, self.firmware.as_deref(), "none");
            });

            ui.add_space(6.0);
            ui.separator();
            ui.label(RichText::new("Options (blank: the default)").weak());
            row(ui, "MAC address", |ui| {
                ui.add(egui::TextEdit::singleline(&mut self.mac).hint_text("7C:DF:A1:…").desired_width(160.0))
                    .on_hover_text("The device's identity on the server (--mac)");
            });
            row(ui, "Flash file", |ui| {
                let start =
                    self.flash.as_deref().and_then(Path::parent).or(self.firmware.as_deref().and_then(Path::parent));
                if ui.button("Choose…").on_hover_text("Where NVS and SPIFFS persist between runs (--flash)").clicked()
                {
                    let d = rfd::FileDialog::new().set_file_name("sim-flash.bin");
                    if let Some(p) = start.map_or(d.clone(), |s| d.set_directory(s)).save_file() {
                        self.flash = Some(p);
                    }
                }
                clear_button(ui, &mut self.flash);
                path_label(ui, self.flash.as_deref(), "sim-flash.bin next to the firmware");
            });
            row(ui, "OTA firmware", |ui| {
                let start = self
                    .ota_firmware
                    .as_deref()
                    .and_then(Path::parent)
                    .or(self.firmware.as_deref().and_then(Path::parent));
                if ui
                    .button("Choose…")
                    .on_hover_text(
                        "The app image the built-in server offers for updates, its .elf next to it (--ota-firmware)",
                    )
                    .clicked()
                {
                    let d = rfd::FileDialog::new().add_filter("App image", &["bin"]);
                    if let Some(p) = start.map_or(d.clone(), |s| d.set_directory(s)).pick_file() {
                        self.ota_firmware = Some(p);
                    }
                }
                clear_button(ui, &mut self.ota_firmware);
                path_label(ui, self.ota_firmware.as_deref(), "none");
            });
            row(ui, "", |ui| {
                ui.checkbox(&mut self.erase, "Erase flash")
                    .on_hover_text("Start factory-fresh: forget WiFi, API key and settings (--erase; not remembered)");
                ui.checkbox(&mut self.turbo, "Turbo").on_hover_text("Run as fast as possible (--turbo)");
                ui.checkbox(&mut self.fast_sleep, "Fast sleep")
                    .on_hover_text("Fast-forward deep sleeps (--fast-sleep)");
                ui.checkbox(&mut self.offline, "Offline")
                    .on_hover_text("Only the host is reachable from the device (--offline)");
            });

            let problems = self.problems();
            for e in &problems {
                ui.add(egui::Label::new(RichText::new(e).color(ui.visuals().error_fg_color)).wrap());
            }
            ui.add_space(8.0);
            let launch = self.launch().filter(|_| problems.is_empty());
            if ui.add_enabled(launch.is_some(), egui::Button::new("Start")).clicked()
                && let Some(l) = launch
            {
                save_last(&l);
                *self.result.lock().unwrap() = Some(l);
                ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
            }
        });
    }
}

/// A labelled row.
fn row(ui: &mut Ui, label: &str, add: impl FnOnce(&mut Ui)) {
    ui.horizontal(|ui| {
        ui.add_sized([LABEL_W, 20.0], egui::Label::new(label));
        add(ui);
    });
    ui.add_space(2.0);
}

/// A path cut at its start to fit the rest of the row (the full path on hover), or `none`.
fn path_label(ui: &mut Ui, path: Option<&Path>, none: &str) {
    let Some(p) = path else {
        ui.label(RichText::new(none).weak());
        return;
    };
    let shown = p.display().to_string();
    let font = egui::TextStyle::Monospace.resolve(ui.style());
    let char_w = ui.fonts_mut(|f| f.glyph_width(&font, 'x')).max(1.0);
    let fits = (ui.available_width() / char_w) as usize;
    ui.add(egui::Label::new(RichText::new(elide_start(&shown, fits)).monospace()).truncate()).on_hover_text(shown);
}

/// Back to the default, once a path is chosen.
fn clear_button(ui: &mut Ui, path: &mut Option<PathBuf>) {
    if path.is_some() && ui.button("Default").on_hover_text("Forget this path").clicked() {
        *path = None;
    }
}

/// "7C:DF:A1:12:34:56" (or with dashes).
fn parse_mac(s: &str) -> Option<[u8; 6]> {
    let bytes: Vec<u8> = s.trim().split([':', '-']).map(|p| u8::from_str_radix(p, 16).ok()).collect::<Option<_>>()?;
    bytes.try_into().ok()
}

fn format_mac(m: &[u8; 6]) -> String {
    m.iter().map(|b| format!("{b:02X}")).collect::<Vec<_>>().join(":")
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

/// The choices made last time (the launcher starts from them).
fn load_last(app: &mut Launcher) {
    let v: Value = settings_path()
        .and_then(|p| std::fs::read(p).ok())
        .and_then(|d| serde_json::from_slice(&d).ok())
        .unwrap_or_default();
    let path = |k: &str| v[k].as_str().map(PathBuf::from);
    app.board = v["env"].as_str().and_then(|e| app.boards.iter().position(|b| b.env == e));
    app.firmware = path("firmware");
    app.mac = v["mac"].as_str().unwrap_or_default().to_string();
    app.flash = path("flash");
    app.ota_firmware = path("ota_firmware");
    app.turbo = v["turbo"].as_bool().unwrap_or(false);
    app.fast_sleep = v["fast_sleep"].as_bool().unwrap_or(false);
    app.offline = v["offline"].as_bool().unwrap_or(false);
}

/// Remember a launch, except `erase`: a factory reset is never repeated by accident.
fn save_last(l: &Launch) {
    let Some(path) = settings_path() else { return };
    let p = |p: &Option<PathBuf>| p.as_ref().map(|p| p.to_string_lossy().into_owned());
    let v = json!({
        "env": l.env,
        "firmware": l.firmware.to_string_lossy(),
        "mac": l.mac.as_ref().map(format_mac),
        "flash": p(&l.flash),
        "ota_firmware": p(&l.ota_firmware),
        "turbo": l.turbo,
        "fast_sleep": l.fast_sleep,
        "offline": l.offline,
    });
    // Remembering is a convenience: a read-only config directory isn't worth an error.
    let _ = path.parent().map(std::fs::create_dir_all);
    let _ = std::fs::write(path, serde_json::to_vec_pretty(&v).unwrap_or_default());
}

/// Ask for the board, the firmware image and options in a small window (`None`: closed
/// without starting). Call it on the main thread, before [`crate::run`].
pub fn launch(boards: Vec<BoardChoice>) -> anyhow::Result<Option<Launch>> {
    let result = Arc::new(Mutex::new(None));
    let native = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("TRMNL Simulator")
            .with_inner_size([640.0, 360.0])
            .with_icon(crate::app_icon()),
        ..Default::default()
    };
    let mut app = Launcher { boards, result: result.clone(), ..Default::default() };
    load_last(&mut app);
    eframe::run_native("TRMNL Simulator launcher", native, Box::new(move |_| Ok(Box::new(app))))
        .map_err(|e| anyhow::anyhow!("GUI error: {e}"))?;
    Ok(result.lock().unwrap().take())
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui_kittest::kittest::Queryable;

    #[test]
    fn mac() {
        assert_eq!(parse_mac("7c:df:a1:12:34:56"), Some([0x7c, 0xdf, 0xa1, 0x12, 0x34, 0x56]));
        assert_eq!(parse_mac(" 7C-DF-A1-12-34-56 "), Some([0x7c, 0xdf, 0xa1, 0x12, 0x34, 0x56]));
        assert_eq!(parse_mac("7C:DF:A1:12:34"), None);
        assert_eq!(format_mac(&[0x7c, 0xdf, 0xa1, 0x12, 0x34, 0x56]), "7C:DF:A1:12:34:56");
    }

    #[test]
    fn elides_the_start() {
        assert_eq!(elide_start("/a/b/c.bin", 20), "/a/b/c.bin");
        assert_eq!(elide_start("/a/b/c.bin", 6), "…c.bin");
    }

    /// A long firmware path is cut short at its start; Choose… and Start stay in the window.
    #[test]
    fn long_path_fits() {
        let boards = vec![BoardChoice { env: "trmnl".into(), name: "TRMNL OG".into() }];
        let dir = std::env::temp_dir().join("very-long-directory-name".repeat(8)).join("TRMNL_X");
        std::fs::create_dir_all(&dir).unwrap();
        let long = dir.join("merged_firmware.bin");
        std::fs::write(&long, b"").unwrap();
        std::fs::write(long.with_extension("elf"), b"").unwrap();
        let app = Launcher { boards, board: Some(0), firmware: Some(long), ..Default::default() };
        let mut h = egui_kittest::Harness::builder()
            .with_size([640.0, 360.0])
            .with_pixels_per_point(2.0)
            .build_eframe(move |_| app);
        h.run();
        let window = h.ctx.content_rect();
        for node in h.query_all_by_label("Choose…").chain([h.get_by_label("Start")]) {
            let r = node.rect();
            assert!(window.contains_rect(r), "{r:?} is outside {window:?}");
        }
        assert!(h.query_by_label_contains("TRMNL_X/merged_firmware.bin").is_some(), "the path's end is shown");
        if let Ok(dir) = std::env::var("SIM_UI_RENDER_DIR") {
            h.render().expect("render").save(format!("{dir}/launcher.png")).expect("save");
        }
    }
}
