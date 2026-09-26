//! Runs the GUI against a fake emulator thread.
//!
//!     cargo run -p sim-ui --example demo
//!
//! The fake device boots, logs, refreshes the panel (with a flashing waveform), deep-sleeps for
//! 20 virtual seconds and repeats. The button wakes it from sleep; holding it 15 s "halts" the
//! fake emulator (Reset / Power-cycle recover). Pass `--halt` to start in the halted state;
//! set `SIM_UI_THEME=light|dark` to force a theme.

use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use sim_api::{Command, Frame, RunState, SimPorts};

const W: usize = 800;
const H: usize = 480;
const TICK: Duration = Duration::from_millis(20);

fn main() -> anyhow::Result<()> {
    let frame = Arc::new(Mutex::new(Frame { width: W, height: H, pixels: vec![0; W * H], generation: 0 }));
    let (handle, ports) = sim_api::channel(frame);
    {
        let mut s = handle.status.lock();
        s.firmware = "trmnl-firmware 1.8.14 (demo)".into();
    }
    let emu = std::thread::spawn(move || fake_emulator(ports));
    sim_ui::run(handle, sim_ui::UiOptions { title: "TRMNL Simulator — demo".into(), scale: 1.0 })?;
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
        self.target_img = test_image(self.screen_no);
        self.phase = Phase::Refreshing { start: self.t };
        self.p.status.lock().display_busy = true;
        self.idf('I', "display", &format!("full refresh #{}", self.screen_no));
    }

    /// invert → black → white → final, each ~0.5 s, with a quick ramp inside each phase.
    fn refresh_step(&mut self, start: u64) -> bool {
        let el = (self.t - start) as f32 / 1e9;
        let phase = ((el / 0.5) as usize).min(4);
        let tphase = ((el % 0.5) / 0.3).min(1.0);
        let inv: Vec<u8> = self.prev_img.iter().map(|&d| 255 - d).collect();
        let steps: [&dyn Fn(usize) -> u8; 5] =
            [&|i| self.prev_img[i], &|i| inv[i], &|_| 255, &|_| 0, &|i| self.target_img[i]];
        let (from, to) = (steps[phase.min(3)], steps[(phase + 1).min(4)]);
        let t = if phase >= 4 { 1.0 } else { tphase };
        let mut f = self.p.frame.lock();
        for i in 0..W * H {
            let a = from(i) as f32;
            let b = to(i) as f32;
            f.pixels[i] = (a + (b - a) * t) as u8;
        }
        f.generation += 1;
        phase >= 4
    }

    fn handle(&mut self, c: Command) {
        self.sim(&format!("command: {c:?}"));
        match c {
            Command::Press { ms } => {
                self.sim(&format!("press for {ms} ms"));
                if let Phase::Sleeping { .. } = self.phase {
                    self.boot("button");
                }
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
            Command::Quit => {}
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

fn fake_emulator(p: SimPorts) {
    let mut f = Fake {
        target_img: vec![0; W * H],
        prev_img: vec![0; W * H],
        p,
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
        fr.pixels = test_image(0);
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

/// Test pattern: header bar, "text" blocks, grey steps, a gradient, a circle and a checkerboard.
fn test_image(n: u64) -> Vec<u8> {
    let mut px = vec![0u8; W * H];
    fn rect(px: &mut [u8], x0: usize, y0: usize, w: usize, h: usize, v: u8) {
        for y in y0..(y0 + h).min(H) {
            for x in x0..(x0 + w).min(W) {
                px[y * W + x] = v;
            }
        }
    }
    // Header bar with white "title" blocks.
    rect(&mut px, 0, 0, W, 44, 255);
    rect(&mut px, 20, 14, 180, 16, 0);
    rect(&mut px, 210, 14, 90, 16, 0);
    // Text-ish paragraphs (vary with n so each refresh visibly changes).
    let mut seed = 0x9e37_79b9u32.wrapping_add(n as u32 * 7919);
    for line in 0..14 {
        let y = 70 + line * 26;
        let mut x = 24;
        let limit = if line % 5 == 4 { 260 } else { 440 };
        while x < limit {
            seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
            let w = 12 + (seed >> 16) as usize % 60;
            rect(&mut px, x, y, w.min(limit - x), 12, if line == 0 { 255 } else { 230 });
            x += w + 8;
        }
    }
    // Four grey steps.
    for (i, v) in [0u8, 85, 170, 255].into_iter().enumerate() {
        rect(&mut px, 500 + i * 70, 70, 70, 90, v);
    }
    // Outline around the steps.
    rect(&mut px, 498, 68, 284, 2, 255);
    rect(&mut px, 498, 160, 284, 2, 255);
    rect(&mut px, 498, 68, 2, 94, 255);
    rect(&mut px, 780, 68, 2, 94, 255);
    // Smooth gradient.
    for y in 180..220 {
        for x in 500..780 {
            px[y * W + x] = ((x - 500) * 255 / 279) as u8;
        }
    }
    // Checkerboard.
    for y in 240..330 {
        for x in 500..620 {
            if ((x / 6) + (y / 6)) % 2 == 0 {
                px[y * W + x] = 255;
            }
        }
    }
    // Circle.
    let (cx, cy, r) = (700.0f32, 285.0f32, 44.0f32);
    for y in 230..340 {
        for x in 640..760 {
            let d = ((x as f32 - cx).powi(2) + (y as f32 - cy).powi(2)).sqrt();
            if (d - r).abs() < 2.5 {
                px[y * W + x] = 255;
            }
        }
    }
    // Refresh counter as a row of squares.
    for i in 0..(n as usize % 20) {
        rect(&mut px, 500 + i * 14, 360, 10, 10, 255);
    }
    // Footer rule.
    rect(&mut px, 20, 440, W - 40, 2, 170);
    rect(&mut px, 20, 450, 120, 10, 170);
    px
}
