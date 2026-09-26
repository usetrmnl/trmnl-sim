//! Tests drive the panel with the exact pin/bus sequences FastEPD produces for the
//! TRMNL X (EPDiyV7RowControl + bbepWriteRow), with row data built by ports of
//! FastEPD's own encoders (LUTB_16/LUTW_16, pGrayUpper/pGrayLower, MIRROR_X).

use super::*;

const W: usize = 1872;
const H: usize = 1404;
const RB: usize = W / 4;
const PAD: usize = 44;
const US: u64 = 1_000;

/// trmnl-firmware src/display.cpp `u8_graytable`: 16 levels x 9 passes, 1 = black, 2 = white.
#[rustfmt::skip]
const GRAYTABLE: [u8; 144] = [
/* 0 */  0, 0, 0, 0, 0, 0, 1, 1, 1,
/* 1 */  0, 0, 1, 1, 1, 2, 2, 1, 1,
/* 2 */  0, 0, 0, 0, 1, 2, 2, 1, 1,
/* 3 */  1, 1, 2, 2, 1, 1, 1, 1, 2,
/* 4 */  0, 0, 0, 1, 2, 1, 1, 1, 2,
/* 5 */  1, 2, 2, 2, 2, 1, 1, 1, 2,
/* 6 */  0, 0, 1, 1, 2, 2, 1, 1, 2,
/* 7 */  0, 1, 1, 2, 1, 1, 2, 1, 2,
/* 8 */  0, 1, 1, 1, 2, 1, 2, 1, 2,
/* 9 */  0, 1, 1, 1, 1, 2, 2, 1, 2,
/* 10 */ 1, 1, 1, 2, 1, 1, 1, 2, 2,
/* 11 */ 0, 0, 1, 2, 1, 1, 1, 2, 2,
/* 12 */ 0, 0, 0, 1, 2, 1, 1, 2, 2,
/* 13 */ 0, 0, 0, 0, 1, 2, 1, 2, 2,
/* 14 */ 0, 1, 1, 1, 2, 2, 2, 2, 2,
/* 15 */ 0, 0, 0, 0, 0, 0, 0, 0, 2,
];
const PASSES: usize = 9;

/// Emulates the firmware side: GPIO row control + LCD_CAM transfers, with guest time.
struct Driver {
    epd: ParallelEpd,
    t: u64,
    spv: bool,
    ckv: bool,
    le: bool,
    dma: Vec<u8>,
}

impl Driver {
    fn new() -> Self {
        Driver {
            epd: ParallelEpd::new(PanelGeometry::TRMNL_X),
            t: 0,
            spv: false,
            ckv: false,
            le: false,
            dma: vec![0; 2 * RB + PAD],
        }
    }
    fn pins(&mut self, spv: bool, ckv: bool, le: bool, delay_us: u64) {
        (self.spv, self.ckv, self.le) = (spv, ckv, le);
        self.epd.set_row_pins(self.t, spv, ckv, le);
        self.t += delay_us * US;
    }
    fn ckv(&mut self, v: bool, d: u64) {
        self.pins(self.spv, v, self.le, d)
    }
    fn spv(&mut self, v: bool, d: u64) {
        self.pins(v, self.ckv, self.le, d)
    }
    fn le(&mut self, v: bool) {
        self.pins(self.spv, self.ckv, v, 0)
    }
    /// EPDiyV7EinkPower
    fn power(&mut self, on: bool) {
        if on {
            self.spv(true, 0);
            self.t += 3_000 * US;
            self.epd.set_power(self.t, true);
        } else {
            self.epd.set_power(self.t, false);
            self.spv(false, 1_000);
        }
    }
    /// EPDiyV7RowControl(ROW_START)
    fn row_start(&mut self) {
        self.ckv(true, 7);
        self.spv(false, 10);
        self.ckv(false, 0);
        self.ckv(true, 8);
        self.spv(true, 10);
        self.ckv(false, 0);
        self.ckv(true, 10);
        self.ckv(false, 0);
        self.ckv(true, 10);
        self.ckv(false, 0);
        self.ckv(true, 0);
    }
    /// bbepWriteRow: optional ROW_STEP, CKV on, one 512-byte transfer from the
    /// ping-pong DMA buffer (the 44 padding bytes are whatever follows the row).
    fn write_row(&mut self, off: usize, step: bool) {
        if step {
            self.ckv(false, 0);
            self.le(true);
            self.le(false);
        }
        self.ckv(true, 0);
        let t = self.t;
        let (epd, dma) = (&mut self.epd, &self.dma);
        epd.bus_transfer(t, &dma[off..off + RB + PAD]);
        self.t += 10 * US;
    }
    /// One scan: `fill(row, buf)` writes the 468 row bytes.
    fn scan(&mut self, mut fill: impl FnMut(usize, &mut [u8])) {
        self.row_start();
        let mut off = 0;
        for i in 0..H {
            fill(i, &mut self.dma[off..off + RB]);
            self.write_row(off, i != 0);
            off ^= RB;
        }
        self.t += 230 * US;
    }
    /// bbepClear(val, count) over the whole panel.
    fn clear(&mut self, val: u8, count: usize) {
        for _ in 0..count {
            self.scan(|_, d| d.fill(val));
        }
    }
    fn clear_fast(&mut self) {
        self.clear(0x55, 8);
        self.clear(0xaa, 8);
    }
    fn generation(&self) -> u64 {
        self.epd.frame().lock().generation
    }
    fn pixels(&self) -> Vec<u8> {
        self.epd.frame().lock().pixels.clone()
    }
}

// ---- Ports of FastEPD's encoders (MIRROR_X paths) ----

/// LUTW_16 / LUTB_16 (bbepSetPanelType), MIRROR_X variant, byte-swapped.
fn luts() -> (Vec<u16>, Vec<u16>, Vec<u16>) {
    let (mut lw, mut lb, mut lbw) = (vec![0u16; 256], vec![0u16; 256], vec![0u16; 256]);
    for i in 0..256usize {
        let (mut uw, mut ub, mut ubw) = (0u16, 0u16, 0u16);
        for j in 0..8 {
            let (w, b, bw) = if i & (1 << (7 - j)) == 0 { (2, 1, 1) } else { (3, 3, 2) };
            uw |= w << (j * 2);
            ub |= b << (j * 2);
            ubw |= bw << (j * 2);
        }
        lw[i] = uw.swap_bytes();
        lb[i] = ub.swap_bytes();
        lbw[i] = ubw.swap_bytes();
    }
    (lw, lb, lbw)
}

fn put16(d: &mut [u8], v: u16) {
    d[..2].copy_from_slice(&v.to_le_bytes()); // ESP32-S3 is little-endian
}

/// 1bpp image (FastEPD layout: MSB = leftmost, 1 = white) from a predicate.
fn image_1bpp(black: impl Fn(usize, usize) -> bool) -> Vec<u8> {
    let mut img = vec![0xffu8; W / 8 * H];
    for y in 0..H {
        for x in 0..W {
            if black(x, y) {
                img[y * W / 8 + x / 8] &= !(0x80 >> (x & 7));
            }
        }
    }
    img
}

/// bbepFullUpdate 1bpp: pTemp built with LUTB_16 (after a clear).
fn encode_1bpp_full(img: &[u8], lut: &[u16]) -> Vec<u8> {
    let mut tmp = vec![0u8; RB * H];
    for i in 0..H {
        let s = &img[i * W / 8..(i + 1) * W / 8];
        let d = &mut tmp[i * RB..(i + 1) * RB];
        let mut si = W / 8;
        for n in (0..RB).step_by(4) {
            si -= 1;
            let dram2 = s[si];
            si -= 1;
            let dram1 = s[si];
            put16(&mut d[n..], lut[dram2 as usize]);
            put16(&mut d[n + 2..], lut[dram1 as usize]);
        }
    }
    tmp
}

/// bbepPartialUpdate 1bpp diff rows.
fn encode_1bpp_partial(prev: &[u8], cur: &[u8], lw: &[u16], lb: &[u16]) -> Vec<u8> {
    let mut tmp = vec![0u8; RB * H];
    for i in 0..H {
        let (p, c) = (&prev[i * W / 8..(i + 1) * W / 8], &cur[i * W / 8..(i + 1) * W / 8]);
        let d = &mut tmp[i * RB..(i + 1) * RB];
        let mut si = W / 8;
        let mut di = 0;
        for _ in 0..W / 16 {
            for half in 0..2 {
                si -= 1;
                let (cu, pr) = (c[si], p[si]);
                let diffw = pr & !cu;
                let diffb = !pr & cu;
                put16(&mut d[di + 2 * half..], lw[diffw as usize] & lb[diffb as usize]);
            }
            di += 4;
        }
    }
    tmp
}

/// bbepSetCustomMatrix, MIRROR_X: (pGrayUpper, pGrayLower) per pass.
fn gray_tables() -> (Vec<u8>, Vec<u8>) {
    let (mut up, mut lo) = (vec![0u8; 256 * PASSES], vec![0u8; 256 * PASSES]);
    let m = &GRAYTABLE;
    for j in 0..PASSES {
        for i in 0..256 {
            let v = (m[(i & 0xf) * PASSES + j] << 2) | m[(i >> 4) * PASSES + j];
            lo[j * 256 + i] = v;
            up[j * 256 + i] = v << 4;
        }
    }
    (up, lo)
}

/// 4bpp image: high nibble = left pixel, 0 = black .. 15 = white.
fn image_4bpp(level: impl Fn(usize, usize) -> u8) -> Vec<u8> {
    let mut img = vec![0u8; W / 2 * H];
    for y in 0..H {
        for x in (0..W).step_by(2) {
            img[y * W / 2 + x / 2] = (level(x, y) << 4) | level(x + 1, y);
        }
    }
    img
}

fn encode_4bpp_row(img: &[u8], y: usize, pass: usize, up: &[u8], lo: &[u8], d: &mut [u8]) {
    let (u, l) = (&up[pass * 256..], &lo[pass * 256..]);
    let row = &img[y * W / 2..(y + 1) * W / 2];
    let mut s = W / 2 - 8;
    for n in (0..RB).step_by(4) {
        d[n] = u[row[s + 7] as usize] | l[row[s + 6] as usize];
        d[n + 1] = u[row[s + 5] as usize] | l[row[s + 4] as usize];
        d[n + 2] = u[row[s + 3] as usize] | l[row[s + 2] as usize];
        d[n + 3] = u[row[s + 1] as usize] | l[row[s] as usize];
        s = s.wrapping_sub(8);
    }
}

// ---- Firmware update sequences ----

fn full_update_1bpp(dr: &mut Driver, img: &[u8], slow: bool) {
    let (_, lb, _) = luts();
    dr.power(true);
    dr.clear_fast();
    if slow {
        dr.clear_fast();
    }
    dr.clear(0x00, 1);
    let tmp = encode_1bpp_full(img, &lb);
    for _ in 0..3 {
        dr.scan(|i, d| d.copy_from_slice(&tmp[i * RB..(i + 1) * RB]));
    }
    dr.clear(0x00, 1);
    dr.power(false);
}

fn full_update_4bpp(dr: &mut Driver, img: &[u8]) {
    let (up, lo) = gray_tables();
    dr.power(true);
    dr.clear_fast();
    dr.clear(0x00, 1);
    for pass in 0..PASSES {
        dr.scan(|i, d| encode_4bpp_row(img, i, pass, &up, &lo, d));
    }
    dr.clear(0x00, 1);
    dr.power(false);
}

fn partial_update_1bpp(dr: &mut Driver, prev: &[u8], cur: &[u8]) {
    let (lw, lb, _) = luts();
    let tmp = encode_1bpp_partial(prev, cur, &lw, &lb);
    dr.power(true);
    for _ in 0..3 {
        dr.scan(|i, d| d.copy_from_slice(&tmp[i * RB..(i + 1) * RB]));
    }
    dr.clear(0x00, 1);
    dr.power(false);
}

fn pattern_a(x: usize, y: usize) -> bool {
    (x < 200 && y < 100) || ((x / 37) + (y / 29)).is_multiple_of(3) || (x * 7 + y * 3) % 11 < 2
}
fn pattern_b(x: usize, y: usize) -> bool {
    (x > W - 300 && y > H - 150) || ((x / 53) + (y / 41)).is_multiple_of(2)
}

// ---- Physics calibration (single pixel) ----

fn replay(seq: &[u8], d0: f32, last0: u8) -> (f32, u8) {
    let (mut d, mut last) = (d0, last0);
    for &c in seq {
        apply_push(&mut d, &mut last, c);
    }
    (d, last)
}

fn cleared_white() -> (f32, u8) {
    let mut seq = vec![1u8; 8];
    seq.extend([2u8; 8]);
    replay(&seq, 0.0, 0)
}

fn gray_levels() -> Vec<f32> {
    let (w, l) = cleared_white();
    (0..16).map(|lv| replay(&GRAYTABLE[lv * PASSES..(lv + 1) * PASSES], w, l).0).collect()
}

#[test]
fn physics_clear_and_1bpp_endpoints() {
    let (b8, _) = replay(&[1; 8], 0.0, 0);
    assert!(b8 > 0.99, "8 darken frames: {b8}");
    let (w, _) = cleared_white();
    assert!(w < 0.01, "8 lighten frames: {w}");
    // In the firmware flow the 1bpp pushes follow the clear's lighten phase
    // (a reversal), and white pushes of a partial update follow black ones.
    let (w, wl) = cleared_white();
    let (b3, bl) = replay(&[1; 3], w, wl);
    assert!(b3 > 0.9, "3 black pushes after clear: {b3}");
    let (w3, _) = replay(&[2; 3], b3, bl);
    assert!(w3 < 0.1, "3 white pushes after black: {w3}");
    let (b3, _) = replay(&[1; 3], 0.0, 0);
    assert!(b3 > 0.95, "3 black pushes from rest: {b3}");
}

#[test]
fn physics_graytable_is_monotonic_ramp() {
    let lv = gray_levels();
    eprintln!("gray levels (darkness): {:?}", lv.iter().map(|d| format!("{d:.3}")).collect::<Vec<_>>());
    eprintln!("gray levels (pixel): {:?}", lv.iter().map(|&d| to_pixel(d)).collect::<Vec<_>>());
    assert!(lv[0] > 0.85, "level 0 should be black: {}", lv[0]);
    assert!(lv[15] < 0.02, "level 15 should be white: {}", lv[15]);
    for i in 0..15 {
        assert!(lv[i] - lv[i + 1] >= 0.01, "level {i} ({}) not darker than {} ({})", lv[i], i + 1, lv[i + 1]);
    }
    let px: Vec<u8> = lv.iter().map(|&d| to_pixel(d)).collect();
    assert!(px.windows(2).all(|p| p[0] > p[1]), "{px:?}");
}

// ---- Full-panel tests ----

#[test]
fn full_1bpp_update_reproduces_pattern() {
    let mut dr = Driver::new();
    let img = image_1bpp(pattern_a);
    let g0 = dr.generation();
    full_update_1bpp(&mut dr, &img, false);
    // 16 clear + 3 data scans pushed pixels; the two neutral scans didn't.
    assert_eq!(dr.generation() - g0, 19);
    let px = dr.pixels();
    let mut wrong = 0;
    for y in 0..H {
        for x in 0..W {
            let p = px[y * W + x];
            let ok = if pattern_a(x, y) { p >= 220 } else { p <= 5 };
            if !ok {
                wrong += 1;
                assert!(wrong > 10, "({x},{y}) = {p}, want black={}", pattern_a(x, y));
            }
        }
    }
    assert_eq!(wrong, 0);
    // Orientation: the pattern is far from mirror-symmetric, so a MIRROR_X mix-up
    // would have failed above. The solid block sits top-left.
    let asym = (0..H)
        .step_by(7)
        .flat_map(|y| (0..W).map(move |x| (x, y)))
        .filter(|&(x, y)| pattern_a(x, y) != pattern_a(W - 1 - x, y))
        .count();
    assert!(asym > 50_000, "{asym}");
    assert!(px[50 * W + 100] >= 220);
    let s = dr.epd.stats();
    assert_eq!(s.frames_scanned, 8 + 8 + 1 + 3 + 1);
    assert_eq!(s.frames_driven, s.frames_scanned);
    assert_eq!(s.incomplete_frames, 0);
    assert_eq!(s.ignored_transfers, 0);
    assert_eq!(s.rows, s.frames_scanned * H as u64);
    assert_eq!(s.latches, s.frames_scanned * (H as u64 - 1));
    assert_eq!(s.updates, 1);
    let u = s.last_update.unwrap();
    assert_eq!((u.kind, u.frames, u.clear_frames, u.data_frames, u.neutral_frames), (UpdateKind::Full, 21, 16, 3, 2));
    assert!(!s.update_in_progress && !s.powered);
}

#[test]
fn full_4bpp_graytable_ramp_is_monotonic() {
    let mut dr = Driver::new();
    // Paint something first so the clear has to work.
    full_update_1bpp(&mut dr, &image_1bpp(pattern_b), false);
    let band = |x: usize| (x * 16 / W) as u8;
    let img = image_4bpp(|x, _| band(x));
    full_update_4bpp(&mut dr, &img);
    let px = dr.pixels();
    let mut levels = Vec::new();
    for lv in 0..16 {
        let x0 = lv * W / 16;
        let x1 = (lv + 1) * W / 16;
        let v = px[700 * W + (x0 + x1) / 2];
        // Uniform across the band and down the panel.
        for y in [0, 351, 1403] {
            for x in [x0 + 1, x1 - 1] {
                assert_eq!(px[y * W + x], v, "level {lv} not uniform at ({x},{y})");
            }
        }
        levels.push(v);
    }
    eprintln!("4bpp panel ramp (pixel darkness): {levels:?}");
    assert!(levels[0] >= 220, "{levels:?}");
    assert!(levels[15] <= 5, "{levels:?}");
    assert!(levels.windows(2).all(|p| p[0] > p[1]), "not monotonic: {levels:?}");
    let expect: Vec<u8> = gray_levels().iter().map(|&d| to_pixel(d)).collect();
    assert_eq!(levels, expect);
    let u = dr.epd.stats().last_update.unwrap();
    assert_eq!((u.kind, u.clear_frames, u.data_frames), (UpdateKind::Full, 16, 9));
}

#[test]
fn partial_update_changes_only_differing_pixels() {
    let mut dr = Driver::new();
    let a = image_1bpp(pattern_a);
    let b = image_1bpp(pattern_b);
    full_update_1bpp(&mut dr, &a, false);
    let before = dr.pixels();
    let g = dr.generation();
    partial_update_1bpp(&mut dr, &a, &b);
    assert_eq!(dr.generation() - g, 3);
    let after = dr.pixels();
    let mut changed = 0;
    for y in 0..H {
        for x in 0..W {
            let i = y * W + x;
            let (pa, pb) = (pattern_a(x, y), pattern_b(x, y));
            if pa == pb {
                assert_eq!(after[i], before[i], "unchanged pixel ({x},{y}) moved");
            } else {
                changed += 1;
                assert_eq!(after[i] >= 128, pb, "({x},{y}) = {} want black={pb}", after[i]);
            }
        }
    }
    assert!(changed > 100_000);
    let s = dr.epd.stats();
    assert_eq!(s.updates, 2);
    let u = s.last_update.unwrap();
    assert_eq!((u.kind, u.clear_frames, u.data_frames, u.neutral_frames), (UpdateKind::Partial, 0, 3, 1));
}

#[test]
fn nothing_changes_while_powered_off_or_oe_low() {
    let mut dr = Driver::new();
    let g = dr.generation();
    dr.clear(0x55, 2); // never powered
    assert_eq!(dr.generation(), g);
    assert!(dr.pixels().iter().all(|&p| p == 0));
    dr.power(true);
    dr.epd.set_output_enable(dr.t, false);
    dr.clear(0x55, 2);
    assert_eq!(dr.generation(), g);
    dr.epd.set_output_enable(dr.t, true);
    dr.clear(0x55, 1);
    assert_eq!(dr.generation(), g + 1);
    dr.power(false);
    dr.clear(0xaa, 3);
    assert_eq!(dr.generation(), g + 1);
    assert!(dr.pixels().iter().all(|&p| p > 100));
    let s = dr.epd.stats();
    assert_eq!(s.frames_scanned, 8);
    assert_eq!(s.frames_driven, 1);
    assert_eq!(s.updates, 1);
    assert_eq!(s.last_update.unwrap().kind, UpdateKind::Clear);
}

#[test]
fn generation_advances_every_push_scan_and_shows_flash() {
    let mut dr = Driver::new();
    dr.power(true);
    let mut dark = Vec::new();
    for val in [0x55u8; 8].into_iter().chain([0xaa; 8]) {
        let g = dr.generation();
        dr.clear(val, 1);
        assert_eq!(dr.generation(), g + 1);
        dark.push(dr.pixels()[5 * W + 5]);
    }
    // Animation: darkens monotonically to black, then lightens back to white.
    assert!(dark[..8].windows(2).all(|p| p[0] <= p[1]) && dark[7] == 255, "{dark:?}");
    assert!(dark[8..].windows(2).all(|p| p[0] >= p[1]) && dark[15] == 0, "{dark:?}");
    dr.clear(0x00, 1);
    let g = dr.generation();
    dr.clear(0x00, 1);
    assert_eq!(dr.generation(), g, "neutral scans don't touch the frame");
    // Update still open (power on, no idle gap yet); poll closes it after the gap.
    assert!(dr.epd.stats().update_in_progress);
    dr.epd.poll(dr.t + UPDATE_IDLE_GAP_NS);
    let s = dr.epd.stats();
    assert!(!s.update_in_progress);
    assert_eq!(s.last_update.unwrap().kind, UpdateKind::Clear);
}

#[test]
fn keep_on_updates_split_on_idle_gap() {
    let mut dr = Driver::new();
    let (_, lb, _) = luts();
    let tmp = encode_1bpp_full(&image_1bpp(pattern_a), &lb);
    dr.power(true);
    dr.clear_fast();
    dr.clear(0x00, 1);
    for _ in 0..3 {
        dr.scan(|i, d| d.copy_from_slice(&tmp[i * RB..(i + 1) * RB]));
    }
    dr.clear(0x00, 1);
    dr.t += 100_000 * US; // firmware renders the next screen, rails stay up
    dr.clear(0x55, 1);
    let s = dr.epd.stats();
    assert_eq!(s.updates, 1);
    assert_eq!(s.last_update.unwrap().kind, UpdateKind::Full);
    assert!(s.update_in_progress);
}

#[test]
fn robust_against_odd_input() {
    let mut dr = Driver::new();
    dr.power(true);
    // Transfers before any start pulse, short ones, and too many rows.
    dr.epd.bus_transfer(0, &[0x55; 512]);
    dr.epd.bus_transfer(0, &[]);
    dr.row_start();
    dr.epd.bus_transfer(dr.t, &[0x55; 10]);
    for _ in 0..H + 5 {
        dr.epd.bus_transfer(dr.t, &[0x55; RB]);
    }
    let s = dr.epd.stats();
    assert_eq!(s.ignored_transfers, 3 + 5);
    assert_eq!(s.frames_scanned, 1);
    // Random pin wiggles and data never panic; an interrupted scan is counted.
    let mut x = 0x1234_5678u32;
    let mut rnd = || {
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        x
    };
    let mut buf = vec![0u8; 600];
    for i in 0..20_000u64 {
        let r = rnd();
        if r & 3 == 0 {
            let n = (rnd() % 600) as usize;
            buf.iter_mut().for_each(|b| *b = rnd() as u8);
            dr.epd.bus_transfer(i, &buf[..n]);
        } else if r & 0xff == 1 {
            dr.epd.set_power(i, rnd() & 1 == 0);
        } else {
            dr.epd.set_row_pins(i, r & 4 != 0, r & 8 != 0, r & 16 != 0);
        }
        dr.epd.poll(i);
    }
    let _ = dr.epd.stats();
    // A tiny panel with non-mirrored geometry and a wide row still works.
    let mut e = ParallelEpd::new(PanelGeometry { width: 6, height: 2, mirror_x: false, row_bytes: 2 });
    e.set_power(0, true);
    e.set_row_pins(0, false, false, false);
    e.set_row_pins(1, false, true, false);
    e.bus_transfer(2, &[0b01_10_00_01, 0b01_01_01_01]);
    e.bus_transfer(3, &[0, 0]);
    let f = e.frame();
    let f = f.lock();
    assert_eq!(f.generation, 1);
    assert_eq!(&f.pixels[..6], &[to_pixel(PUSH_RATE_BLACK), 0, 0, to_pixel(PUSH_RATE_BLACK), 163, 163]);
}

/// cargo test --release --no-default-features parallel_epd::tests::bench -- --ignored --nocapture
#[test]
#[ignore]
fn bench_full_updates() {
    let img4 = image_4bpp(|x, y| ((x / 117 + y / 88) % 16) as u8);
    let img1 = image_1bpp(pattern_a);
    let mut dr = Driver::new();
    // Warm up.
    full_update_1bpp(&mut dr, &img1, false);
    for (name, which) in
        [("4bpp CLEAR_FAST (27 scans)", 0), ("1bpp CLEAR_FAST (21 scans)", 1), ("1bpp CLEAR_SLOW (37 scans)", 2)]
    {
        let s0 = dr.epd.stats().frames_scanned;
        let t = std::time::Instant::now();
        match which {
            0 => full_update_4bpp(&mut dr, &img4),
            1 => full_update_1bpp(&mut dr, &img1, false),
            _ => full_update_1bpp(&mut dr, &img1, true),
        }
        let el = t.elapsed();
        let n = dr.epd.stats().frames_scanned - s0;
        eprintln!(
            "{name}: {:.1} ms total, {:.2} ms/scan ({n} scans, incl. driver-side encoding)",
            el.as_secs_f64() * 1e3,
            el.as_secs_f64() * 1e3 / n as f64
        );
    }
    // Device-only cost: pre-encoded rows, no driver work besides pin calls.
    let rows: Vec<Vec<u8>> = (0..PASSES)
        .map(|p| {
            let (up, lo) = gray_tables();
            let mut v = vec![0u8; RB * H + PAD];
            for y in 0..H {
                encode_4bpp_row(&img4, y, p, &up, &lo, &mut v[y * RB..(y + 1) * RB]);
            }
            v
        })
        .collect();
    let clear: Vec<Vec<u8>> = [0x55u8, 0xaa, 0].iter().map(|&b| vec![b; RB * H + PAD]).collect();
    let mut seq: Vec<&Vec<u8>> = Vec::new();
    seq.extend(std::iter::repeat_n(&clear[0], 8));
    seq.extend(std::iter::repeat_n(&clear[1], 8));
    seq.push(&clear[2]);
    seq.extend(rows.iter());
    seq.push(&clear[2]);
    let mut e = ParallelEpd::new(PanelGeometry::TRMNL_X);
    e.set_power(0, true);
    let t = std::time::Instant::now();
    let mut now = 0;
    for buf in &seq {
        e.set_row_pins(now, true, true, false);
        e.set_row_pins(now, false, false, false);
        e.set_row_pins(now, false, true, false);
        e.set_row_pins(now, true, true, false);
        for y in 0..H {
            now += 10_000;
            e.set_row_pins(now, true, false, true);
            e.set_row_pins(now, true, false, false);
            e.set_row_pins(now, true, true, false);
            e.bus_transfer(now, &buf[y * RB..y * RB + RB + PAD]);
        }
    }
    let el = t.elapsed();
    eprintln!(
        "device only, 4bpp full update ({} scans): {:.1} ms total, {:.2} ms/scan, {:.0} ns/row",
        seq.len(),
        el.as_secs_f64() * 1e3,
        el.as_secs_f64() * 1e3 / seq.len() as f64,
        el.as_secs_f64() * 1e9 / (seq.len() * H) as f64
    );
    assert_eq!(e.stats().frames_scanned, seq.len() as u64);
}
