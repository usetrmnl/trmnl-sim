//! The window shown when the simulator starts without a firmware image: pick a released build
//! (downloaded, its board known) or a local image and its board, and optionally the MAC and
//! erasing the flash; then the simulator window opens. The last choice is remembered for next
//! time, except erasing the flash.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crossbeam_channel::{Receiver, TryRecvError, unbounded};
use egui::{RichText, Ui};
use serde_json::{Value, json};

use crate::releases::{self, Model, Progress, ReleaseFiles, Version};

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
    /// `--erase`
    pub erase: bool,
    /// `--hot-reload`
    pub hot_reload: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
enum Tab {
    #[default]
    Release,
    Custom,
}

/// The list of released firmware, fetched when the Release tab is first shown.
#[derive(Default)]
enum Catalog {
    #[default]
    NotLoaded,
    Loading(Receiver<Result<Vec<Model>, String>>),
    Ready(Vec<Model>),
    Failed(String),
}

enum DownloadEvent {
    Progress(Progress),
    Done(Result<(), String>),
}

/// A release being downloaded, to start when it's done.
struct Download {
    events: Receiver<DownloadEvent>,
    progress: Option<Progress>,
    cancel: Arc<AtomicBool>,
    launch: Launch,
}

impl Drop for Download {
    /// Closing the window or pressing Cancel stops the transfer.
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

#[derive(Default)]
struct Launcher {
    boards: Vec<BoardChoice>,
    tab: Tab,
    // Custom firmware.
    board: Option<usize>,
    firmware: Option<PathBuf>,
    hot_reload: bool,
    // Release firmware.
    catalog: Catalog,
    /// The model's `keyname` and the version's name, which outlive the list (remembered).
    release_model: Option<String>,
    release_version: Option<String>,
    /// `--env`: the board to preselect once the list is in.
    wanted_env: Option<String>,
    download: Option<Download>,
    download_error: Option<String>,
    // Both.
    mac: String,
    erase: bool,
    result: Arc<Mutex<Option<Launch>>>,
}

/// Width of the labels in front of each row.
const LABEL_W: f32 = 96.0;
const WINDOW_SIZE: [f32; 2] = [640.0, 320.0];

impl Launcher {
    /// Use this firmware image, and the board its file name names (`FW1.8.17-trmnl.bin`), if any.
    fn set_firmware(&mut self, p: PathBuf) {
        let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        if let Some(env) = sim_api::env_in_file_name(&name, self.boards.iter().map(|b| b.env.as_str())) {
            self.board = self.boards.iter().position(|b| b.env == env);
        }
        self.firmware = Some(p);
    }

    /// What is wrong with the choices, if anything (Start stays disabled).
    fn problems(&self) -> Vec<String> {
        let mut v = Vec::new();
        if self.tab == Tab::Custom
            && let Some(p) = &self.firmware
        {
            for f in [p.clone(), p.with_extension("elf")] {
                if !f.is_file() {
                    v.push(format!("{} not found", f.display()));
                }
            }
        }
        if self.tab == Tab::Release {
            v.extend(self.download_error.clone());
        }
        if !self.mac.trim().is_empty() && parse_mac(&self.mac).is_none() {
            v.push("MAC address: expected 6 hex bytes like 7C:DF:A1:12:34:56".into());
        }
        v
    }

    fn launch(&self) -> Option<Launch> {
        let (env, firmware, hot_reload) = match self.tab {
            Tab::Custom => (self.boards.get(self.board?)?.env.clone(), self.firmware.clone()?, self.hot_reload),
            Tab::Release => {
                let (env, version) = self.selected_release()?;
                // A release never changes, so there's nothing to reload.
                (env.to_string(), ReleaseFiles::new(&firmware_root()?, env, &version.version).bin, false)
            }
        };
        Some(Launch { env, firmware, mac: parse_mac(&self.mac), erase: self.erase, hot_reload })
    }

    /// The chosen release, if it can run.
    fn selected_release(&self) -> Option<(&'static str, &Version)> {
        let Catalog::Ready(models) = &self.catalog else { return None };
        let model = models.iter().find(|m| Some(&m.keyname) == self.release_model.as_ref())?;
        let version = model.versions.iter().find(|v| Some(&v.version) == self.release_version.as_ref())?;
        let env = self.runnable_env(model, version)?;
        Some((env, version))
    }

    /// The board that runs this release: it needs a board here and an ELF.
    fn runnable_env(&self, model: &Model, version: &Version) -> Option<&'static str> {
        version.elf_url.as_ref()?;
        self.env_here(model, version)
    }

    /// The env of this release, if the simulator has its board.
    fn env_here(&self, model: &Model, version: &Version) -> Option<&'static str> {
        let env = releases::env_of(&model.keyname, &version.version)?;
        self.boards.iter().any(|b| b.env == env).then_some(env)
    }

    /// Whether the simulator has a board for any build of this model.
    fn model_has_board(&self, model: &Model) -> bool {
        model.versions.iter().any(|v| self.env_here(model, v).is_some())
    }

    /// Make the selection valid for the list: `--env`'s board, else the remembered model, else
    /// the first with a board; the remembered version, else the newest that can run.
    fn settle_release(&mut self) {
        let Catalog::Ready(models) = &self.catalog else { return };
        let on_env = |m: &&Model| {
            let wanted = self.wanted_env.as_deref();
            wanted.is_some() && m.versions.iter().any(|v| releases::env_of(&m.keyname, &v.version) == wanted)
        };
        let remembered = |m: &&Model| Some(&m.keyname) == self.release_model.as_ref() && self.model_has_board(m);
        let model = models
            .iter()
            .find(on_env)
            .or_else(|| models.iter().find(remembered))
            .or_else(|| models.iter().find(|m| self.model_has_board(m)));
        let Some(model) = model else {
            (self.release_model, self.release_version) = (None, None);
            return;
        };
        let runnable = |v: &&Version| self.runnable_env(model, v).is_some();
        let on_wanted = |v: &&Version| runnable(v) && self.runnable_env(model, v) == self.wanted_env.as_deref();
        let version = model
            .versions
            .iter()
            .find(on_wanted)
            .or_else(|| {
                model.versions.iter().find(|v| runnable(v) && Some(&v.version) == self.release_version.as_ref())
            })
            .or_else(|| model.versions.iter().find(runnable));
        let chosen = (Some(model.keyname.clone()), version.map(|v| v.version.clone()));
        (self.release_model, self.release_version) = chosen;
    }

    /// Fetch the list when the tab is first shown, and take it when it's in.
    fn poll_catalog(&mut self, ctx: &egui::Context) {
        if self.tab == Tab::Release && matches!(self.catalog, Catalog::NotLoaded) {
            self.catalog = fetch_catalog();
        }
        let Catalog::Loading(rx) = &self.catalog else { return };
        match rx.try_recv() {
            Ok(Ok(models)) => {
                self.catalog = Catalog::Ready(models);
                self.settle_release();
                self.wanted_env = None;
            }
            Ok(Err(e)) => self.catalog = Catalog::Failed(e),
            Err(TryRecvError::Disconnected) => self.catalog = Catalog::Failed("the download stopped".into()),
            Err(TryRecvError::Empty) => ctx.request_repaint_after(Duration::from_millis(100)),
        }
    }

    /// Take the download's progress; start when it's done.
    fn poll_download(&mut self, ctx: &egui::Context) {
        let Some(d) = &mut self.download else { return };
        let mut finished = None;
        loop {
            match d.events.try_recv() {
                Ok(DownloadEvent::Progress(p)) => d.progress = Some(p),
                Ok(DownloadEvent::Done(r)) => finished = Some(r),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    finished.get_or_insert(Err("the download stopped".into()));
                    break;
                }
            }
        }
        ctx.request_repaint_after(Duration::from_millis(100));
        match finished {
            Some(Ok(())) => {
                if let Some(d) = self.download.take() {
                    self.finish(ctx, d.launch.clone());
                }
            }
            Some(Err(e)) => {
                self.download = None;
                self.download_error = Some(format!("Download failed: {e}"));
            }
            None => {}
        }
    }

    /// Download the selected release; [`Self::poll_download`] starts it.
    fn start_download(&mut self, launch: Launch, files: ReleaseFiles) {
        let Some((_, version)) = self.selected_release() else { return };
        let version = version.clone();
        let (tx, events) = unbounded();
        let cancel = Arc::new(AtomicBool::new(false));
        let stop = cancel.clone();
        std::thread::spawn(move || {
            let progress = tx.clone();
            let send = move |p| drop(progress.send(DownloadEvent::Progress(p)));
            let result = releases::download(&version, &files, &stop, send);
            let _ = tx.send(DownloadEvent::Done(result));
        });
        self.download_error = None;
        self.download = Some(Download { events, progress: None, cancel, launch });
    }

    /// Remember the choices and hand over to the simulator.
    fn finish(&mut self, ctx: &egui::Context, launch: Launch) {
        self.save_last();
        *self.result.lock().unwrap() = Some(launch);
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }

    /// Remember the choices of both tabs, except `erase`: a factory reset is never repeated by
    /// accident.
    fn save_last(&self) {
        let Some(path) = settings_path() else { return };
        let v = json!({
            "tab": if self.tab == Tab::Release { "release" } else { "custom" },
            "env": self.board.and_then(|i| self.boards.get(i)).map(|b| &b.env),
            "firmware": self.firmware.as_ref().map(|p| p.to_string_lossy()),
            "hot_reload": self.hot_reload,
            "release_model": self.release_model,
            "release_version": self.release_version,
            "mac": parse_mac(&self.mac).as_ref().map(format_mac),
        });
        // Remembering is a convenience: a read-only config directory isn't worth an error.
        let _ = path.parent().map(std::fs::create_dir_all);
        let _ = std::fs::write(path, serde_json::to_vec_pretty(&v).unwrap_or_default());
    }

    fn custom_tab(&mut self, ui: &mut Ui) {
        row(ui, "Firmware", |ui| {
            let start = self.firmware.as_deref().and_then(Path::parent);
            if ui.button("Choose…").on_hover_text("merged_firmware.bin, with its .elf next to it").clicked()
                && let Some(p) = crate::pick_firmware(start)
            {
                self.set_firmware(p);
            }
            path_label(ui, self.firmware.as_deref(), "none");
        });
        row(ui, "", |ui| {
            ui.checkbox(&mut self.hot_reload, "Hot reload").on_hover_text(
                "On each wake, reset or power cycle, load the firmware again if it was rebuilt (--hot-reload)",
            );
        });

        row(ui, "Board", |ui| {
            let selected = self.board.map(|i| label(&self.boards[i])).unwrap_or_else(|| "Choose…".into());
            egui::ComboBox::from_id_salt("board").selected_text(selected).width(360.0).show_ui(ui, |ui| {
                for (i, b) in self.boards.iter().enumerate() {
                    ui.selectable_value(&mut self.board, Some(i), label(b));
                }
            });
        });
    }

    fn release_tab(&mut self, ui: &mut Ui) {
        match &self.catalog {
            Catalog::NotLoaded | Catalog::Loading(_) => {
                row(ui, "", |ui| {
                    ui.spinner();
                    ui.label("Getting the list of releases…");
                });
                return;
            }
            Catalog::Failed(e) => {
                let e = e.clone();
                ui.add(egui::Label::new(RichText::new(e).color(ui.visuals().error_fg_color)).wrap());
                ui.add_space(4.0);
                if ui.button("Retry").clicked() {
                    self.catalog = Catalog::NotLoaded;
                }
                return;
            }
            Catalog::Ready(_) => {}
        }
        let Catalog::Ready(models) = &self.catalog else { return };
        let current = models.iter().find(|m| Some(&m.keyname) == self.release_model.as_ref());
        // (name, shown, can be chosen)
        let model_items: Vec<(String, String, bool)> = models
            .iter()
            .map(|m| {
                let has_board = self.model_has_board(m);
                let text = if has_board { m.label.clone() } else { format!("{} (not simulated)", m.label) };
                (m.keyname.clone(), text, has_board)
            })
            .collect();
        let version_items: Vec<(String, String, bool)> = current
            .iter()
            .flat_map(|m| m.versions.iter().map(|v| (*m, v)))
            .map(|(m, v)| {
                let runnable = self.runnable_env(m, v).is_some();
                let text = if runnable { v.version.clone() } else { format!("{} (no ELF)", v.version) };
                (v.version.clone(), text, runnable)
            })
            .collect();
        let model_text = current.map_or_else(|| "Choose…".to_string(), |m| m.label.clone());

        let mut new_model = None;
        row(ui, "Device", |ui| {
            egui::ComboBox::from_id_salt("model").selected_text(model_text).width(420.0).show_ui(ui, |ui| {
                let hint = "The simulator has no board for this device yet";
                new_model = combo_items(ui, &model_items, self.release_model.as_deref(), hint);
            });
        });
        if let Some(m) = new_model {
            if self.release_model.as_ref() != Some(&m) {
                // Another device's versions: take its newest that can run.
                self.release_model = Some(m);
                self.release_version = None;
                self.settle_release();
            }
            return;
        }
        let mut new_version = None;
        row(ui, "Version", |ui| {
            let text = self.release_version.as_deref().unwrap_or("None");
            egui::ComboBox::from_id_salt("version").selected_text(text).width(420.0).show_ui(ui, |ui| {
                let hint = "This build was published without its ELF, which the simulator needs";
                new_version = combo_items(ui, &version_items, self.release_version.as_deref(), hint);
            });
        });
        if new_version.is_some() {
            self.release_version = new_version;
        }
        row(ui, "Board", |ui| match self.selected_release() {
            Some((env, _)) => {
                let name = self.boards.iter().find(|b| b.env == env).map_or(env, |b| b.name.as_str());
                ui.label(format!("{name} ({env})"));
            }
            None => {
                ui.label(RichText::new("No build of this device has been published with its ELF yet").weak());
            }
        });
    }

    /// The transfer in progress, with a way out.
    fn download_row(&mut self, ui: &mut Ui) {
        let Some(d) = &self.download else { return };
        let mut cancel = false;
        ui.horizontal(|ui| {
            cancel = ui.button("Cancel").clicked();
            let (fraction, text) = match &d.progress {
                Some(p) => {
                    let done = format!("{} {:.1} MB", p.file, p.done as f32 / 1e6);
                    match p.total {
                        Some(t) if t > 0 => (p.done as f32 / t as f32, format!("{done} of {:.1} MB", t as f32 / 1e6)),
                        _ => (0.0, done),
                    }
                }
                None => (0.0, "Connecting…".into()),
            };
            ui.add(egui::ProgressBar::new(fraction).text(text));
        });
        if cancel {
            self.download = None;
        }
    }
}

/// Fetch the list on another thread.
fn fetch_catalog() -> Catalog {
    let (tx, rx) = unbounded();
    std::thread::spawn(move || {
        let _ = tx.send(releases::fetch_catalog());
    });
    Catalog::Loading(rx)
}

impl eframe::App for Launcher {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.poll_catalog(&ctx);
        self.poll_download(&ctx);
        egui::CentralPanel::default().show(ui, |ui| {
            let idle = self.download.is_none();
            ui.add_enabled_ui(idle, |ui| {
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut self.tab, Tab::Release, "Release firmware");
                    ui.selectable_value(&mut self.tab, Tab::Custom, "Custom firmware");
                });
                ui.separator();
                ui.add_space(4.0);
                match self.tab {
                    Tab::Release => self.release_tab(ui),
                    Tab::Custom => self.custom_tab(ui),
                }
                ui.add_space(6.0);
                ui.separator();
                ui.label(RichText::new("Options (blank: the default)").weak());
                row(ui, "MAC address", |ui| {
                    ui.add(egui::TextEdit::singleline(&mut self.mac).hint_text("7C:DF:A1:…").desired_width(160.0))
                        .on_hover_text("The device's identity on the server (--mac)");
                });
                row(ui, "", |ui| {
                    ui.checkbox(&mut self.erase, "Erase flash").on_hover_text(
                        "Start factory-fresh: forget WiFi, API key and settings (--erase; not remembered)",
                    );
                });
            });

            let problems = self.problems();
            for e in &problems {
                ui.add(egui::Label::new(RichText::new(e).color(ui.visuals().error_fg_color)).wrap());
            }
            ui.add_space(8.0);
            self.download_row(ui);
            // A failed download can be tried again, so it doesn't hold Start back.
            let blocked = problems.iter().any(|p| Some(p) != self.download_error.as_ref());
            let launch = self.launch().filter(|_| !blocked && self.download.is_none());
            if ui.add_enabled(launch.is_some(), egui::Button::new("Start")).clicked()
                && let Some(l) = launch
            {
                let files = (self.tab == Tab::Release)
                    .then(|| self.selected_release().map(|(env, v)| (env, v.version.clone())))
                    .flatten()
                    .and_then(|(env, v)| Some(ReleaseFiles::new(&firmware_root()?, env, &v)));
                match files {
                    Some(f) if !f.downloaded() => self.start_download(l, f),
                    _ => self.finish(ui.ctx(), l),
                }
            }
        });
    }
}

/// The entries of a combo box, those that can't be chosen greyed out with `hint`: the one clicked.
fn combo_items(ui: &mut Ui, items: &[(String, String, bool)], chosen: Option<&str>, hint: &str) -> Option<String> {
    let mut clicked = None;
    for (name, text, enabled) in items {
        let selected = chosen == Some(name.as_str());
        let item = ui.add_enabled(*enabled, egui::Button::selectable(selected, text)).on_disabled_hover_text(hint);
        if item.clicked() {
            clicked = Some(name.clone());
        }
    }
    clicked
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

/// The simulator's directory in the user's config directory (`~/Library/Application Support`
/// on macOS, `%APPDATA%` on Windows, else `$XDG_CONFIG_HOME` or `~/.config`).
fn config_dir() -> Option<PathBuf> {
    let home = || std::env::var_os("HOME").map(PathBuf::from);
    let dir = if cfg!(target_os = "macos") {
        home()?.join("Library/Application Support")
    } else if cfg!(windows) {
        PathBuf::from(std::env::var_os("APPDATA")?)
    } else {
        std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).or_else(|| Some(home()?.join(".config")))?
    };
    Some(dir.join("trmnl-sim"))
}

/// Where the last choice is kept.
fn settings_path() -> Option<PathBuf> {
    Some(config_dir()?.join("preferences.json"))
}

/// Where downloaded releases are kept, with each board's flash image: not a cache, as that flash
/// holds the device's WiFi and API key.
fn firmware_root() -> Option<PathBuf> {
    Some(config_dir()?.join("firmware"))
}

/// The choices made last time (the launcher starts from them).
fn load_last(app: &mut Launcher) {
    let v: Value = settings_path()
        .and_then(|p| std::fs::read(p).ok())
        .and_then(|d| serde_json::from_slice(&d).ok())
        .unwrap_or_default();
    apply_last(app, &v);
}

fn apply_last(app: &mut Launcher, v: &Value) {
    app.board = v["env"].as_str().and_then(|e| app.boards.iter().position(|b| b.env == e));
    app.firmware = v["firmware"].as_str().map(PathBuf::from);
    app.mac = v["mac"].as_str().unwrap_or_default().to_string();
    app.hot_reload = v["hot_reload"].as_bool().unwrap_or(false);
    app.release_model = v["release_model"].as_str().map(Into::into);
    app.release_version = v["release_version"].as_str().map(Into::into);
    // Release unless the last start was from Custom.
    app.tab = if v["tab"] == "custom" { Tab::Custom } else { Tab::Release };
}

/// Ask for the firmware (a release, or a local image and its board), the MAC and whether to
/// erase in a small window (`None`: closed without starting); `env` (`--env`) preselects the
/// board. Call it on the main thread, before [`crate::run`].
pub fn launch(boards: Vec<BoardChoice>, env: Option<String>) -> anyhow::Result<Option<Launch>> {
    let result = Arc::new(Mutex::new(None));
    let native = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("TRMNL Simulator")
            .with_inner_size(WINDOW_SIZE)
            .with_icon(crate::app_icon()),
        ..Default::default()
    };
    let mut app = Launcher { boards, result: result.clone(), ..Default::default() };
    load_last(&mut app);
    if let Some(e) = env {
        app.board = app.boards.iter().position(|b| b.env == e).or(app.board);
        app.wanted_env = Some(e);
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
    fn board_from_the_file_name() {
        let boards = ["trmnl", "TRMNL_X"].map(|e| BoardChoice { env: e.into(), name: e.into() }).into();
        let mut app = Launcher { boards, board: Some(0), ..Default::default() };
        app.set_firmware("/fw/FW1.8.17-TRMNL_X.bin".into());
        assert_eq!(app.board, Some(1));
        // A name without an env leaves the board as it was.
        app.set_firmware("/fw/merged_firmware.bin".into());
        assert_eq!(app.board, Some(1));
    }

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
        let app = Launcher { boards, tab: Tab::Custom, board: Some(0), firmware: Some(long), ..Default::default() };
        let mut h = egui_kittest::Harness::builder()
            .with_size(WINDOW_SIZE)
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

    /// Release is the default, also for preferences saved before there were tabs.
    #[test]
    fn opens_on_release_unless_custom_was_last() {
        let tab = |v: Value| {
            let mut app = Launcher::default();
            apply_last(&mut app, &v);
            app.tab
        };
        assert_eq!(tab(Value::Null), Tab::Release);
        assert_eq!(tab(json!({"firmware": "/fw/a.bin", "env": "trmnl"})), Tab::Release);
        assert_eq!(tab(json!({"tab": "release"})), Tab::Release);
        assert_eq!(tab(json!({"tab": "custom"})), Tab::Custom);
    }

    fn boards(envs: &[&str]) -> Vec<BoardChoice> {
        envs.iter().map(|e| BoardChoice { env: (*e).into(), name: format!("{e} board") }).collect()
    }

    fn version(name: &str, elf: bool) -> Version {
        Version {
            version: name.into(),
            url: format!("https://x/{name}.bin"),
            elf_url: elf.then(|| format!("https://x/{name}.elf")),
        }
    }

    fn model(keyname: &str, versions: Vec<Version>) -> Model {
        Model { keyname: keyname.into(), label: format!("{keyname} label"), versions }
    }

    fn release_launcher(models: Vec<Model>) -> Launcher {
        let boards = boards(&["trmnl", "TRMNL_X", "xteink_x4", "xteink_x4_pwr_btn"]);
        Launcher { boards, tab: Tab::Release, catalog: Catalog::Ready(models), ..Default::default() }
    }

    /// Choosing a release chooses its board, and only a build with an ELF can be chosen.
    #[test]
    fn newest_runnable_version_of_the_first_device_with_a_board() {
        let mut app = release_launcher(vec![
            model("seeed_E1003", vec![version("FW3", true)]), // no such board here
            model("trmnl", vec![version("FW2", false), version("FW1", true)]),
        ]);
        app.settle_release();
        assert_eq!(app.release_model.as_deref(), Some("trmnl"));
        assert_eq!(app.release_version.as_deref(), Some("FW1"));
        assert_eq!(app.selected_release().map(|(env, _)| env), Some("trmnl"));
        assert_eq!(app.launch().map(|l| l.env).as_deref(), Some("trmnl"));
    }

    #[test]
    fn a_device_without_elfs_has_nothing_to_start() {
        let mut app = release_launcher(vec![model("trmnl", vec![version("FW1", false)])]);
        app.settle_release();
        assert_eq!(app.release_model.as_deref(), Some("trmnl"));
        assert_eq!(app.release_version, None);
        assert!(app.launch().is_none());
    }

    #[test]
    fn remembered_choice_is_kept_if_it_can_still_run() {
        let list = || {
            vec![
                model("trmnl", vec![version("FW1", true)]),
                model("trmnl_x", vec![version("FW3", true), version("FW2", true), version("FW1", false)]),
            ]
        };
        let mut app = release_launcher(list());
        (app.release_model, app.release_version) = (Some("trmnl_x".into()), Some("FW2".into()));
        app.settle_release();
        assert_eq!((app.release_model.as_deref(), app.release_version.as_deref()), (Some("trmnl_x"), Some("FW2")));
        // FW1 lost its ELF: the newest takes over.
        let mut app = release_launcher(list());
        (app.release_model, app.release_version) = (Some("trmnl_x".into()), Some("FW1".into()));
        app.settle_release();
        assert_eq!(app.release_version.as_deref(), Some("FW3"));
    }

    /// `--env` picks the device, and the variant build of a model that has one.
    #[test]
    fn env_option_preselects_the_release() {
        let x4 = vec![version("FW2", true), version("FW1-push-to-refresh", true)];
        let mut app = release_launcher(vec![model("trmnl", vec![version("FW1", true)]), model("xteink_x4", x4)]);
        app.wanted_env = Some("xteink_x4_pwr_btn".into());
        app.settle_release();
        assert_eq!(app.release_version.as_deref(), Some("FW1-push-to-refresh"));
        assert_eq!(app.selected_release().map(|(env, _)| env), Some("xteink_x4_pwr_btn"));
    }

    /// The Release tab shows the device, the version and the board they imply, within the window.
    #[test]
    fn release_tab_names_the_board() {
        let mut app = release_launcher(vec![
            model("trmnl", vec![version("FW1.8.18", true), version("FW1.8.17", false)]),
            model("seeed_E1003", vec![version("FW1.8.18", true)]),
        ]);
        app.settle_release();
        let mut h = egui_kittest::Harness::builder()
            .with_size(WINDOW_SIZE)
            .with_pixels_per_point(2.0)
            .build_eframe(move |_| app);
        h.run();
        assert!(h.query_by_label("trmnl board (trmnl)").is_some());
        let window = h.ctx.content_rect();
        let r = h.get_by_label("Start").rect();
        assert!(window.contains_rect(r), "{r:?} is outside {window:?}");
        if let Ok(dir) = std::env::var("SIM_UI_RENDER_DIR") {
            h.render().expect("render").save(format!("{dir}/launcher-release.png")).expect("save");
        }
    }

    /// Against the real API and bucket, into `$HOME`'s config directory: the list loads, the
    /// newest OG build with an ELF downloads on Start, and the launch points at it
    /// (`HOME=/some/dir cargo test -p sim-ui live_release -- --ignored`).
    #[test]
    #[ignore = "uses the network and downloads a firmware"]
    fn live_release_download() {
        let boards = boards(&["trmnl"]);
        let result = Arc::new(Mutex::new(None));
        let app = Launcher { boards, tab: Tab::Release, result: result.clone(), ..Default::default() };
        let mut h = egui_kittest::Harness::builder().with_size(WINDOW_SIZE).build_eframe(move |_| app);
        let wait = |h: &mut egui_kittest::Harness<'_, Launcher>, done: &dyn Fn(&Launcher) -> bool| {
            let start = std::time::Instant::now();
            while !done(h.state()) {
                assert!(start.elapsed() < Duration::from_secs(300), "timed out");
                std::thread::sleep(Duration::from_millis(50));
                h.step();
            }
        };
        wait(&mut h, &|a| matches!(a.catalog, Catalog::Ready(_) | Catalog::Failed(_)));
        h.step();
        assert_eq!(h.state().release_model.as_deref(), Some("trmnl"));
        h.get_by_label("Start").click();
        h.step();
        h.step();
        wait(&mut h, &|a| a.download.is_none());
        assert_eq!(h.state().download_error, None);
        let launch = result.lock().unwrap().clone().expect("started");
        println!("{launch:?}");
        assert!(launch.firmware.is_file() && launch.firmware.with_extension("elf").is_file());
    }
}
