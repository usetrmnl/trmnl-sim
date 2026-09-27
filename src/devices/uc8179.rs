//! UltraChip UC8179 e-paper controller driving a 7.5" 800x480 B/W panel
//! (TRMNL OG). Commands arrive either as bytes from a SPI host or bit-banged
//! over GPIO. Refreshes are simulated by integrating the LUT waveform so
//! grayscale, fast/partial modes and flashing all look roughly like hardware.
//!
//! The same command set also drives color panels with one image plane (`DTM1`) and a
//! long built-in refresh with no LUT registers ([`ColorPanel`]): the 4-color
//! black/white/yellow/red panel of the TRMNL BWRY (GDEM075F52, 2 bits/pixel) and the
//! 7.3" Spectra 6 panel of the Seeed reTerminal E1002 (GDEP073E01, 4 bits/pixel).

use std::sync::Arc;

use parking_lot::Mutex;
use sim_api::{Frame, SharedFrame};

use crate::savepoint::{StateReader, StateWriter, read_f32s_into};

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

/// Pixel codes of the color panels, as the host sends them (Spectra 6 has no code 4).
const BWRY_BLACK: u8 = 0;
const BWRY_WHITE: u8 = 1;
const BWRY_YELLOW: u8 = 2;
const BWRY_RED: u8 = 3;
const SPECTRA_BLUE: u8 = 5;
const SPECTRA_GREEN: u8 = 6;

/// A color panel: one image plane with a built-in (OTP) refresh.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColorPanel {
    /// Black/white/yellow/red, 2 bits per pixel (TRMNL BWRY).
    Bwry,
    /// E Ink Spectra 6: black/white/yellow/red/blue/green, 4 bits per pixel (reTerminal E1002).
    Spectra6,
}

/// The color refresh: the OTP waveform shakes the particles with rapid flashes (one
/// color every `COLOR_FLASH_MS`) until `image_ms`, then the image settles; BUSY ends at
/// `refresh_ms` (the firmware waits up to 30-35 s).
const COLOR_FLASH_MS: u64 = 100;

impl ColorPanel {
    fn bits(self) -> usize {
        match self {
            ColorPanel::Bwry => 2,
            ColorPanel::Spectra6 => 4,
        }
    }

    fn ram_len(self) -> usize {
        WIDTH * HEIGHT * self.bits() / 8
    }

    /// Image RAM filled with white.
    fn white_ram(self) -> Vec<u8> {
        let per_byte = 8 / self.bits();
        let byte = (0..per_byte).fold(0u8, |b, _| b << self.bits() | BWRY_WHITE);
        vec![byte; self.ram_len()]
    }

    /// Pixel `i`'s code in image RAM (leftmost pixel in the high bits).
    fn code(self, ram: &[u8], i: usize) -> u8 {
        let bits = self.bits();
        let per_byte = 8 / bits;
        ram[i / per_byte] >> (8 - bits * (i % per_byte + 1)) & ((1 << bits) - 1) as u8
    }

    /// What a viewer sees for a code (undefined codes leave the particles white).
    fn rgb(self, code: u8) -> [u8; 3] {
        match (self, code) {
            (_, BWRY_BLACK) => [0, 0, 0],
            (_, BWRY_YELLOW) => [255, 255, 0],
            (_, BWRY_RED) => [255, 0, 0],
            (ColorPanel::Spectra6, SPECTRA_BLUE) => [0, 0, 255],
            (ColorPanel::Spectra6, SPECTRA_GREEN) => [0, 255, 0],
            _ => [255, 255, 255],
        }
    }

    /// The colors the refresh flashes through.
    fn flashes(self) -> &'static [u8] {
        match self {
            ColorPanel::Bwry => &[BWRY_BLACK, BWRY_WHITE],
            ColorPanel::Spectra6 => {
                &[BWRY_BLACK, BWRY_WHITE, BWRY_RED, BWRY_YELLOW, SPECTRA_BLUE, SPECTRA_GREEN, BWRY_WHITE]
            }
        }
    }

    fn image_ms(self) -> u64 {
        match self {
            ColorPanel::Bwry => 9_000,
            ColorPanel::Spectra6 => 12_000,
        }
    }

    fn refresh_ms(self) -> u64 {
        match self {
            ColorPanel::Bwry => 16_000,
            ColorPanel::Spectra6 => 19_000,
        }
    }

    /// Stage `i` of the refresh: (start ms, what the panel shows from then on, a solid
    /// color or `None` for the image).
    fn stage(self, i: usize) -> Option<(u64, Option<u8>)> {
        let flashes = (self.image_ms() / COLOR_FLASH_MS) as usize;
        let colors = self.flashes();
        match i {
            _ if i < flashes => Some((i as u64 * COLOR_FLASH_MS, Some(colors[i % colors.len()]))),
            _ if i == flashes => Some((self.image_ms(), None)),
            _ => None,
        }
    }
}

/// Particle response per frame of drive: each frame towards black moves a pixel this
/// fraction of the way to full black, each frame towards white this fraction of the way
/// to white (white particles respond faster). With [`optical`], fitted so bb_epaper's
/// 4-gray waveforms for the 7.5" panel land on the intended 0/85/170/255 and the OTP
/// full refresh on solid black and white.
const K_BLACK: f32 = 0.1425;
const K_WHITE: f32 = 0.3425;

/// How dark a pixel looks for a particle state (0 = white .. 1 = black): a mild S-curve,
/// as partly-driven particles scatter less light than their position suggests.
fn optical(d: f32) -> f32 {
    const S: f32 = 0.9;
    (d + S * d * (1.0 - d) * (2.0 * d - 1.0)).clamp(0.0, 1.0)
}

/// A refresh in progress: a per-pixel drive schedule played out over time.
struct Refresh {
    start: u64,
    /// Time per waveform frame.
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
    /// Color panel: its kind and image RAM, and the refresh in progress
    /// (start, stages shown so far).
    color: Option<(ColorPanel, Vec<u8>)>,
    /// Show the flashes of a color refresh (else the old image stays up).
    pub flashing: bool,
    color_refresh: Option<(u64, usize)>,
    pub frame: SharedFrame,
    pub refresh_count: u64,
    /// Fault: BUSY_N held low forever (the controller never finishes).
    pub busy_stuck: bool,
}

impl Uc8179 {
    pub fn new(rev: u32) -> Self {
        let frame = Frame::new(WIDTH, HEIGHT);
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
            color: None,
            color_refresh: None,
            flashing: true,
            frame: Arc::new(Mutex::new(frame)),
            refresh_count: 0,
            busy_stuck: false,
        }
    }

    pub fn color_panel(&self) -> Option<ColorPanel> {
        self.color.as_ref().map(|c| c.0)
    }

    /// The 4-color (black/white/yellow/red) panel variant.
    pub fn new_bwry(rev: u32) -> Self {
        Self::new_color(rev, ColorPanel::Bwry)
    }

    /// A color panel variant.
    pub fn new_color(rev: u32, kind: ColorPanel) -> Self {
        let mut p = Self::new(rev);
        p.color = Some((kind, kind.white_ram()));
        p.frame.lock().rgb = Some(vec![255; WIDTH * HEIGHT * 3]);
        p
    }

    /// Save point state: the image on the glass, and with `powered` the controller's
    /// registers and image RAM (for later differential refreshes). A refresh in progress
    /// is not saved (the caller waits for BUSY to end).
    pub fn save_state(&self, w: &mut StateWriter, powered: bool) {
        w.f32s(&self.state);
        {
            let frame = self.frame.lock();
            w.bytes(&frame.pixels);
            w.opt_bytes(frame.rgb.as_deref());
        }
        w.u64(self.refresh_count);
        if !powered {
            return;
        }
        for v in [self.cs, self.dc, self.sck, self.rst, self.partial_mode, self.powered, self.asleep] {
            w.bool(v);
        }
        w.u8(self.shift);
        w.u8(self.nbits);
        w.u8(self.cmd);
        w.bytes(&self.args);
        w.u64(self.data_ptr as u64);
        w.bytes(&self.old);
        w.bytes(&self.new);
        w.bytes(self.luts.as_flattened());
        w.u8(self.psr);
        w.bytes(&self.cdi);
        w.u8(self.pll);
        let (x0, y0, x1, y1) = self.window;
        w.u32s(&[x0 as u32, y0 as u32, x1 as u32, y1 as u32]);
        w.u8(self.cascade);
        w.u8(self.forced_temp);
        w.u64(self.busy_until);
        w.u8(self.temperature_c as u8);
        w.opt_bytes(self.color.as_ref().map(|c| c.1.as_slice()));
    }

    /// Load `save_state` output. Without `powered` the controller comes back as after
    /// power-on, showing the saved image.
    pub fn restore_state(&mut self, r: &mut StateReader, powered: bool) -> anyhow::Result<()> {
        let mut fresh = match self.color_panel() {
            Some(kind) => Self::new_color(self.rev, kind),
            None => Self::new(self.rev),
        };
        fresh.frame = self.frame.clone();
        fresh.flashing = self.flashing;
        fresh.temperature_c = self.temperature_c;
        *self = fresh;
        read_f32s_into(r, &mut self.state)?;
        {
            let mut frame = self.frame.lock();
            r.fill_u8(&mut frame.pixels)?;
            let rgb = r.opt_bytes()?;
            if rgb.is_some() != frame.rgb.is_some() {
                anyhow::bail!("save point display is of a different panel type");
            }
            if let (Some(dst), Some(src)) = (frame.rgb.as_mut(), rgb) {
                if dst.len() != src.len() {
                    anyhow::bail!("save point display is of a different panel type");
                }
                dst.copy_from_slice(src);
            }
            frame.generation += 1;
        }
        self.refresh_count = r.u64()?;
        if !powered {
            return Ok(());
        }
        for v in [
            &mut self.cs,
            &mut self.dc,
            &mut self.sck,
            &mut self.rst,
            &mut self.partial_mode,
            &mut self.powered,
            &mut self.asleep,
        ] {
            *v = r.bool()?;
        }
        self.shift = r.u8()?;
        self.nbits = r.u8()?;
        self.cmd = r.u8()?;
        self.args = r.bytes()?.to_vec();
        self.data_ptr = r.u64()? as usize;
        r.fill_u8(&mut self.old)?;
        r.fill_u8(&mut self.new)?;
        r.fill_u8(self.luts.as_flattened_mut())?;
        self.psr = r.u8()?;
        self.cdi = r.array()?;
        self.pll = r.u8()?;
        let win = r.u32s()?;
        let [x0, y0, x1, y1] = win[..] else { anyhow::bail!("save point display window is corrupt") };
        self.window = (x0 as usize, y0 as usize, x1 as usize, y1 as usize);
        self.cascade = r.u8()?;
        self.forced_temp = r.u8()?;
        self.busy_until = r.u64()?;
        self.temperature_c = r.u8()? as i8;
        match (&mut self.color, r.opt_bytes()?) {
            (Some((_, dst)), Some(src)) if dst.len() == src.len() => dst.copy_from_slice(src),
            (None, None) => {}
            _ => anyhow::bail!("save point display is of a different panel type"),
        }
        Ok(())
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
        !self.busy_stuck && now >= self.busy_until
    }

    /// Next time the panel wants to be polled (for refresh animation / busy release).
    pub fn next_event(&self, now: u64) -> Option<u64> {
        if let (Some((start, shown)), Some(kind)) = (self.color_refresh, self.color_panel()) {
            let next = kind.stage(shown).map_or(kind.refresh_ms(), |s| s.0);
            return Some(start + next * MS);
        }
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
        log::trace!(
            "uc8179: cmd {c:#04x} (previous {:#04x} took {} data bytes)",
            self.cmd,
            self.data_ptr + self.args.len()
        );
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
            0x12 if self.color.is_some() => self.start_color_refresh(now),
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
        if let Some((_, ram)) = &mut self.color {
            // One full-screen plane; DTM2 isn't used by these panels.
            if self.cmd == 0x10 && self.data_ptr < ram.len() {
                ram[self.data_ptr] = b;
            }
            self.data_ptr += 1;
            return;
        }
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

    fn start_color_refresh(&mut self, now: u64) {
        let Some(kind) = self.color_panel() else { return };
        self.busy_until = now + kind.refresh_ms() * MS;
        self.color_refresh = Some((now, 0));
        self.refresh_count += 1;
        log::debug!("uc8179: {kind:?} refresh");
        self.update(now);
    }

    /// Show one stage of a color refresh: a solid color, or (None) the image.
    fn show_color(&mut self, solid: Option<u8>) {
        let Some((kind, ram)) = &self.color else { return };
        let mut frame = self.frame.lock();
        let frame = &mut *frame;
        let rgb = frame.rgb.get_or_insert_with(|| vec![255; WIDTH * HEIGHT * 3]);
        for i in 0..WIDTH * HEIGHT {
            let c = kind.rgb(solid.unwrap_or_else(|| kind.code(ram, i)));
            rgb[i * 3..i * 3 + 3].copy_from_slice(&c);
            let luma = (c[0] as u32 * 30 + c[1] as u32 * 59 + c[2] as u32 * 11) / 100;
            frame.pixels[i] = 255 - luma as u8;
            self.state[i] = frame.pixels[i] as f32 / 255.0;
        }
        frame.generation += 1;
    }

    fn update_color(&mut self, now: u64) {
        let (Some((start, mut shown)), Some(kind)) = (self.color_refresh, self.color_panel()) else { return };
        let elapsed_ms = now.saturating_sub(start) / MS;
        // Only the latest stage that has started matters.
        let mut stage = None;
        while let Some((at, what)) = kind.stage(shown)
            && at <= elapsed_ms
        {
            stage = Some(what);
            shown += 1;
        }
        if let Some(solid) = stage
            && (solid.is_none() || self.flashing)
        {
            self.show_color(solid);
        }
        self.color_refresh = if elapsed_ms >= kind.refresh_ms() { None } else { Some((start, shown)) };
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

    /// What a schedule does to a pixel's darkness over frames [from, to), as an affine map
    /// `d -> a * d + b` (the per-frame particle response is affine, so frames compose).
    fn response(s: &[Phase], from: u32, to: u32) -> (f32, f32) {
        let (mut a, mut b) = (1.0f32, 0.0f32);
        let mut t = 0u32;
        for ph in s {
            let n = ((t + ph.frames).min(to) as i32 - t.max(from) as i32).max(0);
            // One frame towards black: d += K_BLACK * (1 - d); towards white: d -= K_WHITE * d.
            let (fa, fb) = match ph.level {
                1 => (1.0 - K_BLACK, K_BLACK),
                2 => (1.0 - K_WHITE, 0.0),
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
        if self.color.is_some() {
            return self.update_color(now);
        }
        let Some(r) = &mut self.refresh else { return };
        let target = (((now.saturating_sub(r.start)) / r.frame_ns) as u32).min(r.total_frames);
        if target == r.frames_done {
            if target >= r.total_frames {
                self.refresh = None;
            }
            return;
        }
        let resp: [(f32, f32); 4] = std::array::from_fn(|i| Self::response(&r.schedules[i], r.frames_done, target));
        r.frames_done = target;
        let (x0, y0, x1, _) = r.region;
        let w = x1 - x0;
        let mut frame = self.frame.lock();
        for (i, (&sel, s)) in r.sel.iter().zip(r.start_state.iter_mut()).enumerate() {
            let (a, b) = resp[sel as usize];
            *s = a * *s + b;
            let k = (y0 + i / w) * WIDTH + x0 + i % w;
            self.state[k] = *s;
            frame.pixels[k] = (optical(*s) * 255.0).round() as u8;
        }
        frame.generation += 1;
        drop(frame);
        if target >= r.total_frames {
            self.refresh = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmd(p: &mut Uc8179, now: u64, c: u8, data: &[u8]) {
        p.set_pins(now, false, false, false, false, true); // CS low, DC low: command
        p.spi_bytes(now, &[c]);
        p.set_pins(now, false, true, false, false, true); // DC high: data
        p.spi_bytes(now, data);
    }

    #[test]
    fn bwry_refresh_shows_the_four_colors() {
        let mut p = Uc8179::new_bwry(0);
        // Quarters of each row: black, white, yellow, red (4 pixels per byte).
        let q = WIDTH / 4 / 4;
        let row: Vec<u8> = [BWRY_BLACK, BWRY_WHITE, BWRY_YELLOW, BWRY_RED]
            .iter()
            .flat_map(|&c| std::iter::repeat_n(c * 0x55, q))
            .collect();
        let img: Vec<u8> = row.iter().copied().cycle().take(WIDTH * HEIGHT / 4).collect();
        cmd(&mut p, 0, 0x04, &[]);
        cmd(&mut p, 0, 0x10, &img);
        cmd(&mut p, 0, 0x12, &[0x00]);
        assert!(!p.busy_n(MS));
        // Rapid black/white flashes until the image appears.
        let first = |p: &Uc8179| p.frame.lock().rgb.as_ref().unwrap()[..3].to_vec();
        let mut flashes = 0;
        let mut last = None;
        let mut t = 0;
        while t < ColorPanel::Bwry.image_ms() {
            p.update(t * MS);
            let c = first(&p);
            assert!(c == [0, 0, 0] || c == [255, 255, 255], "{c:?} at {t} ms");
            if last.as_ref() != Some(&c) {
                flashes += 1;
            }
            last = Some(c);
            t += COLOR_FLASH_MS / 2;
        }
        assert!(flashes >= 60, "{flashes} flashes");
        let done = ColorPanel::Bwry.refresh_ms() * MS;
        p.update(9_500 * MS);
        assert_eq!(p.next_event(9_500 * MS), Some(done));
        p.update(done);
        assert!(p.busy_n(done));
        let f = p.frame.lock();
        let rgb = f.rgb.as_ref().unwrap();
        let at = |x: usize, y: usize| &rgb[(y * WIDTH + x) * 3..(y * WIDTH + x) * 3 + 3];
        let w4 = WIDTH / 4;
        assert_eq!(at(10, 5), &ColorPanel::Bwry.rgb(0));
        assert_eq!(at(w4 + 10, 100), &ColorPanel::Bwry.rgb(1));
        assert_eq!(at(2 * w4 + 10, 200), &[255, 255, 0]);
        assert_eq!(at(3 * w4 + 10, 479), &[255, 0, 0]);
        assert_eq!(p.refresh_count, 1);
    }

    #[test]
    fn bwry_refresh_without_flashing_keeps_the_old_image() {
        let mut p = Uc8179::new_bwry(0);
        p.flashing = false;
        let red = vec![BWRY_RED * 0x55; WIDTH * HEIGHT / 4];
        cmd(&mut p, 0, 0x04, &[]);
        cmd(&mut p, 0, 0x10, &red);
        cmd(&mut p, 0, 0x12, &[0x00]);
        let first = |p: &Uc8179| p.frame.lock().rgb.as_ref().map(|c| c[..3].to_vec());
        let before = first(&p);
        for t in (0..ColorPanel::Bwry.image_ms()).step_by(COLOR_FLASH_MS as usize / 2) {
            p.update(t * MS);
            assert_eq!(first(&p), before, "changed at {t} ms");
        }
        p.update(ColorPanel::Bwry.image_ms() * MS);
        assert_eq!(first(&p), Some(ColorPanel::Bwry.rgb(BWRY_RED).to_vec()));
        assert!(!p.busy_n(ColorPanel::Bwry.image_ms() * MS)); // timing unchanged
        assert!(p.busy_n(ColorPanel::Bwry.refresh_ms() * MS));
    }

    #[test]
    fn spectra6_refresh_shows_the_six_colors() {
        let kind = ColorPanel::Spectra6;
        let mut p = Uc8179::new_color(0, kind);
        // Eighths of each row, 2 pixels per byte (left pixel in the high nibble): the six
        // colors, the undefined code 4, and a white/black pair.
        let codes = [BWRY_BLACK, BWRY_WHITE, BWRY_YELLOW, BWRY_RED, SPECTRA_BLUE, SPECTRA_GREEN, 4, 0x10];
        let e = WIDTH / 2 / 8;
        let row: Vec<u8> =
            codes.iter().flat_map(|&c| std::iter::repeat_n(if c == 0x10 { 0x10 } else { c << 4 | c }, e)).collect();
        let img: Vec<u8> = row.iter().copied().cycle().take(kind.ram_len()).collect();
        cmd(&mut p, 0, 0x04, &[]);
        cmd(&mut p, 0, 0x10, &img);
        cmd(&mut p, 0, 0x12, &[0x00]);
        // The refresh flashes through the colors before the image settles.
        let first = |p: &Uc8179| p.frame.lock().rgb.as_ref().unwrap()[..3].to_vec();
        let mut seen = std::collections::HashSet::new();
        for t in (0..kind.image_ms()).step_by(COLOR_FLASH_MS as usize) {
            p.update(t * MS);
            seen.insert(first(&p));
        }
        assert_eq!(seen.len(), 6, "{seen:?}");
        assert!(!p.busy_n(kind.image_ms() * MS));
        let done = kind.refresh_ms() * MS;
        p.update(done);
        assert!(p.busy_n(done));
        let f = p.frame.lock();
        let rgb = f.rgb.as_ref().unwrap();
        let at = |x: usize| rgb[(100 * WIDTH + x) * 3..(100 * WIDTH + x) * 3 + 3].to_vec();
        let w8 = WIDTH / 8;
        let expect: [[u8; 3]; 7] =
            [[0, 0, 0], [255; 3], [255, 255, 0], [255, 0, 0], [0, 0, 255], [0, 255, 0], [255; 3]];
        for (k, c) in expect.iter().enumerate() {
            assert_eq!(at(k * w8 + 10), c.to_vec(), "code {}", codes[k]);
        }
        assert_eq!(at(7 * w8 + 10), vec![255; 3]); // 0x10: white, then black
        assert_eq!(at(7 * w8 + 11), vec![0; 3]);
    }

    /// A LUT register: rows of (level patterns, 4 frame counts, repeat), zero-padded to 42 bytes.
    fn lut(rows: &[[u8; 6]]) -> Vec<u8> {
        let mut v: Vec<u8> = rows.iter().flatten().copied().collect();
        v.resize(42, 0);
        v
    }

    #[test]
    fn four_gray_waveform_gives_the_intended_levels() {
        // bb_epaper's epd75_old_gray_init (TRMNL OG 4-gray mode): WW, BW, WB, BB.
        let luts: [&[[u8; 6]]; 4] = [
            &[[0x40, 10, 0, 0, 0, 1], [0x90, 20, 20, 10, 0, 1], [0x20, 20, 10, 10, 0, 1], [0xa0, 19, 10, 4, 0, 1]],
            &[[0x40, 10, 0, 0, 0, 1], [0x90, 25, 25, 0, 0, 1], [0x10, 25, 15, 0, 0, 1], [0x99, 17, 4, 6, 6, 1]],
            &[[0x40, 10, 0, 0, 0, 1], [0x90, 25, 25, 0, 0, 1], [0x10, 25, 15, 0, 0, 1], [0x99, 16, 6, 8, 3, 1]],
            &[[0x40, 10, 0, 0, 0, 1], [0x00, 18, 18, 14, 0, 1], [0x40, 18, 22, 0, 0, 1], [0x50, 35, 1, 0, 0, 1]],
        ];
        let mut p = Uc8179::new(0);
        cmd(&mut p, 0, 0x00, &[0x3f]); // PSR: LUTs from registers
        cmd(&mut p, 0, 0x50, &[0x00, 0x07]);
        for (i, l) in luts.iter().enumerate() {
            cmd(&mut p, 0, 0x21 + i as u8, &lut(l));
        }
        // Pixels 0..3 of the first row get (old, new) = 00, 01, 10, 11.
        let mut old = vec![0u8; STRIDE * HEIGHT];
        let mut new = old.clone();
        old[0] = 0b0011_0000;
        new[0] = 0b0101_0000;
        cmd(&mut p, 0, 0x04, &[]);
        cmd(&mut p, 0, 0x10, &old);
        cmd(&mut p, 0, 0x13, &new);
        cmd(&mut p, 0, 0x12, &[]);
        p.update(60_000 * MS);
        let f = p.frame.lock();
        let mut levels: Vec<u8> = f.pixels[..4].to_vec();
        levels.sort();
        for (got, want) in levels.iter().zip([0u8, 85, 170, 255]) {
            assert!(got.abs_diff(want) <= 3, "levels {levels:?}, want 0/85/170/255");
        }
    }
}
