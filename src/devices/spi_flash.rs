//! Generic SPI NOR flash chip (GD25Q32-like). Operations complete instantly.
//! Contents are persisted to a file so NVS/SPIFFS survive simulator restarts.
//!
//! A [`PowerLossTrigger`] cuts the power at a chosen program/erase, optionally leaving it
//! half done (a torn page program or a half-erased sector). The chip then ignores
//! everything until [`SpiFlash::restore_power`]; the SoC notices [`SpiFlash::power_lost`]
//! and the runner power-cycles the device.

use std::path::PathBuf;

use sim_api::{CutPoint, FlashOp};

/// When to cut the power: the `nth` program/erase matching `op` that overlaps `range`.
#[derive(Debug, Clone, PartialEq)]
pub struct PowerLossTrigger {
    pub op: FlashOp,
    /// Byte range `[start, end)`; None = anywhere.
    pub range: Option<(u32, u32)>,
    pub nth: u32,
    pub cut: CutPoint,
    /// What the range is (e.g. "partition nvs"), for messages.
    pub what: String,
}

pub struct SpiFlash {
    pub data: Vec<u8>,
    pub jedec_id: [u8; 3],
    wel: bool,
    status: u32,
    deep_power_down: bool,
    backing: Option<PathBuf>,
    pub dirty: bool,
    /// Page programs / erases executed (with WREN), for statistics.
    pub programs: u64,
    pub erases: u64,
    /// Armed power-loss trigger and how many matching operations it has seen.
    trigger: Option<(PowerLossTrigger, u32)>,
    /// Power was cut (description of the interrupted operation).
    power_lost: Option<String>,
}

impl SpiFlash {
    pub fn new(data: Vec<u8>, backing: Option<PathBuf>) -> Self {
        let size_code = (data.len().trailing_zeros()) as u8; // 4 MB -> 0x16
        SpiFlash {
            data,
            jedec_id: [0xc8, 0x40, size_code],
            wel: false,
            status: 0,
            deep_power_down: false,
            backing,
            dirty: false,
            programs: 0,
            erases: 0,
            trigger: None,
            power_lost: None,
        }
    }

    /// Arm (or with None, disarm) a power-loss trigger. Counting starts now.
    pub fn set_power_loss(&mut self, trigger: Option<PowerLossTrigger>) {
        if self.trigger.as_ref().map(|t| &t.0) != trigger.as_ref() {
            self.trigger = trigger.map(|t| (t, 0));
        }
    }

    /// Set once the trigger fired: what was interrupted.
    pub fn power_lost(&self) -> Option<&str> {
        self.power_lost.as_deref()
    }

    /// Power is back (the device was reset).
    pub fn restore_power(&mut self) {
        self.power_lost = None;
    }

    /// Count an operation against the trigger; returns how much of it to apply if the
    /// power fails now.
    fn check_power_loss(&mut self, op: FlashOp, addr: u32, len: u32) -> Option<CutPoint> {
        let (t, seen) = self.trigger.as_mut()?;
        let (start, end) = (addr as u64, addr as u64 + len as u64);
        let hit =
            (t.op == FlashOp::Any || t.op == op) && t.range.is_none_or(|(a, b)| start < b as u64 && end > a as u64);
        if !hit {
            return None;
        }
        *seen += 1;
        if *seen < t.nth {
            return None;
        }
        let cut = t.cut;
        let what = if t.what.is_empty() { String::new() } else { format!(" in {}", t.what) };
        self.power_lost = Some(format!("{} #{} at {addr:#x} (+{len:#x}){what}, cut {}", op.name(), t.nth, cut.name()));
        self.trigger = None;
        Some(cut)
    }

    /// Write back to the backing file if anything changed.
    pub fn flush(&mut self) -> std::io::Result<()> {
        if self.dirty {
            if let Some(p) = &self.backing {
                std::fs::write(p, &self.data)?;
            }
            self.dirty = false;
        }
        Ok(())
    }

    /// Commit a host-side edit before publishing it to the guest. On failure the live
    /// image and previous backing file remain intact. This bypasses guest flash faults.
    pub fn replace_image(&mut self, data: Vec<u8>) -> std::io::Result<()> {
        use std::io::Write;
        if data.len() != self.data.len() {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "flash size changed"));
        }
        if let Some(path) = &self.backing {
            let path = if path.exists() { path.canonicalize()? } else { path.clone() };
            let parent = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(std::path::Path::new("."));
            let mut temp = tempfile::NamedTempFile::new_in(parent)?;
            if let Ok(meta) = std::fs::metadata(&path) {
                temp.as_file().set_permissions(meta.permissions())?;
            }
            temp.write_all(&data)?;
            temp.as_file().sync_all()?;
            temp.persist(&path).map_err(|e| e.error)?;
        }
        self.data = data;
        self.dirty = false;
        Ok(())
    }

    fn mask(&self, addr: u32) -> usize {
        addr as usize & (self.data.len() - 1)
    }

    pub fn read(&self, addr: u32, out: &mut [u8]) {
        for (i, b) in out.iter_mut().enumerate() {
            *b = self.data[self.mask(addr.wrapping_add(i as u32))];
        }
    }

    /// NOR semantics: programming can only clear bits. Wraps within the 256-byte page.
    pub fn program(&mut self, addr: u32, bytes: &[u8]) {
        if !self.wel {
            log::warn!("flash: program @{addr:#x} without WREN");
            return;
        }
        self.programs += 1;
        log::trace!(target: "flash", "program {addr:#x} +{:#x}", bytes.len());
        let mut bytes = bytes;
        match self.check_power_loss(FlashOp::Program, addr, bytes.len() as u32) {
            None | Some(CutPoint::After) => {}
            Some(CutPoint::Before) => return,
            // The first half of the bytes made it into the cells.
            Some(CutPoint::Torn) => bytes = &bytes[..bytes.len() / 2],
        }
        let page = addr & !0xff;
        for (i, &b) in bytes.iter().enumerate() {
            let a = page | ((addr + i as u32) & 0xff);
            let k = self.mask(a);
            self.data[k] &= b;
        }
        self.wel = false;
        self.dirty = true;
    }

    pub fn erase(&mut self, addr: u32, len: u32) {
        if !self.wel {
            log::warn!("flash: erase @{addr:#x} without WREN");
            return;
        }
        self.erases += 1;
        log::trace!(target: "flash", "erase {addr:#x} +{len:#x}");
        let start = self.mask(addr & !(len - 1));
        let mut end = (start + len as usize).min(self.data.len());
        match self.check_power_loss(FlashOp::Erase, start as u32, (end - start) as u32) {
            None | Some(CutPoint::After) => {}
            Some(CutPoint::Before) => return,
            // Half-erased: the first half of the block is blank, the rest still holds data.
            Some(CutPoint::Torn) => end = start + (end - start) / 2,
        }
        self.data[start..end].fill(0xff);
        self.wel = false;
        self.dirty = true;
    }

    pub fn status(&self) -> u32 {
        self.status & !3 | (self.wel as u32) << 1
    }

    /// Execute a generic SPI transaction: command, optional address, MOSI payload,
    /// returning `miso_len` bytes.
    pub fn transact(&mut self, cmd: u8, addr: Option<u32>, mosi: &[u8], miso_len: usize) -> Vec<u8> {
        let mut out = vec![0u8; miso_len];
        if self.power_lost.is_some() || self.deep_power_down && cmd != 0xab {
            return vec![0xff; miso_len];
        }
        let a = addr.unwrap_or(0);
        match cmd {
            0x03 | 0x0b | 0x3b | 0xbb | 0x6b | 0xeb | 0x13 | 0x0c => self.read(a, &mut out),
            0x02 | 0x32 | 0x12 => self.program(a, mosi),
            0x20 | 0x21 => self.erase(a, 4096),
            0x52 => self.erase(a, 32 * 1024),
            0xd8 | 0xdc => self.erase(a, 64 * 1024),
            0x60 | 0xc7 => {
                let n = self.data.len() as u32;
                self.erase(0, n)
            }
            0x06 => self.wel = true,
            0x04 => self.wel = false,
            0x05 => out.iter_mut().for_each(|b| *b = self.status() as u8),
            0x35 => out.iter_mut().for_each(|b| *b = (self.status >> 8) as u8),
            0x15 => out.iter_mut().for_each(|b| *b = (self.status >> 16) as u8),
            0x01 => {
                if let Some(&s) = mosi.first() {
                    self.status = self.status & !0xff | s as u32;
                }
                if let Some(&s) = mosi.get(1) {
                    self.status = self.status & !0xff00 | (s as u32) << 8;
                }
                self.wel = false;
            }
            0x31 => {
                if let Some(&s) = mosi.first() {
                    self.status = self.status & !0xff00 | (s as u32) << 8;
                }
                self.wel = false;
            }
            0x9f => {
                for (i, b) in out.iter_mut().enumerate() {
                    *b = *self.jedec_id.get(i).unwrap_or(&0);
                }
            }
            0xab => {
                self.deep_power_down = false;
                out.iter_mut().for_each(|b| *b = self.jedec_id[2] - 1);
            }
            0xb9 => self.deep_power_down = true,
            0x5a => out.fill(0xff), // SFDP: not supported
            0x4b => out.iter_mut().enumerate().for_each(|(i, b)| *b = 0x5a ^ i as u8), // unique id
            0x75 | 0x7a | 0xb0 | 0x30 | 0x66 | 0x99 | 0xff | 0x50 | 0xa3 => {} // suspend/resume/reset/etc.
            _ => log::debug!("flash: unhandled command {cmd:#04x}"),
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_edits_persist_and_io_failure_keeps_memory_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("flash.bin");
        let mut flash = SpiFlash::new(vec![255; 4096], Some(path.clone()));
        flash.replace_image(vec![7; 4096]).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), flash.data);
        assert!(!flash.dirty);
        let mut failed = SpiFlash::new(vec![255; 4096], Some(dir.path().join("missing/flash.bin")));
        assert!(failed.replace_image(vec![7; 4096]).is_err());
        assert_eq!(failed.data, vec![255; 4096]);
        assert!(flash.replace_image(vec![0; 4]).is_err());
        assert_eq!(std::fs::read(path).unwrap(), vec![7; 4096]);
    }

    fn flash() -> SpiFlash {
        SpiFlash::new(vec![0xff; 1 << 16], None)
    }

    fn program(f: &mut SpiFlash, addr: u32, data: &[u8]) {
        f.transact(0x06, None, &[], 0);
        f.transact(0x02, Some(addr), data, 0);
    }

    fn erase(f: &mut SpiFlash, addr: u32) {
        f.transact(0x06, None, &[], 0);
        f.transact(0x20, Some(addr), &[], 0);
    }

    fn trigger(op: FlashOp, range: Option<(u32, u32)>, nth: u32, cut: CutPoint) -> Option<PowerLossTrigger> {
        Some(PowerLossTrigger { op, range, nth, cut, what: String::new() })
    }

    #[test]
    fn torn_program_lands_half_the_bytes() {
        let mut f = flash();
        f.set_power_loss(trigger(FlashOp::Program, None, 1, CutPoint::Torn));
        program(&mut f, 0x100, &[0x00; 32]);
        assert!(f.power_lost().unwrap().contains("program #1 at 0x100"), "{:?}", f.power_lost());
        assert_eq!(&f.data[0x100..0x110], &[0x00; 16]);
        assert_eq!(&f.data[0x110..0x120], &[0xff; 16]);
        // Unpowered: nothing else happens, reads float high.
        program(&mut f, 0x200, &[0x00; 4]);
        assert_eq!(&f.data[0x200..0x204], &[0xff; 4]);
        assert_eq!(f.transact(0x03, Some(0x100), &[], 2), vec![0xff, 0xff]);
        f.restore_power();
        assert_eq!(f.transact(0x03, Some(0x100), &[], 2), vec![0x00, 0x00]);
        // One-shot.
        program(&mut f, 0x200, &[0x00; 4]);
        assert!(f.power_lost().is_none());
        assert_eq!(&f.data[0x200..0x204], &[0x00; 4]);
    }

    #[test]
    fn torn_erase_leaves_a_half_erased_sector() {
        let mut f = flash();
        program(&mut f, 0x1000, &[0x00; 256]);
        program(&mut f, 0x1800, &[0x00; 256]);
        f.set_power_loss(trigger(FlashOp::Erase, None, 1, CutPoint::Torn));
        erase(&mut f, 0x1000);
        assert_eq!(f.data[0x1000], 0xff);
        assert_eq!(f.data[0x1800], 0x00);
    }

    #[test]
    fn counts_only_matching_operations_in_range() {
        let mut f = flash();
        f.set_power_loss(trigger(FlashOp::Program, Some((0x3000, 0x4000)), 2, CutPoint::Before));
        program(&mut f, 0x100, &[0x00]); // outside
        erase(&mut f, 0x3000); // not a program
        program(&mut f, 0x3000, &[0x00]); // 1st
        assert!(f.power_lost().is_none());
        program(&mut f, 0x3ffc, &[0x00; 4]); // 2nd: power fails before it lands
        assert!(f.power_lost().is_some());
        assert_eq!(f.data[0x3000], 0x00);
        assert_eq!(f.data[0x3ffc], 0xff);
        assert_eq!((f.programs, f.erases), (3, 1));
    }

    #[test]
    fn cut_after_completes_the_operation() {
        let mut f = flash();
        f.set_power_loss(trigger(FlashOp::Any, None, 1, CutPoint::After));
        program(&mut f, 0x10, &[0x12, 0x34]);
        assert_eq!(&f.data[0x10..0x12], &[0x12, 0x34]);
        assert!(f.power_lost().is_some());
    }
}
