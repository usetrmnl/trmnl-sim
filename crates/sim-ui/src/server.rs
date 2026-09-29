//! "Server" side panel: drives the built-in mock TRMNL server ([`mock_trmnl::MockServer`]).
//! Images (added with a file picker or dropped on the window), the playlist, the
//! `/api/display` answer, one-shot actions (OTA, reset), HTTP and connection faults and a
//! live log of the device's requests.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{SystemTime, UNIX_EPOCH};

use egui::{Color32, RichText, TextureHandle, TextureOptions, Ui, Vec2};
use mock_trmnl::{ConvertOptions, DEFAULT_IMAGE, FileSource, Fit, HttpFault, Image, MockServer, Request, Route};
use serde_json::{Map, Value, json};
use sim_api::{BoardInfo, Command, RunState, SimHandle, Status};

use crate::{dot, format_sim_time, section};

/// Where the OTA firmware is served from.
const FIRMWARE_PATH: &str = "/firmware.bin";
const CUSTOM_FIRMWARE_PATH: &str = "/firmware-custom.bin";
/// Requests shown in the log (newest first).
const LOG_ROWS: usize = 200;
/// The simulated access point (any password works).
const SSID: &str = "TRMNL-Sim";

/// Onboarding through the captive portal, driven from the panel.
#[derive(Debug, Clone, PartialEq)]
enum Onboard {
    Idle,
    /// Waiting for the device's setup portal (after asking it to forget its WiFi).
    WaitingForPortal,
    Connecting,
}

enum Event {
    /// An image conversion finished: (message, error).
    Converted(String, bool),
    Onboarded(Result<(), String>),
}

pub(crate) struct ServerPanel {
    mock: MockServer,
    port: u16,
    opts: ConvertOptions,
    /// Wake a sleeping device when the served content changes.
    wake_on_change: bool,
    thumbs: HashMap<String, (u64, TextureHandle)>,
    events_tx: Sender<Event>,
    events: Receiver<Event>,
    /// Conversions running in the background.
    converting: usize,
    onboard: Onboard,
    /// A firmware file picked instead of this build's.
    custom_firmware: Option<PathBuf>,
    /// The fault being set up for each route, and how many requests it takes (0: until
    /// cleared).
    fault_edit: [(HttpFault, u32); 2],
    /// Messages for the status bar: (text, error).
    pub notices: Vec<(String, bool)>,
}

impl ServerPanel {
    pub fn new(mock: MockServer) -> Self {
        let (events_tx, events) = channel();
        ServerPanel {
            port: mock.port().unwrap_or(mock_trmnl::DEFAULT_PORT),
            mock,
            opts: ConvertOptions::default(),
            wake_on_change: true,
            thumbs: HashMap::new(),
            events_tx,
            events,
            converting: 0,
            onboard: Onboard::Idle,
            custom_firmware: None,
            fault_edit: [(HttpFault::Status(500), 1), (HttpFault::Truncate(None), 1)],
            notices: Vec::new(),
        }
    }

    pub fn is_running(&self) -> bool {
        self.mock.port().is_some()
    }

    /// Convert and add an image in the background (named after its file).
    pub fn add_file(&mut self, file_name: &str, bytes: Vec<u8>) {
        let name = mock_trmnl::name_from_file(&self.mock.state(), file_name);
        let (mock, opts, tx) = (self.mock.clone(), self.opts, self.events_tx.clone());
        self.converting += 1;
        std::thread::spawn(move || {
            let ev = match mock.add_image(&name, &bytes, opts) {
                Ok(img) => Event::Converted(format!("Added image {}", img.name), false),
                Err(e) => Event::Converted(format!("{name}: {e}"), true),
            };
            let _ = tx.send(ev);
        });
    }

    /// Handle finished background work; call once per frame.
    pub fn poll(&mut self, status: &Status) {
        while let Ok(ev) = self.events.try_recv() {
            match ev {
                Event::Converted(text, error) => {
                    self.converting = self.converting.saturating_sub(1);
                    self.notices.push((text, error));
                }
                Event::Onboarded(r) => {
                    self.onboard = Onboard::Idle;
                    match r {
                        Ok(()) => self.notices.push(("Sent WiFi and server to the setup portal".into(), false)),
                        Err(e) => self.notices.push((format!("Onboarding failed: {e}"), true)),
                    }
                }
            }
        }
        if self.onboard == Onboard::WaitingForPortal
            && let Some(url) = &status.portal_url
        {
            self.connect_portal(url.clone());
        }
    }

    /// Submit the simulated WiFi and this server's URL on the device's setup page.
    fn connect_portal(&mut self, portal_url: String) {
        let Some(server) = self.mock.device_url() else { return };
        self.onboard = Onboard::Connecting;
        let tx = self.events_tx.clone();
        std::thread::spawn(move || {
            let r = mock_trmnl::portal_connect(&portal_url, SSID, "password", &server).map(|_| ());
            let _ = tx.send(Event::Onboarded(r));
        });
    }

    fn start(&mut self) {
        match self.mock.start(self.port) {
            Ok(a) => self.notices.push((format!("Mock server listening on port {}", a.port()), false)),
            Err(e) => self.notices.push((format!("Can't start the mock server on port {}: {e}", self.port), true)),
        }
    }

    /// Something the device will see changed: wake it if asked to.
    fn changed(&self, h: &SimHandle, status: &Status) {
        if self.wake_on_change && matches!(status.state, RunState::DeepSleep { .. }) {
            h.send(Command::WakeFromSleep);
        }
    }

    pub fn ui(&mut self, ui: &mut Ui, h: &SimHandle, status: &Status, board: &BoardInfo) {
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 6.0;
            self.server_section(ui, status, h, board);
            self.images_section(ui, h, status);
            self.playlist_section(ui, h, status);
            self.response_section(ui, board);
            self.actions_section(ui, h, status);
            self.faults_section(ui, h, status);
            self.log_section(ui);
        });
    }

    fn server_section(&mut self, ui: &mut Ui, status: &Status, h: &SimHandle, board: &BoardInfo) {
        section(ui, "Mock server");
        let dark = ui.visuals().dark_mode;
        let green = if dark { Color32::from_rgb(0x5c, 0xc8, 0x6c) } else { Color32::from_rgb(0x1f, 0x8a, 0x34) };
        match self.mock.port() {
            Some(port) => {
                ui.horizontal(|ui| {
                    dot(ui, green, true);
                    ui.label(RichText::new(format!("Running on port {port}")).color(green));
                    if ui.button("Stop").clicked() {
                        self.mock.stop();
                    }
                });
                let url = self.mock.device_url().unwrap_or_default();
                ui.horizontal(|ui| {
                    ui.label("Device URL");
                    ui.label(RichText::new(&url).monospace().strong());
                    if ui.small_button("Copy").clicked() {
                        ui.ctx().copy_text(url.clone());
                        self.notices.push((format!("Copied {url}"), false));
                    }
                });
                ui.weak(
                    "Enter it as the server on the device's setup page (the device reaches this machine at 10.0.2.2).",
                );
            }
            None => {
                ui.horizontal(|ui| {
                    dot(ui, ui.visuals().weak_text_color(), false);
                    ui.weak("Stopped");
                    ui.add(egui::DragValue::new(&mut self.port).range(0..=65535).prefix("port "))
                        .on_hover_text("0 = any free port. Keep it fixed: the device remembers the URL.");
                    if ui.button("▶ Start").clicked() {
                        self.start();
                    }
                });
                ui.weak("A TRMNL API the device can use instead of trmnl.app: your images, your refresh rate.");
            }
        }

        section(ui, "Onboarding");
        let running = self.is_running();
        match self.onboard {
            Onboard::Connecting => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Submitting the setup page…");
                });
            }
            Onboard::WaitingForPortal => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Waiting for the device's setup portal…");
                    if ui.small_button("Cancel").clicked() {
                        self.onboard = Onboard::Idle;
                    }
                });
            }
            Onboard::Idle if status.portal_url.is_some() => {
                ui.label("The device is in WiFi setup.");
                if ui
                    .add_enabled(running, egui::Button::new("Connect it to this server"))
                    .on_hover_text(format!("Joins {SSID} and sets the server URL, like a phone on the setup page"))
                    .on_disabled_hover_text("Start the server first")
                    .clicked()
                {
                    self.connect_portal(status.portal_url.clone().unwrap_or_default());
                }
            }
            Onboard::Idle => {
                let hint = if board.has_button {
                    "Holds the button 6 s so the device forgets its WiFi and opens its setup portal, \
                     then joins it to this server (the API key is kept)"
                } else {
                    "Waits for the device's setup portal, then joins it to this server. A fresh device \
                     (--erase) opens it by itself; otherwise use the device's WiFi reset"
                };
                if ui
                    .add_enabled(running, egui::Button::new("Onboard the device here…"))
                    .on_hover_text(hint)
                    .on_disabled_hover_text("Start the server first")
                    .clicked()
                {
                    if board.has_button {
                        h.send(Command::Press { ms: 6000 });
                    }
                    self.onboard = Onboard::WaitingForPortal;
                }
            }
        }
    }

    fn images_section(&mut self, ui: &mut Ui, h: &SimHandle, status: &Status) {
        let (images, current, playlist, panel) = {
            let st = self.mock.state();
            (st.images.clone(), st.current.clone(), st.playlist.clone(), st.panel)
        };
        section(ui, &format!("Images ({})", images.len()));
        ui.horizontal(|ui| {
            if ui.button("➕ Add images…").on_hover_text("PNG, JPEG, BMP or GIF; or drop files on the window").clicked()
            {
                let files = rfd::FileDialog::new()
                    .add_filter("Images", &["png", "jpg", "jpeg", "bmp", "gif"])
                    .pick_files()
                    .unwrap_or_default();
                for f in files {
                    match std::fs::read(&f) {
                        Ok(b) => self.add_file(&f.to_string_lossy(), b),
                        Err(e) => self.notices.push((format!("{}: {e}", f.display()), true)),
                    }
                }
            }
            if self.converting > 0 {
                ui.spinner();
                ui.weak(format!("converting {}", self.converting));
            }
        });
        let (w, hgt) = panel.size();
        ui.weak(format!("Converted to {w}×{hgt} {} (drop files on the window to add them)", panel.format()));
        ui.horizontal(|ui| {
            ui.checkbox(&mut self.opts.dither, "Dither");
            ui.label("Fit");
            for (fit, label) in [(Fit::Contain, "contain"), (Fit::Cover, "cover"), (Fit::Stretch, "stretch")] {
                ui.selectable_value(&mut self.opts.fit, fit, label);
            }
        });

        let thumb_w = 96.0;
        let thumb = Vec2::new(thumb_w, thumb_w * hgt as f32 / w as f32);
        for img in &images {
            let tex = self.thumbnail(ui.ctx(), img);
            let is_current = img.name == current;
            let frame = egui::Frame::group(ui.style()).inner_margin(4.0).fill(if is_current {
                ui.visuals().selection.bg_fill.gamma_multiply(0.35)
            } else {
                Color32::TRANSPARENT
            });
            frame.show(ui, |ui| {
                ui.horizontal(|ui| {
                    let resp = ui
                        .add(egui::Image::new((tex.id(), thumb)).sense(egui::Sense::click()))
                        .on_hover_text("Click to serve this image");
                    ui.vertical(|ui| {
                        let mut name = RichText::new(&img.name);
                        if is_current {
                            name = name.strong();
                        }
                        ui.label(name);
                        ui.label(RichText::new(&img.filename).weak().small());
                        ui.horizontal(|ui| {
                            let show = ui
                                .add_enabled(!is_current, egui::Button::new("Show").small())
                                .on_hover_text("Serve this image from the next request on");
                            if show.clicked() || (resp.clicked() && !is_current) {
                                let _ = self.mock.state().set_current(&img.name);
                                self.changed(h, status);
                            }
                            let mut listed = playlist.contains(&img.name);
                            if ui.toggle_value(&mut listed, "☰").on_hover_text("In the playlist").changed() {
                                let mut st = self.mock.state();
                                if listed {
                                    st.playlist.push(img.name.clone());
                                } else {
                                    st.playlist.retain(|n| n != &img.name);
                                }
                                st.version += 1;
                            }
                            if img.name != DEFAULT_IMAGE && ui.small_button("🗑").on_hover_text("Remove").clicked() {
                                self.mock.state().remove_image(&img.name);
                            }
                        });
                    });
                });
            });
        }
    }

    fn thumbnail(&mut self, ctx: &egui::Context, img: &Image) -> TextureHandle {
        if let Some((v, t)) = self.thumbs.get(&img.name)
            && *v == img.version
        {
            return t.clone();
        }
        // Box-filter down to ~192 px wide (2x for sharp thumbnails on high-DPI screens).
        let p = &img.preview;
        let k = (p.width as usize).div_ceil(192).max(1);
        let (tw, th) = (p.width as usize / k, p.height as usize / k);
        let mut rgb = Vec::with_capacity(tw * th * 3);
        for ty in 0..th {
            for tx in 0..tw {
                let mut sum = [0u32; 3];
                for y in ty * k..ty * k + k {
                    for x in tx * k..tx * k + k {
                        let c = p.rgb(x as u32, y as u32);
                        (0..3).for_each(|i| sum[i] += c[i] as u32);
                    }
                }
                rgb.extend(sum.iter().map(|s| (s / (k * k) as u32) as u8));
            }
        }
        let tex = ctx.load_texture(
            format!("mock-{}", img.name),
            egui::ColorImage::from_rgb([tw, th], &rgb),
            TextureOptions::LINEAR,
        );
        self.thumbs.insert(img.name.clone(), (img.version, tex.clone()));
        tex
    }

    fn playlist_section(&mut self, ui: &mut Ui, h: &SimHandle, status: &Status) {
        section(ui, "Playlist");
        let mut st = self.mock.state();
        let mut auto = st.auto_advance;
        if ui
            .checkbox(&mut auto, "Next image on every request")
            .on_hover_text("Each /api/display serves the next playlist entry, like a TRMNL playlist")
            .changed()
        {
            st.auto_advance = auto;
            st.version += 1;
        }
        if st.playlist.is_empty() {
            ui.weak("Empty: add images with ☰.");
            return;
        }
        let mut action = None;
        for (i, name) in st.playlist.iter().enumerate() {
            ui.horizontal(|ui| {
                let current = *name == st.current;
                let text = RichText::new(format!("{}. {name}", i + 1));
                ui.label(if current { text.strong() } else { text });
                if current {
                    ui.weak("◀ current");
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.small_button("✕").clicked() {
                        action = Some((i, false));
                    }
                    if i > 0 && ui.small_button("⬆").clicked() {
                        action = Some((i, true));
                    }
                });
            });
        }
        match action {
            Some((i, true)) => st.playlist.swap(i, i - 1),
            Some((i, false)) => {
                st.playlist.remove(i);
            }
            None => {}
        }
        let mut step = 0;
        ui.horizontal(|ui| {
            if ui.button("⏮ Previous").clicked() {
                step = -1;
            }
            if ui.button("Next ⏭").clicked() {
                step = 1;
            }
        });
        if step != 0 {
            st.advance(step);
        }
        if action.is_some() || step != 0 {
            st.version += 1;
            drop(st);
            if step != 0 {
                self.changed(h, status);
            }
        }
    }

    fn response_section(&mut self, ui: &mut Ui, board: &BoardInfo) {
        section(ui, "Display response");
        let mut st = self.mock.state();
        ui.horizontal_wrapped(|ui| {
            ui.label("Refresh");
            ui.add(egui::DragValue::new(&mut st.refresh_rate).range(1..=86_400).suffix(" s"));
            for (label, s) in [("1m", 60), ("5m", 300), ("15m", 900), ("1h", 3600)] {
                if ui.selectable_label(st.refresh_rate == s, label).clicked() {
                    st.refresh_rate = s;
                }
            }
        });
        ui.horizontal(|ui| {
            ui.label("Double-click").on_hover_text(
                "special_function: what the device does on a double-click (stored on the device, \
                 then reported back with the next request)",
            );
            let mut sf = st.special_function.clone();
            egui::ComboBox::from_id_salt("mock_special_function").selected_text(&sf).show_ui(ui, |ui| {
                for f in mock_trmnl::SPECIAL_FUNCTIONS {
                    ui.selectable_value(&mut sf, f.to_string(), f);
                }
            });
            if sf != st.special_function {
                st.special_function = sf;
            }
        });
        let mut full = st.extra.get("maximum_compatibility").and_then(Value::as_bool).unwrap_or(false);
        if ui
            .checkbox(&mut full, "Full refresh (maximum_compatibility)")
            .on_hover_text("Ask for a full refresh instead of a fast/partial one")
            .changed()
        {
            set_extra(&mut st.extra, "maximum_compatibility", full.then_some(Value::Bool(true)));
        }
        if board.has_touchbar {
            ui.horizontal(|ui| {
                ui.label("Touch bar mode");
                let mut mode = st.extra.get("touchbar_mode").and_then(Value::as_str).unwrap_or("").to_string();
                for (m, label) in [("", "unset"), ("tap", "tap"), ("slide", "slide")] {
                    ui.selectable_value(&mut mode, m.to_string(), label);
                }
                let old = st.extra.get("touchbar_mode").and_then(Value::as_str).unwrap_or("");
                if mode != old {
                    set_extra(&mut st.extra, "touchbar_mode", (!mode.is_empty()).then(|| json!(mode)));
                }
            });
        }
        egui::CollapsingHeader::new("Advanced").id_salt("mock_advanced").show(ui, |ui| {
            ui.checkbox(&mut st.registered, "/api/setup registers the device")
                .on_hover_text("Off: /api/setup answers \"status\": 404, \"MAC ... not registered\"");
            ui.horizontal(|ui| {
                ui.label("Status");
                let mut status = st.extra.get("status").and_then(Value::as_i64).unwrap_or(0);
                for (s, label, tip) in [
                    (0, "0", "Normal"),
                    (202, "202", "Not registered yet: the device polls quickly"),
                    (500, "500", "Server error: the device forgets its credentials!"),
                ] {
                    ui.selectable_value(&mut status, s, label).on_hover_text(tip);
                }
                if Some(status) != st.extra.get("status").and_then(Value::as_i64).or(Some(0)) {
                    set_extra(&mut st.extra, "status", (status != 0).then(|| json!(status)));
                }
            });
            ui.horizontal(|ui| {
                ui.label("Friendly ID");
                ui.add(egui::TextEdit::singleline(&mut st.friendly_id).desired_width(80.0));
            });
        });
    }

    fn actions_section(&mut self, ui: &mut Ui, h: &SimHandle, status: &Status) {
        section(ui, "Next request only");
        let (queue, has_build_fw) = {
            let st = self.mock.state();
            (st.queue.clone(), st.files.contains_key(FIRMWARE_PATH))
        };
        let fw_label = match &self.custom_firmware {
            Some(p) => p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default(),
            None if has_build_fw => "firmware.bin (this build)".into(),
            None => "none".into(),
        };
        ui.horizontal(|ui| {
            ui.label("Firmware");
            ui.label(RichText::new(fw_label).monospace());
            if ui
                .small_button("Choose…")
                .on_hover_text("Another build's firmware.bin. The simulator also needs its firmware.elf (--elf).")
                .clicked()
                && let Some(p) = rfd::FileDialog::new().add_filter("Firmware", &["bin"]).pick_file()
            {
                self.mock.set_file(CUSTOM_FIRMWARE_PATH, FileSource::Path(p.clone()));
                self.custom_firmware = Some(p);
            }
        });
        let running = self.is_running();
        ui.horizontal_wrapped(|ui| {
            let fw_path = if self.custom_firmware.is_some() { Some(CUSTOM_FIRMWARE_PATH) } else { None };
            let fw_path = fw_path.or(has_build_fw.then_some(FIRMWARE_PATH));
            if ui
                .add_enabled(running && fw_path.is_some(), egui::Button::new("⬆ Firmware update"))
                .on_hover_text("update_firmware with a firmware_url on this server (OTA)")
                .clicked()
            {
                let url = format!("{}{}", self.mock.device_url().unwrap_or_default(), fw_path.unwrap_or_default());
                self.enqueue(h, status, [("update_firmware", json!(true)), ("firmware_url", json!(url))]);
            }
            if ui
                .add_enabled(running, egui::Button::new("⟲ Reset device"))
                .on_hover_text(
                    "reset_firmware: the device forgets WiFi, API key and settings, then restarts into setup",
                )
                .clicked()
            {
                self.enqueue(h, status, [("reset_firmware", json!(true))]);
            }
        });
        if !queue.is_empty() {
            ui.horizontal_wrapped(|ui| {
                ui.label(
                    RichText::new(format!("Queued: {}", queue.iter().map(describe).collect::<Vec<_>>().join("; ")))
                        .italics(),
                );
                if ui.small_button("✕").on_hover_text("Clear the queue").clicked() {
                    self.mock.state().queue.clear();
                }
            });
        }
        ui.horizontal(|ui| {
            ui.checkbox(&mut self.wake_on_change, "Wake the device on changes")
                .on_hover_text("End a deep sleep when you pick an image or queue an action, so it asks right away");
            let sleeping = matches!(status.state, RunState::DeepSleep { .. } | RunState::LightSleep { .. });
            if ui.add_enabled(sleeping, egui::Button::new("☀ Wake").small()).clicked() {
                h.send(Command::WakeFromSleep);
            }
        });
    }

    /// HTTP and connection failures per route (the firmware's scripts/mock_server.py set):
    /// each route's queue is used up in order, one request per use.
    fn faults_section(&mut self, ui: &mut Ui, h: &SimHandle, status: &Status) {
        let queued = {
            let st = self.mock.state();
            st.display_faults.len() + st.image_faults.len()
        };
        ui.horizontal(|ui| {
            section(ui, &if queued > 0 { format!("HTTP faults ({queued})") } else { "HTTP faults".into() });
            if queued > 0 && ui.small_button("Clear").clicked() {
                self.mock.state().clear_faults();
            }
        });
        for (i, route, label) in [(0, Route::Display, "/api/display"), (1, Route::Image, "Images")] {
            ui.push_id(route.name(), |ui| {
                let (fault, count) = &mut self.fault_edit[i];
                let mut add = false;
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new(label).monospace());
                    egui::ComboBox::from_id_salt("kind")
                        .selected_text(kind_label(fault))
                        .show_ui(ui, |ui| {
                            for k in HttpFault::kinds(route) {
                                if ui
                                    .selectable_label(k.kind() == fault.kind(), kind_label(&k))
                                    .on_hover_text(k.help())
                                    .clicked()
                                {
                                    *fault = k;
                                }
                            }
                        })
                        .response
                        .on_hover_text(fault.help());
                    fault_args(ui, fault);
                    ui.add(
                        egui::DragValue::new(count)
                            .range(0..=99)
                            .custom_formatter(|n, _| if n == 0.0 { "always".into() } else { format!("×{n}") }),
                    )
                    .on_hover_text("How many requests it fails (drag; 0: every one until cleared)");
                    add = ui.button("Add").clicked();
                });
                if add {
                    let n = (*count > 0).then_some(*count);
                    let _ = self.mock.state().add_fault(route, fault.clone(), n);
                    self.changed(h, status);
                }
                let queue: Vec<String> = self.mock.state().faults(route).iter().map(|f| f.to_string()).collect();
                if !queue.is_empty() {
                    ui.horizontal_wrapped(|ui| {
                        ui.weak("Queued:");
                        for (j, spec) in queue.iter().enumerate() {
                            ui.label(RichText::new(spec).monospace().small());
                            if ui.small_button("✕").on_hover_text("Remove").clicked() {
                                let mut st = self.mock.state();
                                st.faults_mut(route).remove(j);
                                st.version += 1;
                            }
                        }
                    });
                }
            });
        }
    }

    fn enqueue<const N: usize>(&mut self, h: &SimHandle, status: &Status, fields: [(&str, Value); N]) {
        let map: Map<String, Value> = fields.into_iter().map(|(k, v)| (k.to_string(), v)).collect();
        self.mock.state().enqueue(map);
        self.changed(h, status);
    }

    fn log_section(&mut self, ui: &mut Ui) {
        let (total, reqs): (u64, Vec<Request>) = {
            let st = self.mock.state();
            (st.total_requests, st.requests.iter().rev().take(LOG_ROWS).cloned().collect())
        };
        ui.horizontal(|ui| {
            section(ui, &format!("Requests ({total})"));
            if !reqs.is_empty() && ui.small_button("Clear").clicked() {
                self.mock.state().requests.clear();
            }
        });
        if reqs.is_empty() {
            ui.weak("None yet.");
            return;
        }
        let weak = ui.visuals().weak_text_color();
        let err = ui.visuals().error_fg_color;
        for r in &reqs {
            let resp = ui
                .vertical(|ui| {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(clock(r.at)).monospace().small().color(weak)).on_hover_text("UTC");
                        ui.label(RichText::new(&r.method).monospace().small());
                        ui.label(RichText::new(&r.path).monospace().strong());
                        // 0: a fault sent no response (timeout, reset, close)
                        let code = if r.status == 0 { "—".to_string() } else { r.status.to_string() };
                        let code = RichText::new(code).monospace().small();
                        let failed = r.status == 0 || r.status >= 400 || r.summary.starts_with("fault ");
                        ui.label(if failed { code.color(err) } else { code.color(weak) });
                    });
                    let key = key_headers(r);
                    if !key.is_empty() {
                        ui.label(RichText::new(key).small());
                    }
                    if !r.summary.is_empty() {
                        ui.label(RichText::new(format!("→ {}", r.summary)).small().color(weak));
                    }
                })
                .response;
            resp.on_hover_ui(|ui| {
                ui.set_max_width(460.0);
                if let Some(t) = r.sim_time_ns {
                    ui.label(format!("virtual time {}", format_sim_time(t)));
                }
                for (k, v) in &r.headers {
                    ui.label(RichText::new(format!("{k}: {v}")).monospace().small());
                }
                if !r.body.is_empty() {
                    let body = String::from_utf8_lossy(&r.body[..r.body.len().min(2000)]).to_string();
                    ui.separator();
                    ui.label(RichText::new(body).monospace().small());
                }
            });
            ui.separator();
        }
    }
}

/// A fault kind as the menu shows it.
fn kind_label(f: &HttpFault) -> &'static str {
    if let HttpFault::Status(_) = f { "HTTP status" } else { f.kind() }
}

/// Editors for a fault's arguments.
fn fault_args(ui: &mut Ui, f: &mut HttpFault) {
    let secs = |ui: &mut Ui, s: &mut f32| {
        ui.add(egui::DragValue::new(s).range(0.0..=600.0).speed(0.5).suffix(" s"));
    };
    let bytes = |ui: &mut Ui, b: &mut usize| {
        ui.add(egui::DragValue::new(b).range(0..=10_000_000).speed(64).suffix(" B"));
    };
    match f {
        HttpFault::Status(c) => {
            ui.add(egui::DragValue::new(c).range(100..=599));
        }
        HttpFault::Timeout(s) => secs(ui, s),
        HttpFault::Redirect(c) => {
            ui.selectable_value(c, 307, "307");
            ui.selectable_value(c, 308, "308");
        }
        HttpFault::JsonStatus(n) => {
            ui.add(egui::DragValue::new(n).range(0..=999))
                .on_hover_text("202: not registered yet (fast poll); 500: the device WIPES its credentials");
        }
        HttpFault::Truncate(n) => {
            let mut half = n.is_none();
            if ui.checkbox(&mut half, "half").changed() {
                *n = if half { None } else { Some(1024) };
            }
            if let Some(b) = n {
                bytes(ui, b);
            }
        }
        HttpFault::Slow(b, s) => {
            bytes(ui, b);
            ui.weak("then");
            secs(ui, s);
        }
        _ => {}
    }
}

fn set_extra(extra: &mut Map<String, Value>, key: &str, v: Option<Value>) {
    match v {
        Some(v) => extra.insert(key.to_string(), v),
        None => extra.remove(key),
    };
}

/// A queued one-shot answer, briefly.
fn describe(q: &Map<String, Value>) -> String {
    let flags: Vec<&str> = q.iter().filter(|(_, v)| **v == json!(true)).map(|(k, _)| k.as_str()).collect();
    if flags.is_empty() { q.keys().cloned().collect::<Vec<_>>().join(", ") } else { flags.join(", ") }
}

/// The headers worth a glance: why the device woke, battery, signal, firmware.
fn key_headers(r: &Request) -> String {
    let mut parts = Vec::new();
    if let Some(v) = r.header("Update-Source") {
        parts.push(v.to_string());
    }
    if let Some(v) = r.header("Battery-Voltage") {
        parts.push(format!("{v} V"));
    }
    if let Some(v) = r.header("RSSI") {
        parts.push(format!("{v} dBm"));
    }
    if let Some(v) = r.header("FW-Version") {
        parts.push(format!("fw {v}"));
    }
    if r.header("special_function").is_some() {
        parts.push("double-click".into());
    }
    parts.join(" · ")
}

/// `HH:MM:SS` UTC.
fn clock(t: SystemTime) -> String {
    let s = t.duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0) % 86_400;
    format!("{:02}:{:02}:{:02}", s / 3600, (s / 60) % 60, s % 60)
}
