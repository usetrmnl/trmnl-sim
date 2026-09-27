//! Solomon Systech SSD16xx e-paper controllers (bb_epaper's `BBEP_CHIP_SSD16xx`) as the
//! TRMNL firmware drives them through bb_epaper: the SSD1677 of the 4.26" and 3.97"
//! 800x480 panels and the SSD1683 of the 4.2" 400x300 panel (CrowPanel).
//!
//! Modelled: the RAM address windows and counters (0x44/0x45/0x4E/0x4F) with the data
//! entry mode (0x11, including the X- or Y-decrementing layouts of the 800x480 panels),
//! the two RAM planes (0x24 black/white, 0x26 "red" = previous image), display update
//! control (0x21 RAM options, 0x22 sequence) and master activation (0x20) with the
//! built-in (OTP) full / fast / differential ("partial") waveforms and custom LUTs (0x32,
//! bb_epaper's 4-gray modes), soft and hardware reset, deep sleep (0x10), the temperature
//! register (0x18/0x1A) that selects OTP waveforms, and BUSY (driven high while busy).
//!
//! A refresh plays its waveform out frame by frame on a simple particle model (as in the
//! UC8179 model), so full refreshes flash, partial ones don't, and 4-gray LUTs land on
//! their grays. The whole panel is refreshed every time (SSD16xx have no partial window;
//! differential mode only drives the pixels whose two RAM planes differ).

use std::sync::Arc;

use parking_lot::Mutex;
use sim_api::{Frame, SharedFrame};

use crate::savepoint::{StateReader, StateWriter, read_f32s_into};

const MS: u64 = 1_000_000;

/// The controller family member: how the LUT register and the RAM X address look.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Chip {
    /// SSD1677: 10-bit X addresses in pixels (a data byte covers 8 of them), 105-byte LUT
    /// (5x10 voltage bytes, 10 groups of TP A-D + RP, frame rate).
    Ssd1677,
    /// SSD1683: X addresses in bytes, UC8179-style LUT (VCOM + 4 rows of 6 groups of RP,
    /// 4 phases of 2-bit level + 6-bit frames, 2 SR; then frame rate, XON, EOPT).
    Ssd1683,
}

/// A panel (glass + controller) and how it is mounted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Glass {
    /// 4.26" 800x480 (bb_epaper EP426_800x480): RAM X runs right to left.
    Ep426,
    /// 3.97" 800x480 (EP397_800x480, GDEM0397T81P): RAM Y runs bottom to top.
    Ep397,
    /// The 3.97" panel mounted the other way round (Seeed Sticky: its bb_epaper 2.1.11
    /// writes the RAM like the 4.26" panel, rotated 180 degrees from bb_epaper 2.1.9's
    /// layout for the Waveshare board; we assume the board mounts the panel to match).
    Ep397Flipped,
    /// 4.2" 400x300 (EP42B_400x300, GDEY042T81): RAM in screen order.
    Ep42b,
}

impl Glass {
    pub fn chip(self) -> Chip {
        match self {
            Glass::Ep42b => Chip::Ssd1683,
            _ => Chip::Ssd1677,
        }
    }

    pub fn size(self) -> (usize, usize) {
        match self {
            Glass::Ep42b => (400, 300),
            _ => (800, 480),
        }
    }

    /// RAM to screen: (mirror X, mirror Y).
    fn mirror(self) -> (bool, bool) {
        match self {
            Glass::Ep426 | Glass::Ep397Flipped => (true, false),
            Glass::Ep397 => (false, true),
            Glass::Ep42b => (false, false),
        }
    }

    /// Particle response per frame of drive towards black / white (see [`optical`]):
    /// fitted so bb_epaper's 4-gray LUTs for each panel land on 0/85/170/255.
    fn k(self) -> (f32, f32) {
        match self {
            Glass::Ep397 | Glass::Ep397Flipped => (0.355, 0.355),
            Glass::Ep426 | Glass::Ep42b => (0.215, 0.200),
        }
    }

    /// The temperature register value whose OTP waveform is the 4-gray one (bb_epaper
    /// 2.1.9 selects it with 0x1A 0x5A on the 3.97" panel).
    fn otp_gray_temp(self) -> Option<u8> {
        match self {
            Glass::Ep397 | Glass::Ep397Flipped => Some(0x5a),
            _ => None,
        }
    }
}

/// How dark a pixel looks for a particle state (0 = white .. 1 = black): a mild S-curve,
/// as partly-driven particles scatter less light than their position suggests.
fn optical(d: f32) -> f32 {
    const S: f32 = 0.9;
    (d + S * d * (1.0 - d) * (2.0 * d - 1.0)).clamp(0.0, 1.0)
}

/// Drive levels: SSD16xx LUT voltage codes VSS / VSH1 / VSL / VSH2.
const IDLE: u8 = 0;
const BLACK: u8 = 1;
const WHITE: u8 = 2;

fn level_of(code: u8) -> u8 {
    match code & 3 {
        0 => IDLE,
        2 => WHITE,
        _ => BLACK, // VSH1 and VSH2
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Phase {
    level: u8,
    frames: u32,
}

/// The waveform loaded into the LUT register.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lut {
    /// Nothing loaded since reset: a display update drives nothing.
    None,
    /// OTP full refresh (display mode 1 at room temperature): flashes, ~3 s.
    Full,
    /// OTP fast refresh (mode 1 with a high forced temperature): one flash, ~1.5 s.
    Fast,
    /// OTP differential refresh (display mode 2): pixels whose RAM planes differ go to
    /// the new color without flashing, ~0.4 s.
    Partial,
    /// OTP 4-gray waveform (3.97" panel at the forced temperature 0x5A).
    Gray,
    /// Written with 0x32.
    Custom,
}

impl Lut {
    fn code(self) -> u8 {
        self as u8
    }

    fn from_code(c: u8) -> anyhow::Result<Lut> {
        Ok(match c {
            0 => Lut::None,
            1 => Lut::Full,
            2 => Lut::Fast,
            3 => Lut::Partial,
            4 => Lut::Gray,
            5 => Lut::Custom,
            _ => anyhow::bail!("save point display LUT is corrupt"),
        })
    }
}

/// How a pixel's two RAM bits (after the 0x21 options) pick its schedule.
#[derive(Clone, Copy)]
enum Select {
    /// Schedule 0 = to white (BW bit 1), 1 = to black.
    Target,
    /// 0 = unchanged (RED == BW: not driven), 1 = to white, 2 = to black.
    Diff,
    /// SSD1677 LUT0..3 = RED<<1 | BW.
    Lut1677,
    /// SSD1683 rows WW, WB, BW, BB of (BW, RED) with W = 1.
    Lut1683,
}

/// A refresh in progress: a per-pixel drive schedule played out over time.
struct Refresh {
    start: u64,
    frame_ns: u64,
    schedules: Vec<Vec<Phase>>,
    total_frames: u32,
    /// Per screen pixel: index into `schedules`.
    sel: Vec<u8>,
    frames_done: u32,
}

/// The registers a (hardware or soft) reset restores.
#[derive(Clone)]
struct Regs {
    /// 0x11: bit 0 X increments, bit 1 Y increments, bit 2 Y first.
    entry: u8,
    /// 0x44/0x45 window (start, end), in address units (pixels or bytes for X).
    x_win: (i32, i32),
    y_win: (i32, i32),
    /// Address counters (0x4E/0x4F).
    x: i32,
    y: i32,
    /// 0x21: RED and BW RAM options (high / low nibble of the first byte).
    upd1: [u8; 2],
    /// 0x22: the sequence 0x20 runs.
    upd2: u8,
    temp_sensor: u8,
    /// Temperature register, integer degrees C (selects the OTP waveform).
    temp: u8,
    lut: Lut,
    /// The 0x32 data.
    custom: Vec<u8>,
}

impl Regs {
    fn reset(w: usize, h: usize, chip: Chip) -> Self {
        let x_end = match chip {
            Chip::Ssd1677 => w as i32 - 1,
            Chip::Ssd1683 => w as i32 / 8 - 1,
        };
        Regs {
            entry: 0x03,
            x_win: (0, x_end),
            y_win: (0, h as i32 - 1),
            x: 0,
            y: 0,
            upd1: [0, 0],
            upd2: 0xff,
            temp_sensor: 0x48,
            temp: 0x7f,
            lut: Lut::None,
            custom: Vec::new(),
        }
    }
}

pub struct Ssd16xx {
    pub glass: Glass,
    chip: Chip,
    w: usize,
    h: usize,
    // Serial interface
    cs: bool,
    dc: bool,
    sck: bool,
    rst: bool,
    shift: u8,
    nbits: u8,
    cmd: u8,
    args: Vec<u8>,

    regs: Regs,
    /// RAM planes in RAM coordinates, 1 bit per pixel (MSB = lowest X address).
    bw: Vec<u8>,
    red: Vec<u8>,
    /// 0 awake, else the deep sleep mode (1 keeps RAM, 2 loses it).
    sleep: u8,
    /// The panel's supply is on (boards that switch it, see `set_power`).
    rail: bool,
    busy_until: u64,
    refresh: Option<Refresh>,
    /// What the internal temperature sensor reads.
    pub temperature_c: i8,
    /// The waveform of the last display update (for tests and diagnostics).
    pub last_lut: Lut,

    /// Physical particle state per screen pixel, 0.0 = white, 1.0 = black.
    state: Vec<f32>,
    pub frame: SharedFrame,
    pub refresh_count: u64,
    /// Fault: BUSY held high forever.
    pub busy_stuck: bool,
}

impl Ssd16xx {
    pub fn new(glass: Glass) -> Self {
        let (w, h) = glass.size();
        let chip = glass.chip();
        Ssd16xx {
            glass,
            chip,
            w,
            h,
            cs: true,
            dc: true,
            sck: false,
            rst: true,
            shift: 0,
            nbits: 0,
            cmd: 0,
            args: Vec::new(),
            regs: Regs::reset(w, h, chip),
            bw: vec![0; w / 8 * h],
            red: vec![0; w / 8 * h],
            sleep: 0,
            rail: true,
            busy_until: 0,
            refresh: None,
            temperature_c: 22,
            last_lut: Lut::None,
            state: vec![0.0; w * h],
            frame: Arc::new(Mutex::new(Frame::new(w, h))),
            refresh_count: 0,
            busy_stuck: false,
        }
    }

    pub fn busy(&self, now: u64) -> bool {
        self.rail && (self.busy_stuck || now < self.busy_until)
    }

    pub fn next_event(&self, now: u64) -> Option<u64> {
        if let Some(r) = &self.refresh {
            return Some(r.start + (r.frames_done as u64 + 1) * r.frame_ns);
        }
        (self.busy_until > now).then_some(self.busy_until)
    }

    /// The panel supply switched: without it the controller forgets everything (the
    /// image on the glass stays, it's e-paper).
    pub fn set_power(&mut self, now: u64, on: bool) {
        if on == self.rail {
            return;
        }
        self.update(now);
        self.rail = on;
        self.hw_reset();
        self.bw.fill(0);
        self.red.fill(0);
        self.refresh = None;
        self.busy_until = if on { now + MS } else { 0 };
    }

    fn hw_reset(&mut self) {
        self.regs = Regs::reset(self.w, self.h, self.chip);
        self.sleep = 0;
        self.nbits = 0;
        self.cmd = 0;
        self.args.clear();
    }

    // ---- serial interface ------------------------------------------------------------------

    pub fn set_pins(&mut self, now: u64, cs: bool, dc: bool, sck: bool, mosi: bool, rst: bool) {
        if !self.rail {
            return;
        }
        if !rst && self.rst {
            // A reset stops a refresh in progress where it is.
            self.update(now);
            self.refresh = None;
            self.hw_reset();
            self.busy_until = 0;
        }
        if rst && !self.rst {
            self.busy_until = now + MS; // the controller initialises itself
        }
        self.rst = rst;
        if !rst {
            (self.cs, self.dc, self.sck) = (cs, dc, sck);
            return;
        }
        if cs && !self.cs {
            self.nbits = 0; // CS released: byte framing restarts
        }
        let rising = sck && !self.sck;
        (self.cs, self.dc, self.sck) = (cs, dc, sck);
        if !cs && rising {
            self.shift = self.shift << 1 | mosi as u8;
            self.nbits += 1;
            if self.nbits == 8 {
                self.nbits = 0;
                let b = self.shift;
                self.byte(now, b);
            }
        }
    }

    pub fn spi_bytes(&mut self, now: u64, data: &[u8]) {
        if self.cs || !self.rst || !self.rail || self.sleep != 0 {
            return;
        }
        if self.dc && matches!(self.cmd, 0x24 | 0x26) {
            for &b in data {
                self.ram_write(b);
            }
            return;
        }
        for &b in data {
            self.byte(now, b);
        }
    }

    fn byte(&mut self, now: u64, b: u8) {
        if self.sleep != 0 {
            return;
        }
        if !self.dc {
            self.command(now, b);
        } else if matches!(self.cmd, 0x24 | 0x26) {
            self.ram_write(b);
        } else {
            self.args.push(b);
            self.apply_arg();
        }
    }

    // ---- commands ------------------------------------------------------------------------

    fn command(&mut self, now: u64, c: u8) {
        log::trace!("ssd16xx: cmd {c:#04x} (previous {:#04x} took {} data bytes)", self.cmd, self.args.len());
        self.cmd = c;
        self.args.clear();
        match c {
            0x12 => {
                // Soft reset: registers to defaults, RAM kept; BUSY while it runs.
                self.regs = Regs::reset(self.w, self.h, self.chip);
                self.busy_until = self.busy_until.max(now) + 3 * MS;
            }
            0x20 => self.activate(now),
            0x32 => {
                self.regs.custom.clear();
                self.regs.lut = Lut::Custom;
            }
            _ => {}
        }
    }

    /// A data byte for the current command (`args` holds all bytes so far).
    fn apply_arg(&mut self) {
        let n = self.args.len();
        let b = self.args[n - 1];
        let r = &mut self.regs;
        let lo = |v: &mut i32| *v = (*v & !0xff) | b as i32;
        let hi = |v: &mut i32| *v = (*v & 0xff) | ((b & 3) as i32) << 8;
        match (self.cmd, self.chip, n) {
            (0x10, _, 1) => {
                self.sleep = match b & 3 {
                    0 => 0,
                    1 => 1,
                    _ => 2,
                };
                if self.sleep == 2 {
                    self.bw.fill(0);
                    self.red.fill(0);
                }
            }
            (0x11, _, 1) => r.entry = b & 7,
            (0x18, _, 1) => r.temp_sensor = b,
            (0x1a, _, 1) => r.temp = b,
            (0x21, _, 1..=2) => r.upd1[n - 1] = b,
            (0x22, _, 1) => r.upd2 = b,
            (0x32, _, _) => r.custom.push(b),
            (0x44, Chip::Ssd1677, 1) => lo(&mut r.x_win.0),
            (0x44, Chip::Ssd1677, 2) => hi(&mut r.x_win.0),
            (0x44, Chip::Ssd1677, 3) => lo(&mut r.x_win.1),
            (0x44, Chip::Ssd1677, 4) => hi(&mut r.x_win.1),
            (0x44, Chip::Ssd1683, 1) => r.x_win.0 = (b & 0x3f) as i32,
            (0x44, Chip::Ssd1683, 2) => r.x_win.1 = (b & 0x3f) as i32,
            (0x45, _, 1) => lo(&mut r.y_win.0),
            (0x45, _, 2) => hi(&mut r.y_win.0),
            (0x45, _, 3) => lo(&mut r.y_win.1),
            (0x45, _, 4) => hi(&mut r.y_win.1),
            (0x4e, Chip::Ssd1677, 1) => lo(&mut r.x),
            (0x4e, Chip::Ssd1677, 2) => hi(&mut r.x),
            (0x4e, Chip::Ssd1683, 1) => r.x = (b & 0x3f) as i32,
            (0x4f, _, 1) => lo(&mut r.y),
            (0x4f, _, 2) => hi(&mut r.y),
            _ => {}
        }
    }

    /// One byte into the RAM plane of the current command at the address counter, which
    /// then advances as the data entry mode says, wrapping inside the window.
    fn ram_write(&mut self, b: u8) {
        let r = &self.regs;
        let x_inc = r.entry & 1 != 0;
        let row_bytes = self.w / 8;
        let plane = if self.cmd == 0x24 { &mut self.bw } else { &mut self.red };
        let y = r.y;
        if (0..self.h as i32).contains(&y) {
            for i in 0..8 {
                // SSD1677: 8 pixel addresses from the counter on, in counting direction;
                // SSD1683: the 8 pixels of the byte address.
                let x = match self.chip {
                    Chip::Ssd1677 if x_inc => r.x + i,
                    Chip::Ssd1677 => r.x - i,
                    Chip::Ssd1683 => r.x * 8 + i,
                };
                if (0..self.w as i32).contains(&x) {
                    let idx = y as usize * row_bytes + x as usize / 8;
                    let mask = 0x80u8 >> (x as usize % 8);
                    if b & (0x80 >> i) != 0 {
                        plane[idx] |= mask;
                    } else {
                        plane[idx] &= !mask;
                    }
                }
            }
        }
        self.advance();
    }

    fn advance(&mut self) {
        let x_step = match self.chip {
            Chip::Ssd1677 => 8,
            Chip::Ssd1683 => 1,
        };
        let r = &mut self.regs;
        let (x_inc, y_inc, y_first) = (r.entry & 1 != 0, r.entry & 2 != 0, r.entry & 4 != 0);
        // Step a counter; true if it ran past the window end (it then restarts).
        fn step(v: &mut i32, inc: bool, by: i32, (start, end): (i32, i32)) -> bool {
            let n = if inc { *v + by } else { *v - by };
            let past = if inc { n > end } else { n < end };
            *v = if past { start } else { n };
            past
        }
        if y_first {
            if step(&mut r.y, y_inc, 1, r.y_win) {
                step(&mut r.x, x_inc, x_step, r.x_win);
            }
        } else if step(&mut r.x, x_inc, x_step, r.x_win) {
            step(&mut r.y, y_inc, 1, r.y_win);
        }
    }

    /// Master activation: run the 0x22 sequence. Bits: 7 clock on, 6 analog on, 5 load
    /// temperature, 4 load LUT, 3 display mode 2, 2 display, 1 analog off, 0 clock off.
    fn activate(&mut self, now: u64) {
        self.update(now);
        let seq = self.regs.upd2;
        let bit = |n: u32| seq >> n & 1 != 0;
        let mut t = self.busy_until.max(now);
        if bit(7) {
            t += MS;
        }
        if bit(6) {
            t += 15 * MS; // charge pumps
        }
        if bit(5) {
            t += 5 * MS;
            self.regs.temp = self.temperature_c.max(0) as u8; // internal or external sensor alike
        }
        if bit(4) {
            t += 10 * MS;
            let temp = self.regs.temp;
            self.regs.lut = if bit(3) {
                Lut::Partial
            } else if self.glass.otp_gray_temp() == Some(temp) {
                Lut::Gray
            } else if (0x50..0x80).contains(&temp) {
                Lut::Fast
            } else {
                Lut::Full
            };
        }
        if bit(2) {
            t = self.start_refresh(t);
            if bit(3) {
                // Display mode 2 keeps the RED RAM as "the image on screen" for the next
                // differential update: the new image replaces it.
                self.red.copy_from_slice(&self.bw);
            }
        }
        if bit(1) {
            t += 5 * MS;
        }
        if bit(0) {
            t += MS;
        }
        self.busy_until = t;
        log::debug!("ssd16xx: activate {seq:#04x} lut={:?} busy {} ms", self.regs.lut, (t - now) / MS);
    }

    // ---- refresh simulation ----------------------------------------------------------------

    /// The pixel's (RED, BW) RAM bits at a screen position, after the 0x21 options.
    fn ram_bits(&self, sx: usize, sy: usize) -> (bool, bool) {
        let (mx, my) = self.glass.mirror();
        let rx = if mx { self.w - 1 - sx } else { sx };
        let ry = if my { self.h - 1 - sy } else { sy };
        let idx = ry * (self.w / 8) + rx / 8;
        let mask = 0x80 >> (rx % 8);
        let opt = |bit: bool, o: u8| match o & 0x0c {
            0x04 => false, // bypass as 0
            0x08 => !bit,  // inverse
            _ => bit,
        };
        let o = self.regs.upd1[0];
        (opt(self.red[idx] & mask != 0, o >> 4), opt(self.bw[idx] & mask != 0, o))
    }

    /// The loaded waveform as per-pixel-class schedules, how pixels pick one, and the
    /// frame time.
    fn waveform(&self) -> (Vec<Vec<Phase>>, Select, u64) {
        let ph = |level, frames| Phase { level, frames };
        match self.regs.lut {
            Lut::None => (Vec::new(), Select::Target, 20 * MS),
            Lut::Full => {
                let n = 30;
                let white = vec![ph(BLACK, n), ph(WHITE, n), ph(BLACK, n), ph(WHITE, 2 * n)];
                let black = vec![ph(WHITE, n), ph(BLACK, n), ph(WHITE, n), ph(BLACK, 2 * n)];
                (vec![white, black], Select::Target, 20 * MS)
            }
            Lut::Fast => {
                let n = 25;
                (
                    vec![vec![ph(BLACK, n), ph(WHITE, 2 * n)], vec![ph(WHITE, n), ph(BLACK, 2 * n)]],
                    Select::Target,
                    20 * MS,
                )
            }
            Lut::Partial => (vec![vec![ph(IDLE, 20)], vec![ph(WHITE, 20)], vec![ph(BLACK, 20)]], Select::Diff, 20 * MS),
            Lut::Gray => {
                // The phases of bb_epaper 2.1.11's custom 4-gray LUT for this panel.
                let rows = [[0x9a, 0x00], [0x99, 0x90], [0x96, 0x60], [0x94, 0x00]];
                let tp = [[10, 10, 7, 2, 0], [6, 1, 4, 0, 0]];
                let s = rows.iter().map(|r| Self::expand_1677(r, &tp)).collect();
                (s, Select::Lut1677, 20 * MS)
            }
            Lut::Custom => self.custom_waveform(),
        }
    }

    /// SSD1677 LUT rows: one byte of four 2-bit phase levels per group, each group's
    /// phases lasting TP A..D frames, repeated RP+1 times.
    fn expand_1677(row: &[u8], tp: &[[u8; 5]]) -> Vec<Phase> {
        let mut v = Vec::new();
        for (g, t) in tp.iter().enumerate() {
            let byte = row.get(g).copied().unwrap_or(0);
            for _ in 0..=t[4] {
                for p in 0..4 {
                    if t[p] > 0 {
                        v.push(Phase { level: level_of(byte >> (6 - 2 * p)), frames: t[p] as u32 });
                    }
                }
            }
        }
        v
    }

    fn custom_waveform(&self) -> (Vec<Vec<Phase>>, Select, u64) {
        let d = &self.regs.custom;
        let at = |i: usize| d.get(i).copied().unwrap_or(0);
        match self.chip {
            Chip::Ssd1677 => {
                // 5 rows x 10 groups of levels (LUT0-3, VCOM), 10 groups x (TP A-D, RP),
                // then the frame rate (low nibble; bb_epaper's 0x22 gives ~27 Hz).
                let tp: Vec<[u8; 5]> = (0..10).map(|g| std::array::from_fn(|i| at(50 + g * 5 + i))).collect();
                let rows =
                    (0..4).map(|r| Self::expand_1677(&d[(r * 10).min(d.len())..(r * 10 + 10).min(d.len())], &tp));
                let hz = match d.get(100).map(|f| f & 0x0f) {
                    Some(0..=2) => 27,
                    _ => 50,
                };
                (rows.collect(), Select::Lut1677, 1_000_000_000 / hz)
            }
            Chip::Ssd1683 => {
                // VCOM + 4 rows of 6 groups: RP, 4 phases (level << 6 | frames), SR AB, SR CD;
                // then 2 unused groups, the frame rate, XON, EOPT.
                let rows = (1..5).map(|r| {
                    let mut v = Vec::new();
                    for g in 0..6 {
                        let base = r * 42 + g * 7;
                        let phase = |p: usize| Phase {
                            level: level_of(at(base + 1 + p) >> 6),
                            frames: (at(base + 1 + p) & 0x3f) as u32,
                        };
                        for _ in 0..at(base).max(1) {
                            for (pair, sr) in [(0, at(base + 5)), (2, at(base + 6))] {
                                for _ in 0..sr.max(1) {
                                    v.extend([phase(pair), phase(pair + 1)].into_iter().filter(|p| p.frames > 0));
                                }
                            }
                        }
                    }
                    v
                });
                let hz = match at(5 * 42 + 14) {
                    0x02 => 100,
                    _ => 50,
                };
                (rows.collect(), Select::Lut1683, 1_000_000_000 / hz)
            }
        }
    }

    /// Start driving the panel at `start`; returns when the waveform ends.
    fn start_refresh(&mut self, start: u64) -> u64 {
        let (schedules, select, frame_ns) = self.waveform();
        let total_frames = schedules.iter().map(|s| s.iter().map(|p| p.frames).sum::<u32>()).max().unwrap_or(0);
        self.refresh_count += 1;
        self.last_lut = self.regs.lut;
        if total_frames == 0 {
            log::warn!("ssd16xx: display update with no waveform loaded ({:?})", self.regs.lut);
            return start;
        }
        let mut sel = Vec::with_capacity(self.w * self.h);
        for sy in 0..self.h {
            for sx in 0..self.w {
                let (r, b) = self.ram_bits(sx, sy);
                sel.push(match select {
                    Select::Target => !b as u8,
                    Select::Diff if r == b => 0,
                    Select::Diff => 1 + !b as u8,
                    Select::Lut1677 => (r as u8) << 1 | b as u8,
                    Select::Lut1683 => 3 - ((b as u8) << 1 | r as u8),
                });
            }
        }
        self.refresh = Some(Refresh { start, frame_ns, schedules, total_frames, sel, frames_done: 0 });
        log::debug!("ssd16xx: refresh {:?}, {} frames", self.regs.lut, total_frames);
        start + total_frames as u64 * frame_ns
    }

    /// What a schedule does to a pixel's darkness over frames [from, to), as an affine map
    /// `d -> a * d + b` (the per-frame particle response is affine, so frames compose).
    fn response(&self, s: &[Phase], from: u32, to: u32) -> (f32, f32) {
        let (kb, kw) = self.glass.k();
        let (mut a, mut b) = (1.0f32, 0.0f32);
        let mut t = 0u32;
        for ph in s {
            let n = ((t + ph.frames).min(to) as i32 - t.max(from) as i32).max(0);
            // One frame towards black: d += kb * (1 - d); towards white: d -= kw * d.
            let (fa, fb) = match ph.level {
                BLACK => (1.0 - kb, kb),
                WHITE => (1.0 - kw, 0.0),
                _ => (1.0, 0.0),
            };
            for _ in 0..n {
                (a, b) = (fa * a, fa * b + fb);
            }
            t += ph.frames;
            if t >= to {
                break;
            }
        }
        (a, b)
    }

    /// Advance the refresh animation to `now`.
    pub fn update(&mut self, now: u64) {
        let Some(r) = &self.refresh else { return };
        let target = ((now.saturating_sub(r.start) / r.frame_ns) as u32).min(r.total_frames);
        if target > r.frames_done {
            let resp: Vec<(f32, f32)> = r.schedules.iter().map(|s| self.response(s, r.frames_done, target)).collect();
            let mut frame = self.frame.lock();
            for (i, &sel) in r.sel.iter().enumerate() {
                let (a, b) = resp[sel as usize];
                let d = a * self.state[i] + b;
                self.state[i] = d;
                frame.pixels[i] = (optical(d) * 255.0).round() as u8;
            }
            frame.generation += 1;
        }
        let r = self.refresh.as_mut().unwrap();
        r.frames_done = target;
        if target >= r.total_frames {
            self.refresh = None;
        }
    }

    // ---- save points ---------------------------------------------------------------------

    /// The image on the glass, and with `powered` the controller's registers and RAM. A
    /// refresh in progress is not saved (the caller waits for BUSY to end).
    pub fn save_state(&self, w: &mut StateWriter, powered: bool) {
        w.f32s(&self.state);
        w.bytes(&self.frame.lock().pixels);
        w.u64(self.refresh_count);
        if !powered {
            return;
        }
        for v in [self.cs, self.dc, self.sck, self.rst, self.rail] {
            w.bool(v);
        }
        w.u8(self.shift);
        w.u8(self.nbits);
        w.u8(self.cmd);
        w.bytes(&self.args);
        let r = &self.regs;
        w.u8(r.entry);
        w.u32s(&[r.x_win.0, r.x_win.1, r.y_win.0, r.y_win.1, r.x, r.y].map(|v| v as u32));
        w.bytes(&r.upd1);
        w.u8(r.upd2);
        w.u8(r.temp_sensor);
        w.u8(r.temp);
        w.u8(r.lut.code());
        w.bytes(&r.custom);
        w.bytes(&self.bw);
        w.bytes(&self.red);
        w.u8(self.sleep);
        w.u64(self.busy_until);
        w.u8(self.temperature_c as u8);
    }

    /// Load `save_state` output. Without `powered` the controller comes back as after
    /// power-on, showing the saved image.
    pub fn restore_state(&mut self, r: &mut StateReader, powered: bool) -> anyhow::Result<()> {
        let mut fresh = Self::new(self.glass);
        fresh.frame = self.frame.clone();
        fresh.temperature_c = self.temperature_c;
        *self = fresh;
        read_f32s_into(r, &mut self.state)?;
        {
            let mut frame = self.frame.lock();
            r.fill_u8(&mut frame.pixels)?;
            frame.generation += 1;
        }
        self.refresh_count = r.u64()?;
        if !powered {
            return Ok(());
        }
        for v in [&mut self.cs, &mut self.dc, &mut self.sck, &mut self.rst, &mut self.rail] {
            *v = r.bool()?;
        }
        self.shift = r.u8()?;
        self.nbits = r.u8()?;
        self.cmd = r.u8()?;
        self.args = r.bytes()?.to_vec();
        let g = &mut self.regs;
        g.entry = r.u8()?;
        let v = r.u32s()?;
        let [xs, xe, ys, ye, x, y] = v[..] else { anyhow::bail!("save point display registers are corrupt") };
        (g.x_win, g.y_win, g.x, g.y) = ((xs as i32, xe as i32), (ys as i32, ye as i32), x as i32, y as i32);
        g.upd1 = r.array()?;
        g.upd2 = r.u8()?;
        g.temp_sensor = r.u8()?;
        g.temp = r.u8()?;
        g.lut = Lut::from_code(r.u8()?)?;
        g.custom = r.bytes()?.to_vec();
        r.fill_u8(&mut self.bw)?;
        r.fill_u8(&mut self.red)?;
        self.sleep = r.u8()?;
        self.busy_until = r.u64()?;
        self.temperature_c = r.u8()? as i8;
        Ok(())
    }
}

impl crate::devices::epd::SpiEpd for Ssd16xx {
    fn set_pins(&mut self, now: u64, cs: bool, dc: bool, sck: bool, mosi: bool, rst: bool) {
        Ssd16xx::set_pins(self, now, cs, dc, sck, mosi, rst);
    }
    fn spi_bytes(&mut self, now: u64, data: &[u8]) {
        Ssd16xx::spi_bytes(self, now, data);
    }
    fn busy(&self, now: u64) -> bool {
        Ssd16xx::busy(self, now)
    }
    fn busy_level(&self) -> bool {
        true
    }
    fn set_power(&mut self, now: u64, on: bool) {
        Ssd16xx::set_power(self, now, on);
    }
    fn next_event(&self, now: u64) -> Option<u64> {
        Ssd16xx::next_event(self, now)
    }
    fn update(&mut self, now: u64) {
        Ssd16xx::update(self, now);
    }
    fn frame(&self) -> SharedFrame {
        self.frame.clone()
    }
    fn refresh_count(&self) -> u64 {
        self.refresh_count
    }
    fn set_busy_stuck(&mut self, stuck: bool) {
        self.busy_stuck = stuck;
    }
    fn save_state(&self, w: &mut StateWriter, powered: bool) {
        Ssd16xx::save_state(self, w, powered);
    }
    fn restore_state(&mut self, r: &mut StateReader, powered: bool) -> anyhow::Result<()> {
        Ssd16xx::restore_state(self, r, powered)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devices::epd::SpiEpd;

    const BUSY_WAIT: u8 = 0xff;

    // bb_epaper 2.1.9's init sequences (bb_ep.inl), as the firmware sends them.
    const EP426_FULL: &[u8] = &[
        1, 0x12, BUSY_WAIT, 2, 0x18, 0x80, 6, 0x0c, 0xae, 0xc7, 0xc3, 0xc0, 0x80, 4, 0x01, 0xdf, 0x01, 0x02, 2, 0x3c,
        0x01, 2, 0x11, 0x02, 5, 0x44, 0x1f, 0x03, 0x00, 0x00, 5, 0x45, 0x00, 0x00, 0xdf, 0x01, 3, 0x4e, 0x1f, 0x03, 3,
        0x4f, 0x00, 0x00, BUSY_WAIT, 0,
    ];
    const EP426_FAST: &[u8] = &[
        1, 0x12, BUSY_WAIT, 2, 0x18, 0x80, 6, 0x0c, 0xae, 0xc7, 0xc3, 0xc0, 0x80, 4, 0x01, 0xdf, 0x01, 0x02, 2, 0x3c,
        0x01, 2, 0x11, 0x02, 5, 0x44, 0x1f, 0x03, 0x00, 0x00, 5, 0x45, 0x00, 0x00, 0xdf, 0x01, 3, 0x4e, 0x1f, 0x03, 3,
        0x4f, 0x00, 0x00, BUSY_WAIT, 2, 0x1a, 0x5a, 2, 0x22, 0x91, 1, 0x20, BUSY_WAIT, 0,
    ];
    const EP426_PART: &[u8] = &[
        3, 0x21, 0x00, 0x00, 2, 0x3c, 0x80, 2, 0x11, 0x02, 5, 0x44, 0x1f, 0x03, 0x00, 0x00, 5, 0x45, 0x00, 0x00, 0xdf,
        0x01, 3, 0x4e, 0x1f, 0x03, 3, 0x4f, 0x00, 0x00, 0,
    ];
    const EP397_FULL: &[u8] = &[
        1, 0x12, BUSY_WAIT, 2, 0x18, 0x80, 6, 0x0c, 0xae, 0xc7, 0xc3, 0xc0, 0x80, 4, 0x01, 0xdf, 0x01, 0x02, 2, 0x3c,
        0x01, 2, 0x11, 0x01, 5, 0x44, 0x00, 0x00, 0x1f, 0x03, 5, 0x45, 0xdf, 0x01, 0x00, 0x00, 3, 0x4e, 0x00, 0x00, 3,
        0x4f, 0x00, 0x00, BUSY_WAIT, 0,
    ];
    const EP42B_FULL: &[u8] = &[
        1, 0x12, BUSY_WAIT, 4, 0x01, 0x2b, 0x01, 0x00, 3, 0x21, 0x40, 0x00, 2, 0x11, 0x03, 3, 0x44, 0x00, 0x31, 5,
        0x45, 0x00, 0x00, 0x2b, 0x01, 2, 0x3c, 0x05, 2, 0x18, 0x80, 2, 0x4e, 0x00, 3, 0x4f, 0x00, 0x00, BUSY_WAIT, 0,
    ];

    /// epd426g_init's custom LUT (4-gray).
    fn ep426_gray_lut() -> Vec<u8> {
        let mut v = vec![
            0x55, 0x55, 0xaa, 0xaa, 0xaa, 0xaa, 0xaa, 0xaa, 0x00, 0x00, //
            0x55, 0x55, 0xaa, 0xaa, 0xaa, 0xaa, 0xaa, 0x50, 0x00, 0x00, //
            0x55, 0x55, 0xaa, 0xaa, 0x55, 0x55, 0x55, 0xa0, 0x00, 0x00, //
            0x55, 0x55, 0xaa, 0xaa, 0x55, 0x55, 0x55, 0x50, 0x00, 0x00, //
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        ];
        for g in 0..10 {
            v.extend(if g < 8 { [1, 1, 1, 1, 0] } else { [0; 5] });
        }
        v.extend([0x22; 5]);
        v
    }

    /// epd42b_init_gray's custom LUT (SSD1683 4-gray).
    fn ep42b_gray_lut() -> Vec<u8> {
        let mut v = Vec::new();
        for g0 in [
            [0x01, 0x10, 0x10, 0x0a, 0x07, 0x01, 0x01],
            [0x01, 0x50, 0x90, 0x4a, 0x47, 0x01, 0x01],
            [0x01, 0x50, 0x90, 0x8f, 0x42, 0x01, 0x01],
            [0x01, 0x50, 0x90, 0x4f, 0x82, 0x01, 0x01],
            [0x01, 0x50, 0x90, 0x8a, 0x87, 0x01, 0x01],
        ] {
            v.extend(g0);
            v.extend([0u8; 35]);
        }
        v.extend([0u8; 14]);
        v.extend([0x02, 0x00, 0x00, 0x22]);
        v
    }

    /// bb_epaper 2.1.11's epd397g_init_full custom LUT (91-byte command: truncated).
    fn ep397_gray_lut() -> Vec<u8> {
        let mut v = vec![0u8; 90];
        for (r, row) in [[0x9a, 0x00], [0x99, 0x90], [0x96, 0x60], [0x94, 0x00]].iter().enumerate() {
            v[r * 10..r * 10 + 2].copy_from_slice(row);
        }
        v[50..55].copy_from_slice(&[0x0a, 0x0a, 0x07, 0x02, 0x00]);
        v[55..60].copy_from_slice(&[0x06, 0x01, 0x04, 0x00, 0x00]);
        v
    }

    fn cmd(p: &mut Ssd16xx, now: u64, c: u8, data: &[u8]) {
        p.set_pins(now, false, false, false, false, true);
        p.spi_bytes(now, &[c]);
        p.set_pins(now, false, true, false, false, true);
        p.spi_bytes(now, data);
        p.set_pins(now, true, true, false, false, true);
    }

    /// Wait (in 1 ms steps, like bbepWaitBusy's polling) until BUSY drops.
    fn wait(p: &mut Ssd16xx, mut now: u64) -> u64 {
        while p.busy(now) {
            now += MS;
            p.update(now);
            assert!(now < 60_000 * MS, "BUSY stuck");
        }
        now
    }

    /// bbepSendCMDSequence.
    fn send_seq(p: &mut Ssd16xx, mut now: u64, seq: &[u8]) -> u64 {
        let mut i = 0;
        while seq[i] != 0 {
            let n = seq[i] as usize;
            i += 1;
            if n == BUSY_WAIT as usize {
                now = wait(p, now + 11 * MS);
            } else {
                cmd(p, now, seq[i], &seq[i + 1..i + n]);
                i += n;
            }
        }
        now
    }

    /// bbepWriteImage at orientation 0: a screen-order 1-bpp image (1 = white), row by row.
    fn write_plane(p: &mut Ssd16xx, now: u64, c: u8, img: &[u8]) {
        let pitch = p.w / 8;
        cmd(p, now, c, &[]);
        p.set_pins(now, false, true, false, false, true);
        for row in img.chunks(pitch) {
            p.spi_bytes(now, row);
        }
        p.set_pins(now, true, true, false, false, true);
    }

    /// A screen-order image: black where `ink(x, y)`.
    fn image(p: &Ssd16xx, ink: impl Fn(usize, usize) -> bool) -> Vec<u8> {
        let mut v = vec![0xffu8; p.w / 8 * p.h];
        for y in 0..p.h {
            for x in 0..p.w {
                if ink(x, y) {
                    v[y * p.w / 8 + x / 8] &= !(0x80 >> (x % 8));
                }
            }
        }
        v
    }

    fn inverted(img: &[u8]) -> Vec<u8> {
        img.iter().map(|b| !b).collect()
    }

    fn refresh(p: &mut Ssd16xx, now: u64, seq: u8) -> u64 {
        cmd(p, now, 0x22, &[seq]);
        cmd(p, now, 0x20, &[]);
        wait(p, now + 11 * MS)
    }

    fn px(p: &Ssd16xx, x: usize, y: usize) -> u8 {
        p.frame.lock().pixels[y * p.w + x]
    }

    /// Black in the top-left corner, a black bar along the right edge.
    fn corner_ink(x: usize, y: usize) -> bool {
        (x < 100 && y < 50) || x >= 780
    }

    fn assert_corner_image(p: &Ssd16xx) {
        assert!(px(p, 10, 10) > 250, "top left {}", px(p, 10, 10));
        assert!(px(p, 790, 400) > 250, "right edge {}", px(p, 790, 400));
        assert!(px(p, 700, 10) < 5, "top right {}", px(p, 700, 10));
        assert!(px(p, 10, 400) < 5, "bottom left {}", px(p, 10, 400));
        assert!(px(p, 400, 240) < 5, "middle {}", px(p, 400, 240));
    }

    #[test]
    fn ep426_full_refresh_shows_the_image_upright() {
        let mut p = Ssd16xx::new(Glass::Ep426);
        let img = image(&p, corner_ink);
        let now = send_seq(&mut p, 0, EP426_FULL);
        write_plane(&mut p, now, 0x24, &img);
        write_plane(&mut p, now, 0x26, &inverted(&img));
        let t0 = now + MS;
        let done = refresh(&mut p, t0, 0xf7);
        assert_corner_image(&p);
        assert_eq!(p.last_lut, Lut::Full);
        let ms = (done - t0) / MS;
        assert!((2500..4000).contains(&ms), "full refresh took {ms} ms");
        assert_eq!(p.refresh_count, 1);
    }

    #[test]
    fn ep397_layouts_show_the_image_upright() {
        // bb_epaper 2.1.9 (Waveshare): X increments, Y counts down from 479.
        let mut p = Ssd16xx::new(Glass::Ep397);
        let img = image(&p, corner_ink);
        let now = send_seq(&mut p, 0, EP397_FULL);
        write_plane(&mut p, now, 0x24, &img);
        refresh(&mut p, now, 0xf7);
        assert_corner_image(&p);

        // bb_epaper 2.1.11 (Sticky): addressed like the 4.26" panel, on a flipped mount.
        let mut p = Ssd16xx::new(Glass::Ep397Flipped);
        let now = send_seq(&mut p, 0, EP426_FULL);
        write_plane(&mut p, now, 0x24, &img);
        refresh(&mut p, now, 0xf7);
        assert_corner_image(&p);
    }

    #[test]
    fn ep42b_writes_through_the_address_window() {
        let mut p = Ssd16xx::new(Glass::Ep42b);
        let img = image(&p, |x, y| x < 40 && y < 20);
        let now = send_seq(&mut p, 0, EP42B_FULL);
        // bbepSetAddrWindow(0, 0, 400, 300), the bytes version.
        cmd(&mut p, now, 0x44, &[0, 49]);
        cmd(&mut p, now, 0x4e, &[0]);
        cmd(&mut p, now, 0x45, &[0, 0, 0x2b, 0x01]);
        cmd(&mut p, now, 0x4f, &[0, 0]);
        write_plane(&mut p, now, 0x24, &img);
        // A 16x8 window at (80, 100): only that area changes.
        cmd(&mut p, now, 0x44, &[10, 11]);
        cmd(&mut p, now, 0x4e, &[10]);
        cmd(&mut p, now, 0x45, &[100, 0, 107, 0]);
        cmd(&mut p, now, 0x4f, &[100, 0]);
        cmd(&mut p, now, 0x24, &[0x00; 16]);
        refresh(&mut p, now, 0xf7);
        assert!(px(&p, 5, 5) > 250 && px(&p, 50, 5) < 5);
        assert!(px(&p, 80, 100) > 250 && px(&p, 95, 107) > 250);
        assert!(px(&p, 79, 100) < 5 && px(&p, 96, 100) < 5 && px(&p, 80, 108) < 5);
    }

    #[test]
    fn y_first_entry_mode_fills_columns() {
        let mut p = Ssd16xx::new(Glass::Ep42b);
        cmd(&mut p, 0, 0x11, &[0x07]); // Y first, both incrementing
        cmd(&mut p, 0, 0x44, &[0, 1]);
        cmd(&mut p, 0, 0x45, &[0, 0, 2, 0]);
        cmd(&mut p, 0, 0x4e, &[0]);
        cmd(&mut p, 0, 0x4f, &[0, 0]);
        cmd(&mut p, 0, 0x24, &[0xa1, 0xa2, 0xa3, 0xb1, 0xb2, 0xb3, 0xc0]);
        let row = p.w / 8;
        assert_eq!([p.bw[row], p.bw[2 * row]], [0xa2, 0xa3]);
        assert_eq!([p.bw[1], p.bw[row + 1], p.bw[2 * row + 1]], [0xb1, 0xb2, 0xb3]);
        assert_eq!(p.bw[0], 0xc0, "the 7th byte wrapped back to the window start");
        assert_eq!((p.regs.x, p.regs.y), (0, 1));
    }

    #[test]
    fn full_refresh_flashes_but_partial_does_not() {
        let mut p = Ssd16xx::new(Glass::Ep426);
        let black = image(&p, |_, _| true);
        let now = send_seq(&mut p, 0, EP426_FULL);
        write_plane(&mut p, now, 0x24, &black);
        let now = refresh(&mut p, now, 0xf7);
        // A full refresh to the same image flashes the black pixels white on the way.
        let (mut lightest, mut t) = (255u8, now);
        cmd(&mut p, t, 0x22, &[0xf7]);
        cmd(&mut p, t, 0x20, &[]);
        while p.busy(t) {
            t += 20 * MS;
            p.update(t);
            lightest = lightest.min(px(&p, 400, 240));
        }
        assert!(lightest < 30, "full refresh never flashed ({lightest})");
        // Partial (bb_epaper's PLANE_FALSE_DIFF: the old plane holds the inverse): the
        // left half turns white without the right half ever flashing.
        let img = image(&p, |x, _| x >= 400);
        let t = send_seq(&mut p, t, EP426_PART);
        write_plane(&mut p, t, 0x24, &img);
        write_plane(&mut p, t, 0x26, &inverted(&img));
        let t0 = t;
        let mut t = t;
        cmd(&mut p, t, 0x22, &[0xff]);
        cmd(&mut p, t, 0x20, &[]);
        while p.busy(t) {
            t += 5 * MS;
            p.update(t);
            assert!(px(&p, 600, 100) > 245, "right half flashed at {} ms", (t - t0) / MS);
        }
        assert_eq!(p.last_lut, Lut::Partial);
        assert!(px(&p, 100, 100) < 8);
        let ms = (t - t0) / MS;
        assert!((300..700).contains(&ms), "partial refresh took {ms} ms");
    }

    #[test]
    fn partial_refresh_leaves_unchanged_pixels_alone() {
        let mut p = Ssd16xx::new(Glass::Ep426);
        let now = send_seq(&mut p, 0, EP426_FULL);
        let white = image(&p, |_, _| false);
        write_plane(&mut p, now, 0x24, &white);
        let now = refresh(&mut p, now, 0xf7);
        p.state[0] = 0.5; // a pixel that is half gray on the glass
        let old = image(&p, |x, _| x < 100);
        let new = image(&p, |x, _| x < 200);
        let now = send_seq(&mut p, now, EP426_PART);
        write_plane(&mut p, now, 0x26, &old);
        write_plane(&mut p, now, 0x24, &new);
        refresh(&mut p, now, 0xff);
        let (mx, _) = Glass::Ep426.mirror();
        assert!(mx);
        assert!(px(&p, 150, 10) > 245, "changed pixel");
        assert!(px(&p, 50, 10) < 5, "unchanged, not driven: stays white");
        assert!((p.state[0] - 0.5).abs() < 1e-6, "unchanged pixel kept its gray");
    }

    #[test]
    fn fast_refresh_uses_the_forced_temperature() {
        let mut p = Ssd16xx::new(Glass::Ep426);
        let img = image(&p, corner_ink);
        let now = send_seq(&mut p, 0, EP426_FAST);
        write_plane(&mut p, now, 0x24, &img);
        let done = refresh(&mut p, now, 0xc7);
        assert_eq!(p.last_lut, Lut::Fast);
        assert_corner_image(&p);
        let ms = (done - now) / MS;
        assert!((1200..2000).contains(&ms), "fast refresh took {ms} ms");
        // 0xC7 without a loaded LUT (after a reset) drives nothing.
        let now = send_seq(&mut p, done, EP426_FULL);
        let white = image(&p, |_, _| false);
        write_plane(&mut p, now, 0x24, &white);
        refresh(&mut p, now, 0xc7);
        assert_eq!(p.last_lut, Lut::None);
        assert_corner_image(&p);
    }

    /// Write a 4-gray image the way the firmware's png_draw does (plane 0 = the low bit of
    /// 3 - v, plane 1 the high bit, v = 0 black .. 3 white) and return the four levels
    /// at the four quarters of the screen, sorted.
    fn gray_levels(p: &mut Ssd16xx, now: u64, seq: u8) -> Vec<u8> {
        let (w, h) = (p.w, p.h);
        let level = move |x: usize| x * 4 / w;
        let plane0 = image(p, |x, _| (3 - level(x)) & 1 == 0);
        let plane1 = image(p, |x, _| (3 - level(x)) & 2 == 0);
        write_plane(p, now, 0x24, &plane0);
        write_plane(p, now, 0x26, &plane1);
        refresh(p, now, seq);
        let got: Vec<u8> = (0..4).map(|q| px(p, q * w / 4 + w / 8, h / 2)).collect();
        // v = 0 (left) is black.
        assert!(got.windows(2).all(|g| g[0] > g[1]), "not black to white left to right: {got:?}");
        got
    }

    fn assert_gray(got: &[u8]) {
        for (g, want) in got.iter().zip([255u8, 170, 85, 0]) {
            assert!(g.abs_diff(want) <= 8, "levels {got:?}, want 255/170/85/0");
        }
    }

    #[test]
    fn four_gray_luts_give_the_intended_levels() {
        // 4.26": custom LUT (epd426g_init), 0xC7.
        let mut p = Ssd16xx::new(Glass::Ep426);
        let now = send_seq(&mut p, 0, EP426_FULL);
        cmd(&mut p, now, 0x32, &ep426_gray_lut());
        assert_gray(&gray_levels(&mut p, now, 0xc7));
        assert_eq!(p.last_lut, Lut::Custom);

        // 4.2" SSD1683: custom LUT (epd42b_init_gray), 0xCF.
        let mut p = Ssd16xx::new(Glass::Ep42b);
        let now = send_seq(&mut p, 0, EP42B_FULL);
        cmd(&mut p, now, 0x21, &[0x00, 0x00]); // epd42b_init_gray leaves it at reset
        cmd(&mut p, now, 0x32, &ep42b_gray_lut());
        assert_gray(&gray_levels(&mut p, now, 0xcf));

        // 3.97" with bb_epaper 2.1.9: the OTP 4-gray waveform at 0x5A, 0xD7.
        let mut p = Ssd16xx::new(Glass::Ep397);
        let now = send_seq(&mut p, 0, EP397_FULL);
        cmd(&mut p, now, 0x1a, &[0x5a]);
        assert_gray(&gray_levels(&mut p, now, 0xd7));
        assert_eq!(p.last_lut, Lut::Gray);

        // 3.97" with bb_epaper 2.1.11: its custom LUT (displayed without reloading).
        let mut p = Ssd16xx::new(Glass::Ep397Flipped);
        let now = send_seq(&mut p, 0, EP426_FULL);
        cmd(&mut p, now, 0x32, &ep397_gray_lut());
        assert_gray(&gray_levels(&mut p, now, 0xc7));
    }

    #[test]
    fn busy_follows_the_controller() {
        let mut p = Ssd16xx::new(Glass::Ep426);
        assert!(!p.busy(0));
        assert!(p.busy_level());
        cmd(&mut p, 0, 0x12, &[]);
        assert!(p.busy(MS) && !p.busy(4 * MS));
        cmd(&mut p, 10 * MS, 0x22, &[0x91]); // load the LUT only: no refresh
        cmd(&mut p, 10 * MS, 0x20, &[]);
        let done = wait(&mut p, 10 * MS);
        assert!(done < 50 * MS);
        assert_eq!(p.refresh_count, 0);
        p.busy_stuck = true;
        assert!(p.busy(done + 1000 * MS));
    }

    #[test]
    fn deep_sleep_ignores_commands_until_reset() {
        let mut p = Ssd16xx::new(Glass::Ep426);
        let now = send_seq(&mut p, 0, EP426_FULL);
        let img = image(&p, corner_ink);
        write_plane(&mut p, now, 0x24, &img);
        cmd(&mut p, now, 0x10, &[0x01]);
        cmd(&mut p, now, 0x11, &[0x07]);
        assert_eq!(p.regs.entry, 0x02, "asleep: ignored");
        // bbepWakeUp: RST low, high, wait for BUSY.
        p.set_pins(now, true, true, false, false, false);
        p.set_pins(now + 10 * MS, true, true, false, false, true);
        assert!(p.busy(now + 10 * MS));
        let now = wait(&mut p, now + 30 * MS);
        refresh(&mut p, now, 0xf7);
        assert_corner_image(&p); // deep sleep mode 1 kept the RAM
        // Mode 2 loses it.
        cmd(&mut p, now, 0x10, &[0x03]);
        assert!(p.bw.iter().all(|&b| b == 0));
    }

    #[test]
    fn power_off_forgets_everything_but_the_glass() {
        let mut p = Ssd16xx::new(Glass::Ep42b);
        let img = image(&p, |x, y| x < 40 && y < 20);
        let now = send_seq(&mut p, 0, EP42B_FULL);
        write_plane(&mut p, now, 0x24, &img);
        let now = refresh(&mut p, now, 0xf7);
        p.set_power(now, false);
        assert!(!p.busy(now));
        cmd(&mut p, now, 0x11, &[0x00]);
        assert_eq!(p.regs.entry, 0x03, "unpowered: ignored");
        assert!(px(&p, 5, 5) > 250, "the image stays");
        p.set_power(now, true);
        assert!(p.bw.iter().all(|&b| b == 0));
    }

    #[test]
    fn bit_banged_bytes_are_received() {
        let mut p = Ssd16xx::new(Glass::Ep42b);
        let mut t = 0;
        let mut bang = |p: &mut Ssd16xx, dc: bool, b: u8| {
            for i in (0..8).rev() {
                let bit = b >> i & 1 != 0;
                t += 1000;
                p.set_pins(t, false, dc, false, bit, true);
                t += 1000;
                p.set_pins(t, false, dc, true, bit, true);
            }
        };
        bang(&mut p, false, 0x11);
        bang(&mut p, true, 0x05);
        assert_eq!(p.regs.entry, 0x05);
    }

    #[test]
    fn save_points_keep_ram_and_registers() {
        let mut p = Ssd16xx::new(Glass::Ep426);
        let img = image(&p, corner_ink);
        let now = send_seq(&mut p, 0, EP426_FULL);
        write_plane(&mut p, now, 0x24, &img);
        let now = refresh(&mut p, now, 0xf7);
        cmd(&mut p, now, 0x32, &ep426_gray_lut());
        let mut w = StateWriter::new();
        p.save_state(&mut w, true);
        let data = w.into_bytes();
        let mut q = Ssd16xx::new(Glass::Ep426);
        q.restore_state(&mut StateReader::new(&data), true).unwrap();
        assert_eq!(q.bw, p.bw);
        assert_eq!(q.regs.lut, Lut::Custom);
        assert_eq!(q.regs.custom, p.regs.custom);
        assert_eq!((q.regs.x, q.regs.y, q.regs.entry), (p.regs.x, p.regs.y, 0x02));
        assert_corner_image(&q);
        // Unpowered: the image only.
        let mut w = StateWriter::new();
        p.save_state(&mut w, false);
        let data = w.into_bytes();
        let mut q = Ssd16xx::new(Glass::Ep426);
        q.restore_state(&mut StateReader::new(&data), false).unwrap();
        assert_corner_image(&q);
        assert_eq!(q.regs.lut, Lut::None);
        assert_eq!(q.refresh_count, 1);
    }
}
