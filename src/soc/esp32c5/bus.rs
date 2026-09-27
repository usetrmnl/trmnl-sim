//! ESP32-C5 memory map.
//!
//! | range                    | what                                               |
//! |--------------------------|----------------------------------------------------|
//! | 0x2080_0000..0x2080_2000 | CLIC (configuration, per-interrupt control words)  |
//! | 0x4000_0000..0x4005_0000 | ROM                                                |
//! | 0x4080_0000..0x4086_0000 | HP SRAM (one address for instruction and data bus) |
//! | 0x4200_0000..0x4400_0000 | flash (and PSRAM) through the cache MMU            |
//! | 0x5000_0000..0x5000_4000 | LP SRAM (RTC memory)                               |
//! | 0x6000_0000..0x600D_0000 | peripherals                                        |

use crate::arch::{BusFault, BusResult, MemBus};
use crate::board::Board;
use crate::devices::spi_flash::SpiFlash;
use crate::memcheck::Memcheck;
pub use crate::soc::esp32c3::bus::Clock;

use super::periph::Periph;

pub const ROM_BASE: u32 = 0x4000_0000;
pub const ROM_SIZE: usize = 0x5_0000;
pub const SRAM_BASE: u32 = 0x4080_0000;
pub const SRAM_SIZE: usize = 0x6_0000;
pub const LP_BASE: u32 = 0x5000_0000;
pub const LP_SIZE: usize = 0x4000;
pub const EXT_BASE: u32 = 0x4200_0000;
pub const PERIPH_BASE: u32 = 0x6000_0000;
pub const PERIPH_SIZE: usize = 0xD_0000;
pub const CLIC_BASE: u32 = 0x2080_0000;
pub const CLIC_SIZE: u32 = 0x2000;
/// 64 KB pages over the 32 MB window.
pub const MMU_ENTRIES: usize = 512;
/// MMU entry bits (ext_mem_defs.h): page number, SOC_MMU_ACCESS_SPIRAM, SOC_MMU_VALID.
pub const MMU_PAGE_MASK: u32 = 0x1ff;
pub const MMU_SPIRAM: u32 = 1 << 9;
pub const MMU_VALID: u32 = 1 << 10;
/// The XTAL the chip boots on (the C5's is 48 MHz).
pub const XTAL_HZ: u64 = 48_000_000;

pub struct C5Bus {
    pub clock: Clock,
    pub rom: Box<[u8]>,
    pub sram: Box<[u8]>,
    pub lp: Box<[u8]>,
    pub flash: SpiFlash,
    pub mmu: [u32; MMU_ENTRIES],
    pub p: Periph,
    pub board: Box<dyn Board>,
    /// Cycle count at which `Soc::service` must run next.
    pub next_event: u64,
    /// Set when something may have changed interrupt state.
    pub irq_dirty: bool,
    /// Last bus fault, for diagnostics.
    pub last_fault: Option<(u32, bool)>,
    /// `--memcheck`: shadow checks of CPU loads and stores.
    pub mc: Option<Box<Memcheck>>,
}

impl C5Bus {
    pub fn new(rom: Box<[u8]>, flash: SpiFlash, board: Box<dyn Board>) -> Self {
        C5Bus {
            clock: Clock::new(XTAL_HZ),
            rom,
            sram: vec![0u8; SRAM_SIZE].into_boxed_slice(),
            lp: vec![0u8; LP_SIZE].into_boxed_slice(),
            flash,
            mmu: [0; MMU_ENTRIES],
            p: Periph::new(),
            board,
            next_event: 0,
            irq_dirty: true,
            last_fault: None,
            mc: None,
        }
    }

    #[inline]
    pub fn now_ns(&self) -> u64 {
        self.clock.ns()
    }

    /// Translate a cache virtual address to a flash offset (PSRAM pages aren't backed).
    #[inline]
    fn mmu_translate(&self, addr: u32) -> Option<usize> {
        let off = addr - EXT_BASE;
        let e = self.mmu[(off >> 16) as usize];
        if e & MMU_VALID == 0 || e & MMU_SPIRAM != 0 {
            return None;
        }
        Some((((e & MMU_PAGE_MASK) << 16) | (off & 0xffff)) as usize)
    }

    /// Resolve an address to host memory (for plain loads/stores).
    #[inline(always)]
    fn mem(&self, addr: u32, len: usize) -> Option<(&[u8], usize)> {
        match addr >> 24 {
            0x40 => {
                if addr < ROM_BASE + ROM_SIZE as u32 {
                    return Some((&self.rom[..], (addr - ROM_BASE) as usize)).filter(|(_, o)| o + len <= ROM_SIZE);
                }
                if (SRAM_BASE..SRAM_BASE + SRAM_SIZE as u32).contains(&addr) {
                    return Some((&self.sram[..], (addr - SRAM_BASE) as usize)).filter(|(_, o)| o + len <= SRAM_SIZE);
                }
                None
            }
            0x42 | 0x43 => {
                let f = self.mmu_translate(addr)?;
                if f + len > self.flash.data.len() || (addr & 0xffff) as usize + len > 0x1_0000 {
                    return None;
                }
                Some((&self.flash.data[..], f))
            }
            0x50 => {
                let o = (addr - LP_BASE) as usize;
                (o + len <= LP_SIZE).then_some((&self.lp[..], o))
            }
            _ => None,
        }
    }

    #[inline(always)]
    fn mem_mut(&mut self, addr: u32, len: usize) -> Option<(&mut [u8], usize)> {
        match addr >> 24 {
            0x40 if (SRAM_BASE..SRAM_BASE + SRAM_SIZE as u32).contains(&addr) => {
                let o = (addr - SRAM_BASE) as usize;
                (o + len <= SRAM_SIZE).then_some((&mut self.sram[..], o))
            }
            0x50 => {
                let o = (addr.wrapping_sub(LP_BASE)) as usize;
                (o + len <= LP_SIZE).then_some((&mut self.lp[..], o))
            }
            _ => None,
        }
    }

    #[inline(always)]
    fn is_periph(addr: u32) -> bool {
        (PERIPH_BASE..PERIPH_BASE + PERIPH_SIZE as u32).contains(&addr)
            || (CLIC_BASE..CLIC_BASE + CLIC_SIZE).contains(&addr)
    }

    /// Check a CPU access against the memcheck shadow. A bad one is looked at by the run
    /// loop once the instruction retired, which `irq_dirty` makes it do.
    #[inline(always)]
    fn check(&mut self, addr: u32, len: u32, write: bool) {
        if self.mc.is_some() {
            self.check_shadow(addr, len, write);
        }
    }

    #[cold]
    #[inline(never)]
    fn check_shadow(&mut self, addr: u32, len: u32, write: bool) {
        if let Some(mc) = self.mc.as_deref_mut()
            && mc.access(addr, len, write)
        {
            self.irq_dirty = true;
        }
    }

    fn fault(&mut self, addr: u32, write: bool) -> BusFault {
        self.last_fault = Some((addr, write));
        BusFault
    }

    /// Read-modify-write of a byte/halfword in a peripheral register.
    fn periph_write_part(&mut self, addr: u32, v: u32, mask: u32) {
        let a = addr & !3;
        let sh = (addr & 3) * 8;
        let old = self.periph_stored(a);
        self.periph_write(a, (old & !(mask << sh)) | ((v & mask) << sh));
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

impl MemBus for C5Bus {
    #[inline(always)]
    fn fetch16(&mut self, addr: u32) -> BusResult<u16> {
        match self.mem(addr, 2) {
            Some((m, o)) => Ok(u16::from_le_bytes([m[o], m[o + 1]])),
            None => Err(self.fault(addr, false)),
        }
    }

    #[inline(always)]
    fn read8(&mut self, addr: u32) -> BusResult<u8> {
        self.check(addr, 1, false);
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
        self.check(addr, 2, false);
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
        self.check(addr, 4, false);
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
        self.check(addr, 1, true);
        if let Some((m, o)) = self.mem_mut(addr, 1) {
            m[o] = v;
            return Ok(());
        }
        if Self::is_periph(addr) {
            self.periph_write_part(addr, v as u32, 0xff);
            return Ok(());
        }
        Err(self.fault(addr, true))
    }

    #[inline(always)]
    fn write16(&mut self, addr: u32, v: u16) -> BusResult<()> {
        self.check(addr, 2, true);
        if let Some((m, o)) = self.mem_mut(addr, 2) {
            m[o..o + 2].copy_from_slice(&v.to_le_bytes());
            return Ok(());
        }
        if Self::is_periph(addr) {
            self.periph_write_part(addr & !1, v as u32, 0xffff);
            return Ok(());
        }
        Err(self.fault(addr, true))
    }

    #[inline(always)]
    fn write32(&mut self, addr: u32, v: u32) -> BusResult<()> {
        self.check(addr, 4, true);
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

    #[inline(always)]
    fn tick(&mut self) {
        self.clock.cycles += 1;
    }
}
