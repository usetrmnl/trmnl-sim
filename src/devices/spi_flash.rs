//! Generic SPI NOR flash chip (GD25Q32-like). Operations complete instantly.
//! Contents are persisted to a file so NVS/SPIFFS survive simulator restarts.

use std::path::PathBuf;

pub struct SpiFlash {
    pub data: Vec<u8>,
    pub jedec_id: [u8; 3],
    wel: bool,
    status: u32,
    deep_power_down: bool,
    backing: Option<PathBuf>,
    pub dirty: bool,
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
        }
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
        let start = self.mask(addr & !(len - 1));
        let end = (start + len as usize).min(self.data.len());
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
        if self.deep_power_down && cmd != 0xab {
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
