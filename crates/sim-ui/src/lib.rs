//! Interactive desktop front-end for the TRMNL simulator (eframe/egui).
//!
//! The emulator runs on its own thread and publishes state through a [`sim_api::SimHandle`];
//! this crate only reads that shared state and sends [`sim_api::Command`]s.

mod console;
mod device;

use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Context as _;
use egui::{
    Align, Align2, Color32, FontId, Key, KeyboardShortcut, Layout, Modifiers, Pos2, Rect, RichText, Sense, Stroke,
    StrokeKind, Ui, Vec2,
};
use sim_api::{Command, RunState, SimHandle, Status};

use console::ConsoleView;
use device::{BEZEL_RATIO, Screen};

/// Options for [`run`].
#[derive(Clone, Debug)]
pub struct UiOptions {
    /// Window title.
    pub title: String,
    /// Initial display zoom (1.0 = one e-paper pixel per logical point).
    /// A value `<= 0.0` starts in "fit to window" mode.
    pub scale: f32,
}

impl Default for UiOptions {
    fn default() -> Self {
        UiOptions { title: "TRMNL Simulator".to_string(), scale: 1.0 }
    }
}

/// Open the simulator window. Must be called on the main thread; blocks until the window
/// closes, then sends [`Command::Quit`].
pub fn run(handle: SimHandle, opts: UiOptions) -> anyhow::Result<()> {
    let native = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(opts.title.clone())
            .with_inner_size([1280.0, 860.0])
            .with_min_inner_size([640.0, 420.0]),
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

/// A value we asked the emulator to change; shown optimistically until the status catches up.
struct Pending<T> {
    value: T,
    since: Instant,
}

impl<T: PartialEq + Copy> Pending<T> {
    const TIMEOUT: Duration = Duration::from_millis(1500);

    fn resolve(slot: &mut Option<Self>, actual: T) -> T {
        match slot {
            Some(p) if p.value != actual && p.since.elapsed() < Self::TIMEOUT => p.value,
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
    zoom: Zoom,
    white_bezel: bool,
    button: ButtonState,
    battery_pending: Option<Pending<u32>>,
    turbo_pending: Option<Pending<bool>>,
    wifi_pending: Option<Pending<bool>>,
    pause_pending: Option<Pending<bool>>,
    notice: Option<Notice>,
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
        SimApp {
            screen: Screen::new(h.frame.clone()),
            console: ConsoleView::new(h.console.clone()),
            status,
            h,
            show_console: true,
            zoom: if opts.scale > 0.0 { Zoom::Fixed(opts.scale) } else { Zoom::Fit },
            white_bezel: false,
            button: ButtonState::default(),
            battery_pending: None,
            turbo_pending: None,
            wifi_pending: None,
            pause_pending: None,
            notice: None,
        }
    }

    fn send(&self, c: Command) {
        self.h.send(c);
    }

    fn is_sleeping(&self) -> bool {
        matches!(self.status.state, RunState::DeepSleep { .. } | RunState::LightSleep { .. })
    }

    fn notify(&mut self, text: impl Into<String>, error: bool) {
        self.notice = Some(Notice { text: text.into(), error, at: Instant::now() });
    }

    // ---- input ------------------------------------------------------------------------------

    fn handle_keys(&mut self, ctx: &egui::Context) {
        let text_focus = ctx.text_edit_focused();
        let focused = ctx.input(|i| i.viewport().focused.unwrap_or(true));
        let (down, tap, save) = ctx.input_mut(|i| {
            let save = i.consume_shortcut(&KeyboardShortcut::new(Modifiers::COMMAND, Key::S));
            if text_focus || !focused {
                return (false, false, save);
            }
            // Consume Space presses so they don't also activate a focused widget.
            let tap = i.consume_key(Modifiers::NONE, Key::Space);
            (i.key_down(Key::Space), tap, save)
        });
        self.button.space_held = down || tap;
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
        let s = &self.status;
        let dark = ui.visuals().dark_mode;
        ui.horizontal(|ui| {
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

            section(ui, "Network");
            let mut wifi = Pending::resolve(&mut self.wifi_pending, self.status.wifi_available);
            if ui.checkbox(&mut wifi, "Access point available").changed() {
                self.send(Command::SetWifiAvailable(wifi));
                Pending::set(&mut self.wifi_pending, wifi);
            }
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
                ui.selectable_value(&mut self.zoom, Zoom::Fit, "Fit");
                for (label, z) in [("50%", 0.5), ("100%", 1.0), ("200%", 2.0)] {
                    ui.selectable_value(&mut self.zoom, Zoom::Fixed(z), label);
                }
            });
            ui.horizontal(|ui| {
                ui.label("Bezel");
                ui.selectable_value(&mut self.white_bezel, false, "Black");
                ui.selectable_value(&mut self.white_bezel, true, "White");
            });
            if ui.button("📷 Save screenshot…").on_hover_text("Save the panel as a PNG (⌘S / Ctrl+S)").clicked() {
                self.save_screenshot();
            }
            ui.checkbox(&mut self.show_console, "Show serial console");
        });
    }

    fn device_view(&mut self, ui: &mut Ui) {
        const SIDE_W: f32 = 150.0;
        const SIDE_H: f32 = 196.0;
        const GAP: f32 = 22.0;
        const PAD: f32 = 8.0;

        let ctx = ui.ctx().clone();
        let avail = ui.available_size();
        let scr = self.screen.size();
        let border_unit = scr.x * BEZEL_RATIO;
        let dev_unit = Vec2::new(scr.x + 2.0 * border_unit, scr.y + 2.0 * border_unit);
        let zoom = match self.zoom {
            Zoom::Fixed(z) => z,
            Zoom::Fit => {
                let zx = (avail.x - SIDE_W - GAP - 2.0 * PAD) / dev_unit.x;
                let zy = (avail.y - 2.0 * PAD) / dev_unit.y;
                zx.min(zy).clamp(0.1, 8.0)
            }
        };
        let ppp = ctx.pixels_per_point();
        let nearest = zoom * ppp >= 0.999;
        self.screen.update(&ctx, nearest);

        let dev = dev_unit * zoom;
        let group = Vec2::new(dev.x + GAP + SIDE_W, dev.y.max(SIDE_H));
        let content = group + Vec2::splat(2.0 * PAD);

        egui::ScrollArea::both().id_salt("device_scroll").auto_shrink([false, false]).show(ui, |ui| {
            let size = content.max(ui.available_size());
            let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
            let g = Rect::from_center_size(rect.center(), group);
            let snap = |v: f32| (v * ppp).round() / ppp;
            let dev_min = Pos2::new(snap(g.min.x), snap(g.center().y - dev.y / 2.0));
            let dev_rect = Rect::from_min_size(dev_min, dev);
            self.screen.paint(ui.painter(), dev_rect, zoom, self.white_bezel);
            let side = Rect::from_min_size(
                Pos2::new(dev_rect.max.x + GAP, g.center().y - SIDE_H / 2.0),
                Vec2::new(SIDE_W, SIDE_H),
            );
            self.physical_button(ui, side);
        });
    }

    fn physical_button(&mut self, ui: &mut Ui, area: Rect) {
        let v = ui.visuals().clone();
        let painter = ui.painter().clone();
        let dia = 76.0;
        let center = Pos2::new(area.center().x, area.min.y + dia / 2.0 + 2.0);
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
        painter.circle_stroke(center + offset, dia / 2.0 - 7.0, Stroke::new(1.0, ring.gamma_multiply(0.6)));
        let label_col = if pressed { v.selection.stroke.color } else { v.text_color() };
        painter.text(center + offset, Align2::CENTER_CENTER, "PUSH", FontId::proportional(13.0), label_col);

        let mut y = hit.max.y + 8.0;
        painter.text(
            Pos2::new(area.center().x, y),
            Align2::CENTER_TOP,
            "Button",
            FontId::proportional(14.0),
            v.strong_text_color(),
        );
        y += 18.0;
        painter.text(
            Pos2::new(area.center().x, y),
            Align2::CENTER_TOP,
            "hold Space",
            FontId::proportional(11.0),
            v.weak_text_color(),
        );
        y += 24.0;

        // Hold timer: sqrt scale over 0..16 s so the 1 s / 5 s / 15 s marks are spread out.
        const MAX_S: f32 = 16.0;
        let pos = |s: f32| (s / MAX_S).clamp(0.0, 1.0).sqrt();
        let bar = Rect::from_min_size(Pos2::new(area.min.x, y), Vec2::new(area.width(), 10.0));
        painter.rect_filled(bar, 5.0, v.extreme_bg_color);
        painter.rect_stroke(bar, 5.0, Stroke::new(1.0, v.widgets.noninteractive.bg_stroke.color), StrokeKind::Inside);
        let held = self.button.pressed_at.map(|p| p.elapsed().as_secs_f32());
        if let Some(h) = held {
            let col = if h >= 15.0 {
                Color32::from_rgb(0xd6, 0x3c, 0x3c)
            } else if h >= 5.0 {
                Color32::from_rgb(0xe0, 0x9a, 0x2a)
            } else if h >= 1.0 {
                accent
            } else {
                v.widgets.inactive.fg_stroke.color
            };
            let fill = Rect::from_min_max(bar.min, Pos2::new(bar.min.x + bar.width() * pos(h), bar.max.y));
            painter.rect_filled(fill, 5.0, col);
        }
        for t in [1.0f32, 5.0, 15.0] {
            let x = bar.min.x + bar.width() * pos(t);
            let reached = held.is_some_and(|h| h >= t);
            let col = if reached { v.strong_text_color() } else { v.weak_text_color() };
            painter.line_segment([Pos2::new(x, bar.min.y - 3.0), Pos2::new(x, bar.max.y + 3.0)], Stroke::new(1.5, col));
            let align = if t >= 15.0 { Align2::RIGHT_TOP } else { Align2::CENTER_TOP };
            let tx = if t >= 15.0 { bar.max.x } else { x };
            painter.text(Pos2::new(tx, bar.max.y + 4.0), align, format!("{t:.0}s"), FontId::proportional(10.0), col);
        }
        y = bar.max.y + 20.0;

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
        painter.text(Pos2::new(area.center().x, y), Align2::CENTER_TOP, line, FontId::monospace(11.0), v.text_color());
    }
}

impl eframe::App for SimApp {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.status = self.h.status.lock().clone();
        let console_changed = self.console.poll();
        self.handle_keys(&ctx);
        if self.notice.as_ref().is_some_and(|n| n.at.elapsed() > Duration::from_secs(8)) {
            self.notice = None;
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
            if !self.show_console {
                ui.horizontal(|ui| {
                    if ui.small_button("⬆ Serial console").clicked() {
                        self.show_console = true;
                    }
                });
            }
            self.device_view(ui);
        });

        self.apply_button();

        let s = &self.status;
        let active = matches!(s.state, RunState::Running | RunState::Idle)
            || s.display_busy
            || self.button.down
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

/// Write the frame as an 8-bit greyscale PNG (white = 255, i.e. `255 - darkness`).
fn write_png(path: &Path, frame: &sim_api::Frame) -> anyhow::Result<()> {
    let file = std::fs::File::create(path).with_context(|| format!("creating {}", path.display()))?;
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), frame.width as u32, frame.height as u32);
    enc.set_color(png::ColorType::Grayscale);
    enc.set_depth(png::BitDepth::Eight);
    let mut w = enc.write_header()?;
    let data: Vec<u8> = frame.pixels[..frame.width * frame.height].iter().map(|&d| 255 - d).collect();
    w.write_image_data(&data)?;
    w.finish()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formatting() {
        assert_eq!(format_sim_time(3_723_456_000_000), "1:02:03.456");
        assert_eq!(format_countdown(872_000_000_000), "14:32");
        assert_eq!(format_countdown(1), "0:01");
        assert_eq!(format_countdown(3_600_000_000_000), "1:00:00");
        assert_eq!(utc_timestamp().len(), 15);
    }
}
