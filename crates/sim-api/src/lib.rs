//! The contract between the emulator thread and front-ends (GUI, headless CLI).
//! Front-ends only see this module; they never touch the machine directly.

use std::collections::VecDeque;
use std::sync::Arc;

use crossbeam_channel::{Receiver, Sender};
use parking_lot::Mutex;

/// What the viewer sees on the e-paper: per-pixel darkness 0 (white) ..= 255 (black).
#[derive(Clone)]
pub struct Frame {
    pub width: usize,
    pub height: usize,
    /// Darkness per pixel: 0 = paper, 255 = full ink.
    pub pixels: Vec<u8>,
    /// Color panels: what a viewer sees, RGB888 per pixel. `None` on grayscale panels,
    /// whose color follows from `pixels`.
    pub rgb: Option<Vec<u8>>,
    /// Bumped whenever `pixels` changes.
    pub generation: u64,
}

impl Frame {
    pub fn new(width: usize, height: usize) -> Self {
        Frame { width, height, pixels: vec![0; width * height], rgb: None, generation: 0 }
    }

    pub fn is_color(&self) -> bool {
        self.rgb.is_some()
    }

    /// A region as seen by a viewer: gray (1 byte/pixel, 0 = black, 255 = paper) or,
    /// on color panels, RGB (3 bytes/pixel). Returns (width, height, channels, data).
    pub fn viewer(&self, crop: Option<(usize, usize, usize, usize)>) -> (usize, usize, usize, Vec<u8>) {
        let (x0, y0, w, h) = crop.unwrap_or((0, 0, self.width, self.height));
        let (x0, y0) = (x0.min(self.width), y0.min(self.height));
        let (w, h) = (w.min(self.width - x0), h.min(self.height - y0));
        let ch = if self.rgb.is_some() { 3 } else { 1 };
        let mut out = Vec::with_capacity(w * h * ch);
        for y in y0..y0 + h {
            let row = y * self.width + x0;
            match &self.rgb {
                Some(rgb) => out.extend_from_slice(&rgb[row * 3..(row + w) * 3]),
                None => out.extend(self.pixels[row..row + w].iter().map(|d| 255 - d)),
            }
        }
        (w, h, ch, out)
    }
}

pub type SharedFrame = Arc<Mutex<Frame>>;

/// A zone of a capacitive touch bar (TRMNL X).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TouchZone {
    Left,
    Center,
    Right,
}

impl TouchZone {
    pub fn bit(self) -> u8 {
        match self {
            TouchZone::Left => 1,
            TouchZone::Center => 2,
            TouchZone::Right => 4,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            TouchZone::Left => "left",
            TouchZone::Center => "center",
            TouchZone::Right => "right",
        }
    }

    pub fn parse(s: &str) -> Option<TouchZone> {
        match s {
            "left" => Some(TouchZone::Left),
            "center" | "centre" | "middle" => Some(TouchZone::Center),
            "right" => Some(TouchZone::Right),
            _ => None,
        }
    }
}

/// What the simulated board has, so front-ends only offer relevant controls.
#[derive(Debug, Clone, Default)]
pub struct BoardInfo {
    pub name: String,
    pub has_button: bool,
    pub has_touchbar: bool,
    pub has_dock: bool,
    pub has_5ghz: bool,
    /// The panel's refresh flashes can be switched off (`Command::SetRefreshFlashing`).
    pub has_refresh_flashing: bool,
}

/// Requests from the front-end to the emulator thread.
#[derive(Debug, Clone)]
pub enum Command {
    /// The physical button is held down (true) or released (false).
    Button(bool),
    /// Press the button and release it after exactly this many milliseconds of
    /// virtual time (precise even when running faster or slower than real time).
    Press {
        ms: u64,
    },
    /// Tap a touch bar zone (finger down for `ms` of virtual time, default ~120 ms).
    Touch {
        zone: TouchZone,
        ms: u64,
    },
    /// Finger down on a touch bar zone (several zones may be held at once).
    TouchDown(TouchZone),
    /// Finger lifted from a touch bar zone.
    TouchUp(TouchZone),
    /// Put the device on (true) or take it off (false) its magnetic dock.
    SetDocked(bool),
    /// Battery voltage in millivolts.
    SetBatteryMv(u32),
    /// Show the black/white flashes of a (4-color) panel refresh (true, the default), or
    /// keep the old image until the new one appears (false). Refresh timing is unchanged.
    SetRefreshFlashing(bool),
    /// Press the reset button (chip reset, RTC memory and display kept).
    Reset,
    /// Remove and restore power: RTC memory is lost; flash and display are kept.
    PowerCycle,
    /// End a deep/light sleep now, as if its timer expired.
    WakeFromSleep,
    /// Enable/disable the simulated access point the device joins.
    SetWifiAvailable(bool),
    /// Run as fast as possible (true) or pace to wall-clock time (false).
    SetTurbo(bool),
    Pause(bool),
    /// Write CPU state and board diagnostics to the console (as [sim] lines).
    DumpDebug,
    Quit,
}

#[derive(Debug, Clone, PartialEq)]
pub enum RunState {
    Running,
    Paused,
    /// Waiting for an interrupt (FreeRTOS idle); still "awake".
    Idle,
    LightSleep {
        wake_at_ns: Option<u64>,
    },
    DeepSleep {
        wake_at_ns: Option<u64>,
    },
    /// The emulator stopped: CPU exception, unimplemented feature, etc.
    Halted(String),
}

#[derive(Debug, Clone)]
pub struct Status {
    pub state: RunState,
    /// Virtual time since power-on, in nanoseconds.
    pub sim_time_ns: u64,
    /// Emulation speed in millions of guest instructions per second.
    pub mips: f64,
    /// Virtual time / wall time over the last second (1.0 = realtime).
    pub speed_ratio: f64,
    pub board: BoardInfo,
    pub battery_mv: u32,
    pub button_down: bool,
    /// Touch bar zone currently touched, if any.
    pub touching: Option<TouchZone>,
    /// All touch bar zones currently held (bit 0 left, 1 center, 2 right).
    pub touch_mask: u8,
    pub docked: bool,
    /// The battery is being charged (docked and not full).
    pub charging: bool,
    /// Number of completed `Command::Touch` taps.
    pub touches_done: u64,
    pub wifi_available: bool,
    pub wifi_connected: bool,
    pub ip: Option<String>,
    /// Host URL of the device's captive portal while it runs its setup access point.
    pub portal_url: Option<String>,
    /// True while the e-paper panel reports BUSY (refreshing).
    pub display_busy: bool,
    pub display_refreshes: u64,
    pub boot_count: u32,
    /// Number of completed `Command::Press` presses.
    pub presses_done: u64,
    pub firmware: String,
    pub turbo: bool,
}

impl Default for Status {
    fn default() -> Self {
        Status {
            state: RunState::Running,
            sim_time_ns: 0,
            mips: 0.0,
            speed_ratio: 0.0,
            board: BoardInfo::default(),
            battery_mv: 4100,
            button_down: false,
            touching: None,
            touch_mask: 0,
            docked: false,
            charging: false,
            touches_done: 0,
            wifi_available: true,
            wifi_connected: false,
            ip: None,
            portal_url: None,
            display_busy: false,
            display_refreshes: 0,
            boot_count: 0,
            presses_done: 0,
            firmware: String::new(),
            turbo: false,
        }
    }
}

/// Serial console output, split into lines.
#[derive(Default)]
pub struct Console {
    pub lines: VecDeque<String>,
    partial: Vec<u8>,
    /// Total lines ever pushed (lets viewers detect new output cheaply).
    pub total: u64,
}

impl Console {
    /// Lines with absolute index >= `since` still in the buffer, as (index, line).
    pub fn lines_since(&self, since: u64) -> Vec<(u64, String)> {
        let first = self.total - self.lines.len() as u64;
        let start = since.max(first);
        self.lines
            .iter()
            .skip((start - first) as usize)
            .enumerate()
            .map(|(i, l)| (start + i as u64, l.clone()))
            .collect()
    }
}

impl Console {
    const MAX_LINES: usize = 20_000;

    pub fn push_bytes(&mut self, bytes: &[u8]) {
        for &b in bytes {
            match b {
                b'\n' => {
                    let line = String::from_utf8_lossy(&self.partial).trim_end_matches('\r').to_string();
                    self.partial.clear();
                    self.push_line(line);
                }
                _ => self.partial.push(b),
            }
        }
    }

    /// A line generated by the simulator itself (not the guest).
    pub fn push_sim(&mut self, msg: &str) {
        self.push_line(format!("[sim] {msg}"));
    }

    fn push_line(&mut self, line: String) {
        self.lines.push_back(line);
        self.total += 1;
        while self.lines.len() > Self::MAX_LINES {
            self.lines.pop_front();
        }
    }
}

/// Everything a front-end needs. Cheap to clone.
#[derive(Clone)]
pub struct SimHandle {
    pub frame: SharedFrame,
    pub console: Arc<Mutex<Console>>,
    pub status: Arc<Mutex<Status>>,
    pub commands: Sender<Command>,
}

impl SimHandle {
    pub fn send(&self, c: Command) {
        let _ = self.commands.send(c);
    }
}

/// The emulator thread's side of the handle.
pub struct SimPorts {
    pub frame: SharedFrame,
    pub console: Arc<Mutex<Console>>,
    pub status: Arc<Mutex<Status>>,
    pub commands: Receiver<Command>,
}

pub fn channel(frame: SharedFrame) -> (SimHandle, SimPorts) {
    let (tx, rx) = crossbeam_channel::unbounded();
    let console = Arc::new(Mutex::new(Console::default()));
    let status = Arc::new(Mutex::new(Status::default()));
    (
        SimHandle { frame: frame.clone(), console: console.clone(), status: status.clone(), commands: tx },
        SimPorts { frame, console, status, commands: rx },
    )
}
