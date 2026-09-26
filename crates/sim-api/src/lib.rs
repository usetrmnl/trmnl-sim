//! The contract between the emulator thread and front-ends (GUI, headless CLI).
//! Front-ends only see this module; they never touch the machine directly.

use std::collections::VecDeque;
use std::path::PathBuf;
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
    /// A fuel gauge on I2C (TRMNL X: BQ27427 at 0x55) that `Faults::i2c_absent` can remove.
    pub has_fuel_gauge: bool,
}

// ---- faults -------------------------------------------------------------------------------------

/// Faults injected into the simulated device on demand (`Command::SetFaults`). All off by
/// default; each field is independent.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Faults {
    pub net: NetFaults,
    /// Cut power when the firmware does a matching flash program/erase (one-shot: cleared
    /// once it fires).
    pub power_loss: Option<PowerLoss>,
    /// 7-bit I2C addresses whose device is gone: every transfer to them NACKs.
    pub i2c_absent: Vec<u8>,
    /// The display never finishes: UC8179 BUSY held low (OG/BWRY); on the TRMNL X, whose
    /// parallel panel has no BUSY line, the PMIC never reports power good.
    pub panel_busy_stuck: bool,
    /// The TRMNL X modem stops answering AT commands (input is ignored; the ROM loader
    /// still works).
    pub modem_unresponsive: bool,
}

impl Faults {
    pub fn is_empty(&self) -> bool {
        *self == Faults::default()
    }

    /// One line for logs, e.g. "latency 300 ms, DNS servfail, power loss at program #1 in nvs".
    pub fn summary(&self) -> String {
        let n = &self.net;
        let mut v = Vec::new();
        if n.latency_ms > 0 {
            v.push(format!("latency {} ms", n.latency_ms));
        }
        if n.loss > 0.0 {
            v.push(format!("loss {:.0}%", n.loss * 100.0));
        }
        if let Some(b) = n.bandwidth_bps {
            v.push(format!("bandwidth {b} B/s"));
        }
        if let Some(d) = n.dns {
            v.push(format!("DNS {}", d.name()));
        }
        if n.no_internet {
            v.push("no internet".into());
        }
        if n.offline {
            v.push("offline".into());
        }
        if let Some(c) = n.tcp_cut {
            let port = c.port.map(|p| format!(" to port {p}")).unwrap_or_default();
            let how = if c.stall { "stall" } else { "reset" };
            v.push(format!("TCP {how} after {} bytes{port}", c.after_bytes));
        }
        if let Some(p) = &self.power_loss {
            let mut w = format!("power loss at {} #{}", p.op.name(), p.nth);
            if let Some(name) = &p.partition {
                w += &format!(" in {name}");
            }
            if let Some((a, b)) = p.range {
                w += &format!(" in {a:#x}..{b:#x}");
            }
            w += &format!(" (cut {})", p.cut.name());
            v.push(w);
        }
        if !self.i2c_absent.is_empty() {
            let a: Vec<String> = self.i2c_absent.iter().map(|a| format!("{a:#04x}")).collect();
            v.push(format!("I2C absent {}", a.join(" ")));
        }
        if self.panel_busy_stuck {
            v.push("panel busy stuck".into());
        }
        if self.modem_unresponsive {
            v.push("modem unresponsive".into());
        }
        if v.is_empty() { "none".into() } else { v.join(", ") }
    }
}

/// Network faults, applied to the S3/C3's own WiFi path (the `vnet` router) and, where
/// they make sense, to the TRMNL X modem's host-side HTTP requests.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct NetFaults {
    /// Extra delay on every packet towards the device (so round trips grow by this much).
    pub latency_ms: u32,
    /// Probability (0..=1) of dropping each packet, in each direction.
    pub loss: f64,
    /// Link rate limit in bytes per second, each direction.
    pub bandwidth_bps: Option<u64>,
    pub dns: Option<DnsFault>,
    /// The access point works (association, DHCP) but nothing is routed beyond it: DNS goes
    /// unanswered and connections (even to the host, 10.0.2.2) time out.
    pub no_internet: bool,
    /// Only the host (10.0.2.2) is reachable, like `--offline`.
    pub offline: bool,
    pub tcp_cut: Option<TcpCut>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DnsFault {
    /// Answer SERVFAIL.
    ServFail,
    /// Answer NXDOMAIN (no such name).
    NxDomain,
    /// Answer NOERROR without addresses.
    Empty,
    /// Never answer.
    Timeout,
}

impl DnsFault {
    pub const ALL: [DnsFault; 4] = [DnsFault::ServFail, DnsFault::NxDomain, DnsFault::Empty, DnsFault::Timeout];

    pub fn name(self) -> &'static str {
        match self {
            DnsFault::ServFail => "servfail",
            DnsFault::NxDomain => "nxdomain",
            DnsFault::Empty => "empty",
            DnsFault::Timeout => "timeout",
        }
    }

    pub fn parse(s: &str) -> Option<DnsFault> {
        DnsFault::ALL.into_iter().find(|d| d.name().eq_ignore_ascii_case(s))
    }
}

/// Cut TCP connections (opened after the fault is set) once `after_bytes` of data went
/// towards the device on them. On the TRMNL X modem path, bytes of HTTP body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TcpCut {
    pub after_bytes: u64,
    /// Stall silently (the connection stays open, no more data) instead of sending a RST.
    pub stall: bool,
    /// Only connections to this destination port.
    pub port: Option<u16>,
}

/// Which flash operations a [`PowerLoss`] counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FlashOp {
    #[default]
    Any,
    Program,
    Erase,
}

impl FlashOp {
    pub fn name(self) -> &'static str {
        match self {
            FlashOp::Any => "any",
            FlashOp::Program => "program",
            FlashOp::Erase => "erase",
        }
    }

    pub fn parse(s: &str) -> Option<FlashOp> {
        [FlashOp::Any, FlashOp::Program, FlashOp::Erase].into_iter().find(|o| o.name() == s)
    }
}

/// How much of the operation that loses power gets done.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CutPoint {
    /// Power fails as the operation starts: nothing changes.
    #[default]
    Before,
    /// Halfway: the first half of a page program lands, or the first half of an erase
    /// (a torn page / half-erased sector, as on real NOR flash).
    Torn,
    /// Right after the operation completed.
    After,
}

impl CutPoint {
    pub fn name(self) -> &'static str {
        match self {
            CutPoint::Before => "before",
            CutPoint::Torn => "torn",
            CutPoint::After => "after",
        }
    }

    pub fn parse(s: &str) -> Option<CutPoint> {
        [CutPoint::Before, CutPoint::Torn, CutPoint::After].into_iter().find(|c| c.name() == s)
    }
}

/// Cut power at the `nth` flash operation matching `op` and the address filters (a named
/// partition and/or a byte range; an operation matches if it overlaps them).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PowerLoss {
    pub op: FlashOp,
    /// Partition label from the partition table, e.g. "nvs", "otadata", "app1", "spiffs".
    pub partition: Option<String>,
    /// Flash byte range `[start, end)`.
    pub range: Option<(u32, u32)>,
    /// 1 = the first matching operation (counted from when the fault is set).
    pub nth: u32,
    pub cut: CutPoint,
}

impl Default for PowerLoss {
    fn default() -> Self {
        PowerLoss { op: FlashOp::Any, partition: None, range: None, nth: 1, cut: CutPoint::Before }
    }
}

/// A partition table entry, for front-ends (`Status::partitions`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartitionInfo {
    pub label: String,
    /// 0 = app, 1 = data.
    pub kind: u8,
    pub subtype: u8,
    pub offset: u32,
    pub size: u32,
}

impl PartitionInfo {
    /// Look up a partition by label, or by the usual aliases (`ota_0`, `ota_1`, ... for the
    /// app slots, `littlefs` for the filesystem partition, `factory`).
    pub fn find<'a>(parts: &'a [PartitionInfo], name: &str) -> Option<&'a PartitionInfo> {
        if let Some(p) = parts.iter().find(|p| p.label.eq_ignore_ascii_case(name)) {
            return Some(p);
        }
        let (kind, subtype) = match name.to_ascii_lowercase().as_str() {
            "factory" => (0, 0),
            "littlefs" | "fat" => (1, 0x82),
            n => (0, 0x10 + n.strip_prefix("ota_")?.parse::<u8>().ok().filter(|&i| i < 16)?),
        };
        parts.iter().find(|p| p.kind == kind && p.subtype == subtype)
    }
}

/// A save point the emulator keeps in memory (see `Command::SavePoint`).
#[derive(Debug, Clone, PartialEq)]
pub struct SavePointInfo {
    /// Slot number, for `SavePointSource::Slot`.
    pub id: u32,
    pub label: String,
    /// Taken in deep sleep (restores into that sleep). Otherwise only non-volatile state
    /// was kept, and restoring powers the device on.
    pub deep_sleep: bool,
    /// Virtual time when it was taken.
    pub sim_time_ns: u64,
    /// Deep-sleep wake time (virtual ns), if a timer was armed.
    pub wake_at_ns: Option<u64>,
    /// The file it was saved to or loaded from.
    pub path: Option<PathBuf>,
    /// Compressed size in bytes.
    pub bytes: usize,
}

/// Where to restore a save point from.
#[derive(Debug, Clone, PartialEq)]
pub enum SavePointSource {
    /// An in-memory slot (`SavePointInfo::id`).
    Slot(u32),
    File(PathBuf),
}

/// Answer to a save point command: what was saved / restored, or why not.
pub type SavePointReply = Sender<Result<SavePointInfo, String>>;

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
    /// `count` presses of `ms` each, `gap_ms` apart (virtual time), e.g. a double click;
    /// counts as one press in `presses_done` once the last one is released.
    PressRepeat {
        ms: u64,
        gap_ms: u64,
        count: u32,
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
    /// Replace the set of injected faults.
    SetFaults(Faults),
    /// Write CPU state and board diagnostics to the console (as [sim] lines).
    DumpDebug,
    /// Take a save point into a new in-memory slot, and also write it to `path` if given.
    /// Full state in deep sleep; at other times only what survives a battery pull.
    SavePoint {
        label: Option<String>,
        path: Option<PathBuf>,
        reply: Option<SavePointReply>,
    },
    /// Replace the device with a save point (from a slot, or a file, which is also added
    /// as a slot). It must come from the same firmware build.
    RestoreSavePoint {
        from: SavePointSource,
        reply: Option<SavePointReply>,
    },
    /// Write the code coverage recorded so far as an lcov tracefile (to `path`, default
    /// the `--coverage` file), then forget it if `reset`. Needs `--coverage`.
    WriteCoverage {
        path: Option<std::path::PathBuf>,
        reset: bool,
        reply: Sender<Result<CoverageSummary, String>>,
    },
    /// Reply with the `--memcheck` report (JSON; `{"enabled": false}` when it's off).
    Memcheck(Sender<String>),
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
    /// In-memory save points, oldest first.
    pub savepoints: Vec<SavePointInfo>,
    /// Faults currently injected.
    pub faults: Faults,
    /// Times power was cut by a `Faults::power_loss` trigger.
    pub power_losses: u64,
    /// Flash page programs and erases since the simulator started.
    pub flash_programs: u64,
    pub flash_erases: u64,
    /// The flash's partition table.
    pub partitions: Vec<PartitionInfo>,
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
            savepoints: Vec::new(),
            faults: Faults::default(),
            power_losses: 0,
            flash_programs: 0,
            flash_erases: 0,
            partitions: Vec::new(),
        }
    }
}

/// Totals of a written coverage report.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CoverageSummary {
    /// Where the lcov tracefile was written.
    pub path: String,
    pub files: u64,
    /// Source lines with code, and how many of them executed.
    pub lines_found: u64,
    pub lines_hit: u64,
    pub functions_found: u64,
    pub functions_hit: u64,
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
