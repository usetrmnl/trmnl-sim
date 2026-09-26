//! UltraChip UC8179 e-paper controller driving a 7.5" 800x480 B/W panel
//! (TRMNL OG). Commands arrive either as bytes from a SPI host or bit-banged
//! over GPIO. Refreshes are simulated by integrating the LUT waveform so
//! grayscale, fast/partial modes and flashing all look roughly like hardware.

use std::sync::Arc;

use parking_lot::Mutex;
use sim_api::{Frame, SharedFrame};

pub const WIDTH: usize = 800;
pub const HEIGHT: usize = 480;
const STRIDE: usize = WIDTH / 8;
const MS: u64 = 1_000_000;

#[derive(Clone, Copy)]
struct Phase {
    /// 0=GND, 1=VDH (towards black), 2=VDL (towards white), 3=VDHR
    level: u8,
    frames: u32,
}

/// A refresh in progress: a per-pixel drive schedule played out over time.
struct Refresh {
    start: u64,
    frame_ns: u64,
    /// Per LUT (index = old<<1 | new, in "color" space: 1=black), the flattened phases.
    schedules: [Vec<Phase>; 4],
    total_frames: u32,
    /// Pixel-space region being refreshed (x0, y0, x1, y1) exclusive.
    region: (usize, usize, usize, usize),
    /// Per-pixel LUT selector for the region.
    sel: Vec<u8>,
    /// Darkness at start (0.0..1.0) for the region.
    start_state: Vec<f32>,
    frames_done: u32,
}

pub struct Uc8179 {
    // Serial interface
    cs: bool,
    dc: bool,
    sck: bool,
    rst: bool,
    shift: u8,
    nbits: u8,
    /// Bits the panel drives on MOSI when answering a read command.
    read_bits: Vec<bool>,
    read_pos: usize,
    pub mosi_out: Option<bool>,

    cmd: u8,
    args: Vec<u8>,
    data_ptr: usize,
    old: Vec<u8>,
    new: Vec<u8>,
    luts: [[u8; 42]; 6], // 0x20..=0x25
    psr: u8,
    cdi: [u8; 2],
    pll: u8,
    partial_mode: bool,
    window: (usize, usize, usize, usize),
    cascade: u8,
    forced_temp: u8,
    powered: bool,
    asleep: bool,
    busy_until: u64,
    refresh: Option<Refresh>,
    pub rev: u32,
    pub temperature_c: i8,

    /// Physical particle state, 0.0 = white, 1.0 = black.
    state: Vec<f32>,
    pub frame: SharedFrame,
    pub refresh_count: u64,
}

impl Uc8179 {
    pub fn new(rev: u32) -> Self {
        let frame = Frame { width: WIDTH, height: HEIGHT, pixels: vec![0; WIDTH * HEIGHT], generation: 0 };
        Uc8179 {
            cs: true,
            dc: true,
            sck: false,
            rst: true,
            shift: 0,
            nbits: 0,
            read_bits: Vec::new(),
            read_pos: 0,
            mosi_out: None,
            cmd: 0,
            args: Vec::new(),
            data_ptr: 0,
            old: vec![0; STRIDE * HEIGHT],
            new: vec![0; STRIDE * HEIGHT],
            luts: [[0; 42]; 6],
            psr: 0x1f,
            cdi: [0x11, 0x07],
            pll: 0x06,
            partial_mode: false,
            window: (0, 0, WIDTH, HEIGHT),
            cascade: 0,
            forced_temp: 0,
            powered: false,
            asleep: false,
            busy_until: 0,
            refresh: None,
            rev,
            temperature_c: 22,
            state: vec![0.0; WIDTH * HEIGHT],
            frame: Arc::new(Mutex::new(frame)),
            refresh_count: 0,
        }
    }

    /// Hardware reset (RST pin low). Display contents survive: it's e-paper.
    fn reset(&mut self) {
        self.psr = 0x1f;
        self.cdi = [0x11, 0x07];
        self.pll = 0x06;
        self.partial_mode = false;
        self.window = (0, 0, WIDTH, HEIGHT);
        self.cascade = 0;
        self.forced_temp = 0;
        self.powered = false;
        self.asleep = false;
        self.busy_until = 0;
        self.nbits = 0;
        self.read_bits.clear();
        self.mosi_out = None;
    }

    /// BUSY_N output: low while busy.
    pub fn busy_n(&self, now: u64) -> bool {
        now >= self.busy_until
    }

    /// Next time the panel wants to be polled (for refresh animation / busy release).
    pub fn next_event(&self, now: u64) -> Option<u64> {
        if let Some(r) = &self.refresh {
            return Some(r.start + (r.frames_done as u64 + 1) * r.frame_ns);
        }
        (self.busy_until > now).then_some(self.busy_until)
    }

    // ---- pin interface -------------------------------------------------------------------

    pub fn set_pins(&mut self, now: u64, cs: bool, dc: bool, sck: bool, mosi: bool, rst: bool) {
        if !rst && self.rst {
            self.reset();
        }
        self.rst = rst;
        if !rst {
            self.cs = cs;
            self.dc = dc;
            self.sck = sck;
            return;
        }
        if cs && !self.cs {
            // CS released: byte framing restarts
            self.nbits = 0;
            self.mosi_out = None;
        }
        if dc != self.dc && !cs && !self.dc && !self.read_bits.is_empty() {
            // DC went high after a read command: start driving the response
            self.read_pos = 0;
        }
        let rising = sck && !self.sck;
        let falling = !sck && self.sck;
        self.cs = cs;
        self.dc = dc;
        self.sck = sck;
        if cs {
            return;
        }
        if self.dc && !self.read_bits.is_empty() && self.read_pos < self.read_bits.len() {
            // Read phase: shift one response bit out per clock pulse
            if falling {
                self.mosi_out = Some(self.read_bits[self.read_pos]);
                self.read_pos += 1;
            }
            return;
        }
        if rising {
            self.shift = (self.shift << 1) | mosi as u8;
            self.nbits += 1;
            if self.nbits == 8 {
                self.nbits = 0;
                let b = self.shift;
                self.byte(now, b);
            }
        }
    }

    pub fn spi_bytes(&mut self, now: u64, data: &[u8]) {
        if self.cs || !self.rst {
            return;
        }
        if self.dc && (self.cmd == 0x10 || self.cmd == 0x13) && !self.asleep {
            // Fast path for bulk image data
            for &b in data {
                self.pixel_data(b);
            }
            return;
        }
        for &b in data {
            self.byte(now, b);
        }
    }

    fn byte(&mut self, now: u64, b: u8) {
        if self.asleep {
            return;
        }
        if !self.dc {
            self.command(now, b);
        } else {
            self.data(now, b);
        }
    }

    // ---- command processing --------------------------------------------------------------

    fn command(&mut self, now: u64, c: u8) {
        self.cmd = c;
        self.args.clear();
        self.data_ptr = 0;
        self.read_bits.clear();
        match c {
            0x02 => {
                self.powered = false;
                self.busy_until = now + 20 * MS;
            }
            0x04 => {
                self.powered = true;
                self.busy_until = now + 60 * MS;
            }
            0x11 => {} // data stop
            0x12 => self.start_refresh(now),
            0x10 | 0x13 if self.partial_mode => {
                self.data_ptr = 0;
            }
            0x40 => self.respond(&[(self.temperature_c as u8), 0]),
            0x70 => {
                let r = self.rev.to_be_bytes();
                self.respond(&[0xff, 0xff, 0xff, r[0], r[1], r[2], r[3]]);
            }
            0x71 => {
                let busy_n = self.busy_n(now) as u8;
                let pon = self.powered as u8;
                self.respond(&[busy_n | (pon << 2)]);
            }
            0x91 => self.partial_mode = true,
            0x92 => self.partial_mode = false,
            _ => {}
        }
    }

    fn respond(&mut self, bytes: &[u8]) {
        self.read_bits = bytes.iter().flat_map(|b| (0..8).rev().map(move |i| b >> i & 1 != 0)).collect();
        self.read_pos = 0;
    }

    fn data(&mut self, now: u64, b: u8) {
        match self.cmd {
            0x10 | 0x13 => self.pixel_data(b),
            _ => {
                self.args.push(b);
                self.apply_args(now);
            }
        }
    }

    fn apply_args(&mut self, _now: u64) {
        let a = &self.args;
        match self.cmd {
            0x00 => self.psr = a[0],
            0x07 if a[0] == 0xa5 => {
                self.asleep = true;
            }
            0x20..=0x25 => {
                let i = (self.cmd - 0x20) as usize;
                if a.len() <= 42 {
                    self.luts[i][a.len() - 1] = a[a.len() - 1];
                }
            }
            0x30 => self.pll = a[0],
            0x50 if a.len() <= 2 => {
                self.cdi[a.len() - 1] = a[a.len() - 1];
            }
            0x90 if a.len() >= 8 => {
                let hs = ((a[0] as usize) << 8 | a[1] as usize) & !7;
                let he = ((a[2] as usize) << 8 | a[3] as usize) | 7;
                let vs = (a[4] as usize) << 8 | a[5] as usize;
                let ve = (a[6] as usize) << 8 | a[7] as usize;
                self.window = (hs.min(WIDTH), vs.min(HEIGHT), (he + 1).min(WIDTH), (ve + 1).min(HEIGHT));
            }
            0xe0 => self.cascade = a[0],
            0xe5 => self.forced_temp = a[0],
            _ => {}
        }
    }

    fn pixel_data(&mut self, b: u8) {
        let (x0, y0, x1, y1) = if self.partial_mode { self.window } else { (0, 0, WIDTH, HEIGHT) };
        let row_bytes = (x1 - x0) / 8;
        if row_bytes == 0 {
            return;
        }
        let row = self.data_ptr / row_bytes;
        let col = self.data_ptr % row_bytes;
        self.data_ptr += 1;
        if y0 + row >= y1 {
            return;
        }
        let idx = (y0 + row) * STRIDE + x0 / 8 + col;
        if self.cmd == 0x10 {
            self.old[idx] = b;
        } else {
            self.new[idx] = b;
        }
    }

    // ---- refresh simulation --------------------------------------------------------------

    /// Is the pixel "black" in the given plane? Polarity follows CDI.DDX[0].
    fn bit(plane: &[u8], x: usize, y: usize) -> bool {
        plane[y * STRIDE + x / 8] & (0x80 >> (x & 7)) != 0
    }

    fn start_refresh(&mut self, now: u64) {
        let region = if self.partial_mode { self.window } else { (0, 0, WIDTH, HEIGHT) };
        let use_reg_lut = self.psr & 0x20 != 0;
        let invert = self.cdi[0] & 1 != 0; // DDX[0]: 1 => data bit 1 means white
        let frame_hz = match self.pll & 0x3f {
            0x06 => 50,
            0x3c => 50,
            0x3a => 100,
            0x29 => 150,
            0x39 => 200,
            0x31 => 171,
            _ => 50,
        };
        let frame_ns = 1_000_000_000 / frame_hz;

        let schedules: [Vec<Phase>; 4] = if use_reg_lut {
            // Selector index: old<<1 | new, in raw bit space (not color). Map to LUT registers:
            // WW=0x21, BW=0x22, WB=0x23, BB=0x24 where W/B are colors after polarity.
            let lut = |idx: usize| Self::expand_lut(&self.luts[idx]);
            let (w, b) = (0usize, 1usize);
            let mut s: [Vec<Phase>; 4] = Default::default();
            for old in 0..2 {
                for new in 0..2 {
                    let oc = if (old == 1) != invert { b } else { w };
                    let nc = if (new == 1) != invert { b } else { w };
                    let reg = match (oc, nc) {
                        (0, 0) => 1,
                        (1, 0) => 2,
                        (0, 1) => 3,
                        _ => 4,
                    };
                    s[old << 1 | new] = lut(reg);
                }
            }
            s
        } else {
            // OTP waveform: flash to the inverse, black, white, then drive to the target.
            let fast = self.cascade & 0x02 != 0 && self.forced_temp >= 0x5a;
            let n = if fast { 8 } else { 18 };
            let to_black = |f| Phase { level: 1, frames: f };
            let to_white = |f| Phase { level: 2, frames: f };
            let tb = vec![to_white(n), to_black(n), to_white(n), to_black(n * 2)];
            let tw = vec![to_black(n), to_white(n), to_black(n), to_white(n * 2)];
            let (bl, wh) = (tb, tw);
            let mut s: [Vec<Phase>; 4] = Default::default();
            for old in 0..2 {
                for new in 0..2 {
                    let nc_black = (new == 1) != invert;
                    s[old << 1 | new] = if nc_black { bl.clone() } else { wh.clone() };
                }
            }
            s
        };
        let total_frames = schedules.iter().map(|s| s.iter().map(|p| p.frames).sum::<u32>()).max().unwrap_or(0);

        let (x0, y0, x1, y1) = region;
        let mut sel = Vec::with_capacity((x1 - x0) * (y1 - y0));
        let mut start_state = Vec::with_capacity(sel.capacity());
        for y in y0..y1 {
            for x in x0..x1 {
                let o = Self::bit(&self.old, x, y) as u8;
                let n = Self::bit(&self.new, x, y) as u8;
                sel.push(o << 1 | n);
                start_state.push(self.state[y * WIDTH + x]);
            }
        }
        self.busy_until = now + total_frames as u64 * frame_ns + 5 * MS;
        self.refresh =
            Some(Refresh { start: now, frame_ns, schedules, total_frames, region, sel, start_state, frames_done: 0 });
        self.refresh_count += 1;
        if self.cdi[0] & 0x08 != 0 {
            // N2OCP: copy NEW to OLD after refresh
            self.old.copy_from_slice(&self.new);
        }
        log::debug!("uc8179: refresh {:?} reg_lut={} frames={}", region, use_reg_lut, total_frames);
    }

    fn expand_lut(l: &[u8; 42]) -> Vec<Phase> {
        let mut v = Vec::new();
        for g in 0..7 {
            let b = &l[g * 6..g * 6 + 6];
            let rep = b[5].max(1) as u32;
            if b[1..5].iter().all(|&f| f == 0) {
                continue;
            }
            for _ in 0..rep {
                for p in 0..4 {
                    let level = (b[0] >> (6 - 2 * p)) & 3;
                    let frames = b[1 + p] as u32;
                    if frames > 0 {
                        v.push(Phase { level, frames });
                    }
                }
            }
        }
        v
    }

    /// Net drive (in frames, +black/-white) a schedule applies over frames [from, to).
    fn drive(s: &[Phase], from: u32, to: u32) -> f32 {
        let mut t = 0u32;
        let mut d = 0.0f32;
        for ph in s {
            let (a, b) = (t.max(from), (t + ph.frames).min(to));
            if b > a {
                d += match ph.level {
                    1 => 1.0,
                    2 => -1.0,
                    _ => 0.0,
                } * (b - a) as f32;
            }
            t += ph.frames;
            if t >= to {
                break;
            }
        }
        d
    }

    /// Advance the refresh animation to `now`.
    pub fn update(&mut self, now: u64) {
        let Some(r) = &mut self.refresh else { return };
        let target = (((now.saturating_sub(r.start)) / r.frame_ns) as u32).min(r.total_frames);
        if target == r.frames_done {
            if target >= r.total_frames {
                self.refresh = None;
            }
            return;
        }
        // Particle response per frame of drive; ~20 frames saturates.
        const RATE: f32 = 1.0 / 20.0;
        let delta: [f32; 4] = std::array::from_fn(|i| Self::drive(&r.schedules[i], r.frames_done, target) * RATE);
        r.frames_done = target;
        let (x0, y0, x1, _) = r.region;
        let w = x1 - x0;
        let mut frame = self.frame.lock();
        for (i, (&sel, s)) in r.sel.iter().zip(r.start_state.iter_mut()).enumerate() {
            *s = (*s + delta[sel as usize]).clamp(0.0, 1.0);
            let k = (y0 + i / w) * WIDTH + x0 + i % w;
            self.state[k] = *s;
            frame.pixels[k] = (*s * 255.0) as u8;
        }
        frame.generation += 1;
        drop(frame);
        if target >= r.total_frames {
            self.refresh = None;
        }
    }
}
