//! Interactive desktop front-end for the TRMNL simulator (eframe/egui).
//!
//! The emulator runs on its own thread and publishes state through a [`sim_api::SimHandle`];
//! this crate only reads that shared state and sends [`sim_api::Command`]s.

mod bluetooth;
mod console;
mod device;
mod faults;
mod launcher;
mod preferences;
mod server;
mod touch;

use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Context as _;
use egui::{
    Align, Align2, Color32, FontId, Key, KeyboardShortcut, Layout, Modifiers, Pos2, Rect, RichText, Sense, Stroke,
    StrokeKind, Ui, Vec2,
};
use sim_api::{BoardInfo, Command, RunState, SavePointInfo, SavePointSource, SimHandle, Status};

use console::ConsoleView;
use device::{Geometry, Look, Screen, ZoneVis};
use server::ServerPanel;
use touch::{TouchInput, ZONES};

/// Options for [`run`].
#[derive(Clone, Debug)]
pub struct UiOptions {
    /// Window title.
    pub title: String,
    /// Initial display zoom (1.0 = one e-paper pixel per logical point).
    /// A value `<= 0.0` starts in "fit to window" mode.
    pub scale: f32,
    /// The built-in mock TRMNL server, for the "Server" panel (shown at startup if it runs).
    pub mock: Option<mock_trmnl::MockServer>,
    /// The app image the Server panel offers for OTA updates (`--ota-firmware`).
    pub ota_firmware: Option<std::path::PathBuf>,
}

impl Default for UiOptions {
    fn default() -> Self {
        UiOptions { title: "TRMNL Simulator".to_string(), scale: 1.0, mock: None, ota_firmware: None }
    }
}

/// The window/dock icon: a TRMNL OG showing the TRMNL glyph (assets/icon.svg, rendered
/// with `rsvg-convert -w 512 -h 512 icon.svg -o icon.png`).
fn app_icon() -> egui::IconData {
    let mut dec = png::Decoder::new(std::io::Cursor::new(&include_bytes!("../assets/icon.png")[..]));
    dec.set_transformations(png::Transformations::normalize_to_color8() | png::Transformations::ALPHA);
    let mut r = dec.read_info().expect("icon.png");
    let mut buf = vec![0; r.output_buffer_size().expect("icon size")];
    let info = r.next_frame(&mut buf).expect("icon.png");
    buf.truncate(info.buffer_size());
    egui::IconData { rgba: buf, width: info.width, height: info.height }
}

pub use launcher::{BoardChoice, Launch, launch};

/// Ask for a merged firmware image with an open dialog, starting in `dir` (`None`: cancelled).
fn pick_firmware(dir: Option<&std::path::Path>) -> Option<std::path::PathBuf> {
    let dialog = rfd::FileDialog::new();
    let dialog = match dir {
        Some(d) => dialog.set_directory(d),
        None => dialog,
    };
    dialog
        .set_title("Open a merged firmware image (merged_firmware.bin, its .elf next to it)")
        .add_filter("Merged firmware image", &["bin"])
        .pick_file()
}

/// Open the simulator window. Must be called on the main thread; blocks until the window
/// closes, then sends [`Command::Quit`].
pub fn run(handle: SimHandle, opts: UiOptions) -> anyhow::Result<()> {
    let native = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(opts.title.clone())
            .with_inner_size([1280.0, 860.0])
            .with_min_inner_size([640.0, 420.0])
            .with_icon(app_icon()),
        ..Default::default()
    };
    let h = handle.clone();
    let result =
        eframe::run_native(&opts.title.clone(), native, Box::new(move |cc| Ok(Box::new(SimApp::new(cc, h, opts)))));
    handle.send(Command::Quit);
    result.map_err(|e| anyhow::anyhow!("GUI error: {e}"))
}

// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Debug)]
enum Zoom {
    Fit,
    Fixed(f32),
}

/// The left utility panel's tabs; the panel is collapsed when none is selected.
#[derive(Clone, Copy, PartialEq, Debug)]
enum LeftTab {
    Server,
    Faults,
    Nvs,
    Bluetooth,
}

impl LeftTab {
    fn label(self) -> &'static str {
        match self {
            LeftTab::Server => "Server",
            LeftTab::Bluetooth => "BLE",
            LeftTab::Faults => "Faults",
            LeftTab::Nvs => "NVS",
        }
    }

    fn hover(self) -> &'static str {
        match self {
            LeftTab::Server => "The built-in TRMNL server: serve your own images to the device",
            LeftTab::Bluetooth => "The mock Bluetooth controller: advertisement and a manual central",
            LeftTab::Faults => "Network, flash and hardware faults",
            LeftTab::Nvs => "Saved firmware preferences in live NVS flash",
        }
    }
}

/// A value we asked the emulator to change; shown optimistically until the status catches up.
struct Pending<T> {
    value: T,
    since: Instant,
}

impl<T: PartialEq + Clone> Pending<T> {
    const TIMEOUT: Duration = Duration::from_millis(1500);

    fn resolve(slot: &mut Option<Self>, actual: T) -> T {
        match slot {
            Some(p) if p.value != actual && p.since.elapsed() < Self::TIMEOUT => p.value.clone(),
            _ => {
                *slot = None;
                actual
            }
        }
    }

    fn set(slot: &mut Option<Self>, value: T) {
        *slot = Some(Pending { value, since: Instant::now() });
    }
}

#[derive(Default)]
struct ButtonState {
    /// What we last sent to the emulator.
    down: bool,
    pressed_at: Option<Instant>,
    last_release: Option<Instant>,
    /// Duration of the previous press.
    last_hold: Option<Duration>,
    /// Gap between the previous release and the latest press (for double-click feedback).
    last_gap: Option<Duration>,
    mouse_held: bool,
    space_held: bool,
}

struct Notice {
    text: String,
    error: bool,
    at: Instant,
}

struct SimApp {
    h: SimHandle,
    status: Status,
    screen: Screen,
    console: ConsoleView,
    show_console: bool,
    preferences: preferences::PreferencesPanel,
    bluetooth: bluetooth::BluetoothPanel,
    zoom: Zoom,
    /// The user picked a zoom preset (disables the initial auto-fit fallback).
    zoom_user_set: bool,
    white_bezel: bool,
    /// Show the panel's refresh flashes (boards with `has_refresh_flashing`).
    refresh_flashing: bool,
    touch: TouchInput,
    docked_pending: Option<Pending<bool>>,
    button: ButtonState,
    battery_pending: Option<Pending<u32>>,
    turbo_pending: Option<Pending<bool>>,
    wifi_pending: Option<Pending<bool>>,
    pause_pending: Option<Pending<bool>>,
    /// A save point command waiting for the emulator's answer (true = restore).
    savepoint_reply: Option<(crossbeam_channel::Receiver<Result<SavePointInfo, String>>, bool)>,
    faults_pending: Option<Pending<sim_api::Faults>>,
    notice: Option<Notice>,
    server: Option<ServerPanel>,
    left_tab: Option<LeftTab>,
}

impl SimApp {
    fn new(cc: &eframe::CreationContext<'_>, h: SimHandle, opts: UiOptions) -> Self {
        // Follows the system theme by default; SIM_UI_THEME=light|dark forces one.
        match std::env::var("SIM_UI_THEME").as_deref() {
            Ok("light") => cc.egui_ctx.set_theme(egui::Theme::Light),
            Ok("dark") => cc.egui_ctx.set_theme(egui::Theme::Dark),
            _ => {}
        }
        let status = h.status.lock().clone();
        let server = opts.mock.clone().map(|m| ServerPanel::new(m, opts.ota_firmware.clone()));
        SimApp {
            left_tab: server.as_ref().is_some_and(|s| s.is_running()).then_some(LeftTab::Server),
            server,
            screen: Screen::new(h.frame.clone()),
            console: ConsoleView::new(h.console.clone()),
            status,
            h,
            show_console: true,
            preferences: preferences::PreferencesPanel::default(),
            bluetooth: bluetooth::BluetoothPanel::default(),
            zoom: if opts.scale > 0.0 { Zoom::Fixed(opts.scale) } else { Zoom::Fit },
            zoom_user_set: false,
            white_bezel: false,
            refresh_flashing: true,
            touch: TouchInput::default(),
            docked_pending: None,
            button: ButtonState::default(),
            battery_pending: None,
            turbo_pending: None,
            wifi_pending: None,
            pause_pending: None,
            savepoint_reply: None,
            faults_pending: None,
            notice: None,
        }
    }

    fn send(&self, c: Command) {
        self.h.send(c);
    }

    /// The board description, defaulting to an OG-style button board when the emulator
    /// hasn't published one (all-false `BoardInfo::default()`).
    fn board(&self) -> BoardInfo {
        let mut b = self.status.board.clone();
        if b.name.is_empty() && !b.has_button && !b.has_touchbar {
            b.has_button = true;
        }
        b
    }

    fn is_sleeping(&self) -> bool {
        matches!(self.status.state, RunState::DeepSleep { .. } | RunState::LightSleep { .. })
    }

    fn notify(&mut self, text: impl Into<String>, error: bool) {
        self.notice = Some(Notice { text: text.into(), error, at: Instant::now() });
    }

    /// The left panel's tab strip. Clicking the selected tab collapses the panel. A star
    /// marks a tab with something live: the mock server running, the firmware advertising or
    /// a central connected, a fault on.
    fn left_tabs(&mut self, ui: &mut Ui) {
        let tabs = [LeftTab::Server, LeftTab::Faults, LeftTab::Nvs, LeftTab::Bluetooth];
        let has_server = self.server.is_some();
        for tab in tabs.into_iter().filter(|&t| t != LeftTab::Server || has_server) {
            let selected = self.left_tab == Some(tab);
            let starred = match tab {
                LeftTab::Server => self.server.as_ref().is_some_and(|s| s.is_running()),
                LeftTab::Bluetooth => self.status.bluetooth.advertising || self.status.bluetooth.connection.is_some(),
                LeftTab::Faults => self.faults_on(),
                LeftTab::Nvs => false,
            };
            let label = if starred { format!("{}*", tab.label()) } else { tab.label().to_string() };
            if ui.selectable_label(selected, label).on_hover_text(tab.hover()).clicked() {
                self.left_tab = if selected { None } else { Some(tab) };
            }
        }
    }

    // ---- input ------------------------------------------------------------------------------

    fn handle_keys(&mut self, ctx: &egui::Context) {
        let board = self.board();
        let text_focus = ctx.text_edit_focused();
        let focused = ctx.input(|i| i.viewport().focused.unwrap_or(true));
        let active = !text_focus && focused;
        let (space, zones, save) = ctx.input_mut(|i| {
            let save = i.consume_shortcut(&KeyboardShortcut::new(Modifiers::COMMAND, Key::S));
            let mut space = false;
            let mut zones = [false; 3];
            if active && board.has_button {
                // Consume Space presses so they don't also activate a focused widget.
                let tap = i.consume_key(Modifiers::NONE, Key::Space);
                space = tap || i.key_down(Key::Space);
            }
            if active && board.has_touchbar {
                let keys = [[Key::ArrowLeft, Key::Num1], [Key::ArrowDown, Key::Num2], [Key::ArrowRight, Key::Num3]];
                for (z, ks) in keys.iter().enumerate() {
                    for &k in ks {
                        // Presses are consumed so arrows don't also move focus / sliders.
                        let tap = i.consume_key(Modifiers::NONE, k);
                        zones[z] |= tap || i.key_down(k);
                    }
                }
            }
            (space, zones, save)
        });
        self.button.space_held = space;
        for (i, &down) in zones.iter().enumerate() {
            if down {
                self.touch.hold(i);
            }
        }
        if board.has_touchbar && !focused && self.touch.any_active() {
            let h = self.h.clone();
            self.touch.release_all(|c| h.send(c));
        }
        if save {
            self.save_screenshot();
        }
    }

    fn apply_button(&mut self) {
        let want = self.button.mouse_held || self.button.space_held;
        if want == self.button.down {
            return;
        }
        self.button.down = want;
        self.send(Command::Button(want));
        let now = Instant::now();
        if want {
            self.button.last_gap = self.button.last_release.map(|r| now - r);
            self.button.pressed_at = Some(now);
        } else {
            self.button.last_hold = self.button.pressed_at.take().map(|p| now - p);
            self.button.last_release = Some(now);
        }
    }

    /// Image files dropped on the window go to the mock server.
    fn handle_dropped_files(&mut self, ctx: &egui::Context) {
        let dropped = ctx.input(|i| i.raw.dropped_files.clone());
        if dropped.is_empty() {
            return;
        }
        let Some(p) = &mut self.server else {
            self.notify("Dropped files go to the mock server, which this simulator doesn't have", true);
            return;
        };
        for f in dropped {
            match f.bytes() {
                Ok(b) => p.add_file(&f.path().to_string_lossy(), b),
                Err(e) => self.notice = Some(Notice { text: e, error: true, at: Instant::now() }),
            }
        }
        self.left_tab = Some(LeftTab::Server);
    }

    fn paint_drop_hint(&self, ctx: &egui::Context) {
        if self.server.is_none() || ctx.input(|i| i.raw.hovered_files.is_empty()) {
            return;
        }
        let painter = ctx.layer_painter(egui::LayerId::new(egui::Order::Foreground, egui::Id::new("drop_hint")));
        let rect = ctx.content_rect();
        painter.rect_filled(rect, 0.0, Color32::from_black_alpha(160));
        painter.text(
            rect.center(),
            Align2::CENTER_CENTER,
            "Drop images to add them to the mock server",
            FontId::proportional(22.0),
            Color32::WHITE,
        );
    }

    // ---- screenshot ---------------------------------------------------------------------------

    fn save_screenshot(&mut self) {
        let frame = self.h.frame.lock().clone();
        if frame.width == 0 || frame.height == 0 || frame.pixels.len() < frame.width * frame.height {
            self.notify("Nothing to save: the frame is empty", true);
            return;
        }
        let name = format!("trmnl-{}.png", utc_timestamp());
        let path = rfd::FileDialog::new().set_file_name(&name).add_filter("PNG image", &["png"]).save_file();
        let Some(path) = path else { return };
        match write_png(&path, &frame) {
            Ok(()) => self.notify(format!("Saved {}", path.display()), false),
            Err(e) => self.notify(format!("Screenshot failed: {e:#}"), true),
        }
    }

    // ---- save points ----------------------------------------------------------------------------

    fn save_point(&mut self, path: Option<std::path::PathBuf>) {
        let (tx, rx) = crossbeam_channel::bounded(1);
        self.send(Command::SavePoint { label: None, path, reply: Some(tx) });
        self.savepoint_reply = Some((rx, false));
    }

    fn restore_save_point(&mut self, from: SavePointSource) {
        let (tx, rx) = crossbeam_channel::bounded(1);
        self.send(Command::RestoreSavePoint { from, reply: Some(tx) });
        self.savepoint_reply = Some((rx, true));
    }

    fn poll_save_point(&mut self) {
        let Some((rx, restore)) = &self.savepoint_reply else { return };
        let restore = *restore;
        let Ok(result) = rx.try_recv() else { return };
        self.savepoint_reply = None;
        match (result, restore) {
            (Ok(i), false) => self.notify(format!("Saved \"{}\"", i.label), false),
            (Ok(i), true) => self.notify(format!("Restored \"{}\"", i.label), false),
            (Err(e), false) => self.notify(format!("Save failed: {e}"), true),
            (Err(e), true) => self.notify(format!("Restore failed: {e}"), true),
        }
    }

    fn save_points(&mut self, ui: &mut Ui) {
        section(ui, "Save points");
        let idle = self.savepoint_reply.is_none();
        let full = matches!(self.status.state, RunState::DeepSleep { .. });
        let tip = if full {
            "Save the whole device (deep sleep: flash, RTC memory, screen, chips); restoring resumes this sleep"
        } else {
            "Not in deep sleep: saves only what survives a battery pull (flash, screen); \
             restoring powers the device on. Save in deep sleep for the full state"
        };
        ui.horizontal_wrapped(|ui| {
            if ui.add_enabled(idle, egui::Button::new("💾 Save")).on_hover_text(tip).clicked() {
                self.save_point(None);
            }
            if ui.add_enabled(idle, egui::Button::new("Save as…")).on_hover_text(tip).clicked() {
                let name = format!("trmnl-{}.trmnlsave", utc_timestamp());
                let dialog = rfd::FileDialog::new().set_file_name(&name).add_filter("Save point", &["trmnlsave"]);
                if let Some(path) = dialog.save_file() {
                    self.save_point(Some(path));
                }
            }
            if ui
                .add_enabled(idle, egui::Button::new("Open…"))
                .on_hover_text("Restore a save point file taken with this firmware build")
                .clicked()
                && let Some(path) = rfd::FileDialog::new().add_filter("Save point", &["trmnlsave"]).pick_file()
            {
                self.restore_save_point(SavePointSource::File(path));
            }
        });
        let mut restore = None;
        for sp in self.status.savepoints.iter().rev() {
            ui.horizontal(|ui| {
                if ui.add_enabled(idle, egui::Button::new("⟲").small()).on_hover_text("Restore").clicked() {
                    restore = Some(sp.id);
                }
                let kind = if sp.deep_sleep { "" } else { " (power-off)" };
                let resp = ui.add(egui::Label::new(format!("#{} {}{kind}", sp.id, sp.label)).truncate());
                if let Some(p) = &sp.path {
                    resp.on_hover_text(p.display().to_string());
                }
            });
        }
        if let Some(id) = restore {
            self.restore_save_point(SavePointSource::Slot(id));
        }
    }

    // ---- panels ---------------------------------------------------------------------------------

    fn halted_banner(&mut self, ui: &mut Ui, msg: &str) {
        let red = Color32::from_rgb(0xb4, 0x23, 0x23);
        egui::Panel::top("halted_banner")
            .resizable(false)
            .frame(egui::Frame::NONE.fill(red).inner_margin(egui::Margin::symmetric(12, 10)))
            .show(ui, |ui| {
                let btn = |text: &str| {
                    egui::Button::new(RichText::new(text).color(Color32::WHITE))
                        .fill(Color32::from_rgb(0x7a, 0x12, 0x12))
                        .stroke(Stroke::new(1.0, Color32::from_white_alpha(120)))
                };
                ui.horizontal(|ui| {
                    ui.label(RichText::new("⛔ EMULATOR HALTED").strong().size(16.0).color(Color32::WHITE));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui.add(btn("Power-cycle")).clicked() {
                            self.send(Command::PowerCycle);
                        }
                        if ui.add(btn("Reset")).clicked() {
                            self.send(Command::Reset);
                        }
                        if ui.add(btn("Copy")).clicked() {
                            ui.ctx().copy_text(msg.to_string());
                        }
                    });
                });
                ui.add(egui::Label::new(RichText::new(msg).monospace().color(Color32::WHITE)).wrap());
            });
    }

    fn status_bar(&mut self, ui: &mut Ui) {
        let board = self.board();
        let s = &self.status;
        let dark = ui.visuals().dark_mode;
        ui.horizontal(|ui| {
            if !board.name.is_empty() {
                ui.label(RichText::new(&board.name).strong());
                ui.separator();
            }
            let (label, color) = state_label(s, dark);
            dot(ui, color, true);
            ui.label(RichText::new(label).color(color).strong());
            ui.separator();
            ui.label(RichText::new(format_sim_time(s.sim_time_ns)).monospace())
                .on_hover_text("Virtual time since power-on");
            ui.separator();
            ui.label(format!("{:.1} MIPS", s.mips));
            let speed =
                if s.turbo { format!("×{:.2} turbo", s.speed_ratio) } else { format!("×{:.2}", s.speed_ratio) };
            ui.label(speed).on_hover_text("Virtual time / wall time");
            ui.separator();
            let wifi = if !s.wifi_available {
                "WiFi: AP off".to_string()
            } else if s.wifi_connected {
                format!("WiFi: {}", s.ip.as_deref().unwrap_or("connected"))
            } else {
                "WiFi: not connected".to_string()
            };
            ui.label(wifi);
            ui.separator();
            let busy_col =
                if s.display_busy { Color32::from_rgb(0xe0, 0x8a, 0x1e) } else { ui.visuals().weak_text_color() };
            dot(ui, busy_col, s.display_busy);
            ui.label(RichText::new(if s.display_busy { "BUSY" } else { "idle" }).color(busy_col))
                .on_hover_text("E-paper BUSY line");
            ui.label(format!("refreshes {}", s.display_refreshes));
            ui.label(format!("boots {}", s.boot_count));
            ui.label(format!("{:.2} V", s.battery_mv as f32 / 1000.0));
            if board.has_dock {
                let green =
                    if dark { Color32::from_rgb(0x5c, 0xc8, 0x6c) } else { Color32::from_rgb(0x1f, 0x8a, 0x34) };
                if s.docked {
                    dot(ui, green, true);
                    ui.label(RichText::new(if s.charging { "docked · charging" } else { "docked" }).color(green))
                        .on_hover_text("On the magnetic dock (USB power)");
                } else {
                    dot(ui, ui.visuals().weak_text_color(), false);
                    ui.weak("on battery");
                }
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if !s.firmware.is_empty() {
                    ui.add(egui::Label::new(RichText::new(&s.firmware).weak()).truncate());
                }
                if let Some(n) = &self.notice {
                    let col = if n.error { ui.visuals().error_fg_color } else { ui.visuals().warn_fg_color };
                    ui.add(egui::Label::new(RichText::new(&n.text).color(col)).truncate());
                }
            });
        });
    }

    fn controls(&mut self, ui: &mut Ui) {
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 6.0;

            section(ui, "Power");
            ui.horizontal(|ui| {
                if ui.button("↺ Reset").on_hover_text("Chip reset (RTC memory and display kept)").clicked() {
                    self.send(Command::Reset);
                }
                if ui
                    .button("⚡ Power-cycle")
                    .on_hover_text("Remove and restore power (RTC memory lost; flash and display kept)")
                    .clicked()
                {
                    self.send(Command::PowerCycle);
                }
            });
            if ui
                .add_enabled(self.is_sleeping(), egui::Button::new("☀ Wake now"))
                .on_hover_text("End the current sleep as if its timer expired")
                .on_disabled_hover_text("Only while the device sleeps")
                .clicked()
            {
                self.send(Command::WakeFromSleep);
            }
            let board = self.board();
            if board.has_dock {
                let mut docked = Pending::resolve(&mut self.docked_pending, self.status.docked);
                if ui
                    .checkbox(&mut docked, "Docked (USB power)")
                    .on_hover_text("Put the device on / take it off its magnetic charging dock")
                    .changed()
                {
                    self.send(Command::SetDocked(docked));
                    Pending::set(&mut self.docked_pending, docked);
                }
            }

            if board.has_button {
                section(ui, "Button");
                ui.horizontal_wrapped(|ui| {
                    for (label, ms) in [("tap", 100), ("1 s", 1100), ("5 s", 5100), ("15 s", 15100)] {
                        let hover = "Press for an exact duration of virtual time (accurate in turbo mode).\n\
                                     Or hold the button next to the device / hold Space.";
                        if ui.small_button(label).on_hover_text(hover).clicked() {
                            self.send(Command::Press { ms });
                        }
                    }
                });
            }

            if board.has_touchbar {
                section(ui, "Touch bar");
                let shift = ui.input(|i| i.modifiers.shift);
                ui.horizontal(|ui| {
                    for (i, label) in ["Left", "Center", "Right"].into_iter().enumerate() {
                        let selected = matches!(self.zone_vis(i, false), ZoneVis::Active | ZoneVis::Latched);
                        let btn = egui::Button::new(label).selected(selected).sense(Sense::click_and_drag());
                        let resp = ui.add(btn);
                        self.touch_zone_input(i, &resp, shift);
                    }
                });
                ui.weak("Click = tap, hold = finger down, shift-click latches. Keys: left/down/right arrows or 1/2/3.");
            }

            section(ui, "Emulation");
            let paused = Pending::resolve(&mut self.pause_pending, self.status.state == RunState::Paused);
            let halted = matches!(self.status.state, RunState::Halted(_));
            ui.horizontal(|ui| {
                let label = if paused { "▶ Resume" } else { "⏸ Pause" };
                if ui.add_enabled(!halted, egui::Button::new(label)).clicked() {
                    self.send(Command::Pause(!paused));
                    Pending::set(&mut self.pause_pending, !paused);
                }
                let mut turbo = Pending::resolve(&mut self.turbo_pending, self.status.turbo);
                if ui
                    .toggle_value(&mut turbo, "⏩ Turbo")
                    .on_hover_text("Run as fast as possible instead of real time")
                    .changed()
                {
                    self.send(Command::SetTurbo(turbo));
                    Pending::set(&mut self.turbo_pending, turbo);
                }
            });

            self.save_points(ui);

            section(ui, "Network");
            let net = if self.status.wifi_connected {
                format!("Connected · {}", self.status.ip.as_deref().unwrap_or("no IP"))
            } else {
                "Not connected".to_string()
            };
            ui.weak(net);

            section(ui, "Battery");
            let mut mv = Pending::resolve(&mut self.battery_pending, self.status.battery_mv);
            let before = mv;
            ui.spacing_mut().slider_width = (ui.available_width() - 80.0).max(60.0);
            ui.add(egui::Slider::new(&mut mv, 3000..=4300).suffix(" mV").step_by(10.0));
            ui.horizontal_wrapped(|ui| {
                for (name, v) in [("Full 4.1 V", 4100), ("Low 3.3 V", 3300), ("Dead 3.0 V", 3000)] {
                    if ui.selectable_label(mv == v, name).clicked() {
                        mv = v;
                    }
                }
            });
            if mv != before {
                self.send(Command::SetBatteryMv(mv));
                Pending::set(&mut self.battery_pending, mv);
            }

            section(ui, "Display");
            ui.horizontal_wrapped(|ui| {
                let mut changed = ui.selectable_value(&mut self.zoom, Zoom::Fit, "Fit").clicked();
                for (label, z) in [("50%", 0.5), ("100%", 1.0), ("200%", 2.0)] {
                    changed |= ui.selectable_value(&mut self.zoom, Zoom::Fixed(z), label).clicked();
                }
                if changed {
                    self.zoom_user_set = true;
                }
            });
            ui.horizontal(|ui| {
                ui.label("Bezel");
                ui.selectable_value(&mut self.white_bezel, false, "Black");
                ui.selectable_value(&mut self.white_bezel, true, "White");
            });
            if self.board().has_refresh_flashing
                && ui
                    .checkbox(&mut self.refresh_flashing, "Refresh flashing")
                    .on_hover_text(
                        "Show the black/white flashes of a refresh, or keep the old image up \
                         until the new one appears (timing is the same either way)",
                    )
                    .changed()
            {
                self.send(Command::SetRefreshFlashing(self.refresh_flashing));
            }
            if ui.button("📷 Save screenshot…").on_hover_text("Save the panel as a PNG (⌘S / Ctrl+S)").clicked() {
                self.save_screenshot();
            }
            ui.checkbox(&mut self.show_console, "Show serial console");
        });
    }

    fn device_view(&mut self, ui: &mut Ui) {
        const GAP: f32 = 16.0;
        const PAD: f32 = 8.0;
        const CAPTION: f32 = 20.0;

        let board = self.board();
        let ctx = ui.ctx().clone();
        let avail = ui.available_size();
        let scr = self.screen.size();
        let geom = Geometry::new(scr, board.has_touchbar, board.has_dock);
        let total_unit = geom.total(scr);
        let below = board.has_button || board.has_touchbar;
        let (below_h, gap) = if below { (BELOW_H, GAP) } else { (0.0, 0.0) };
        let caption = if board.name.is_empty() { 0.0 } else { CAPTION };
        let fit = {
            let zx = (avail.x - 2.0 * PAD) / total_unit.x;
            let zy = (avail.y - 2.0 * PAD - caption - gap - below_h) / total_unit.y;
            zx.min(zy).clamp(0.05, 8.0)
        };
        // Until the user picks a zoom, an initial fixed zoom that doesn't fit switches to fit
        // (e.g. the 1872×1404 TRMNL X panel at the default 100%).
        if let Zoom::Fixed(z) = self.zoom
            && !self.zoom_user_set
            && z > fit
        {
            self.zoom = Zoom::Fit;
        }
        let zoom = match self.zoom {
            Zoom::Fixed(z) => z,
            Zoom::Fit => fit,
        };
        let ppp = ctx.pixels_per_point();
        let nearest = zoom * ppp >= 0.999;
        self.screen.update(&ctx, nearest);

        let body = geom.body(scr) * zoom;
        let dev = total_unit * zoom + Vec2::new(0.0, caption);
        let below_w = if below { BELOW_W } else { 0.0 };
        let group = Vec2::new(dev.x.max(below_w), dev.y + gap + below_h);
        let content = group + Vec2::splat(2.0 * PAD);

        egui::ScrollArea::both().id_salt("device_scroll").auto_shrink([false, false]).show(ui, |ui| {
            let size = content.max(ui.available_size());
            let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
            let g = Rect::from_center_size(rect.center(), group);
            let snap = |v: f32| (v * ppp).round() / ppp;
            let top = g.min.y;
            let left = g.center().x - dev.x / 2.0;
            if caption > 0.0 {
                ui.painter().text(
                    Pos2::new(left + 2.0, top),
                    Align2::LEFT_TOP,
                    &board.name,
                    FontId::proportional(13.0),
                    ui.visuals().weak_text_color(),
                );
            }
            let body_rect = Rect::from_min_size(Pos2::new(snap(left), snap(top + caption)), body);

            let mut zones = [ZoneVis::Idle; 3];
            if board.has_touchbar {
                let rects = geom.zone_rects(body_rect, zoom, scr);
                let shift = ui.input(|i| i.modifiers.shift);
                for (i, r) in rects.iter().enumerate() {
                    let resp = ui
                        .interact(r.expand(2.0), ui.id().with(("touch_zone", i)), Sense::click_and_drag())
                        .on_hover_text(
                            "Touch bar: click = tap, press and hold = finger down.\n\
                             Shift-click latches a zone down (hold two zones, e.g. left + right).\n\
                             Keys: left/down/right arrows or 1/2/3.",
                        );
                    self.touch_zone_input(i, &resp, shift);
                    zones[i] = self.zone_vis(i, resp.hovered());
                }
            }
            let look = Look {
                white_bezel: self.white_bezel,
                docked: board.has_dock && self.status.docked,
                zones,
                accent: ui.visuals().selection.bg_fill,
            };
            self.screen.paint(ui.painter(), body_rect, zoom, &geom, &look);

            if below {
                let row = Rect::from_min_size(
                    Pos2::new(g.center().x - BELOW_W / 2.0, g.min.y + dev.y + GAP),
                    Vec2::new(BELOW_W, BELOW_H),
                );
                if board.has_button {
                    self.physical_button(ui, row);
                } else {
                    self.touch_row(ui, row);
                }
            }
        });
    }

    /// Mouse interaction with touch zone `i` (on the device graphic or a panel button).
    fn touch_zone_input(&mut self, i: usize, resp: &egui::Response, shift: bool) {
        if resp.hovered() {
            resp.ctx.set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        if shift {
            if resp.clicked() {
                self.touch.toggle_latch(i);
            }
        } else if resp.is_pointer_button_down_on() {
            self.touch.unlatch(i);
            self.touch.hold(i);
        }
    }

    fn zone_vis(&self, i: usize, hovered: bool) -> ZoneVis {
        if self.touch.is_latched(i) {
            ZoneVis::Latched
        } else if self.touch.is_active(i) || self.status.touch_mask & ZONES[i].bit() != 0 {
            ZoneVis::Active
        } else if hovered {
            ZoneVis::Hover
        } else {
            ZoneVis::Idle
        }
    }

    /// Row below the device for touch-bar boards: legend on the left, hold timer and
    /// last-touch readout on the right.
    fn touch_row(&mut self, ui: &mut Ui, area: Rect) {
        let v = ui.visuals().clone();
        let painter = ui.painter().clone();
        let mut y = area.center().y - 32.0;
        painter.text(
            Pos2::new(area.min.x, y),
            Align2::LEFT_TOP,
            "Touch bar",
            FontId::proportional(14.0),
            v.strong_text_color(),
        );
        y += 20.0;
        for line in ["click = tap · hold = finger down", "shift-click latches a zone", "keys: arrows or 1 2 3"] {
            painter.text(
                Pos2::new(area.min.x, y),
                Align2::LEFT_TOP,
                line,
                FontId::proportional(11.0),
                v.weak_text_color(),
            );
            y += 15.0;
        }

        let col = hold_column(area);
        let cx = col.center().x;
        let held = self.touch.longest_hold();
        let bar = Rect::from_min_size(Pos2::new(col.min.x, col.center().y - 30.0), Vec2::new(col.width(), 10.0));
        hold_bar(&painter, &v, bar, held, 3.0, &[0.6, 2.0], false);
        let mut y = bar.max.y + 20.0;
        let names =
            |m: [bool; 3]| ZONES.iter().zip(m).filter(|(_, d)| *d).map(|(z, _)| z.name()).collect::<Vec<_>>().join("+");
        let line = if let Some(h) = held {
            format!("{} {h:.2} s", names(self.touch.down_mask()))
        } else if self.touch.any_active() {
            "…".to_string()
        } else if let Some(d) = self.touch.last_hold {
            match self.touch.last_combo {
                Some(m) => format!("last {} {:.2} s", names(m), d.as_secs_f32()),
                None => format!("last touch {:.2} s", d.as_secs_f32()),
            }
        } else {
            String::new()
        };
        painter.text(Pos2::new(cx, y), Align2::CENTER_TOP, line, FontId::monospace(11.0), v.text_color());
        y += 18.0;
        if self.status.touches_done > 0 {
            painter.text(
                Pos2::new(cx, y),
                Align2::CENTER_TOP,
                format!("{} taps done", self.status.touches_done),
                FontId::proportional(10.0),
                v.weak_text_color(),
            );
        }
    }

    /// Row below the device for button boards: the button and its labels on the left, hold
    /// timer and last-press readout on the right.
    fn physical_button(&mut self, ui: &mut Ui, area: Rect) {
        let v = ui.visuals().clone();
        let painter = ui.painter().clone();
        let dia = 52.0;
        let center = Pos2::new(area.min.x + dia / 2.0 + 2.0, area.center().y);
        let hit = Rect::from_center_size(center, Vec2::splat(dia));
        let resp = ui.interact(hit, ui.id().with("physical_button"), Sense::click_and_drag()).on_hover_text(
            "The device's button. Hold for 1 s / 5 s / 15 s; double-click within 800 ms.\nKeyboard: hold Space.",
        );
        self.button.mouse_held = resp.is_pointer_button_down_on();
        if resp.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        let pressed = self.button.down || self.button.mouse_held || self.button.space_held;

        // Button body.
        let (base, ring) = if v.dark_mode {
            (Color32::from_rgb(0x3a, 0x3b, 0x3f), Color32::from_rgb(0x5a, 0x5c, 0x62))
        } else {
            (Color32::from_rgb(0xe4, 0xe3, 0xde), Color32::from_rgb(0xb8, 0xb6, 0xaf))
        };
        let accent = v.selection.bg_fill;
        let fill = if pressed {
            accent
        } else if resp.hovered() {
            base.lerp_to_gamma(accent, 0.18)
        } else {
            base
        };
        let offset = if pressed { Vec2::new(0.0, 1.5) } else { Vec2::ZERO };
        if !pressed {
            painter.circle_filled(center + Vec2::new(0.0, 2.5), dia / 2.0, Color32::from_black_alpha(45));
        }
        painter.circle_filled(center + offset, dia / 2.0, fill);
        painter.circle_stroke(center + offset, dia / 2.0, Stroke::new(1.5, ring));
        painter.circle_stroke(center + offset, dia / 2.0 - 5.0, Stroke::new(1.0, ring.gamma_multiply(0.6)));
        let label_col = if pressed { v.selection.stroke.color } else { v.text_color() };
        painter.text(center + offset, Align2::CENTER_CENTER, "PUSH", FontId::proportional(10.0), label_col);

        let x = hit.max.x + 14.0;
        painter.text(
            Pos2::new(x, center.y - 2.0),
            Align2::LEFT_BOTTOM,
            "Button",
            FontId::proportional(14.0),
            v.strong_text_color(),
        );
        painter.text(
            Pos2::new(x, center.y + 2.0),
            Align2::LEFT_TOP,
            "hold Space",
            FontId::proportional(11.0),
            v.weak_text_color(),
        );

        // Hold timer: sqrt scale over 0..16 s so the 1 s / 5 s / 15 s marks are spread out.
        let col = hold_column(area);
        let bar = Rect::from_min_size(Pos2::new(col.min.x, col.center().y - 22.0), Vec2::new(col.width(), 10.0));
        let held = self.button.pressed_at.map(|p| p.elapsed().as_secs_f32());
        hold_bar(&painter, &v, bar, held, 16.0, &[1.0, 5.0, 15.0], true);

        let line = if let Some(h) = held {
            format!("holding {h:.2} s")
        } else if let Some(d) = self.button.last_hold {
            let mut s = format!("last press {:.2} s", d.as_secs_f32());
            if let Some(g) = self.button.last_gap
                && g < Duration::from_millis(800)
            {
                s = format!("{s} · {:.0} ms gap", g.as_secs_f32() * 1000.0);
            }
            s
        } else {
            String::new()
        };
        painter.text(
            Pos2::new(col.center().x, bar.max.y + 20.0),
            Align2::CENTER_TOP,
            line,
            FontId::monospace(11.0),
            v.text_color(),
        );
    }
}

impl eframe::App for SimApp {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.status = self.h.status.lock().clone();
        let console_changed = self.console.poll();
        self.touch.begin_frame();
        self.handle_keys(&ctx);
        self.poll_save_point();
        if self.notice.as_ref().is_some_and(|n| n.at.elapsed() > Duration::from_secs(8)) {
            self.notice = None;
        }
        self.handle_dropped_files(&ctx);
        self.bluetooth.poll(&self.h, &self.status.bluetooth);
        if let Some(p) = &mut self.server {
            p.poll(&self.status);
            for (text, error) in std::mem::take(&mut p.notices) {
                self.notice = Some(Notice { text, error, at: Instant::now() });
            }
        }

        egui::Panel::bottom("status_bar").resizable(false).show(ui, |ui| {
            ui.add_space(3.0);
            self.status_bar(ui);
            ui.add_space(1.0);
        });
        if let RunState::Halted(msg) = self.status.state.clone() {
            self.halted_banner(ui, &msg);
        }
        egui::Panel::right("controls").resizable(true).default_size(205.0).min_size(180.0).show(ui, |ui| {
            ui.add_space(4.0);
            self.controls(ui);
        });
        // The Server tab disappears with the mock server (`--no-mock`).
        if self.server.is_none() && self.left_tab == Some(LeftTab::Server) {
            self.left_tab = None;
        }
        if let Some(tab) = self.left_tab {
            egui::Panel::left("utility").resizable(true).default_size(360.0).min_size(260.0).show(ui, |ui| {
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    self.left_tabs(ui);
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui.small_button("⏴").on_hover_text("Hide the panel").clicked() {
                            self.left_tab = None;
                        }
                    });
                });
                ui.separator();
                match tab {
                    LeftTab::Server => {
                        let board = self.board();
                        if let Some(p) = &mut self.server {
                            p.ui(ui, &self.h, &self.status, &board);
                        }
                    }
                    LeftTab::Bluetooth => self.bluetooth.ui(ui, &self.h, &self.status.bluetooth),
                    LeftTab::Faults => self.faults_tab(ui),
                    LeftTab::Nvs => self.preferences.ui(ui, &self.h),
                }
            });
        }
        if self.left_tab != Some(LeftTab::Nvs) {
            self.preferences.hidden();
        }
        egui::Panel::bottom("console").resizable(true).default_size(230.0).min_size(90.0).show_collapsible(
            ui,
            &mut self.show_console,
            |ui| {
                ui.add_space(4.0);
                self.console.toolbar(ui);
                ui.add_space(2.0);
                self.console.body(ui);
            },
        );
        egui::CentralPanel::default().show(ui, |ui| {
            if !self.show_console || self.left_tab.is_none() {
                ui.horizontal(|ui| {
                    if self.left_tab.is_none() {
                        self.left_tabs(ui);
                    }
                    if !self.show_console && ui.small_button("⬆ Serial console").clicked() {
                        self.show_console = true;
                    }
                });
            }
            self.device_view(ui);
        });
        self.paint_drop_hint(&ctx);

        self.apply_button();
        let h = self.h.clone();
        self.touch.update(|c| h.send(c));

        let s = &self.status;
        let active = matches!(s.state, RunState::Running | RunState::Idle)
            || s.display_busy
            || self.button.down
            || self.touch.any_active()
            || console_changed;
        let after = if active {
            Duration::from_millis(33)
        } else if self.is_sleeping() {
            Duration::from_millis(100)
        } else {
            Duration::from_millis(250)
        };
        ctx.request_repaint_after(after);
    }
}

/// The row below the device (button or touch bar controls).
const BELOW_W: f32 = 440.0;
const BELOW_H: f32 = 68.0;

/// The right-hand part of the row below the device, holding the hold timer.
fn hold_column(row: Rect) -> Rect {
    Rect::from_min_max(Pos2::new(row.max.x - 240.0, row.min.y), row.max)
}

/// A hold-duration bar with labelled threshold ticks. The fill colour steps up with each
/// threshold reached. `sqrt` spreads out small thresholds on long scales.
fn hold_bar(
    painter: &egui::Painter,
    v: &egui::Visuals,
    bar: Rect,
    held: Option<f32>,
    max_s: f32,
    ticks: &[f32],
    sqrt: bool,
) {
    let pos = |s: f32| {
        let f = (s / max_s).clamp(0.0, 1.0);
        if sqrt { f.sqrt() } else { f }
    };
    painter.rect_filled(bar, 5.0, v.extreme_bg_color);
    painter.rect_stroke(bar, 5.0, Stroke::new(1.0, v.widgets.noninteractive.bg_stroke.color), StrokeKind::Inside);
    if let Some(h) = held {
        let levels = [
            v.widgets.inactive.fg_stroke.color,
            v.selection.bg_fill,
            Color32::from_rgb(0xe0, 0x9a, 0x2a),
            Color32::from_rgb(0xd6, 0x3c, 0x3c),
        ];
        let reached = ticks.iter().filter(|&&t| h >= t).count();
        // With fewer ticks, the last one reached is shown as "max" (red).
        let idx = if reached == ticks.len() && reached > 0 { 3 } else { reached.min(2) };
        let fill = Rect::from_min_max(bar.min, Pos2::new(bar.min.x + bar.width() * pos(h), bar.max.y));
        painter.rect_filled(fill, 5.0, levels[idx]);
    }
    for (n, &t) in ticks.iter().enumerate() {
        let x = bar.min.x + bar.width() * pos(t);
        let reached = held.is_some_and(|h| h >= t);
        let col = if reached { v.strong_text_color() } else { v.weak_text_color() };
        painter.line_segment([Pos2::new(x, bar.min.y - 3.0), Pos2::new(x, bar.max.y + 3.0)], Stroke::new(1.5, col));
        let near_end = n + 1 == ticks.len() && pos(t) > 0.9;
        let (align, tx) = if near_end { (Align2::RIGHT_TOP, bar.max.x) } else { (Align2::CENTER_TOP, x) };
        let label = if t.fract() == 0.0 { format!("{t:.0}s") } else { format!("{t}s") };
        painter.text(Pos2::new(tx, bar.max.y + 4.0), align, label, FontId::proportional(10.0), col);
    }
}

/// A small status dot (filled or hollow) that doesn't depend on font glyph coverage.
fn dot(ui: &mut Ui, color: Color32, filled: bool) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(10.0), Sense::hover());
    if filled {
        ui.painter().circle_filled(rect.center(), 4.0, color);
    } else {
        ui.painter().circle_stroke(rect.center(), 3.5, Stroke::new(1.2, color));
    }
}

fn section(ui: &mut Ui, title: &str) {
    ui.add_space(6.0);
    ui.label(RichText::new(title.to_uppercase()).small().strong().color(ui.visuals().weak_text_color()));
}

fn state_label(s: &Status, dark: bool) -> (String, Color32) {
    let pick = |d: [u8; 3], l: [u8; 3]| {
        let c = if dark { d } else { l };
        Color32::from_rgb(c[0], c[1], c[2])
    };
    let wake = |w: Option<u64>| match w {
        Some(at) => format!("wakes in {}", format_countdown(at.saturating_sub(s.sim_time_ns))),
        None => "no timer".to_string(),
    };
    match &s.state {
        RunState::Running => ("Running".into(), pick([0x5c, 0xc8, 0x6c], [0x1f, 0x8a, 0x34])),
        RunState::Idle => ("Idle".into(), pick([0x8c, 0xb8, 0x94], [0x4f, 0x7a, 0x57])),
        RunState::Paused => ("Paused".into(), pick([0xe8, 0xb0, 0x4a], [0xa8, 0x6b, 0x00])),
        RunState::LightSleep { wake_at_ns } => {
            (format!("Light sleep — {}", wake(*wake_at_ns)), pick([0x7a, 0xb4, 0xf0], [0x1d, 0x5f, 0xb0]))
        }
        RunState::DeepSleep { wake_at_ns } => {
            (format!("Deep sleep — {}", wake(*wake_at_ns)), pick([0xa8, 0x9c, 0xf0], [0x5a, 0x3f, 0xc0]))
        }
        RunState::Halted(_) => ("Halted".into(), pick([0xff, 0x6b, 0x6b], [0xc0, 0x1c, 0x1c])),
    }
}

/// `h:mm:ss.mmm`
fn format_sim_time(ns: u64) -> String {
    let ms = ns / 1_000_000;
    let (h, m, s, ms) = (ms / 3_600_000, (ms / 60_000) % 60, (ms / 1000) % 60, ms % 1000);
    format!("{h}:{m:02}:{s:02}.{ms:03}")
}

/// `m:ss` or `h:mm:ss`, rounding up so it never shows 0:00 while time remains.
fn format_countdown(ns: u64) -> String {
    let secs = ns.div_ceil(1_000_000_000);
    let (h, m, s) = (secs / 3600, (secs / 60) % 60, secs % 60);
    if h > 0 { format!("{h}:{m:02}:{s:02}") } else { format!("{m}:{s:02}") }
}

fn utc_timestamp() -> String {
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    // Civil-from-days (Howard Hinnant).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}{m:02}{d:02}-{:02}{:02}{:02}", rem / 3600, (rem / 60) % 60, rem % 60)
}

/// Write the frame as a viewer sees it: 8-bit greyscale (white = 255, i.e. `255 - darkness`),
/// or RGB on color panels.
fn write_png(path: &Path, frame: &sim_api::Frame) -> anyhow::Result<()> {
    let file = std::fs::File::create(path).with_context(|| format!("creating {}", path.display()))?;
    let (width, height, ch, data) = frame.viewer(None);
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), width as u32, height as u32);
    enc.set_color(if ch == 3 { png::ColorType::Rgb } else { png::ColorType::Grayscale });
    enc.set_depth(png::BitDepth::Eight);
    let mut w = enc.write_header()?;
    w.write_image_data(&data)?;
    w.finish()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_icon_is_square_rgba() {
        let icon = app_icon();
        assert_eq!((icon.width, icon.height), (512, 512));
        assert_eq!(icon.rgba.len(), 512 * 512 * 4);
    }

    #[test]
    fn formatting() {
        assert_eq!(format_sim_time(3_723_456_000_000), "1:02:03.456");
        assert_eq!(format_countdown(872_000_000_000), "14:32");
        assert_eq!(format_countdown(1), "0:01");
        assert_eq!(format_countdown(3_600_000_000_000), "1:00:00");
        assert_eq!(utc_timestamp().len(), 15);
    }
}

/// Offscreen renders for eyeballing the layout (needs a GPU; run with
/// `SIM_UI_RENDER_DIR=/some/dir cargo test -p sim-ui render -- --ignored`).
#[cfg(test)]
mod render_tests {
    use super::*;
    use parking_lot::Mutex;
    use sim_api::{BoardInfo, Frame, TouchZone};
    use std::sync::Arc;

    fn harness(
        w: usize,
        h: usize,
        board: BoardInfo,
        docked: bool,
    ) -> (egui_kittest::Harness<'static, SimApp>, sim_api::SimPorts) {
        harness_with(w, h, board, docked, UiOptions::default())
    }

    fn harness_with(
        w: usize,
        h: usize,
        board: BoardInfo,
        docked: bool,
        opts: UiOptions,
    ) -> (egui_kittest::Harness<'static, SimApp>, sim_api::SimPorts) {
        let pixels = (0..w * h)
            .map(|i| if ((i % w) * 16 / w).is_multiple_of(2) { ((i / w) * 255 / h) as u8 } else { 0 })
            .collect();
        let frame = Arc::new(Mutex::new(Frame { width: w, height: h, pixels, rgb: None, generation: 1 }));
        let (handle, ports) = sim_api::channel(frame);
        {
            let mut s = handle.status.lock();
            s.board = board;
            s.docked = docked;
            s.firmware = "render test".into();
            s.state = RunState::DeepSleep { wake_at_ns: Some(872_000_000_000) };
            s.savepoints = vec![sim_api::SavePointInfo {
                id: 1,
                label: "deep sleep at 12.3 s, boot 2".into(),
                deep_sleep: true,
                sim_time_ns: 12_300_000_000,
                wake_at_ns: Some(312_300_000_000),
                path: None,
                bytes: 970_000,
            }];
        }
        for i in 0..50 {
            handle.console.lock().push_bytes(format!("I ({i}) test: line {i}\n").as_bytes());
        }
        let harness = egui_kittest::Harness::builder()
            .with_size([1280.0, 860.0])
            .with_pixels_per_point(2.0)
            .build_eframe(move |cc| SimApp::new(cc, handle, opts));
        (harness, ports)
    }

    fn save(h: &mut egui_kittest::Harness<'static, SimApp>, name: &str) {
        let Ok(dir) = std::env::var("SIM_UI_RENDER_DIR") else { return };
        let img = h.render().expect("render");
        img.save(format!("{dir}/{name}.png")).expect("save");
    }

    #[test]
    fn preferences_displays_unmasked_values_and_refreshes() {
        use egui_kittest::kittest::Queryable;
        let (mut h, ports) = harness(800, 480, BoardInfo::default(), false);
        h.run_steps(3);
        h.get_by_label("NVS").click();
        h.run_steps(3);
        let reply = ports
            .commands
            .try_iter()
            .find_map(|c| match c {
                Command::ReadPreferences(tx) => Some(tx),
                _ => None,
            })
            .expect("opening preferences requests live flash");
        reply
            .send(sim_api::PreferencesSnapshot {
                entries: vec![sim_api::Preference {
                    partition: "nvs".into(),
                    namespace: "settings".into(),
                    key: "password".into(),
                    kind: "string",
                    value: "visible-secret".into(),
                }],
                warnings: vec![],
                editable: true,
            })
            .unwrap();
        h.run_steps(3);
        h.get_by_label("visible-secret");
        h.get_by_label("Edit").click();
        h.run_steps(3);
        h.get_by_label("Edit nvs/settings/password");
        h.get_by_label("Save").click();
        h.run_steps(3);
        let (change, reply) = ports
            .commands
            .try_iter()
            .find_map(|c| match c {
                Command::ChangePreference { change, reply } => Some((change, reply)),
                _ => None,
            })
            .expect("save sends a preference change");
        assert_eq!(change.value, Some(("string".into(), "visible-secret".into())));
        reply.send(Err("deep sleep required".into())).unwrap();
        h.run_steps(3);
        h.get_all_by_label("deep sleep required").next().expect("write rejection shown");
        h.get_by_label("Cancel").click();
        h.run_steps(3);
        h.get_by_label("Refresh").click();
        h.run_steps(3);
        assert!(ports.commands.try_iter().any(|c| matches!(c, Command::ReadPreferences(_))));
        save(&mut h, "preferences");
    }

    #[test]
    fn bluetooth_tab_connects_and_exchanges_att() {
        use egui_kittest::kittest::Queryable;
        let (mut h, ports) = harness(800, 480, BoardInfo::default(), false);
        {
            let mut s = ports.status.lock();
            s.bluetooth.initialized = true;
            s.bluetooth.advertising = true;
            s.bluetooth.advertisement = vec![2, 0x01, 0x06, 6, 0x09, b'T', b'R', b'M', b'N', b'L'];
        }
        h.run_steps(3);
        h.get_by_label("BLE*").click();
        h.run_steps(3);
        h.get_by_label("TRMNL");
        h.get_by_label("Connect").click();
        h.run_steps(3);
        let reply = ports
            .commands
            .try_iter()
            .find_map(|c| match c {
                Command::Bluetooth { operation: sim_api::BluetoothOperation::Connect, reply } => Some(reply),
                _ => None,
            })
            .expect("Connect sends a Bluetooth connect");
        ports.status.lock().bluetooth.connection = Some(7);
        reply.send(Ok(sim_api::BluetoothReply { connection: Some(7), data: vec![] })).unwrap();
        h.run_steps(3);
        // The panel polls notifications on its own connection: answer that, then send a preset.
        for c in ports.commands.try_iter() {
            if let Command::Bluetooth { operation: sim_api::BluetoothOperation::Receive { connection: 7 }, reply } = c {
                reply
                    .send(Ok(sim_api::BluetoothReply { connection: Some(7), data: vec![0x1b, 0x2a, 0x00, 0x01] }))
                    .unwrap();
            }
        }
        h.run_steps(3);
        h.get_by_label("Exchange MTU").click();
        h.run_steps(3);
        let (data, reply) = ports
            .commands
            .try_iter()
            .find_map(|c| match c {
                Command::Bluetooth {
                    operation: sim_api::BluetoothOperation::Exchange { connection: 7, data },
                    reply,
                } => Some((data, reply)),
                _ => None,
            })
            .expect("the preset sends an ATT exchange");
        assert_eq!(data, vec![0x02, 0x00, 0x02]);
        reply.send(Ok(sim_api::BluetoothReply { connection: Some(7), data: vec![0x03, 0x00, 0x02] })).unwrap();
        h.run_steps(3);
        h.get_by_label("MTU rsp 512 · 03 00 02");
        h.get_by_label("Notification 0x002a · 1b 2a 00 01");
        save(&mut h, "bluetooth");
    }

    #[test]
    #[ignore]
    fn render_og() {
        let (mut h, _p) = harness(800, 480, BoardInfo::default(), false);
        h.run_steps(5);
        save(&mut h, "og");
        use egui_kittest::kittest::Queryable;
        h.get_by_label("Faults").click();
        h.run_steps(5);
        h.get_by_label("DNS fails").click();
        h.get_by_label("⚡ Cut power on next NVS write").click();
        h.run_steps(5);
        save(&mut h, "og_faults");
    }

    #[test]
    #[ignore]
    fn render_mock_server() {
        let mock = mock_trmnl::MockServer::new(mock_trmnl::Panel::Og);
        mock.start(0).unwrap();
        let (mut h, _p) =
            harness_with(800, 480, BoardInfo::default(), false, UiOptions { mock: Some(mock), ..Default::default() });
        h.run_steps(5);
        save(&mut h, "mock_server");
    }

    #[test]
    #[ignore]
    fn render_x() {
        let board = BoardInfo {
            name: "TRMNL X".into(),
            has_button: false,
            has_touchbar: true,
            has_dock: true,
            has_5ghz: true,
            ..Default::default()
        };
        let (mut h, ports) = harness(1872, 1404, board, true);
        h.run_steps(3);
        // Hold the left zone via keyboard past the tap window: expect TouchDown(Left).
        h.key_down(Key::ArrowLeft);
        h.run_steps(2);
        std::thread::sleep(touch::TAP_WINDOW + Duration::from_millis(50));
        h.run_steps(2);
        save(&mut h, "x_touch_left");
        let cmds: Vec<String> = ports.commands.try_iter().map(|c| format!("{c:?}")).collect();
        assert!(cmds.iter().any(|c| c == &format!("{:?}", Command::TouchDown(TouchZone::Left))), "{cmds:?}");
    }
}
