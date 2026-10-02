//! Runs the GUI against a fake emulator thread.
//!
//!     cargo run -p sim-ui --example demo
//!
//! The fake device boots, logs, refreshes the panel (with a flashing waveform), deep-sleeps for
//! 20 virtual seconds and repeats. The button wakes it from sleep; holding it 15 s "halts" the
//! fake emulator (Reset / Power-cycle recover). Pass `--halt` to start in the halted state;
//! set `SIM_UI_THEME=light|dark` to force a theme.
//!
//! `--board x` fakes a TRMNL X instead: 1872×1404 16-grey panel, a three-zone touch bar (no
//! button), a charging dock and 5 GHz WiFi. Touches wake it from sleep; holding left + right
//! for 2 s logs the WiFi-reset gesture.

use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use sim_api::{BoardInfo, Command, Frame, RunState, SimPorts, TouchZone};

const TICK: Duration = Duration::from_millis(20);

#[derive(Clone, Copy)]
struct Cfg {
    w: usize,
    h: usize,
    /// Grey levels the panel can show (4 on the OG, 16 on the X).
    levels: u32,
    x: bool,
}

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let x = args.windows(2).any(|w| w[0] == "--board" && w[1].eq_ignore_ascii_case("x"))
        || args.iter().any(|a| a == "--board=x");
    let cfg = if x { Cfg { w: 1872, h: 1404, levels: 16, x } } else { Cfg { w: 800, h: 480, levels: 4, x } };
    let frame = Arc::new(Mutex::new(Frame::new(cfg.w, cfg.h)));
    let (handle, ports) = sim_api::channel(frame);
    {
        let mut s = handle.status.lock();
        s.firmware = "trmnl-firmware 1.8.14 (demo)".into();
        s.board = if x {
            BoardInfo {
                name: "TRMNL X".into(),
                has_button: false,
                has_touchbar: true,
                has_dock: true,
                has_5ghz: true,
                has_fuel_gauge: true,
                ..Default::default()
            }
        } else {
            BoardInfo {
                name: "TRMNL OG".into(),
                has_button: true,
                has_touchbar: false,
                has_dock: false,
                has_5ghz: false,
                has_refresh_flashing: false,
                has_fuel_gauge: false,
            }
        };
    }
    let emu = std::thread::spawn(move || fake_emulator(ports, cfg));
    sim_ui::run(handle, sim_ui::UiOptions { title: "TRMNL Simulator — demo".into(), ..Default::default() })?;
    let _ = emu.join();
    Ok(())
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Phase {
    Booting { until: u64 },
    Refreshing { start: u64 },
    Settling { until: u64 },
    Sleeping { wake_at: u64 },
}

struct Fake {
    p: SimPorts,
    cfg: Cfg,
    /// Virtual time each touch zone went down (TouchDown), if held.
    held: [Option<u64>; 3],
    /// A `Command::Touch` tap in progress: (zone, lift time).
    tap: Option<(TouchZone, u64)>,
    /// A `Command::Press` in progress: release time.
    press_until: Option<u64>,
    combo_logged: bool,
    t: u64,
    phase: Phase,
    turbo: bool,
    paused: bool,
    halted: Option<String>,
    button_since: Option<u64>,
    boot_lines: usize,
    screen_no: u64,
    prev_img: Vec<u8>,
    target_img: Vec<u8>,
}

fn ms(n: u64) -> u64 {
    n * 1_000_000
}

impl Fake {
    fn log(&self, s: &str) {
        let mut c = self.p.console.lock();
        c.push_bytes(s.as_bytes());
        c.push_bytes(b"\n");
    }

    fn sim(&self, s: &str) {
        self.p.console.lock().push_sim(s);
    }

    fn idf(&self, level: char, tag: &str, msg: &str) {
        let color = match level {
            'E' => "\x1b[0;31m",
            'W' => "\x1b[0;33m",
            'I' => "\x1b[0;32m",
            _ => "",
        };
        let reset = if color.is_empty() { "" } else { "\x1b[0m" };
        self.log(&format!("{color}{level} ({}) {tag}: {msg}{reset}", self.t / 1_000_000));
    }

    fn boot(&mut self, why: &str) {
        self.halted = None;
        self.boot_lines = 0;
        self.phase = Phase::Booting { until: self.t + ms(3000) };
        let mut s = self.p.status.lock();
        s.boot_count += 1;
        s.wifi_connected = false;
        s.ip = None;
        s.display_busy = false;
        drop(s);
        self.sim(&format!("boot ({why})"));
        self.log("ESP-ROM:esp32c3-api1-20210207");
        self.log("rst:0x5 (DSLEEP),boot:0xc (SPI_FAST_FLASH_BOOT)");
    }

    fn boot_step(&mut self) {
        let wifi = self.p.status.lock().wifi_available;
        let lines: &[(&str, &str, &str)] = &[
            ("I", "cpu_start", "Pro cpu up."),
            ("I", "cpu_start", "Pro cpu start user code"),
            ("I", "main", "\tTRMNL firmware starting\ttab-separated"),
            ("D", "pins", "button pin state: 1"),
            ("I", "wifi", "connecting to \"demo-ap\"…"),
            ("", "", "[   1510][I][display.cpp:120] display_init(): panel rev 2 (Arduino-style log line)"),
            ("W", "battery", "voltage reading is noisy"),
            (
                "",
                "",
                "plain line with a very long tail ———————————————————————————————————————————————————————————————————————————————————————————————————————— end",
            ),
        ];
        if let Some(&(lvl, tag, msg)) = lines.get(self.boot_lines) {
            if lvl.is_empty() {
                self.log(msg);
            } else {
                self.idf(lvl.chars().next().unwrap(), tag, msg);
            }
        } else if self.boot_lines == lines.len() {
            if wifi {
                let mut s = self.p.status.lock();
                s.wifi_connected = true;
                s.ip = Some("192.168.1.42".into());
                drop(s);
                self.idf('I', "wifi", "got ip: 192.168.1.42");
                self.idf('I', "api", "GET /api/display -> 200");
            } else {
                self.idf('W', "wifi", "no AP found");
                self.log("\x1b[1;31mE (2211) api: request failed: -1\x1b[0m");
            }
        }
        self.boot_lines += 1;
    }

    fn start_refresh(&mut self) {
        self.screen_no += 1;
        self.prev_img = self.p.frame.lock().pixels.clone();
        self.target_img = test_image(self.cfg, self.screen_no);
        self.phase = Phase::Refreshing { start: self.t };
        self.p.status.lock().display_busy = true;
        self.idf('I', "display", &format!("full refresh #{}", self.screen_no));
    }

    /// invert → black → white → final, each ~0.5 s, with a quick ramp inside each phase.
    fn refresh_step(&mut self, start: u64) -> bool {
        let el = (self.t - start) as f32 / 1e9;
        let phase = ((el / 0.5) as usize).min(4);
        let tphase = ((el % 0.5) / 0.3).min(1.0);
        let (prev, target) = (&self.prev_img, &self.target_img);
        let val = |k: usize, i: usize| match k {
            0 => prev[i],
            1 => 255 - prev[i],
            2 => 255,
            3 => 0,
            _ => target[i],
        };
        let (from, to) = (phase.min(3), (phase + 1).min(4));
        let t = if phase >= 4 { 1.0 } else { tphase };
        let mut f = self.p.frame.lock();
        for (i, px) in f.pixels.iter_mut().enumerate() {
            let a = val(from, i) as f32;
            let b = val(to, i) as f32;
            *px = (a + (b - a) * t) as u8;
        }
        f.generation += 1;
        phase >= 4
    }

    fn handle(&mut self, c: Command) {
        self.sim(&format!("command: {c:?}"));
        match c {
            Command::Touch { zone, ms: dur } => {
                self.tap = Some((zone, self.t + ms(dur)));
                self.p.status.lock().touching = Some(zone);
                self.wake_on_input("touch");
            }
            Command::TouchDown(zone) => {
                self.held[zone as usize] = Some(self.t);
                self.update_touching();
                self.wake_on_input("touch");
            }
            Command::TouchUp(zone) => {
                if let Some(t0) = self.held[zone as usize].take() {
                    self.sim(&format!("touch {} held {:.2} s (virtual)", zone.name(), (self.t - t0) as f64 / 1e9));
                }
                self.combo_logged = false;
                self.update_touching();
            }
            Command::SetRefreshFlashing(_)
            | Command::Gesture(_)
            | Command::SetWifiNetworks(_)
            | Command::SetPortalClient(_) => {}
            Command::PressRepeat { ms: dur, .. } => {
                self.press_until = Some(self.t + ms(dur));
                self.p.status.lock().button_down = true;
                self.wake_on_input("button");
            }
            Command::SetDocked(d) => {
                self.p.status.lock().docked = d;
                self.idf('I', "power", if d { "USB power connected (docked)" } else { "USB power removed" });
            }
            Command::Press { ms: dur } => {
                self.press_until = Some(self.t + ms(dur));
                self.p.status.lock().button_down = true;
                self.wake_on_input("button");
            }
            Command::Button(down) => {
                self.p.status.lock().button_down = down;
                if down {
                    self.button_since = Some(self.t);
                    if let Phase::Sleeping { .. } = self.phase {
                        self.sim("button wake");
                        self.boot("button");
                    }
                } else if let Some(s) = self.button_since.take() {
                    self.sim(&format!("button held {:.2} s (virtual)", (self.t - s) as f64 / 1e9));
                }
            }
            Command::SetBatteryMv(mv) => {
                self.p.status.lock().battery_mv = mv;
                if mv < 3300 {
                    self.idf('W', "battery", &format!("low battery: {mv} mV"));
                }
            }
            Command::Reset => self.boot("reset"),
            Command::PowerCycle => {
                self.p.status.lock().boot_count = 0;
                self.boot("power-on");
            }
            Command::WakeFromSleep => {
                if let Phase::Sleeping { .. } = self.phase {
                    self.boot("timer");
                }
            }
            Command::SetWifiAvailable(on) => {
                let mut s = self.p.status.lock();
                s.wifi_available = on;
                if !on {
                    s.wifi_connected = false;
                    s.ip = None;
                }
            }
            Command::SetTurbo(on) => {
                self.turbo = on;
                self.p.status.lock().turbo = on;
            }
            Command::Pause(p) => self.paused = p,
            Command::SetFaults(f) => self.p.status.lock().faults = f,
            Command::DumpDebug => self.log("[demo] no CPU to dump"),
            Command::ChangePreference { reply, .. } => {
                let _ = reply.send(Err("the demo has no firmware preferences".into()));
            }
            Command::ReadPreferences(reply) => {
                let _ = reply.send(sim_api::PreferencesSnapshot::default());
            }
            Command::Memcheck(reply) => {
                let _ = reply.send(r#"{"enabled": false}"#.into());
            }
            Command::SavePoint { reply, .. } | Command::RestoreSavePoint { reply, .. } => {
                if let Some(tx) = reply {
                    let _ = tx.send(Err("the demo has no device to save".into()));
                }
            }
            Command::WriteCoverage { reply, .. } => {
                let _ = reply.send(Err("the demo has no firmware to cover".into()));
            }
            Command::Bluetooth { reply, .. } => {
                let _ = reply.send(Err("the demo has no Bluetooth firmware".into()));
            }
            Command::AddApp { elf, reply } => {
                if let Some(tx) = reply {
                    let _ = tx.send(Ok(elf.display().to_string()));
                }
            }
            Command::Quit => {}
        }
    }

    fn wake_on_input(&mut self, why: &str) {
        if let Phase::Sleeping { .. } = self.phase {
            self.sim(&format!("{why} wake"));
            self.boot(why);
        }
    }

    fn update_touching(&mut self) {
        let z = [TouchZone::Left, TouchZone::Center, TouchZone::Right];
        let held = (0..3).find(|&i| self.held[i].is_some()).map(|i| z[i]);
        self.p.status.lock().touching = held.or(self.tap.map(|t| t.0));
    }

    /// Timed inputs, docking and gestures (runs every tick, even while paused, like real input).
    fn inputs(&mut self) {
        if let Some((zone, until)) = self.tap
            && self.t >= until
        {
            self.tap = None;
            self.p.status.lock().touches_done += 1;
            self.sim(&format!("touch bar tap: {}", zone.name()));
            self.update_touching();
        }
        if let Some(until) = self.press_until
            && self.t >= until
        {
            self.press_until = None;
            let mut s = self.p.status.lock();
            s.button_down = false;
            s.presses_done += 1;
        }
        if let (Some(l), Some(r)) = (self.held[0], self.held[2])
            && self.t - l.max(r) >= ms(2000)
            && !self.combo_logged
        {
            self.combo_logged = true;
            self.idf('W', "touch", "left+right held 2 s: WiFi credentials reset (demo)");
        }
        let mut s = self.p.status.lock();
        if s.docked && s.battery_mv < 4200 && self.t % ms(1000) < ms(20) {
            s.battery_mv += 5; // slow "charging"
        }
    }

    fn step(&mut self) {
        match self.phase {
            Phase::Booting { until } => {
                if self.t >= until {
                    self.start_refresh();
                } else if ((self.t + ms(3000) - until) / ms(300)) as usize > self.boot_lines {
                    self.boot_step();
                }
            }
            Phase::Refreshing { start } => {
                if self.refresh_step(start) {
                    self.p.status.lock().display_refreshes += 1;
                    self.p.status.lock().display_busy = false;
                    self.phase = Phase::Settling { until: self.t + ms(1000) };
                }
            }
            Phase::Settling { until } => {
                if self.t >= until {
                    let wake_at = self.t + ms(20_000);
                    self.idf('I', "sleep", "entering deep sleep for 20 s");
                    self.phase = Phase::Sleeping { wake_at };
                    let mut s = self.p.status.lock();
                    s.wifi_connected = false;
                    s.ip = None;
                }
            }
            Phase::Sleeping { wake_at } => {
                if self.t >= wake_at {
                    self.boot("timer");
                }
            }
        }
        if let Some(since) = self.button_since
            && self.t - since >= ms(15_000)
            && self.halted.is_none()
        {
            self.halted = Some(
                "demo: button held 15 s — simulated guest panic\nGuru Meditation Error: Core 0 panic'ed \
                 (Load access fault). pc=0x42001234 mtval=0x00000000"
                    .into(),
            );
            self.sim("halted");
        }
    }
}

fn fake_emulator(p: SimPorts, cfg: Cfg) {
    let mut f = Fake {
        target_img: vec![0; cfg.w * cfg.h],
        prev_img: vec![0; cfg.w * cfg.h],
        p,
        cfg,
        held: [None; 3],
        tap: None,
        press_until: None,
        combo_logged: false,
        t: 0,
        phase: Phase::Booting { until: 0 },
        turbo: false,
        paused: false,
        halted: None,
        button_since: None,
        boot_lines: 0,
        screen_no: 0,
    };
    // Some initial content and a pile of history to exercise the console view.
    {
        let mut fr = f.p.frame.lock();
        fr.pixels = test_image(cfg, 0);
        fr.generation += 1;
    }
    for i in 0..3000 {
        f.log(&format!("history line {i:04}: lorem ipsum dolor sit amet"));
    }
    f.boot("power-on");
    if std::env::args().any(|a| a == "--halt") {
        f.halted = Some("demo: started with --halt\nunimplemented peripheral register write: 0x6000_8000 <- 0x0000_0001 at pc=0x4200_1234".into());
    }
    let mut rng = 0x1234_5678u32;
    let mut next = Instant::now();
    loop {
        while let Ok(c) = f.p.commands.try_recv() {
            if matches!(c, Command::Quit) {
                return;
            }
            f.handle(c);
        }
        let wall = TICK;
        let running = !f.paused && f.halted.is_none();
        let dt = if running { wall.as_nanos() as u64 * if f.turbo { 10 } else { 1 } } else { 0 };
        f.t += dt;
        if running {
            f.step();
            f.inputs();
        }
        rng ^= rng << 13;
        rng ^= rng >> 17;
        rng ^= rng << 5;
        {
            let mut s = f.p.status.lock();
            s.sim_time_ns = f.t;
            s.speed_ratio = dt as f64 / wall.as_nanos() as f64;
            s.state = if let Some(h) = &f.halted {
                RunState::Halted(h.clone())
            } else if f.paused {
                RunState::Paused
            } else {
                match f.phase {
                    Phase::Sleeping { wake_at } => RunState::DeepSleep { wake_at_ns: Some(wake_at) },
                    Phase::Settling { .. } => RunState::Idle,
                    _ => RunState::Running,
                }
            };
            s.mips = match s.state {
                RunState::Running => 40.0 + (rng % 2000) as f64 / 100.0,
                RunState::Idle => 2.0,
                _ => 0.0,
            };
        }
        next += TICK;
        let now = Instant::now();
        if next > now {
            std::thread::sleep(next - now);
        } else {
            next = now;
        }
    }
}

/// Test pattern laid out on an 800×480 design grid and scaled to the panel: header bar, "text"
/// blocks, grey steps (one per panel level), a gradient, a circle and a checkerboard.
fn test_image(cfg: Cfg, n: u64) -> Vec<u8> {
    let (w, h) = (cfg.w, cfg.h);
    let (sx, sy) = (w as f32 / 800.0, h as f32 / 480.0);
    let mut px = vec![0u8; w * h];
    // Fill a design-space rectangle; `f` gets the position within it (0..1, 0..1) and pixel coords.
    let mut fill = |x0: f32, y0: f32, rw: f32, rh: f32, f: &dyn Fn(f32, f32, usize, usize) -> Option<u8>| {
        let (px0, py0) = ((x0 * sx) as usize, (y0 * sy) as usize);
        let (px1, py1) = ((((x0 + rw) * sx) as usize).min(w), (((y0 + rh) * sy) as usize).min(h));
        for y in py0..py1 {
            for x in px0..px1 {
                let u = (x - px0) as f32 / (px1 - px0).max(1) as f32;
                let v = (y - py0) as f32 / (py1 - py0).max(1) as f32;
                if let Some(d) = f(u, v, x, y) {
                    px[y * w + x] = d;
                }
            }
        }
    };
    let solid = |d: u8| move |_: f32, _: f32, _: usize, _: usize| Some(d);
    // Header bar with white "title" blocks.
    fill(0.0, 0.0, 800.0, 44.0, &solid(255));
    fill(20.0, 14.0, 180.0, 16.0, &solid(0));
    fill(210.0, 14.0, 90.0, 16.0, &solid(0));
    // Text-ish paragraphs (vary with n so each refresh visibly changes).
    let mut seed = 0x9e37_79b9u32.wrapping_add(n as u32 * 7919);
    for line in 0..14 {
        let y = 70.0 + line as f32 * 26.0;
        let mut x = 24.0;
        let limit = if line % 5 == 4 { 260.0 } else { 440.0 };
        while x < limit {
            seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
            let bw = 12.0 + ((seed >> 16) % 60) as f32;
            fill(x, y, bw.min(limit - x), 12.0, &solid(if line == 0 { 255 } else { 230 }));
            x += bw + 8.0;
        }
    }
    // One step per grey level the panel supports.
    let lv = cfg.levels;
    fill(500.0, 70.0, 280.0, 90.0, &|u, _, _, _| {
        let k = ((u * lv as f32) as u32).min(lv - 1);
        Some((k * 255 / (lv - 1)) as u8)
    });
    fill(498.0, 68.0, 284.0, 2.0, &solid(255));
    fill(498.0, 160.0, 284.0, 2.0, &solid(255));
    fill(498.0, 68.0, 2.0, 94.0, &solid(255));
    fill(780.0, 68.0, 2.0, 94.0, &solid(255));
    // Smooth gradient.
    fill(500.0, 180.0, 280.0, 40.0, &|u, _, _, _| Some((u * 255.0) as u8));
    // Checkerboard (in panel pixels, 6 px squares).
    fill(500.0, 240.0, 120.0, 90.0, &|_, _, x, y| (((x / 6) + (y / 6)) % 2 == 0).then_some(255));
    // Ring.
    fill(640.0, 230.0, 120.0, 110.0, &|u, v, _, _| {
        let (dx, dy) = ((u - 0.5) * 120.0 * sx, (v - 0.5) * 110.0 * sy);
        let r = 44.0 * sx.min(sy);
        ((dx * dx + dy * dy).sqrt() - r).abs().lt(&(2.5 * sx.min(sy))).then_some(255)
    });
    // Refresh counter as a row of squares.
    for i in 0..(n as usize % 20) {
        fill(500.0 + i as f32 * 14.0, 360.0, 10.0, 10.0, &solid(255));
    }
    // Footer rule.
    fill(20.0, 440.0, 760.0, 2.0, &solid(170));
    fill(20.0, 450.0, 120.0, 10.0, &solid(170));
    if cfg.x {
        fill(660.0, 448.0, 120.0, 14.0, &solid(85));
    }
    px
}
