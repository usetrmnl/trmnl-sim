//! Azoteq IQS323 capacitive touch controller (TRMNL X touch bar: 0x44, RDY on
//! S3 GPIO3), modelled against lib/IQS323 (Azoteq Arduino driver v1.5.2),
//! lib/trmnl_x/src/iqs323_task.cpp, the deep-sleep wake stub and src/bl.cpp.
//!
//! # Memory map
//! Word-addressed, 16-bit registers sent low byte first; reads and writes
//! auto-increment through low byte, high byte, next address. Odd-length writes
//! change only the bytes sent (`setGestureConfig` writes one byte to 0xA0 and 0xD3).
//! - 0x00 product number 1106, 0x01/0x02 major/minor version (read-only)
//! - 0x10 SYSTEM_STATUS: low byte bit7 SHOW_RESET, bit6 ATI_ERROR, bit5 ATI_ACTIVE,
//!   bit2 SLIDER_EVENT; high byte bit0/1 CH0 prox/touch, bit2/3 CH1, bit4/5 CH2,
//!   bits 6-7 power mode
//! - 0x11 GESTURES low byte: bit0 TAP, bit1 SWIPE+, bit2 SWIPE-, bit3 FLICK+,
//!   bit4 FLICK-, bit5 HOLD, bit6 gesture event
//! - 0x12 slider coordinate (0xFFFF = no finger), 0x13..0x18 CHn counts / LTA
//! - 0x30..0xE1 configuration (stored and read back)
//! - 0xC0 SYSTEM_CONTROL low byte: bit0 ACK_RESET, bit1 SW_RESET, bit2 RE_ATI,
//!   bit3 RESEED (commands, self-clearing, executed at the STOP), bits 4-6 power
//!   mode, bit7 EVENT_MODE
//! - 0xA0 low byte gesture enable (assumed bit0 tap, bit1 swipe, bit2 flick, bit3
//!   hold; the firmware uses 0x09 = tap mode, 0x0B = slide mode), 0xA2 max tap
//!   time (ms), 0xA4 min hold time (ms)
//! - 0xD1 I2C window timeout (ms), 0xD3 low byte events enable (bit0 prox,
//!   bit1 touch, bit2 gesture; firmware: 0x02 tap mode, 0x04 slide mode)
//! - 0xFE/0xFF read 0xEE (the firmware's `check_i2c_lockup()` expects that)
//!
//! # RDY communication windows
//! RDY is open-drain, active low: the device pulls it low while a communication
//! window is open. A window closes at the STOP that ends a transaction which
//! started inside it (or after the 0xD1 timeout if the master never talks).
//! Windows open:
//! - in streaming mode (EVENT_MODE clear) once per report period;
//! - in event mode when an enabled event occurs (touch/prox change, gesture);
//! - [`Iqs323Config::force_comm_latency_ns`] after a "force communication"
//!   request — a transaction that started outside a window and only wrote the
//!   byte 0xFF (`force_I2C_communication()`).
//!
//! Transactions outside a window are still ACKed and served (the firmware's
//! `check_i2c_lockup()` reads 0xFE without asking for a window); they are
//! counted in [`Iqs323Stats::out_of_window_txns`].
//!
//! # Touch input
//! [`Iqs323::touch`] puts a finger on / lifts it from CH0 (left), CH1 (middle)
//! or CH2 (right); several may be down (left+right hold = WiFi-reset gesture).
//! Status bits track fingers live. Gestures are synthesised: TAP when all
//! fingers lift within the max tap time, HOLD (a level, with a one-shot
//! SLIDER_EVENT) once fingers stay down for the min hold time; swipes and
//! flicks come from [`Iqs323::gesture`]. Gesture bits and SLIDER_EVENT latch
//! until a transaction that read GESTURES ends.
//!
//! With [`Iqs323Config::latch_touch_until_read`], a press that raised an event
//! keeps its touch bit set until a transaction that read SYSTEM_STATUS ends,
//! even if the finger already lifted, so a short front-end click is still seen
//! by the wake stub; the release then raises its own event.

use super::I2cDevice;

pub const ADDR: u8 = 0x44;
pub const PRODUCT_NUMBER: u16 = 1106;
pub const VERSION_MAJOR: u16 = 1;
pub const VERSION_MINOR: u16 = 1;

pub const MM_PROD_NUM: u8 = 0x00;
pub const MM_MAJOR_VERSION: u8 = 0x01;
pub const MM_MINOR_VERSION: u8 = 0x02;
pub const MM_SYSTEM_STATUS: u8 = 0x10;
pub const MM_GESTURES: u8 = 0x11;
pub const MM_SLIDER_COORDINATES: u8 = 0x12;
pub const MM_GESTURE_ENABLE: u8 = 0xA0;
pub const MM_MAX_TAP_TIME: u8 = 0xA2;
pub const MM_MIN_HOLD_TIME: u8 = 0xA4;
pub const MM_SYSTEM_CONTROL: u8 = 0xC0;
pub const MM_NP_REPORT_RATE: u8 = 0xC1;
pub const MM_I2C_TIMEOUT: u8 = 0xD1;
pub const MM_EVENTS_ENABLE: u8 = 0xD3;
pub const MM_LOCKUP_CHECK: u8 = 0xFE;

// SYSTEM_STATUS low byte
pub const ST_SHOW_RESET: u8 = 1 << 7;
pub const ST_ATI_ERROR: u8 = 1 << 6;
pub const ST_ATI_ACTIVE: u8 = 1 << 5;
pub const ST_SLIDER_EVENT: u8 = 1 << 2;
// GESTURES low byte
pub const G_TAP: u8 = 1 << 0;
pub const G_SWIPE_POS: u8 = 1 << 1;
pub const G_SWIPE_NEG: u8 = 1 << 2;
pub const G_FLICK_POS: u8 = 1 << 3;
pub const G_FLICK_NEG: u8 = 1 << 4;
pub const G_HOLD: u8 = 1 << 5;
pub const G_EVENT: u8 = 1 << 6;
// SYSTEM_CONTROL low byte
pub const SC_ACK_RESET: u8 = 1 << 0;
pub const SC_SW_RESET: u8 = 1 << 1;
pub const SC_RE_ATI: u8 = 1 << 2;
pub const SC_RESEED: u8 = 1 << 3;
pub const SC_EVENT_MODE: u8 = 1 << 7;
// GESTURE_ENABLE low byte (assumed mapping, see module docs)
const GE_TAP: u8 = 1 << 0;
const GE_SWIPE: u8 = 1 << 1;
const GE_FLICK: u8 = 1 << 2;
const GE_HOLD: u8 = 1 << 3;
// EVENTS_ENABLE low byte
const EV_PROX: u8 = 1 << 0;
const EV_TOUCH: u8 = 1 << 1;
const EV_GESTURE: u8 = 1 << 2;

const US: u64 = 1_000;
const MS: u64 = 1_000_000;

/// Slider gestures the front-end can inject (TAP and HOLD are also synthesised
/// from `touch`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Gesture {
    Tap,
    SwipePos,
    SwipeNeg,
    FlickPos,
    FlickNeg,
    Hold,
}

impl Gesture {
    fn bits(self) -> (u8, u8) {
        match self {
            Gesture::Tap => (G_TAP, GE_TAP),
            Gesture::SwipePos => (G_SWIPE_POS, GE_SWIPE),
            Gesture::SwipeNeg => (G_SWIPE_NEG, GE_SWIPE),
            Gesture::FlickPos => (G_FLICK_POS, GE_FLICK),
            Gesture::FlickNeg => (G_FLICK_NEG, GE_FLICK),
            Gesture::Hold => (G_HOLD, GE_HOLD),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Iqs323Config {
    /// Force-communication request (0xFF write) to RDY low.
    pub force_comm_latency_ns: u64,
    /// Touch / gesture to RDY low in event mode.
    pub event_latency_ns: u64,
    /// Re-ATI duration (ATI_ACTIVE set).
    pub ati_duration_ns: u64,
    /// SW reset / MCLR / power-on until the device can open windows again.
    pub reset_boot_ns: u64,
    /// Overrides the window timeout from register 0xD1.
    pub window_timeout_ns: Option<u64>,
    /// See module docs.
    pub latch_touch_until_read: bool,
    /// Slider coordinate reported for a finger on CH0 / CH1 / CH2.
    pub slider_positions: [u16; 3],
    /// Shortest external low on RDY that the device takes as MCLR.
    pub mclr_min_ns: u64,
}

impl Default for Iqs323Config {
    fn default() -> Self {
        Iqs323Config {
            force_comm_latency_ns: MS,
            event_latency_ns: 5 * MS,
            ati_duration_ns: 30 * MS,
            reset_boot_ns: 10 * MS,
            window_timeout_ns: None,
            latch_touch_until_read: true,
            slider_positions: [256, 1024, 1792],
            mclr_min_ns: 250,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Iqs323Stats {
    pub windows_opened: u32,
    pub windows_timed_out: u32,
    pub force_comm_requests: u32,
    pub out_of_window_txns: u32,
    pub resets: u32,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum EventKind {
    Touch,
    Gesture,
}

pub struct Iqs323 {
    pub cfg: Iqs323Config,
    pub stats: Iqs323Stats,
    regs: [u16; 256],

    // I2C byte pointer
    ptr: u8,
    hi: bool,
    expect_ptr: bool,

    // Current transaction
    in_txn: bool,
    window_at_start: bool,
    txn_bytes_written: u32,
    txn_first_byte: Option<u8>,
    txn_read_any: bool,
    txn_read_status: bool,
    txn_read_gestures: bool,
    pending_cmd: u8,

    // Device state
    show_reset: bool,
    ati_error: bool,
    ati_until: Option<u64>,
    boot_until: Option<u64>,
    fingers: [bool; 3],
    press_latched: [bool; 3],
    down_since: Option<u64>,
    hold_fired: bool,
    gesture_latch: u8,
    slider_event: bool,
    /// An enabled event happened that no status/gesture read has reported yet.
    unreported_event: bool,

    // RDY
    window_open: bool,
    window_opened_at: u64,
    open_at: Option<u64>,
    stream_next: Option<u64>,
    soc_low_since: Option<u64>,
}

impl Default for Iqs323 {
    fn default() -> Self {
        Self::new()
    }
}

impl Iqs323 {
    pub fn new() -> Self {
        Self::with_config(Iqs323Config::default())
    }

    /// A device that has just powered up (SHOW_RESET set, streaming mode).
    pub fn with_config(cfg: Iqs323Config) -> Self {
        let mut d = Iqs323 {
            cfg,
            stats: Iqs323Stats::default(),
            regs: default_regs(),
            ptr: 0,
            hi: false,
            expect_ptr: false,
            in_txn: false,
            window_at_start: false,
            txn_bytes_written: 0,
            txn_first_byte: None,
            txn_read_any: false,
            txn_read_status: false,
            txn_read_gestures: false,
            pending_cmd: 0,
            show_reset: true,
            ati_error: false,
            ati_until: None,
            boot_until: None,
            fingers: [false; 3],
            press_latched: [false; 3],
            down_since: None,
            hold_fired: false,
            gesture_latch: 0,
            slider_event: false,
            unreported_event: false,
            window_open: false,
            window_opened_at: 0,
            open_at: None,
            stream_next: None,
            soc_low_since: None,
        };
        d.reset(0);
        d.stats.resets = 0;
        d
    }

    // ---------------------------------------------------------------- board API

    /// RDY output: the device pulls the line low (communication window open).
    /// The board ANDs this with the SoC's own drive of GPIO3.
    pub fn rdy_low(&mut self, now: u64) -> bool {
        self.advance(now);
        self.window_open
    }

    /// The SoC drives GPIO3 (RDY/MCLR) low (`true`) or releases it. A low pulse of
    /// at least `mclr_min_ns` resets the device when released (the firmware's
    /// `iqs323_hardware_reset()` holds it for 500 us).
    pub fn soc_drive_rdy(&mut self, now: u64, low: bool) {
        self.advance(now);
        match (low, self.soc_low_since) {
            (true, None) => self.soc_low_since = Some(now),
            (false, Some(t0)) => {
                self.soc_low_since = None;
                if now - t0 >= self.cfg.mclr_min_ns {
                    self.mclr(now);
                }
            }
            _ => {}
        }
    }

    /// Hardware reset via MCLR.
    pub fn mclr(&mut self, now: u64) {
        self.advance(now);
        self.reset(now);
    }

    /// Finger down / up on a channel: 0 = left (CH0), 1 = middle (CH1), 2 = right (CH2).
    pub fn touch(&mut self, now: u64, zone: usize, down: bool) {
        self.advance(now);
        let z = zone.min(2);
        if self.fingers[z] == down {
            return;
        }
        let was_any = self.any_down();
        self.fingers[z] = down;
        if down {
            if !was_any {
                self.down_since = Some(now);
                self.hold_fired = false;
            }
        } else if !self.any_down() {
            if let Some(t0) = self.down_since.take()
                && !self.hold_fired
                && now - t0 <= self.reg_ms(MM_MAX_TAP_TIME, 350)
            {
                self.gesture(now, Gesture::Tap);
            }
            self.hold_fired = false;
        }
        let latched = self.raise_event(now, EventKind::Touch);
        if down && latched && self.cfg.latch_touch_until_read {
            self.press_latched[z] = true;
        }
    }

    /// Inject a slider gesture (ignored unless enabled in GESTURE_ENABLE).
    /// Returns whether it was accepted.
    pub fn gesture(&mut self, now: u64, g: Gesture) -> bool {
        self.advance(now);
        let (bit, enable) = g.bits();
        if self.regs[MM_GESTURE_ENABLE as usize] as u8 & enable == 0 {
            return false;
        }
        self.gesture_latch |= bit;
        self.slider_event = true;
        self.raise_event(now, EventKind::Gesture);
        true
    }

    /// Convenience: swipe towards CH2 (`positive`, "next") or CH0.
    pub fn swipe(&mut self, now: u64, positive: bool) -> bool {
        self.gesture(now, if positive { Gesture::SwipePos } else { Gesture::SwipeNeg })
    }

    pub fn finger_down(&self, zone: usize) -> bool {
        self.fingers[zone.min(2)]
    }

    pub fn window_open(&self) -> bool {
        self.window_open
    }

    pub fn event_mode(&self) -> bool {
        self.regs[MM_SYSTEM_CONTROL as usize] as u8 & SC_EVENT_MODE != 0
    }

    pub fn show_reset(&self) -> bool {
        self.show_reset
    }

    /// Raw 16-bit register value as a read would return it.
    pub fn register(&self, now: u64, addr: u8) -> u16 {
        self.word(now, addr)
    }

    // ---------------------------------------------------------------- internals

    fn any_down(&self) -> bool {
        self.fingers.iter().any(|&f| f)
    }

    fn reg_ms(&self, addr: u8, default_ms: u64) -> u64 {
        match self.regs[addr as usize] {
            0 => default_ms * MS,
            v => v as u64 * MS,
        }
    }

    fn window_timeout(&self) -> u64 {
        self.cfg.window_timeout_ns.unwrap_or_else(|| self.reg_ms(MM_I2C_TIMEOUT, 200))
    }

    fn power_mode(&self) -> u8 {
        match self.regs[MM_SYSTEM_CONTROL as usize] >> 4 & 7 {
            m @ 0..=3 => m as u8,
            _ => {
                if self.any_down() {
                    0
                } else {
                    1
                }
            }
        }
    }

    fn report_period(&self) -> u64 {
        let addr = MM_NP_REPORT_RATE + self.power_mode();
        (self.regs[addr as usize].max(1) as u64) * MS
    }

    fn hold_deadline(&self) -> Option<u64> {
        let enabled = self.regs[MM_GESTURE_ENABLE as usize] as u8 & GE_HOLD != 0;
        match self.down_since {
            Some(t0) if enabled && !self.hold_fired => Some(t0 + self.reg_ms(MM_MIN_HOLD_TIME, 600)),
            _ => None,
        }
    }

    fn reset(&mut self, now: u64) {
        self.regs = default_regs();
        self.show_reset = true;
        self.ati_error = false;
        self.ati_until = None;
        self.window_open = false;
        self.open_at = None;
        self.stream_next = None;
        self.boot_until = Some(now + self.cfg.reset_boot_ns);
        self.gesture_latch = 0;
        self.slider_event = false;
        self.press_latched = [false; 3];
        self.unreported_event = false;
        self.hold_fired = self.any_down();
        self.pending_cmd = 0;
        self.ptr = 0;
        self.hi = false;
        self.stats.resets += 1;
    }

    /// Record an event; returns whether it is reportable (event mode with the
    /// event enabled, or streaming mode where every change is reported).
    fn raise_event(&mut self, now: u64, kind: EventKind) -> bool {
        if !self.event_mode() {
            return true;
        }
        let en = self.regs[MM_EVENTS_ENABLE as usize] as u8;
        let enabled = match kind {
            EventKind::Touch => en & (EV_TOUCH | EV_PROX) != 0,
            EventKind::Gesture => en & EV_GESTURE != 0,
        };
        if !enabled {
            return false;
        }
        self.unreported_event = true;
        if !self.window_open {
            self.schedule_window(now + self.cfg.event_latency_ns);
        }
        true
    }

    fn schedule_window(&mut self, t: u64) {
        let t = t.max(self.boot_until.unwrap_or(0));
        self.open_at = Some(self.open_at.map_or(t, |o| o.min(t)));
    }

    fn open_window(&mut self, t: u64) {
        self.window_open = true;
        self.window_opened_at = t;
        self.open_at = None;
        self.stream_next = None;
        self.stats.windows_opened += 1;
    }

    fn close_window(&mut self, t: u64) {
        self.window_open = false;
        if !self.event_mode() {
            self.stream_next = Some(t + self.report_period());
        } else if self.unreported_event {
            self.schedule_window(t + self.cfg.event_latency_ns);
        }
    }

    /// Candidate timed events in priority order (for equal times).
    fn timers(&self) -> [Option<u64>; 6] {
        [
            self.boot_until,
            self.ati_until,
            self.hold_deadline(),
            if self.window_open || self.boot_until.is_some() { None } else { self.open_at },
            if self.window_open && !self.in_txn { Some(self.window_opened_at + self.window_timeout()) } else { None },
            if self.event_mode() || self.window_open || self.boot_until.is_some() { None } else { self.stream_next },
        ]
    }

    fn advance(&mut self, now: u64) {
        loop {
            let Some((idx, t)) =
                self.timers().iter().enumerate().filter_map(|(i, t)| t.map(|t| (i, t))).min_by_key(|&(i, t)| (t, i))
            else {
                return;
            };
            if t > now {
                return;
            }
            match idx {
                0 => {
                    self.boot_until = None;
                    if !self.event_mode() {
                        self.stream_next = Some(t + self.report_period());
                    }
                }
                1 => self.ati_until = None,
                2 => {
                    self.hold_fired = true;
                    self.gesture(t, Gesture::Hold);
                }
                3 => self.open_window(t),
                4 => {
                    // Master never talked: drop the event and move on.
                    self.stats.windows_timed_out += 1;
                    self.unreported_event = false;
                    self.close_window(t);
                }
                _ => self.open_window(t),
            }
        }
    }

    fn touch_reported(&self, z: usize) -> bool {
        self.fingers[z] || self.press_latched[z]
    }

    fn word(&self, now: u64, addr: u8) -> u16 {
        match addr {
            MM_PROD_NUM => PRODUCT_NUMBER,
            MM_MAJOR_VERSION => VERSION_MAJOR,
            MM_MINOR_VERSION => VERSION_MINOR,
            0x03..=0x09 => 0,
            MM_SYSTEM_STATUS => {
                let mut lo = 0u8;
                if self.show_reset {
                    lo |= ST_SHOW_RESET;
                }
                if self.ati_error {
                    lo |= ST_ATI_ERROR;
                }
                if self.ati_until.is_some_and(|t| now < t) {
                    lo |= ST_ATI_ACTIVE;
                }
                if self.slider_event {
                    lo |= ST_SLIDER_EVENT;
                }
                let mut hi = self.power_mode() << 6;
                for z in 0..3 {
                    if self.touch_reported(z) {
                        hi |= 0b11 << (2 * z); // prox + touch
                    }
                }
                u16::from_le_bytes([lo, hi])
            }
            MM_GESTURES => {
                let mut g = self.gesture_latch;
                if self.hold_fired && self.any_down() {
                    g |= G_HOLD;
                }
                if g != 0 {
                    g |= G_EVENT;
                }
                g as u16
            }
            MM_SLIDER_COORDINATES => {
                let pos: Vec<u32> =
                    (0..3).filter(|&z| self.fingers[z]).map(|z| self.cfg.slider_positions[z] as u32).collect();
                if pos.is_empty() { 0xFFFF } else { (pos.iter().sum::<u32>() / pos.len() as u32) as u16 }
            }
            0x13..=0x18 => {
                let ch = ((addr - 0x13) / 2) as usize;
                let lta = 1000 + 10 * ch as u16;
                if (addr - 0x13) % 2 == 1 {
                    lta
                } else if self.touch_reported(ch) {
                    lta + 80
                } else {
                    lta
                }
            }
            0x19..=0x2F => 0,
            0xFE | 0xFF => 0xEEEE,
            a => self.regs[a as usize],
        }
    }

    fn step_ptr(&mut self) {
        if self.hi {
            self.ptr = self.ptr.wrapping_add(1);
        }
        self.hi = !self.hi;
    }

    fn write_data(&mut self, byte: u8) {
        let a = self.ptr as usize;
        if self.ptr >= 0x30 && self.ptr < 0xFE {
            let mut v = byte;
            if self.ptr == MM_SYSTEM_CONTROL && !self.hi {
                self.pending_cmd |= byte & (SC_ACK_RESET | SC_SW_RESET | SC_RE_ATI | SC_RESEED);
                v &= !(SC_ACK_RESET | SC_SW_RESET | SC_RE_ATI | SC_RESEED);
            }
            let [lo, hi] = self.regs[a].to_le_bytes();
            self.regs[a] = if self.hi { u16::from_le_bytes([lo, v]) } else { u16::from_le_bytes([v, hi]) };
        }
        self.step_ptr();
    }
}

impl I2cDevice for Iqs323 {
    fn address(&self) -> u8 {
        ADDR
    }

    fn start(&mut self, now: u64, read: bool) -> bool {
        self.advance(now);
        if !self.in_txn {
            self.in_txn = true;
            self.window_at_start = self.window_open;
            self.txn_bytes_written = 0;
            self.txn_first_byte = None;
            self.txn_read_any = false;
            self.txn_read_status = false;
            self.txn_read_gestures = false;
            if !self.window_open {
                self.stats.out_of_window_txns += 1;
            }
        }
        self.expect_ptr = !read;
        true
    }

    fn write(&mut self, now: u64, byte: u8) -> bool {
        self.advance(now);
        self.txn_bytes_written += 1;
        self.txn_first_byte.get_or_insert(byte);
        if self.expect_ptr {
            self.expect_ptr = false;
            self.ptr = byte;
            self.hi = false;
        } else {
            self.write_data(byte);
        }
        true
    }

    fn read(&mut self, now: u64, _ack: bool) -> u8 {
        self.advance(now);
        self.txn_read_any = true;
        match self.ptr {
            MM_SYSTEM_STATUS => self.txn_read_status = true,
            MM_GESTURES => self.txn_read_gestures = true,
            _ => {}
        }
        let b = self.word(now, self.ptr).to_le_bytes()[self.hi as usize];
        self.step_ptr();
        b
    }

    fn stop(&mut self, now: u64) {
        self.advance(now);
        if !self.in_txn {
            return;
        }
        self.in_txn = false;
        self.expect_ptr = false;

        // 1. Reported data: clear latches.
        let mut release_seen = false;
        if self.txn_read_status {
            for z in 0..3 {
                if std::mem::take(&mut self.press_latched[z]) && !self.fingers[z] {
                    release_seen = true;
                }
            }
        }
        if self.txn_read_gestures {
            self.gesture_latch = 0;
            self.slider_event = false;
        }
        if self.txn_read_status || self.txn_read_gestures {
            self.unreported_event = false;
        }
        // 2. The STOP ends the window this transaction ran in.
        if self.window_at_start && self.window_open {
            self.close_window(now);
        }
        // 3. A latched press whose finger already lifted: now report the release.
        if release_seen {
            self.raise_event(now, EventKind::Touch);
        }
        // 4. SYSTEM_CONTROL commands.
        let cmd = std::mem::take(&mut self.pending_cmd);
        if cmd & SC_SW_RESET != 0 {
            self.reset(now);
            return;
        }
        if cmd & SC_ACK_RESET != 0 {
            self.show_reset = false;
        }
        if cmd & SC_RE_ATI != 0 {
            self.ati_until = Some(now + self.cfg.ati_duration_ns);
            self.ati_error = false;
        }
        // 5. Force-communication request.
        if !self.window_at_start
            && self.txn_bytes_written == 1
            && self.txn_first_byte == Some(0xFF)
            && !self.txn_read_any
        {
            self.stats.force_comm_requests += 1;
            if !self.window_open {
                self.schedule_window(now + self.cfg.force_comm_latency_ns);
            }
        }
    }

    fn update(&mut self, now: u64) {
        self.advance(now);
    }

    fn next_event_ns(&self, now: u64) -> Option<u64> {
        self.timers().into_iter().flatten().min().map(|t| t.max(now))
    }
}

// ------------------------------------------------------------ reset defaults

/// lib/IQS323/IQS323_config_LP.h, laid out as `IQS323::writeMM()` writes it.
pub mod lp_config {
    pub const S0: [u8; 20] = [
        0x01, 0x04, 0x7F, 0x0C, 0x90, 0x13, 0xCF, 0x04, 0x0A, 0x03, 0x00, 0x00, 0x74, 0x17, 0x64, 0x00, 0x44, 0x64,
        0xD9, 0x53,
    ];
    pub const S1: [u8; 20] = [
        0x01, 0x01, 0x7F, 0x0C, 0x90, 0x13, 0xCF, 0x01, 0x0A, 0x03, 0x00, 0x00, 0xC4, 0x12, 0x64, 0x00, 0x47, 0x5E,
        0xD6, 0x53,
    ];
    pub const S2: [u8; 20] = [
        0x01, 0x02, 0x7F, 0x0C, 0x90, 0x13, 0xCF, 0x02, 0x0A, 0x03, 0x00, 0x00, 0x74, 0x17, 0x64, 0x00, 0x44, 0x5C,
        0xA2, 0x4B,
    ];
    pub const CH0: [u8; 8] = [0x00, 0x00, 0x14, 0x44, 0x1B, 0x00, 0xC8, 0x00];
    pub const CH1: [u8; 8] = [0x00, 0x00, 0x14, 0x44, 0x1A, 0x00, 0xC8, 0x00];
    pub const CH2: [u8; 8] = [0x00, 0x00, 0x14, 0x44, 0x1A, 0x00, 0xC8, 0x00];
    pub const SLIDER: [u8; 18] =
        [0x0B, 0x00, 0x00, 0x14, 0xC8, 0x00, 0x00, 0x08, 0x07, 0x00, 0x52, 0x05, 0x30, 0x04, 0x72, 0x04, 0xB4, 0x04];
    pub const GESTURE: [u8; 14] = [0x0B, 0x00, 0x32, 0x00, 0x5E, 0x01, 0xF4, 0x01, 0x58, 0x02, 0x90, 0x01, 0xF4, 0x01];
    pub const FILTER: [u8; 10] = [0x02, 0x01, 0x0C, 0x0C, 0x00, 0x00, 0x02, 0x01, 0x14, 0x00];
    pub const GENERAL: [u8; 10] = [0x00, 0x00, 0xC8, 0x00, 0x75, 0x75, 0x04, 0x18, 0x08, 0x0A];
    pub const SYSTEM: [u8; 12] = [0x10, 0x00, 0x10, 0x00, 0x3C, 0x00, 0x64, 0x00, 0xB8, 0x0B, 0xD0, 0x07];
    pub const I2C_SETUP: [u8; 1] = [0x00];

    /// (start address, bytes) in `writeMM()` order.
    pub const BLOCKS: [(u8, &[u8]); 12] = [
        (0x30, &S0),
        (0x40, &S1),
        (0x50, &S2),
        (0x60, &CH0),
        (0x70, &CH1),
        (0x80, &CH2),
        (0x90, &SLIDER),
        (0xA0, &GESTURE),
        (0xB0, &FILTER),
        (0xD0, &GENERAL),
        (0xC0, &SYSTEM),
        (0xE0, &I2C_SETUP),
    ];
}

fn default_regs() -> [u16; 256] {
    let mut r = [0u16; 256];
    for (addr, bytes) in lp_config::BLOCKS {
        for (i, &b) in bytes.iter().enumerate() {
            let a = addr as usize + i / 2;
            let [lo, hi] = r[a].to_le_bytes();
            r[a] = if i % 2 == 0 { u16::from_le_bytes([b, hi]) } else { u16::from_le_bytes([lo, b]) };
        }
    }
    r[MM_SYSTEM_CONTROL as usize] &= 0xFF00; // power-on: streaming, normal power
    r[0xE1] = 0xF003; // hardware ID
    r
}

#[cfg(test)]
mod tests {
    use super::super::testutil::BbMaster;
    use super::super::{BitBangI2cSlave, I2cBus};
    use super::*;

    /// Firmware harness: the Azoteq driver's Wire helpers plus the RDY ISR
    /// (`iqs323_ready_interrupt`, attached on CHANGE) sampled at every device event.
    struct Fw {
        bus: I2cBus,
        now: u64,
        rdy_prev: bool,
        /// `iqs323_deviceRDY`
        rdy_flag: bool,
        rdy_falls: u32,
    }

    impl Fw {
        fn new(dev: Iqs323) -> Self {
            let mut bus = I2cBus::new();
            bus.add(Box::new(dev));
            let mut fw = Fw { bus, now: 0, rdy_prev: false, rdy_flag: false, rdy_falls: 0 };
            fw.sample();
            fw
        }
        fn dev(&mut self) -> &mut Iqs323 {
            self.bus.device_mut::<Iqs323>().unwrap()
        }
        fn rdy_low(&mut self) -> bool {
            let now = self.now;
            self.dev().rdy_low(now)
        }
        fn sample(&mut self) {
            let low = self.rdy_low();
            if low != self.rdy_prev {
                self.rdy_flag = low; // ISR: level low -> true, high -> false
                if low {
                    self.rdy_falls += 1;
                }
                self.rdy_prev = low;
            }
        }
        /// Let virtual time pass, running the ISR at every RDY edge.
        fn advance_to(&mut self, t: u64) {
            while let Some(ne) = self.bus.next_event_ns(self.now).filter(|&ne| ne <= t) {
                self.now = ne.max(self.now);
                self.bus.update(self.now);
                self.sample();
                let again = self.bus.next_event_ns(self.now);
                assert!(again.is_none_or(|n| n > self.now), "device timer did not advance");
            }
            self.now = t.max(self.now);
            self.bus.update(self.now);
            self.sample();
        }
        fn delay_ms(&mut self, ms: u64) {
            let t = self.now + ms * MS;
            self.advance_to(t);
        }
        fn touch(&mut self, zone: usize, down: bool) {
            let now = self.now;
            self.dev().touch(now, zone, down);
            self.sample();
        }
        /// `force_I2C_communication()`
        fn force_comm(&mut self) {
            if !self.rdy_low() {
                let now = self.now;
                self.bus.write_txn(now, ADDR, &[0xFF]);
                self.rdy_flag = false;
                for _ in 0..100 {
                    if self.rdy_low() {
                        break;
                    }
                    self.delay_ms(1);
                }
            }
            self.sample();
        }
        /// `readRandomBytes()`: force comm, bail if RDY high, write reg + repeated START read + STOP.
        fn read(&mut self, reg: u8, n: usize) -> Option<Vec<u8>> {
            self.force_comm();
            if !self.rdy_low() {
                return None;
            }
            let now = self.now;
            let r = self.bus.write_read(now, ADDR, &[reg], n);
            self.rdy_flag = false;
            self.sample();
            r
        }
        /// `writeRandomBytes()`
        fn write(&mut self, reg: u8, data: &[u8]) -> bool {
            self.force_comm();
            if !self.rdy_low() {
                return false;
            }
            let mut v = vec![reg];
            v.extend_from_slice(data);
            let now = self.now;
            let ok = self.bus.write_txn(now, ADDR, &v);
            self.rdy_flag = false;
            self.sample();
            ok
        }
        /// read 0xC0, set a bit, write it back (acknowledgeReset / ReATI / SW_Reset / setEventMode)
        fn set_sysctl_bit(&mut self, bit: u8) {
            let b = self.read(MM_SYSTEM_CONTROL, 2).unwrap();
            assert!(self.write(MM_SYSTEM_CONTROL, &[b[0] | bit, b[1]]));
        }
        fn status(&mut self) -> [u8; 2] {
            let b = self.read(MM_SYSTEM_STATUS, 2).unwrap();
            [b[0], b[1]]
        }
        /// `check_i2c_lockup()`: no force comm; true = locked up.
        fn check_i2c_lockup(&mut self) -> bool {
            let now = self.now;
            self.bus.write_read(now, ADDR, &[MM_LOCKUP_CHECK], 1) != Some(vec![0xEE])
        }
        /// `setGestureConfig(tap_mode)`
        fn set_gesture_config(&mut self, tap: bool) {
            assert!(self.write(0xA0, &[if tap { 0x09 } else { 0x0B }]));
            assert!(self.write(0xD3, &[if tap { 0x02 } else { 0x04 }]));
        }

        /// iqs323_do_init(false): SW reset + the IQS323::init() state machine,
        /// run every 10 ms. Returns the number of run() iterations.
        fn full_init(&mut self) -> u32 {
            // SW_Reset(STOP): readRandomBytes(0xC0, STOP) then write bit1.
            self.set_sysctl_bit(SC_SW_RESET);
            self.delay_ms(100);
            let mut state = 0;
            let mut iterations = 0;
            loop {
                iterations += 1;
                assert!(iterations < 200, "init stuck in state {state}");
                match state {
                    0 => {
                        let p = self.read(MM_PROD_NUM, 2).unwrap();
                        assert_eq!(u16::from_le_bytes([p[0], p[1]]), PRODUCT_NUMBER);
                        let _maj = self.read(MM_MAJOR_VERSION, 2).unwrap();
                        let _min = self.read(MM_MINOR_VERSION, 2).unwrap();
                        state = 1;
                    }
                    1 => {
                        let s = self.status();
                        state = if s[0] & ST_SHOW_RESET != 0 { 3 } else { 2 };
                    }
                    2 => {
                        self.set_sysctl_bit(SC_SW_RESET);
                        self.delay_ms(100);
                        state = 1;
                    }
                    3 => {
                        for (addr, bytes) in lp_config::BLOCKS {
                            assert!(self.write(addr, bytes), "writeMM {addr:#x}");
                        }
                        state = 4;
                    }
                    4 => {
                        self.set_sysctl_bit(SC_ACK_RESET);
                        state = 5;
                    }
                    5 => {
                        self.set_sysctl_bit(SC_RE_ATI);
                        state = 6;
                    }
                    6 => {
                        if self.status()[0] & ST_ATI_ACTIVE == 0 {
                            state = 7;
                        }
                    }
                    7 => {
                        let d = self.read(MM_SYSTEM_STATUS, 18).unwrap();
                        assert_eq!(d.len(), 18);
                        state = 8;
                    }
                    8 => {
                        self.set_sysctl_bit(SC_EVENT_MODE);
                        self.delay_ms(10);
                        let v = self.read(MM_SYSTEM_CONTROL, 2).unwrap();
                        assert_ne!(v[0] & SC_EVENT_MODE, 0, "event mode verify");
                        return iterations;
                    }
                    _ => unreachable!(),
                }
                self.delay_ms(10);
            }
        }

        /// One pass of the iqs323 task's RUN loop: read on a RDY window.
        fn task_run(&mut self) -> Option<Vec<u8>> {
            if !self.rdy_flag {
                return None;
            }
            let d = self.read(MM_SYSTEM_STATUS, 18);
            assert!(!self.check_i2c_lockup());
            d
        }
    }

    fn initialized_fw() -> Fw {
        let mut fw = Fw::new(Iqs323::new());
        fw.delay_ms(50); // boot, streaming windows come and go
        fw.full_init();
        fw
    }

    #[test]
    fn power_on_state_and_version_registers() {
        let mut fw = Fw::new(Iqs323::new());
        fw.delay_ms(20);
        assert!(fw.dev().show_reset());
        assert!(!fw.dev().event_mode());
        let p = fw.read(0x00, 6).unwrap();
        assert_eq!(p, vec![0x52, 0x04, 1, 0, 1, 0]);
        assert!(!fw.check_i2c_lockup(), "0xFE reads 0xEE");
        let hw = fw.read(0xE1, 2).unwrap();
        assert_eq!(hw, vec![0x03, 0xF0]);
    }

    #[test]
    fn streaming_mode_opens_periodic_windows() {
        let mut fw = Fw::new(Iqs323::new());
        fw.delay_ms(200);
        let falls = fw.rdy_falls;
        assert!(falls >= 1, "RDY toggles in streaming mode");
        // Nobody reads: each window times out after 0xD1 = 200 ms.
        fw.delay_ms(1000);
        assert!(fw.dev().stats.windows_timed_out >= 3);
    }

    #[test]
    fn full_init_sequence() {
        let mut fw = Fw::new(Iqs323::new());
        fw.delay_ms(50);
        let iters = fw.full_init();
        assert!(iters > 8, "ATI polling took a few iterations");
        let d = fw.dev();
        assert!(!d.show_reset());
        assert!(d.event_mode());
        assert!(!d.window_open());
        assert_eq!(d.stats.resets, 1, "one SW reset");
        // Config readback (writeMM stored everything, odd-length 0xE0 write too).
        for (addr, bytes) in lp_config::BLOCKS {
            let r = fw.read(addr, bytes.len()).unwrap();
            if addr == MM_SYSTEM_CONTROL {
                assert_eq!(r[0], 0x10 | SC_EVENT_MODE);
                assert_eq!(&r[1..], &bytes[1..]);
            } else {
                assert_eq!(&r[..], bytes, "block {addr:#x}");
            }
        }
        // Idle in event mode: no windows by themselves.
        let falls = fw.rdy_falls;
        fw.delay_ms(2000);
        assert_eq!(fw.rdy_falls, falls);
        // setGestureConfig: single-byte writes change only the low byte.
        fw.set_gesture_config(true);
        assert_eq!(fw.read(0xA0, 2).unwrap(), vec![0x09, 0x00]);
        assert_eq!(fw.read(0xD3, 2).unwrap(), vec![0x02, 0x18]);
        fw.set_gesture_config(false);
        assert_eq!(fw.read(0xA0, 2).unwrap(), vec![0x0B, 0x00]);
        assert_eq!(fw.read(0xD3, 2).unwrap(), vec![0x04, 0x18]);
    }

    #[test]
    fn force_comm_window_protocol() {
        let mut fw = initialized_fw();
        assert!(!fw.rdy_low());
        let t0 = fw.now;
        fw.bus.write_txn(t0, ADDR, &[0xFF]);
        assert!(!fw.rdy_low(), "window opens after a short delay");
        fw.advance_to(t0 + 999 * US);
        assert!(!fw.rdy_low());
        fw.advance_to(t0 + MS);
        assert!(fw.rdy_low());
        // The STOP ending the transaction closes the window.
        let now = fw.now;
        assert!(fw.bus.write_read(now, ADDR, &[MM_SYSTEM_STATUS], 2).is_some());
        assert!(!fw.rdy_low());
        // An out-of-window transaction (lockup check) neither opens nor closes windows.
        let requests = fw.dev().stats.force_comm_requests;
        assert!(!fw.check_i2c_lockup());
        fw.delay_ms(5);
        assert!(!fw.rdy_low());
        assert_eq!(fw.dev().stats.force_comm_requests, requests);
        // A window nobody uses times out (I2C timeout 0xD1 = 200 ms).
        let t1 = fw.now;
        fw.bus.write_txn(t1, ADDR, &[0xFF]);
        fw.advance_to(t1 + 150 * MS);
        assert!(fw.rdy_low());
        fw.advance_to(t1 + 202 * MS);
        assert!(!fw.rdy_low());
    }

    #[test]
    fn tap_in_tap_mode_awake() {
        let mut fw = initialized_fw();
        fw.set_gesture_config(true);
        fw.delay_ms(100);
        assert!(fw.task_run().is_none());
        fw.touch(1, true);
        fw.delay_ms(10);
        assert!(fw.rdy_low() && fw.rdy_flag, "touch event opens a window");
        let d = fw.task_run().unwrap();
        assert_eq!(d[1] & 0b0011_1111, 0b0000_1100, "CH1 prox+touch");
        assert_eq!(u16::from_le_bytes([d[4], d[5]]), 1024);
        assert!(!fw.rdy_low());
        // Finger still down: no further events in tap mode (touch events only).
        fw.delay_ms(100);
        assert!(fw.task_run().is_none());
        fw.touch(1, false);
        fw.delay_ms(10);
        let d = fw.task_run().unwrap();
        assert_eq!(d[1] & 0b0011_1111, 0);
        assert_eq!(u16::from_le_bytes([d[4], d[5]]), 0xFFFF);
        assert_eq!(d[2] & G_TAP, G_TAP, "TAP gesture latched (released within 350 ms)");
        assert_eq!(d[0] & ST_SLIDER_EVENT, ST_SLIDER_EVENT);
        // Gesture latch cleared by that read.
        let s = fw.read(MM_SYSTEM_STATUS, 4).unwrap();
        assert_eq!((s[0] & ST_SLIDER_EVENT, s[2]), (0, 0));
    }

    #[test]
    fn left_right_hold_slide_and_tap_mode() {
        // Slide mode: gesture events only -> the HOLD gesture opens the window.
        let mut fw = initialized_fw();
        fw.set_gesture_config(false);
        fw.touch(0, true);
        fw.touch(2, true);
        fw.delay_ms(300);
        assert!(!fw.rdy_low(), "touch changes don't raise events in slide mode");
        fw.delay_ms(400);
        assert!(fw.rdy_flag, "HOLD after 600 ms");
        let d = fw.task_run().unwrap();
        assert_eq!(d[1] & 0b0011_1111, 0b0011_0011, "CH0 + CH2 touched");
        assert_eq!(d[0] & ST_SLIDER_EVENT, ST_SLIDER_EVENT);
        assert_eq!(d[2] & G_HOLD, G_HOLD);
        // HOLD stays set while held; lifting clears it without a TAP.
        let g = fw.read(MM_GESTURES, 2).unwrap();
        assert_eq!(g[0] & G_HOLD, G_HOLD);
        fw.touch(0, false);
        fw.touch(2, false);
        let g = fw.read(MM_GESTURES, 2).unwrap();
        assert_eq!(g[0], 0);

        // Tap mode: tap_mode_is_hold() polls; the final forced read still shows both.
        let mut fw = initialized_fw();
        fw.set_gesture_config(true);
        fw.touch(0, true);
        fw.touch(2, true);
        fw.delay_ms(10);
        let d = fw.task_run().unwrap();
        assert_eq!(d[1] & 0b0011_0011, 0b0011_0011);
        for _ in 0..30 {
            fw.delay_ms(20);
            if fw.rdy_flag {
                fw.read(MM_SYSTEM_STATUS, 2);
            }
        }
        let s = fw.status();
        assert_eq!(s[1] & 0b0010_0010, 0b0010_0010, "both still touched after 600 ms");
    }

    #[test]
    fn event_mode_touch_while_asleep_wakes_and_stub_reads_snapshot() {
        let mut fw = initialized_fw();
        // goToSleep(): prepare_sleep (read, lockup check, event mode) + setGestureConfig(tap).
        let _ = fw.read(MM_SYSTEM_STATUS, 18);
        assert!(!fw.check_i2c_lockup());
        fw.set_sysctl_bit(SC_EVENT_MODE);
        fw.set_gesture_config(true);
        // Deep sleep: nobody on the bus for a while; RDY must stay high.
        fw.delay_ms(5000);
        assert!(!fw.rdy_low());
        // Finger down -> RDY low (EXT0 wake source) and it stays low.
        fw.touch(2, true);
        let t_touch = fw.now;
        fw.delay_ms(6);
        assert!(fw.rdy_low());
        fw.delay_ms(40); // SoC boots into the wake stub
        assert!(fw.rdy_low(), "window held until the stub reads");
        // Wake stub: bit-banged read of 18 bytes from 0x10 on GPIO39/40.
        let mut slave = BitBangI2cSlave::new();
        let buf = {
            let mut m = BbMaster::new(&mut slave, &mut fw.bus, fw.now);
            let b = m.read_reg(ADDR, MM_SYSTEM_STATUS, 18).unwrap();
            fw.now = m.now;
            b
        };
        assert_eq!(buf[0] & ST_SHOW_RESET, 0);
        assert_eq!(buf[1] & 0b0011_0000, 0b0011_0000, "CH2 touch in the snapshot");
        assert!(!fw.rdy_low(), "the stub's STOP closed the window");
        assert!(fw.now - t_touch < 200 * MS);
        // Firmware boots: wake-stub path does check_i2c_lockup(), later live reads.
        assert!(!fw.check_i2c_lockup());
        fw.delay_ms(80);
        fw.touch(2, false);
        fw.delay_ms(10);
        assert!(fw.rdy_flag, "release event");
        let s = fw.status();
        assert_eq!(s[1] & 0b0011_0000, 0, "live read shows the release");
    }

    #[test]
    fn short_click_is_latched_until_reported() {
        let mut fw = initialized_fw();
        fw.set_gesture_config(true);
        // Press and release within 1 ms, before the event window even opens.
        fw.touch(1, true);
        fw.delay_ms(1);
        fw.touch(1, false);
        fw.delay_ms(10);
        let mut slave = BitBangI2cSlave::new();
        let buf = {
            let mut m = BbMaster::new(&mut slave, &mut fw.bus, fw.now);
            let b = m.read_reg(ADDR, MM_SYSTEM_STATUS, 18).unwrap();
            fw.now = m.now;
            b
        };
        assert_eq!(buf[1] & 0b1100, 0b1100, "latched press visible to the stub");
        // The release is reported next: another event window opens.
        fw.delay_ms(10);
        assert!(fw.rdy_flag);
        let s = fw.status();
        assert_eq!(s[1] & 0b1100, 0);

        // Slide mode (gesture events only): no latch; the tap arrives as a gesture
        // with the touch bit already clear (matches hardware).
        let mut fw = initialized_fw();
        fw.set_gesture_config(false);
        fw.touch(0, true);
        fw.delay_ms(100);
        assert!(!fw.rdy_low());
        fw.touch(0, false);
        fw.delay_ms(10);
        let d = fw.task_run().unwrap();
        assert_eq!(d[1] & 0b11, 0);
        assert_eq!(d[2] & G_TAP, G_TAP);
    }

    #[test]
    fn swipe_gesture_in_slide_mode() {
        let mut fw = initialized_fw();
        fw.set_gesture_config(true);
        let now = fw.now;
        assert!(!fw.dev().swipe(now, true), "swipe disabled in tap mode (0x09)");
        fw.set_gesture_config(false);
        let now = fw.now;
        assert!(fw.dev().swipe(now, false));
        fw.delay_ms(10);
        let d = fw.task_run().unwrap();
        assert_eq!(d[0] & ST_SLIDER_EVENT, ST_SLIDER_EVENT);
        assert_eq!(d[2] & (G_SWIPE_NEG | G_EVENT), G_SWIPE_NEG | G_EVENT);
    }

    #[test]
    fn mclr_hardware_reset() {
        let mut fw = initialized_fw();
        // iqs323_hardware_reset(): wait for RDY high, drive low 500 us, release.
        assert!(!fw.rdy_low());
        let now = fw.now;
        fw.dev().soc_drive_rdy(now, true);
        fw.advance_to(now + 500 * US);
        let now = fw.now;
        fw.dev().soc_drive_rdy(now, false);
        assert_eq!(fw.dev().stats.resets, 2);
        assert!(fw.dev().show_reset());
        assert!(!fw.dev().event_mode());
        // Probe (address-only write) ACKs; wake-stub-path check sees SHOW_RESET.
        fw.delay_ms(50);
        let now = fw.now;
        assert!(fw.bus.write_txn(now, ADDR, &[]));
        assert_ne!(fw.status()[0] & ST_SHOW_RESET, 0);
        // And a full re-init works.
        fw.full_init();
        assert!(!fw.dev().show_reset());
    }
}
