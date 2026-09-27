//! A large panel driven by two controllers side by side (bb_epaper's `BBEP_SPLIT_BUFFER`
//! panels, e.g. the reTerminal E1004's 13.3" Spectra 6): both share SCK, MOSI, D/C, RST
//! and BUSY; each has its own chip select. bb_epaper selects one or both (`CMD_CS1`,
//! `CMD_CS2`, `CMD_CS1_CS2`) per command, so the init sequence, power on/off and refresh
//! reach both controllers while the image goes to each half in turn.
//!
//! The left controller drives columns `0..w/2`, the right one `w/2..w`; the frame shown is
//! the two halves' frames side by side.

use std::sync::Arc;

use parking_lot::Mutex;
use sim_api::{Frame, SharedFrame};

use super::epd::SpiEpd;
use crate::savepoint::{StateReader, StateWriter};

pub struct DualSpiEpd {
    left: Box<dyn SpiEpd>,
    right: Box<dyn SpiEpd>,
    /// The second chip select's level (see [`SpiEpd::set_cs2`]).
    cs2: bool,
    frame: SharedFrame,
    /// Generations of the halves' frames last copied into `frame`.
    shown: (u64, u64),
}

impl DualSpiEpd {
    /// Two controllers of the same kind; `left` answers to the primary CS.
    pub fn new(left: Box<dyn SpiEpd>, right: Box<dyn SpiEpd>) -> Self {
        let (lw, h, color) = {
            let f = left.frame();
            let f = f.lock();
            (f.width, f.height, f.rgb.is_some())
        };
        let rw = right.frame().lock().width;
        let mut frame = Frame::new(lw + rw, h);
        if color {
            frame.rgb = Some(vec![255; (lw + rw) * h * 3]);
        }
        let mut d =
            DualSpiEpd { left, right, cs2: true, frame: Arc::new(Mutex::new(frame)), shown: (u64::MAX, u64::MAX) };
        d.compose();
        d
    }

    /// Copy whichever half changed into the combined frame.
    fn compose(&mut self) {
        let (lf, rf) = (self.left.frame(), self.right.frame());
        let (l, r) = (lf.lock(), rf.lock());
        if (l.generation, r.generation) == self.shown {
            return;
        }
        let mut f = self.frame.lock();
        let f = &mut *f;
        let w = f.width;
        for (half, x0, changed) in
            [(&*l, 0, l.generation != self.shown.0), (&*r, l.width, r.generation != self.shown.1)]
        {
            if !changed {
                continue;
            }
            let hw = half.width;
            for y in 0..f.height.min(half.height) {
                f.pixels[y * w + x0..y * w + x0 + hw].copy_from_slice(&half.pixels[y * hw..(y + 1) * hw]);
                if let (Some(dst), Some(src)) = (f.rgb.as_mut(), half.rgb.as_ref()) {
                    dst[(y * w + x0) * 3..(y * w + x0 + hw) * 3].copy_from_slice(&src[y * hw * 3..(y + 1) * hw * 3]);
                }
            }
        }
        f.generation += 1;
        self.shown = (l.generation, r.generation);
    }
}

impl SpiEpd for DualSpiEpd {
    fn set_cs2(&mut self, cs2: bool) {
        self.cs2 = cs2;
    }
    fn set_pins(&mut self, now: u64, cs: bool, dc: bool, sck: bool, mosi: bool, rst: bool) {
        self.left.set_pins(now, cs, dc, sck, mosi, rst);
        self.right.set_pins(now, self.cs2, dc, sck, mosi, rst);
    }
    fn spi_bytes(&mut self, now: u64, data: &[u8]) {
        // Each controller ignores bytes clocked while its CS is high.
        self.left.spi_bytes(now, data);
        self.right.spi_bytes(now, data);
        self.compose();
    }
    fn mosi_out(&self) -> Option<bool> {
        self.left.mosi_out().or(self.right.mosi_out())
    }
    fn busy(&self, now: u64) -> bool {
        // One BUSY line: either controller holding it busy is enough.
        self.left.busy(now) || self.right.busy(now)
    }
    fn busy_level(&self) -> bool {
        self.left.busy_level()
    }
    fn next_event(&self, now: u64) -> Option<u64> {
        match (self.left.next_event(now), self.right.next_event(now)) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }
    fn update(&mut self, now: u64) {
        self.left.update(now);
        self.right.update(now);
        self.compose();
    }
    fn frame(&self) -> SharedFrame {
        self.frame.clone()
    }
    fn refresh_count(&self) -> u64 {
        // A refresh of the panel is one DRF to both controllers.
        self.left.refresh_count().max(self.right.refresh_count())
    }
    fn is_color(&self) -> bool {
        self.left.is_color()
    }
    fn set_flashing(&mut self, on: bool) {
        self.left.set_flashing(on);
        self.right.set_flashing(on);
    }
    fn set_busy_stuck(&mut self, stuck: bool) {
        self.left.set_busy_stuck(stuck);
        self.right.set_busy_stuck(stuck);
    }
    fn save_state(&self, w: &mut StateWriter, powered: bool) {
        if powered {
            w.bool(self.cs2);
        }
        w.section(|w| self.left.save_state(w, powered));
        w.section(|w| self.right.save_state(w, powered));
    }
    fn restore_state(&mut self, r: &mut StateReader, powered: bool) -> anyhow::Result<()> {
        self.cs2 = if powered { r.bool()? } else { true };
        r.section(|r| self.left.restore_state(r, powered))?;
        r.section(|r| self.right.restore_state(r, powered))?;
        self.shown = (u64::MAX, u64::MAX);
        self.compose();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devices::uc8179::{ColorPanel, Uc8179};

    const MS: u64 = 1_000_000;

    fn panel() -> DualSpiEpd {
        DualSpiEpd::new(
            Box::new(Uc8179::new_color_sized(0, ColorPanel::Spectra6, 8, 4)),
            Box::new(Uc8179::new_color_sized(0, ColorPanel::Spectra6, 8, 4)),
        )
    }

    /// bb_epaper's bbepWriteCmdData with CS1 and/or CS2 low.
    fn cmd(p: &mut DualSpiEpd, cs1: bool, cs2: bool, c: u8, data: &[u8]) {
        p.set_cs2(!cs2);
        p.set_pins(0, !cs1, false, false, false, true);
        p.spi_bytes(0, &[c]);
        p.set_pins(0, !cs1, true, false, false, true);
        p.spi_bytes(0, data);
        p.set_cs2(true);
        p.set_pins(0, true, true, false, false, true);
    }

    #[test]
    fn each_half_shows_what_its_controller_got() {
        let mut p = panel();
        assert_eq!(p.frame().lock().width, 16);
        cmd(&mut p, true, true, 0x04, &[]);
        cmd(&mut p, true, false, 0x10, &[0x22; 16]); // left half: yellow (code 2)
        cmd(&mut p, false, true, 0x10, &[0x55; 16]); // right half: blue (code 5)
        cmd(&mut p, true, true, 0x12, &[0x01]);
        assert!(p.busy(MS));
        let done = p.next_event(MS).unwrap();
        let mut t = MS;
        while let Some(n) = p.next_event(t) {
            p.update(n);
            t = n;
            if t > 60_000 * MS {
                break;
            }
        }
        assert!(t >= done && !p.busy(t));
        assert_eq!(p.refresh_count(), 1);
        let f = p.frame();
        let f = f.lock();
        let rgb = f.rgb.as_ref().unwrap();
        let at = |x: usize, y: usize| &rgb[(y * 16 + x) * 3..(y * 16 + x) * 3 + 3];
        assert_eq!(at(0, 0), &[255, 255, 0]);
        assert_eq!(at(7, 3), &[255, 255, 0]);
        assert_eq!(at(8, 0), &[0, 0, 255]);
        assert_eq!(at(15, 3), &[0, 0, 255]);
    }

    #[test]
    fn round_trips() {
        let mut p = panel();
        cmd(&mut p, true, false, 0x10, &[0x00; 16]);
        cmd(&mut p, true, true, 0x12, &[0x01]);
        p.update(30_000 * MS);
        for powered in [true, false] {
            let mut w = StateWriter::new();
            p.save_state(&mut w, powered);
            let saved = w.into_bytes();
            let mut q = panel();
            let mut r = StateReader::new(&saved);
            q.restore_state(&mut r, powered).unwrap();
            r.finish().unwrap();
            assert_eq!(q.frame().lock().rgb, p.frame().lock().rgb);
        }
    }
}
