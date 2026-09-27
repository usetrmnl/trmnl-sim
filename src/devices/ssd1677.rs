//! Solomon Systech SSD1677 e-paper controller with an 800x480 black-and-white panel, as
//! bb_epaper drives it for EP426_800x480 / EP426_800x480_4GRAY (4.26" / 3.97" panels such as
//! the M5Paper's): command bytes with D/C low, their parameters with D/C high; BUSY is
//! driven *high* while the controller works.
//!
//! Modelled: the two RAM planes (0x24 "BW", 0x26 "RED"), the RAM window and address
//! counters with pixel-granular X addresses (0x44 / 0x4e) and the data entry mode (0x11:
//! bit 0 X increment, bit 1 Y increment, bit 2 Y-first), SW reset (0x12), deep sleep (0x10),
//! the waveform LUT register (0x32) and a display update (0x22 + 0x20):
//!
//! - With a LUT from OTP (0x22 bit 4 loads it; bb_epaper's "fast" sequence loads one ahead
//!   with 0x22 = 0x91), the panel ends up showing the BW plane (1 = white).
//! - With a LUT written to 0x32 (bb_epaper's 4-gray modes), each pixel plays the waveform of
//!   LUT `RED << 1 | BW`; the resulting shade comes from a linear particle model in which a
//!   frame of VSH1 moves a pixel [`DRIVE_PER_FRAME`] of the way towards black and a frame of
//!   VSL as far towards white.
//!
//! The panel's source lines run right to left: RAM X address `x` is column `799 - x` on
//! the glass, so bb_epaper's X-decrementing entry mode (0x02, from X = 799) puts its first
//! byte at the top left, MSB leftmost. The image changes when the update ends (no flashing).

use std::sync::Arc;

use parking_lot::Mutex;
use sim_api::{Frame, SharedFrame};

use crate::savepoint::{StateReader, StateWriter};

pub const WIDTH: usize = 800;
pub const HEIGHT: usize = 480;
const MS: u64 = 1_000_000;
/// Waveform frame time (bb_epaper's 4-gray LUTs: 32 frames in about 1.2 s).
const FRAME_NS: u64 = 36_750_000;
/// Linear particle response: fraction of the white-to-black range one frame of drive covers.
pub const DRIVE_PER_FRAME: f32 = 1.0 / 6.0;
const LUT_LEN: usize = 105;

/// Where the waveform comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Lut {
    /// Built in, loaded from OTP (full, fast or partial: all end on the BW plane).
    Otp,
    /// Written by the host to 0x32.
    Register,
}

pub struct Ssd1677 {
    // Serial interface
    cs: bool,
    dc: bool,
    sck: bool,
    rst: bool,
    shift: u8,
    nbits: u8,

    cmd: u8,
    args: Vec<u8>,
    /// Plane bits: 1 per pixel, indexed by RAM address (y * WIDTH + x).
    bw: Vec<bool>,
    red: Vec<bool>,
    entry_mode: u8,
    /// RAM window, as written (X in pixels): start and end, either order.
    x_start: usize,
    x_end: usize,
    y_start: usize,
    y_end: usize,
    x: usize,
    y: usize,
    update_ctrl: u8,
    lut: Lut,
    lut_regs: [u8; LUT_LEN],
    asleep: bool,
    busy_until: u64,
    /// The update in progress shows this (darkness per pixel, glass order) when it ends.
    pending: Option<(u64, Vec<u8>)>,
    pub frame: SharedFrame,
    pub refresh_count: u64,
    pub busy_stuck: bool,
}

impl Default for Ssd1677 {
    fn default() -> Self {
        Self::new()
    }
}

impl Ssd1677 {
    pub fn new() -> Self {
        Ssd1677 {
            cs: true,
            dc: true,
            sck: false,
            rst: true,
            shift: 0,
            nbits: 0,
            cmd: 0,
            args: Vec::new(),
            bw: vec![true; WIDTH * HEIGHT],
            red: vec![false; WIDTH * HEIGHT],
            entry_mode: 0x03,
            x_start: 0,
            x_end: WIDTH - 1,
            y_start: 0,
            y_end: HEIGHT - 1,
            x: 0,
            y: 0,
            update_ctrl: 0xff,
            lut: Lut::Otp,
            lut_regs: [0; LUT_LEN],
            asleep: false,
            busy_until: 0,
            pending: None,
            frame: Arc::new(Mutex::new(Frame::new(WIDTH, HEIGHT))),
            refresh_count: 0,
            busy_stuck: false,
        }
    }

    /// Registers to their power-on values (RAM and the glass keep their contents).
    fn reset_registers(&mut self) {
        self.entry_mode = 0x03;
        (self.x_start, self.x_end, self.y_start, self.y_end) = (0, WIDTH - 1, 0, HEIGHT - 1);
        (self.x, self.y) = (0, 0);
        self.update_ctrl = 0xff;
        self.lut = Lut::Otp;
        self.asleep = false;
    }

    pub fn busy(&self, now: u64) -> bool {
        self.busy_stuck || now < self.busy_until
    }

    pub fn set_pins(&mut self, now: u64, cs: bool, dc: bool, sck: bool, mosi: bool, rst: bool) {
        if !rst && self.rst {
            self.reset_registers();
            self.busy_until = 0;
        }
        self.rst = rst;
        if cs && !self.cs {
            self.nbits = 0;
        }
        let rising = sck && !self.sck;
        (self.cs, self.dc, self.sck) = (cs, dc, sck);
        if cs || !rst || !rising {
            return;
        }
        self.shift = self.shift << 1 | mosi as u8;
        self.nbits += 1;
        if self.nbits == 8 {
            self.nbits = 0;
            self.byte(now, self.shift);
        }
    }

    pub fn spi_bytes(&mut self, now: u64, data: &[u8]) {
        if self.cs || !self.rst {
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
            self.data(b);
        }
    }

    fn command(&mut self, now: u64, c: u8) {
        log::trace!("ssd1677: cmd {c:#04x}");
        self.cmd = c;
        self.args.clear();
        match c {
            0x12 => {
                self.reset_registers();
                self.busy_until = now + 10 * MS;
            }
            0x20 => self.activate(now),
            _ => {}
        }
    }

    fn data(&mut self, b: u8) {
        match self.cmd {
            0x24 | 0x26 => return self.ram_byte(b),
            0x32 => {
                if self.args.len() < LUT_LEN {
                    self.lut_regs[self.args.len()] = b;
                }
                self.args.push(b);
                self.lut = Lut::Register;
                return;
            }
            _ => self.args.push(b),
        }
        let a = &self.args;
        let word = |i: usize| a.get(i + 1).map_or(a[i] as usize, |&h| (h as usize) << 8 | a[i] as usize);
        match (self.cmd, a.len()) {
            (0x10, 1) => self.asleep = a[0] & 3 != 0,
            (0x11, 1) => self.entry_mode = a[0] & 7,
            (0x22, 1) => self.update_ctrl = a[0],
            (0x44, 2) => self.x_start = word(0).min(WIDTH - 1),
            (0x44, 4) => self.x_end = word(2).min(WIDTH - 1),
            (0x45, 2) => self.y_start = word(0).min(HEIGHT - 1),
            (0x45, 4) => self.y_end = word(2).min(HEIGHT - 1),
            (0x4e, 1 | 2) => self.x = word(0).min(WIDTH - 1),
            (0x4f, 1 | 2) => self.y = word(0).min(HEIGHT - 1),
            _ => {}
        }
    }

    /// Step one address counter within its window; true when it wrapped.
    fn step(pos: &mut usize, inc: bool, a: usize, b: usize, by: usize) -> bool {
        let (lo, hi) = (a.min(b), a.max(b));
        if inc {
            if *pos + by > hi {
                *pos = lo;
                return true;
            }
            *pos += by;
        } else {
            if *pos < lo + by {
                *pos = hi;
                return true;
            }
            *pos -= by;
        }
        false
    }

    fn ram_byte(&mut self, b: u8) {
        let x_inc = self.entry_mode & 1 != 0;
        let y_inc = self.entry_mode & 2 != 0;
        let plane = if self.cmd == 0x24 { &mut self.bw } else { &mut self.red };
        for i in 0..8 {
            let x = if x_inc { self.x + i } else { self.x.wrapping_sub(i) };
            if x < WIDTH {
                plane[self.y * WIDTH + x] = b & 0x80 >> i != 0;
            }
        }
        if self.entry_mode & 4 == 0 {
            if Self::step(&mut self.x, x_inc, self.x_start, self.x_end, 8) {
                Self::step(&mut self.y, y_inc, self.y_start, self.y_end, 1);
            }
        } else if Self::step(&mut self.y, y_inc, self.y_start, self.y_end, 1) {
            Self::step(&mut self.x, x_inc, self.x_start, self.x_end, 8);
        }
    }

    /// Darkness (0 = white .. 1 = black) each of the four register LUTs leaves a pixel at,
    /// starting from white, and the waveform's length in frames.
    fn register_lut_shades(&self) -> ([f32; 4], u32) {
        let l = &self.lut_regs;
        let mut shades = [0f32; 4];
        let mut frames_total = 0;
        for (lut, shade) in shades.iter_mut().enumerate() {
            let mut d = 0f32;
            let mut frames = 0;
            for group in 0..10 {
                let vs = l[lut * 10 + group];
                let tp = &l[50 + group * 5..50 + group * 5 + 5];
                for _ in 0..=tp[4] {
                    for phase in 0..4 {
                        let n = tp[phase] as u32;
                        frames += n;
                        let step = match vs >> (6 - 2 * phase) & 3 {
                            0b01 => DRIVE_PER_FRAME,
                            0b10 => -DRIVE_PER_FRAME,
                            _ => 0.0,
                        };
                        d = (d + step * n as f32).clamp(0.0, 1.0);
                    }
                }
            }
            *shade = d;
            frames_total = frames_total.max(frames);
        }
        (shades, frames_total)
    }

    /// Master activation: run the update 0x22 selected.
    fn activate(&mut self, now: u64) {
        let ctrl = self.update_ctrl;
        if ctrl & 0x10 != 0 {
            self.lut = Lut::Otp; // load the LUT from OTP
        }
        if ctrl & 0x04 == 0 {
            // clock / analog / LUT loading only
            self.busy_until = now + if ctrl & 0x10 != 0 { 20 * MS } else { MS };
            return;
        }
        let (shade_of, ms): (Box<dyn Fn(bool, bool) -> u8>, u64) = match self.lut {
            Lut::Otp => {
                let ms = if ctrl & 0x08 != 0 {
                    600 // display mode 2: partial
                } else if ctrl & 0x10 != 0 {
                    3_000 // full
                } else {
                    1_500 // preloaded (fast) LUT
                };
                (Box::new(|bw, _| if bw { 0 } else { 255 }), ms)
            }
            Lut::Register => {
                let (shades, frames) = self.register_lut_shades();
                let dark = shades.map(|d| (d * 255.0).round() as u8);
                (Box::new(move |bw, red| dark[(red as usize) << 1 | bw as usize]), frames as u64 * FRAME_NS / MS)
            }
        };
        let mut img = vec![0u8; WIDTH * HEIGHT];
        for y in 0..HEIGHT {
            for x in 0..WIDTH {
                let i = y * WIDTH + x;
                img[y * WIDTH + (WIDTH - 1 - x)] = shade_of(self.bw[i], self.red[i]);
            }
        }
        let done = now + ms.max(1) * MS;
        self.busy_until = done;
        self.pending = Some((done, img));
        self.refresh_count += 1;
        log::debug!("ssd1677: update {ctrl:#04x} with {:?} LUT, {ms} ms", self.lut);
    }

    pub fn next_event(&self, now: u64) -> Option<u64> {
        match &self.pending {
            Some((at, _)) => Some(*at),
            None => (self.busy_until > now).then_some(self.busy_until),
        }
    }

    pub fn update(&mut self, now: u64) {
        if self.pending.as_ref().is_some_and(|(at, _)| now >= *at) {
            let (_, img) = self.pending.take().unwrap();
            let mut f = self.frame.lock();
            f.pixels = img;
            f.generation += 1;
        }
    }

    pub fn save_state(&self, w: &mut StateWriter, powered: bool) {
        w.bytes(&self.frame.lock().pixels);
        w.u64(self.refresh_count);
        if !powered {
            return;
        }
        for v in [self.cs, self.dc, self.sck, self.rst, self.asleep, self.lut == Lut::Register] {
            w.bool(v);
        }
        w.u8(self.shift);
        w.u8(self.nbits);
        w.u8(self.cmd);
        w.bytes(&self.args);
        let pack = |p: &[bool]| p.chunks(8).map(|c| c.iter().fold(0u8, |b, &v| b << 1 | v as u8)).collect::<Vec<_>>();
        w.bytes(&pack(&self.bw));
        w.bytes(&pack(&self.red));
        w.u8(self.entry_mode);
        let pos = [self.x_start, self.x_end, self.y_start, self.y_end, self.x, self.y];
        w.u32s(&pos.map(|v| v as u32));
        w.u8(self.update_ctrl);
        w.bytes(&self.lut_regs);
        w.u64(self.busy_until);
    }

    pub fn restore_state(&mut self, r: &mut StateReader, powered: bool) -> anyhow::Result<()> {
        let frame = self.frame.clone();
        *self = Self { frame, busy_stuck: self.busy_stuck, ..Self::new() };
        {
            let mut f = self.frame.lock();
            r.fill_u8(&mut f.pixels)?;
            f.generation += 1;
        }
        self.refresh_count = r.u64()?;
        if !powered {
            return Ok(());
        }
        let mut reg_lut = false;
        for v in [&mut self.cs, &mut self.dc, &mut self.sck, &mut self.rst, &mut self.asleep, &mut reg_lut] {
            *v = r.bool()?;
        }
        self.lut = if reg_lut { Lut::Register } else { Lut::Otp };
        self.shift = r.u8()?;
        self.nbits = r.u8()?;
        self.cmd = r.u8()?;
        self.args = r.bytes()?.to_vec();
        for plane in [&mut self.bw, &mut self.red] {
            let bytes = r.bytes()?;
            if bytes.len() * 8 != plane.len() {
                anyhow::bail!("save point display is of a different panel type");
            }
            for (i, v) in plane.iter_mut().enumerate() {
                *v = bytes[i / 8] & 0x80 >> (i % 8) != 0;
            }
        }
        self.entry_mode = r.u8()?;
        let pos = r.u32s()?;
        let [xs, xe, ys, ye, x, y] = pos[..] else { anyhow::bail!("save point display window is corrupt") };
        (self.x_start, self.x_end, self.y_start, self.y_end) = (xs as usize, xe as usize, ys as usize, ye as usize);
        (self.x, self.y) = (x as usize, y as usize);
        self.update_ctrl = r.u8()?;
        r.fill_u8(&mut self.lut_regs)?;
        self.busy_until = r.u64()?;
        Ok(())
    }
}

impl crate::devices::epd::SpiEpd for Ssd1677 {
    fn set_pins(&mut self, now: u64, cs: bool, dc: bool, sck: bool, mosi: bool, rst: bool) {
        Ssd1677::set_pins(self, now, cs, dc, sck, mosi, rst);
    }
    fn spi_bytes(&mut self, now: u64, data: &[u8]) {
        Ssd1677::spi_bytes(self, now, data);
    }
    fn busy(&self, now: u64) -> bool {
        Ssd1677::busy(self, now)
    }
    fn busy_level(&self) -> bool {
        true
    }
    fn next_event(&self, now: u64) -> Option<u64> {
        Ssd1677::next_event(self, now)
    }
    fn update(&mut self, now: u64) {
        Ssd1677::update(self, now);
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
        Ssd1677::save_state(self, w, powered);
    }
    fn restore_state(&mut self, r: &mut StateReader, powered: bool) -> anyhow::Result<()> {
        Ssd1677::restore_state(self, r, powered)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmd(p: &mut Ssd1677, c: u8, data: &[u8]) {
        p.set_pins(0, false, false, false, false, true);
        p.spi_bytes(0, &[c]);
        p.set_pins(0, false, true, false, false, true);
        p.spi_bytes(0, data);
        p.set_pins(0, true, true, false, false, true);
    }

    /// bb_epaper's orientation-0 window for the 800 px wide panel (SET_ORIENTATION).
    fn window(p: &mut Ssd1677) {
        cmd(p, 0x11, &[0x02]);
        cmd(p, 0x44, &[0x1f, 0x03, 0x00, 0x00]);
        cmd(p, 0x45, &[0x00, 0x00, 0xdf, 0x01]);
        cmd(p, 0x4e, &[0x1f, 0x03]);
        cmd(p, 0x4f, &[0x00, 0x00]);
    }

    fn finish(p: &mut Ssd1677) -> Vec<u8> {
        let t = p.next_event(0).unwrap();
        p.update(t);
        assert!(!p.busy(t));
        p.frame.lock().pixels.clone()
    }

    #[test]
    fn full_update_shows_the_bw_plane_left_to_right() {
        let mut p = Ssd1677::new();
        cmd(&mut p, 0x12, &[]);
        window(&mut p);
        // row 0: first byte 0x0f (4 black, 4 white), the rest white; row 1 all black
        let mut img = vec![0xff; WIDTH / 8 * HEIGHT];
        img[0] = 0x0f;
        img[WIDTH / 8..WIDTH / 4].fill(0);
        cmd(&mut p, 0x24, &img);
        cmd(&mut p, 0x22, &[0xf7]);
        cmd(&mut p, 0x20, &[]);
        assert!(p.busy(MS));
        let f = finish(&mut p);
        assert_eq!(&f[..9], &[255, 255, 255, 255, 0, 0, 0, 0, 0]);
        assert_eq!(f[WIDTH - 1], 0);
        assert!(f[WIDTH..2 * WIDTH].iter().all(|&v| v == 255));
        assert!(f[2 * WIDTH..].iter().all(|&v| v == 0));
        assert_eq!(p.refresh_count, 1);
    }

    #[test]
    fn register_lut_gives_four_grays() {
        let mut p = Ssd1677::new();
        cmd(&mut p, 0x12, &[]);
        window(&mut p);
        // bb_epaper's epd426g_init 4-gray LUT
        let mut lut = vec![0u8; LUT_LEN];
        lut[..40].copy_from_slice(&[
            0x55, 0x55, 0xaa, 0xaa, 0xaa, 0xaa, 0xaa, 0xaa, 0x00, 0x00, // white
            0x55, 0x55, 0xaa, 0xaa, 0xaa, 0xaa, 0xaa, 0x50, 0x00, 0x00, // light gray
            0x55, 0x55, 0xaa, 0xaa, 0x55, 0x55, 0x55, 0xa0, 0x00, 0x00, // dark gray
            0x55, 0x55, 0xaa, 0xaa, 0x55, 0x55, 0x55, 0x50, 0x00, 0x00, // black
        ]);
        for g in 0..8 {
            lut[50 + g * 5..54 + g * 5].fill(1);
        }
        cmd(&mut p, 0x32, &lut);
        // four 8 px columns per plane combination: (RED, BW) = 00, 01, 10, 11
        let row_bw = [0x00u8, 0xff, 0x00, 0xff];
        let row_red = [0x00u8, 0x00, 0xff, 0xff];
        let plane = |r: [u8; 4]| (0..HEIGHT).flat_map(|_| r.into_iter().chain([0; WIDTH / 8 - 4])).collect::<Vec<_>>();
        cmd(&mut p, 0x24, &plane(row_bw));
        window(&mut p);
        cmd(&mut p, 0x26, &plane(row_red));
        cmd(&mut p, 0x22, &[0xc7]);
        cmd(&mut p, 0x20, &[]);
        let f = finish(&mut p);
        assert_eq!([f[0], f[8], f[16], f[24]], [0, 85, 170, 255]);
    }

    #[test]
    fn round_trips() {
        let mut p = Ssd1677::new();
        window(&mut p);
        cmd(&mut p, 0x24, &[0x5a; 100]);
        cmd(&mut p, 0x22, &[0xf7]);
        cmd(&mut p, 0x20, &[]);
        finish(&mut p);
        for powered in [true, false] {
            let mut w = StateWriter::new();
            p.save_state(&mut w, powered);
            let saved = w.into_bytes();
            let mut q = Ssd1677::new();
            let mut r = StateReader::new(&saved);
            q.restore_state(&mut r, powered).unwrap();
            r.finish().unwrap();
            let mut w = StateWriter::new();
            q.save_state(&mut w, powered);
            assert_eq!(w.into_bytes(), saved);
        }
    }
}
