//! Directly-driven parallel e-paper panel (EPDiy V7-style), as on the TRMNL X:
//! a 10.3" 1872x1404 ED103TC2-class panel whose source drivers are fed by the
//! SoC's LCD_CAM peripheral (i80 mode, 16-bit bus) and whose gate drivers are
//! stepped with GPIOs. There is no controller chip: every scan of the panel
//! ("frame") carries a 2-bit *push code* per pixel, and the image emerges from
//! the sequence of frames the host sends (FastEPD's `bbepFullUpdate`,
//! `bbepPartialUpdate`, `bbepClear`).
//!
//! # What the SoC / board glue feeds in
//!
//! * [`ParallelEpd::set_row_pins`] with the current levels of SPV/STV (GPIO48),
//!   CKV (GPIO45) and LE (GPIO42) whenever any of them changes. Like the real
//!   gate driver, a frame starts when CKV rises while SPV is low (FastEPD's
//!   `EPDiyV7RowControl(ROW_START)` does exactly one such edge). LE falling
//!   edges are counted (source-driver latch) but rows are indexed by transfer
//!   count, see below.
//! * [`ParallelEpd::bus_transfer`] with the bytes of every completed LCD_CAM
//!   transfer. The n-th transfer after the frame start is row n. Only the
//!   first `row_bytes` (468) bytes are pixel data; FastEPD sends 468 + 44 bytes
//!   of line padding and the padding is ignored. Transfers outside a frame,
//!   shorter than `row_bytes`, or beyond the last row are ignored (counted in
//!   [`EpdStats::ignored_transfers`]).
//! * [`ParallelEpd::set_power`]: high voltages up (TPS65185 rails enabled and
//!   PCA9535 PWRUP etc. — the board decides) and
//!   [`ParallelEpd::set_output_enable`] (source-driver OE; defaults to on so a
//!   board that folds OE into `set_power` can ignore it). Pixels only move
//!   while both are on; frames are still scanned (and counted) otherwise.
//! * Optionally [`ParallelEpd::poll`] now and then, so an update that ended
//!   with the high voltages left on (`bKeepOn`) gets closed in the stats.
//!
//! # Decoding a row
//!
//! Byte `k` of a row, pixel `m` (0..4) holds `code = (b >> (6 - 2m)) & 3` for
//! column `x = 4k + m`, or `x = width - 1 - (4k + m)` with `mirror_x`
//! (the TRMNL X panel). Codes: `01` push toward black, `10` push toward white,
//! `00`/`11` no drive.
//!
//! # Particle physics model
//!
//! Each pixel has a darkness `d` in 0.0 (white) ..= 1.0 (black) and remembers
//! the direction of its last push. A push moves `d` a fixed fraction of the
//! remaining distance toward the driven extreme (saturating, so order
//! matters):
//!
//! ```text
//! black push: d += kb * f * (1 - d)       white push: d -= kw * f * d
//! kb = PUSH_RATE_BLACK (0.64), kw = PUSH_RATE_WHITE (0.62),
//! f = REVERSAL_FACTOR (0.4) on the first push after a push in the opposite
//! direction, else 1.0
//! ```
//!
//! The reversal penalty models particle inertia/hysteresis (pigment that was
//! just driven one way responds sluggishly to the first opposite pulse); the
//! memory is cleared when the panel is powered down. With a pure
//! saturating model the firmware's 16-level table (`u8_graytable`) is not
//! monotonic; with the penalty it is. The constants come from a grid search
//! maximising the smallest step between adjacent gray levels, replaying the
//! real firmware flow (direction memory carried over from the clear), subject
//! to: 8 clear pushes saturate (>= 99%), 3 black pushes after a clear >= 90%,
//! 3 white pushes after those <= 10%. Results:
//!
//! * 8 darken frames from white -> 0.9997, then 8 lighten frames -> 0.0009
//! * 1bpp: 3 black pushes after CLEAR_FAST -> 0.904 (pixel 230); 3 white
//!   pushes after that -> 0.098; 3 black pushes from rest -> 0.953
//! * gray levels 0..15 after CLEAR_FAST (darkness): .904 .801 .752 .731 .693
//!   .680 .593 .563 .513 .347 .277 .264 .224 .114 .014 .000 — pixels 230 204
//!   192 186 177 173 151 143 131 88 71 67 57 29 4 0 (smallest step 0.013)
//!
//! The displayed value is `round(d * 255)`.
//!
//! # Output
//!
//! [`ParallelEpd::frame`] is a [`SharedFrame`] updated at the end of every
//! scan that pushed any pixel (its `generation` bumped), so viewers see the
//! flashing of a refresh frame by frame. [`ParallelEpd::stats`] counts scans,
//! rows, latches and "updates": runs of driven scans closed by power-off, or
//! by a neutral (all-`00`) scan followed by [`UPDATE_IDLE_GAP_NS`] of silence.

// Not wired into a board yet (the TRMNL X SoC/board glue lands separately).
#![allow(dead_code)]

use std::sync::Arc;

use parking_lot::Mutex;
use sim_api::{Frame, SharedFrame};

use crate::savepoint::{StateReader, StateWriter, read_f32s_into};

/// Fraction of the remaining distance to black one black push covers.
pub const PUSH_RATE_BLACK: f32 = 0.64;
/// Fraction of the remaining distance to white one white push covers.
pub const PUSH_RATE_WHITE: f32 = 0.62;
/// Rate multiplier for the first push after a push in the other direction.
pub const REVERSAL_FACTOR: f32 = 0.4;
/// An update ends when a neutral scan is followed by this much time without a scan.
pub const UPDATE_IDLE_GAP_NS: u64 = 20_000_000;

/// Push codes, as they appear on the bus (and as stored in the direction memory).
const PUSH_BLACK: u8 = 1;
const PUSH_WHITE: u8 = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PanelGeometry {
    pub width: usize,
    pub height: usize,
    /// Byte 0 of a row drives the rightmost column (FastEPD BB_PANEL_FLAG_MIRROR_X).
    pub mirror_x: bool,
    /// Pixel bytes per row (width / 4); anything after that in a transfer is padding.
    pub row_bytes: usize,
}

impl PanelGeometry {
    /// FastEPD `BB_PANEL_TRMNL_X`: 1872x1404, MIRROR_X, 16-bit bus, 44 bytes padding.
    pub const TRMNL_X: PanelGeometry = PanelGeometry { width: 1872, height: 1404, mirror_x: true, row_bytes: 468 };
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UpdateKind {
    /// Contained clear (flash) scans followed by image data, or looked like a full
    /// redraw without a clear (every pixel pushed in some scan, or >= 5 data scans,
    /// e.g. a 4bpp gray update).
    Full,
    /// Image-data scans only, driving just some pixels (differential update).
    Partial,
    /// Uniform darken/lighten scans only.
    Clear,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UpdateInfo {
    pub kind: UpdateKind,
    pub start_ns: u64,
    pub end_ns: u64,
    /// Driven scans in the update (clear + data + neutral).
    pub frames: u32,
    /// Scans pushing every pixel the same way (0x55 / 0xAA rows).
    pub clear_frames: u32,
    /// Scans with image-dependent pushes.
    pub data_frames: u32,
    /// Scans with no pushes at all (0x00 rows).
    pub neutral_frames: u32,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EpdStats {
    /// Scans started by a gate start pulse that received at least one row.
    pub frames_scanned: u64,
    /// Scans in which at least one row was driven (powered and OE on).
    pub frames_driven: u64,
    /// Scans cut short (new start pulse or power-off before the last row).
    pub incomplete_frames: u64,
    /// Transfers accepted as rows.
    pub rows: u64,
    /// Transfers dropped: outside a frame, too short, or past the last row.
    pub ignored_transfers: u64,
    /// LE falling edges (source-driver latches).
    pub latches: u64,
    /// Completed updates.
    pub updates: u64,
    pub last_update: Option<UpdateInfo>,
    pub update_in_progress: bool,
    pub powered: bool,
    pub output_enabled: bool,
}

/// Number of pixels in a byte with code 01 (black) and 10 (white).
const fn code_counts() -> [[u8; 2]; 256] {
    let mut t = [[0u8; 2]; 256];
    let mut b = 0;
    while b < 256 {
        let mut m = 0;
        while m < 4 {
            match (b >> (6 - 2 * m)) & 3 {
                1 => t[b][0] += 1,
                2 => t[b][1] += 1,
                _ => {}
            }
            m += 1;
        }
        b += 1;
    }
    t
}
static CODE_COUNTS: [[u8; 2]; 256] = code_counts();

/// Apply one push (`code` as on the bus) to a pixel. No-op codes leave it alone.
#[inline(always)]
pub fn apply_push(d: &mut f32, last: &mut u8, code: u8) {
    match code {
        PUSH_BLACK => {
            let f = if *last == PUSH_WHITE { REVERSAL_FACTOR } else { 1.0 };
            *d += PUSH_RATE_BLACK * f * (1.0 - *d);
            *last = PUSH_BLACK;
        }
        PUSH_WHITE => {
            let f = if *last == PUSH_BLACK { REVERSAL_FACTOR } else { 1.0 };
            *d -= PUSH_RATE_WHITE * f * *d;
            *last = PUSH_WHITE;
        }
        _ => {}
    }
}

#[inline(always)]
fn to_pixel(d: f32) -> u8 {
    (d * 255.0 + 0.5) as u8
}

/// Per-scan accumulators.
#[derive(Default)]
struct ScanAcc {
    start_ns: u64,
    rows: usize,
    driven_rows: usize,
    black: u64,
    white: u64,
    /// Any pixel pushed (rows marked in `dirty`).
    pushed: bool,
}

struct UpdateAcc {
    start_ns: u64,
    last_end_ns: u64,
    frames: u32,
    clear: u32,
    data: u32,
    neutral: u32,
    full_coverage: bool,
    last_was_neutral: bool,
}

pub struct ParallelEpd {
    geom: PanelGeometry,
    powered: bool,
    oe: bool,
    spv: bool,
    ckv: bool,
    le: bool,
    in_frame: bool,
    row: usize,
    scan: ScanAcc,
    last_scan_end_ns: u64,
    update: Option<UpdateAcc>,
    /// Darkness 0.0 (white) ..= 1.0 (black) per pixel.
    state: Vec<f32>,
    /// Direction of the last push per pixel (0 = none since power-up).
    last_dir: Vec<u8>,
    /// Displayed bytes, mirrored into `frame` for dirty rows at scan end.
    pixels: Vec<u8>,
    dirty: Vec<bool>,
    frame: SharedFrame,
    stats: EpdStats,
}

impl ParallelEpd {
    pub fn new(geom: PanelGeometry) -> Self {
        let n = geom.width * geom.height;
        let frame = Frame::new(geom.width, geom.height);
        ParallelEpd {
            geom,
            powered: false,
            oe: true,
            spv: true,
            ckv: false,
            le: false,
            in_frame: false,
            row: 0,
            scan: ScanAcc::default(),
            last_scan_end_ns: 0,
            update: None,
            state: vec![0.0; n],
            last_dir: vec![0; n],
            pixels: vec![0; n],
            dirty: vec![false; geom.height],
            frame: Arc::new(Mutex::new(frame)),
            stats: EpdStats { output_enabled: true, ..Default::default() },
        }
    }

    pub fn geometry(&self) -> PanelGeometry {
        self.geom
    }

    pub fn frame(&self) -> SharedFrame {
        self.frame.clone()
    }

    /// Physical darkness (0.0 white ..= 1.0 black) of a pixel; 0.0 outside the panel.
    pub fn darkness(&self, x: usize, y: usize) -> f32 {
        if x < self.geom.width && y < self.geom.height { self.state[y * self.geom.width + x] } else { 0.0 }
    }

    pub fn stats(&self) -> EpdStats {
        let mut s = self.stats.clone();
        s.update_in_progress = self.update.is_some();
        s.powered = self.powered;
        s.output_enabled = self.oe;
        s
    }

    /// Save point state: the particles (and with `powered`, the push direction memory and
    /// the row-control pins). An update in progress is not saved (the caller waits for it).
    pub fn save_state(&self, w: &mut StateWriter, powered: bool) {
        w.f32s(&self.state);
        w.bytes(&self.pixels);
        w.u64(self.stats.updates);
        if powered {
            w.bytes(&self.last_dir);
            for v in [self.powered, self.oe, self.spv, self.ckv, self.le] {
                w.bool(v);
            }
            w.u64(self.last_scan_end_ns);
        }
    }

    /// Load `save_state` output; without `powered` the panel is unpowered and idle. Stats
    /// other than the update count restart from zero.
    pub fn restore_state(&mut self, r: &mut StateReader, powered: bool) -> anyhow::Result<()> {
        let mut fresh = ParallelEpd::new(self.geom);
        fresh.frame = self.frame.clone();
        *self = fresh;
        read_f32s_into(r, &mut self.state)?;
        r.fill_u8(&mut self.pixels)?;
        self.stats.updates = r.u64()?;
        if powered {
            r.fill_u8(&mut self.last_dir)?;
            for v in [&mut self.powered, &mut self.oe, &mut self.spv, &mut self.ckv, &mut self.le] {
                *v = r.bool()?;
            }
            self.stats.output_enabled = self.oe;
            self.last_scan_end_ns = r.u64()?;
        }
        let mut f = self.frame.lock();
        f.width = self.geom.width;
        f.height = self.geom.height;
        f.pixels = self.pixels.clone();
        f.generation += 1;
        Ok(())
    }

    /// High voltages (source/gate rails, VCOM) on or off.
    pub fn set_power(&mut self, now_ns: u64, on: bool) {
        if on == self.powered {
            return;
        }
        if !on {
            // The scan (if any) can't drive anything more; close it and the update.
            if self.in_frame {
                self.in_frame = false;
                if self.scan.rows > 0 {
                    self.finish_scan(now_ns);
                }
            }
            self.finish_update(now_ns);
            // Hysteresis relaxes while undriven.
            self.last_dir.fill(0);
        }
        self.powered = on;
    }

    pub fn set_output_enable(&mut self, _now_ns: u64, on: bool) {
        self.oe = on;
    }

    /// Row-control pin levels (SPV/STV, CKV, LE). Call on any change; only edges matter.
    pub fn set_row_pins(&mut self, now_ns: u64, spv: bool, ckv: bool, le: bool) {
        if self.le && !le {
            self.stats.latches += 1;
        }
        if ckv && !self.ckv && !spv {
            // The gate driver samples the (active-low) start pulse on CKV rising.
            self.start_scan(now_ns);
        }
        self.spv = spv;
        self.ckv = ckv;
        self.le = le;
    }

    /// One completed LCD_CAM transfer: the row after the previous one in this scan.
    pub fn bus_transfer(&mut self, now_ns: u64, data: &[u8]) {
        if !self.in_frame || data.len() < self.geom.row_bytes || self.row >= self.geom.height {
            self.stats.ignored_transfers += 1;
            return;
        }
        let y = self.row;
        self.row += 1;
        self.scan.rows += 1;
        self.stats.rows += 1;
        if self.powered && self.oe {
            self.scan.driven_rows += 1;
            self.drive_row(y, &data[..self.geom.row_bytes]);
        }
        if self.row == self.geom.height {
            self.in_frame = false;
            self.finish_scan(now_ns);
        }
    }

    /// Close an update that ended with a neutral scan and has been idle long enough.
    pub fn poll(&mut self, now_ns: u64) {
        if let Some(u) = &self.update
            && u.last_was_neutral
            && !self.in_frame
            && now_ns.saturating_sub(u.last_end_ns) >= UPDATE_IDLE_GAP_NS
        {
            self.finish_update(now_ns);
        }
    }

    fn start_scan(&mut self, now_ns: u64) {
        if self.in_frame && self.scan.rows > 0 {
            self.finish_scan(now_ns);
        }
        self.in_frame = true;
        self.row = 0;
        self.scan = ScanAcc { start_ns: now_ns, ..Default::default() };
    }

    fn drive_row(&mut self, y: usize, data: &[u8]) {
        let w = self.geom.width;
        let base = y * w;
        let state = &mut self.state[base..base + w];
        let last = &mut self.last_dir[base..base + w];
        let pix = &mut self.pixels[base..base + w];
        let (mut black, mut white) = (0u32, 0u32);
        for (k, &b) in data.iter().enumerate() {
            let [nb, nw] = CODE_COUNTS[b as usize];
            if nb | nw == 0 {
                continue;
            }
            black += nb as u32;
            white += nw as u32;
            for m in 0..4 {
                let code = (b >> (6 - 2 * m)) & 3;
                if code != PUSH_BLACK && code != PUSH_WHITE {
                    continue;
                }
                let col = 4 * k + m;
                if col >= w {
                    continue;
                }
                let x = if self.geom.mirror_x { w - 1 - col } else { col };
                apply_push(&mut state[x], &mut last[x], code);
                pix[x] = to_pixel(state[x]);
            }
        }
        if black | white != 0 {
            self.dirty[y] = true;
            self.scan.pushed = true;
            self.scan.black += black as u64;
            self.scan.white += white as u64;
        }
    }

    fn finish_scan(&mut self, now_ns: u64) {
        let (w, h) = (self.geom.width, self.geom.height);
        self.stats.frames_scanned += 1;
        if self.scan.rows < h {
            self.stats.incomplete_frames += 1;
        }
        if self.scan.pushed {
            let mut f = self.frame.lock();
            if f.pixels.len() != w * h {
                f.width = w;
                f.height = h;
                f.pixels = self.pixels.clone();
            } else {
                for (y, d) in self.dirty.iter_mut().enumerate() {
                    if std::mem::take(d) {
                        f.pixels[y * w..(y + 1) * w].copy_from_slice(&self.pixels[y * w..(y + 1) * w]);
                    }
                }
            }
            f.generation += 1;
            drop(f);
            self.dirty.fill(false);
        }
        let start_ns = self.scan.start_ns;
        if self.scan.driven_rows > 0 {
            self.stats.frames_driven += 1;
            self.account_scan(start_ns, now_ns);
        }
        self.last_scan_end_ns = now_ns;
    }

    /// Group driven scans into updates and classify them.
    fn account_scan(&mut self, start_ns: u64, end_ns: u64) {
        let total = (self.geom.width * self.geom.height) as u64;
        let (b, wh) = (self.scan.black, self.scan.white);
        let neutral = b + wh == 0;
        let clear = (b == total && wh == 0) || (wh == total && b == 0);
        if let Some(u) = &self.update
            && u.last_was_neutral
            && start_ns.saturating_sub(u.last_end_ns) >= UPDATE_IDLE_GAP_NS
        {
            let at = u.last_end_ns;
            self.finish_update(at);
        }
        if self.update.is_none() {
            if neutral {
                return;
            }
            self.update = Some(UpdateAcc {
                start_ns,
                last_end_ns: end_ns,
                frames: 0,
                clear: 0,
                data: 0,
                neutral: 0,
                full_coverage: false,
                last_was_neutral: false,
            });
        }
        let u = self.update.as_mut().unwrap();
        u.frames += 1;
        u.last_end_ns = end_ns;
        u.last_was_neutral = neutral;
        if neutral {
            u.neutral += 1;
        } else if clear {
            u.clear += 1;
        } else {
            u.data += 1;
            u.full_coverage |= b + wh == total;
        }
    }

    fn finish_update(&mut self, now_ns: u64) {
        let Some(u) = self.update.take() else { return };
        let kind = if u.data == 0 {
            UpdateKind::Clear
        } else if u.clear > 0 || u.full_coverage || u.data >= 5 {
            UpdateKind::Full
        } else {
            UpdateKind::Partial
        };
        let end_ns = if u.last_was_neutral { u.last_end_ns } else { now_ns.max(u.last_end_ns) };
        self.stats.updates += 1;
        self.stats.last_update = Some(UpdateInfo {
            kind,
            start_ns: u.start_ns,
            end_ns,
            frames: u.frames,
            clear_frames: u.clear,
            data_frames: u.data,
            neutral_frames: u.neutral,
        });
        log::debug!("parallel_epd: update {:?}", self.stats.last_update);
    }
}

#[cfg(test)]
mod tests;
