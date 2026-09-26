//! ESP32-S3 memory map.
//!
//! | range                    | what                                            |
//! |--------------------------|-------------------------------------------------|
//! | 0x3C00_0000..0x3E00_0000 | DBUS: flash/PSRAM via MMU (data)                |
//! | 0x3FC8_8000..0x3FD0_0000 | SRAM1+SRAM2, data bus                           |
//! | 0x3FF0_0000..0x3FF2_0000 | ROM, data alias (last 128 KB of ROM)            |
//! | 0x4000_0000..0x4006_0000 | ROM                                             |
//! | 0x4037_0000..0x403E_0000 | SRAM0+SRAM1, instruction bus                    |
//! | 0x4200_0000..0x4400_0000 | IBUS: flash/PSRAM via MMU (instructions)        |
//! | 0x5000_0000..0x5000_2000 | RTC slow memory                                 |
//! | 0x600F_E000..0x6010_0000 | RTC fast memory                                 |
//! | 0x6000_0000..0x600D_1000 | peripherals                                     |
//!
//! SRAM is one 512 KB array: IRAM 0x4037_0000 → offset 0; DRAM 0x3FC8_8000 →
//! offset 0x8000 (the DIRAM alias starts after the 32 KB IRAM-only SRAM0), and
//! the DRAM-only SRAM2 (0x3FCF_0000..) sits at offset 0x7_0000.

use crate::arch::{BusFault, BusResult, MemBus};
use crate::board::Board;
use crate::devices::spi_flash::SpiFlash;
use crate::soc::esp32c3::bus::Clock;

use super::periph::Periph;

pub const ROM_BASE: u32 = 0x4000_0000;
pub const ROM_SIZE: usize = 0x6_0000;
pub const DROM_ROM_BASE: u32 = 0x3FF0_0000;
pub const SRAM_SIZE: usize = 0x8_0000;
pub const IRAM_BASE: u32 = 0x4037_0000;
pub const IRAM_END: u32 = 0x403E_0000;
pub const DRAM_BASE: u32 = 0x3FC8_8000;
pub const DRAM_END: u32 = 0x3FD0_0000;
/// Offset of DRAM_BASE in the SRAM array.
const DRAM_OFF: u32 = 0x8000;
pub const RTC_SLOW_BASE: u32 = 0x5000_0000;
pub const RTC_FAST_BASE: u32 = 0x600F_E000;
pub const RTC_SIZE: usize = 0x2000;
pub const PERIPH_BASE: u32 = 0x6000_0000;
pub const PERIPH_SIZE: usize = 0xD_1000;
pub const MMU_TABLE: u32 = 0x600C_5000;
pub const MMU_ENTRIES: usize = 512;
pub const MMU_INVALID: u32 = 1 << 14;
pub const MMU_PSRAM: u32 = 1 << 15;
pub const PSRAM_SIZE: usize = 8 << 20;

pub struct S3Bus {
    pub clock: Clock,
    pub rom: Box<[u8]>,
    pub sram: Box<[u8]>,
    pub rtc_slow: Box<[u8]>,
    pub rtc_fast: Box<[u8]>,
    pub psram: Box<[u8]>,
    pub flash: SpiFlash,
    pub mmu: Box<[u32; MMU_ENTRIES]>,
    pub p: Periph,
    pub board: Box<dyn Board>,
    /// Cycle count at which the SoC must be serviced next.
    pub next_event: u64,
    pub irq_dirty: bool,
    /// Which core is executing (for per-core registers and the tick rule).
    pub core: usize,
    pub last_fault: Option<(u32, bool)>,
}

enum Region {
    Rom,
    Sram,
    RtcSlow,
    RtcFast,
    Flash,
    Psram,
}

impl S3Bus {
    pub fn new(rom: Box<[u8]>, flash: SpiFlash, board: Box<dyn Board>) -> Self {
        S3Bus {
            clock: Clock::new(40_000_000),
            rom,
            sram: vec![0u8; SRAM_SIZE].into_boxed_slice(),
            rtc_slow: vec![0u8; RTC_SIZE].into_boxed_slice(),
            rtc_fast: vec![0u8; RTC_SIZE].into_boxed_slice(),
            psram: vec![0u8; PSRAM_SIZE].into_boxed_slice(),
            flash,
            mmu: Box::new([MMU_INVALID; MMU_ENTRIES]),
            p: Periph::new(),
            board,
            next_event: 0,
            irq_dirty: true,
            core: 0,
            last_fault: None,
        }
    }

    #[inline]
    pub fn now_ns(&self) -> u64 {
        self.clock.ns()
    }

    /// Resolve an address to (region, offset) for plain memory.
    #[inline(always)]
    fn resolve(&self, addr: u32, len: usize) -> Option<(Region, usize)> {
        let (r, off, size) = match addr >> 24 {
            0x3F => {
                if (DRAM_BASE..DRAM_END).contains(&addr) {
                    (Region::Sram, (addr - DRAM_BASE + DRAM_OFF) as usize, SRAM_SIZE)
                } else if (DROM_ROM_BASE..DROM_ROM_BASE + 0x2_0000).contains(&addr) {
                    (Region::Rom, (addr - DROM_ROM_BASE) as usize + 0x4_0000, ROM_SIZE)
                } else {
                    return None;
                }
            }
            0x40 => {
                if addr < ROM_BASE + ROM_SIZE as u32 {
                    (Region::Rom, (addr - ROM_BASE) as usize, ROM_SIZE)
                } else if (IRAM_BASE..IRAM_END).contains(&addr) {
                    (Region::Sram, (addr - IRAM_BASE) as usize, SRAM_SIZE)
                } else {
                    return None;
                }
            }
            0x3C | 0x3D | 0x42 | 0x43 => {
                let off = addr & 0x1FF_FFFF;
                let e = self.mmu[(off >> 16) as usize];
                if e & MMU_INVALID != 0 {
                    return None;
                }
                let phys = (((e & 0x3FFF) << 16) | (off & 0xFFFF)) as usize;
                if e & MMU_PSRAM != 0 {
                    (Region::Psram, phys, PSRAM_SIZE)
                } else {
                    (Region::Flash, phys, self.flash.data.len())
                }
            }
            0x50 if addr < RTC_SLOW_BASE + RTC_SIZE as u32 => {
                (Region::RtcSlow, (addr - RTC_SLOW_BASE) as usize, RTC_SIZE)
            }
            0x60 if (RTC_FAST_BASE..RTC_FAST_BASE + RTC_SIZE as u32).contains(&addr) => {
                (Region::RtcFast, (addr - RTC_FAST_BASE) as usize, RTC_SIZE)
            }
            _ => return None,
        };
        (off + len <= size).then_some((r, off))
    }

    #[inline(always)]
    fn mem(&self, addr: u32, len: usize) -> Option<(&[u8], usize)> {
        let (r, off) = self.resolve(addr, len)?;
        Some((
            match r {
                Region::Rom => &self.rom[..],
                Region::Sram => &self.sram[..],
                Region::RtcSlow => &self.rtc_slow[..],
                Region::RtcFast => &self.rtc_fast[..],
                Region::Flash => &self.flash.data[..],
                Region::Psram => &self.psram[..],
            },
            off,
        ))
    }

    /// Writable memory (flash through the cache is read-only; PSRAM is writable).
    #[inline(always)]
    fn mem_mut(&mut self, addr: u32, len: usize) -> Option<(&mut [u8], usize)> {
        let (r, off) = self.resolve(addr, len)?;
        Some((
            match r {
                Region::Sram => &mut self.sram[..],
                Region::RtcSlow => &mut self.rtc_slow[..],
                Region::RtcFast => &mut self.rtc_fast[..],
                Region::Psram => &mut self.psram[..],
                Region::Rom | Region::Flash => return None,
            },
            off,
        ))
    }

    fn is_periph(addr: u32) -> bool {
        (PERIPH_BASE..PERIPH_BASE + PERIPH_SIZE as u32).contains(&addr)
    }

    fn fault(&mut self, addr: u32, write: bool) -> BusFault {
        self.last_fault = Some((addr, write));
        BusFault
    }

    // ---- helpers for loaders / HLE (no side effects on peripherals) ------------------------

    pub fn load_bytes(&mut self, addr: u32, data: &[u8]) -> bool {
        match self.mem_mut(addr, data.len()) {
            Some((m, o)) => {
                m[o..o + data.len()].copy_from_slice(data);
                true
            }
            None => false,
        }
    }

    pub fn peek_bytes(&self, addr: u32, len: usize) -> Option<Vec<u8>> {
        self.mem(addr, len).map(|(m, o)| m[o..o + len].to_vec())
    }

    pub fn peek32(&self, addr: u32) -> Option<u32> {
        self.mem(addr, 4).map(|(m, o)| u32::from_le_bytes(m[o..o + 4].try_into().unwrap()))
    }

    pub fn poke32(&mut self, addr: u32, v: u32) -> bool {
        self.load_bytes(addr, &v.to_le_bytes())
    }
}

impl MemBus for S3Bus {
    #[inline(always)]
    fn fetch16(&mut self, addr: u32) -> BusResult<u16> {
        match self.mem(addr, 2) {
            Some((m, o)) => Ok(u16::from_le_bytes([m[o], m[o + 1]])),
            None => Err(self.fault(addr, false)),
        }
    }

    #[inline(always)]
    fn read8(&mut self, addr: u32) -> BusResult<u8> {
        if let Some((m, o)) = self.mem(addr, 1) {
            return Ok(m[o]);
        }
        if Self::is_periph(addr) {
            let w = self.periph_read(addr & !3);
            return Ok((w >> ((addr & 3) * 8)) as u8);
        }
        Err(self.fault(addr, false))
    }

    #[inline(always)]
    fn read16(&mut self, addr: u32) -> BusResult<u16> {
        if let Some((m, o)) = self.mem(addr, 2) {
            return Ok(u16::from_le_bytes([m[o], m[o + 1]]));
        }
        if Self::is_periph(addr) {
            let w = self.periph_read(addr & !3);
            return Ok((w >> ((addr & 2) * 8)) as u16);
        }
        Err(self.fault(addr, false))
    }

    #[inline(always)]
    fn read32(&mut self, addr: u32) -> BusResult<u32> {
        if let Some((m, o)) = self.mem(addr, 4) {
            return Ok(u32::from_le_bytes([m[o], m[o + 1], m[o + 2], m[o + 3]]));
        }
        if Self::is_periph(addr) {
            return Ok(self.periph_read(addr & !3));
        }
        Err(self.fault(addr, false))
    }

    #[inline(always)]
    fn write8(&mut self, addr: u32, v: u8) -> BusResult<()> {
        if let Some((m, o)) = self.mem_mut(addr, 1) {
            m[o] = v;
            return Ok(());
        }
        if Self::is_periph(addr) {
            let a = addr & !3;
            let sh = (addr & 3) * 8;
            let old = self.p.store_get(a);
            self.periph_write(a, (old & !(0xff << sh)) | ((v as u32) << sh));
            return Ok(());
        }
        Err(self.fault(addr, true))
    }

    #[inline(always)]
    fn write16(&mut self, addr: u32, v: u16) -> BusResult<()> {
        if let Some((m, o)) = self.mem_mut(addr, 2) {
            m[o..o + 2].copy_from_slice(&v.to_le_bytes());
            return Ok(());
        }
        if Self::is_periph(addr) {
            let a = addr & !3;
            let sh = (addr & 2) * 8;
            let old = self.p.store_get(a);
            self.periph_write(a, (old & !(0xffff << sh)) | ((v as u32) << sh));
            return Ok(());
        }
        Err(self.fault(addr, true))
    }

    #[inline(always)]
    fn write32(&mut self, addr: u32, v: u32) -> BusResult<()> {
        if let Some((m, o)) = self.mem_mut(addr, 4) {
            m[o..o + 4].copy_from_slice(&v.to_le_bytes());
            return Ok(());
        }
        if Self::is_periph(addr) {
            self.periph_write(addr & !3, v);
            return Ok(());
        }
        Err(self.fault(addr, true))
    }

    #[inline(always)]
    fn cycles(&self) -> u64 {
        self.clock.cycles
    }

    /// Only the PRO core advances shared time; the machine runs both cores in
    /// lockstep quanta (see `Esp32s3::run_slice`).
    #[inline(always)]
    fn tick(&mut self) {
        if self.core == 0 {
            self.clock.cycles += 1;
        }
    }
}
