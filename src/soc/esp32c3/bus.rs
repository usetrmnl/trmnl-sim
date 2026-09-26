//! ESP32-C3 memory map.
//!
//! | range                    | what                                      |
//! |--------------------------|-------------------------------------------|
//! | 0x3C00_0000..0x3C80_0000 | DROM: flash via MMU (data bus)            |
//! | 0x3FC7_C000..0x3FCE_0000 | SRAM, data bus alias                      |
//! | 0x3FF0_0000..0x3FF2_0000 | ROM, data bus alias (last 128 KB of ROM)  |
//! | 0x4000_0000..0x4006_0000 | ROM                                       |
//! | 0x4037_C000..0x403E_0000 | SRAM, instruction bus alias               |
//! | 0x4200_0000..0x4280_0000 | IROM: flash via MMU (instruction bus)     |
//! | 0x5000_0000..0x5000_2000 | RTC fast memory                           |
//! | 0x6000_0000..0x600D_1000 | peripherals                               |

use crate::arch::{BusFault, BusResult, MemBus};
use crate::board::Board;
use crate::devices::spi_flash::SpiFlash;
use crate::memcheck::Memcheck;

use super::periph::Periph;

pub const ROM_BASE: u32 = 0x4000_0000;
pub const ROM_SIZE: usize = 0x6_0000;
pub const DROM_ROM_BASE: u32 = 0x3FF0_0000;
pub const SRAM_D_BASE: u32 = 0x3FC7_C000;
pub const SRAM_I_BASE: u32 = 0x4037_C000;
pub const SRAM_SIZE: usize = 0x6_4000;
pub const RTC_BASE: u32 = 0x5000_0000;
pub const RTC_SIZE: usize = 0x2000;
pub const PERIPH_BASE: u32 = 0x6000_0000;
pub const PERIPH_SIZE: usize = 0xD_1000;
pub const MMU_TABLE: u32 = 0x600C_5000;
pub const MMU_ENTRIES: usize = 128;
pub const MMU_INVALID: u32 = 1 << 8;

/// Virtual time. The CPU clock frequency changes during boot, so time is kept as
/// a (cycles, ns) base plus cycles elapsed at the current frequency.
#[derive(Clone)]
pub struct Clock {
    pub cycles: u64,
    base_cycles: u64,
    base_ns: u64,
    pub cpu_hz: u64,
}

impl Clock {
    pub fn new(cpu_hz: u64) -> Self {
        Clock { cycles: 0, base_cycles: 0, base_ns: 0, cpu_hz }
    }

    #[inline]
    pub fn ns(&self) -> u64 {
        self.base_ns + ((self.cycles - self.base_cycles) as u128 * 1_000_000_000 / self.cpu_hz as u128) as u64
    }

    pub fn set_hz(&mut self, hz: u64) {
        if hz != self.cpu_hz && hz > 0 {
            self.base_ns = self.ns();
            self.base_cycles = self.cycles;
            self.cpu_hz = hz;
            log::debug!("cpu clock now {} MHz", hz / 1_000_000);
        }
    }

    /// The cycle count at which `ns()` will reach `target_ns`.
    pub fn cycles_at(&self, target_ns: u64) -> u64 {
        if target_ns <= self.base_ns {
            return self.base_cycles;
        }
        let d = (target_ns - self.base_ns) as u128 * self.cpu_hz as u128;
        self.base_cycles + d.div_ceil(1_000_000_000) as u64
    }

    /// Restart time accounting at `ns` from the current cycle count.
    pub fn set_base(&mut self, ns: u64) {
        self.base_ns = ns;
        self.base_cycles = self.cycles;
    }

    /// Jump forward in time (idle fast-forward / sleep).
    pub fn advance_to_ns(&mut self, target_ns: u64) {
        let c = self.cycles_at(target_ns);
        if c > self.cycles {
            self.cycles = c;
        }
    }
}

pub struct C3Bus {
    pub clock: Clock,
    pub rom: Box<[u8]>,
    pub sram: Box<[u8]>,
    pub rtc: Box<[u8]>,
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

impl C3Bus {
    pub fn new(rom: Box<[u8]>, flash: SpiFlash, board: Box<dyn Board>) -> Self {
        C3Bus {
            clock: Clock::new(40_000_000),
            rom,
            sram: vec![0u8; SRAM_SIZE].into_boxed_slice(),
            rtc: vec![0u8; RTC_SIZE].into_boxed_slice(),
            flash,
            mmu: [MMU_INVALID; MMU_ENTRIES],
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

    /// Translate a flash-cache virtual address to a flash offset.
    #[inline]
    fn mmu_translate(&self, addr: u32) -> Option<usize> {
        let off = addr & 0x7F_FFFF;
        let e = self.mmu[(off >> 16) as usize];
        if e & MMU_INVALID != 0 {
            return None;
        }
        Some((((e & 0xff) << 16) | (off & 0xffff)) as usize)
    }

    /// Resolve an address to host memory (for plain loads/stores).
    #[inline(always)]
    fn mem(&self, addr: u32, len: usize) -> Option<(&[u8], usize)> {
        match addr >> 24 {
            0x3F => {
                if (SRAM_D_BASE..SRAM_D_BASE + SRAM_SIZE as u32).contains(&addr) {
                    return Some((&self.sram[..], (addr - SRAM_D_BASE) as usize)).filter(|(_, o)| o + len <= SRAM_SIZE);
                }
                if (DROM_ROM_BASE..DROM_ROM_BASE + 0x2_0000).contains(&addr) {
                    return Some((&self.rom[..], (addr - DROM_ROM_BASE) as usize + 0x4_0000));
                }
                None
            }
            0x40 => {
                if addr < ROM_BASE + ROM_SIZE as u32 {
                    return Some((&self.rom[..], (addr - ROM_BASE) as usize)).filter(|(_, o)| o + len <= ROM_SIZE);
                }
                if (SRAM_I_BASE..SRAM_I_BASE + SRAM_SIZE as u32).contains(&addr) {
                    return Some((&self.sram[..], (addr - SRAM_I_BASE) as usize)).filter(|(_, o)| o + len <= SRAM_SIZE);
                }
                None
            }
            0x3C | 0x42 => {
                if addr & 0x00FF_FFFF >= 0x80_0000 {
                    return None;
                }
                let f = self.mmu_translate(addr)?;
                if f + len > self.flash.data.len() {
                    return None;
                }
                Some((&self.flash.data[..], f))
            }
            0x50 => {
                let o = (addr - RTC_BASE) as usize;
                if o + len <= RTC_SIZE {
                    return Some((&self.rtc[..], o));
                }
                None
            }
            _ => None,
        }
    }

    #[inline(always)]
    fn mem_mut(&mut self, addr: u32, len: usize) -> Option<(&mut [u8], usize)> {
        match addr >> 24 {
            0x3F if (SRAM_D_BASE..SRAM_D_BASE + SRAM_SIZE as u32).contains(&addr) => {
                let o = (addr - SRAM_D_BASE) as usize;
                (o + len <= SRAM_SIZE).then_some((&mut self.sram[..], o))
            }
            0x40 if (SRAM_I_BASE..SRAM_I_BASE + SRAM_SIZE as u32).contains(&addr) => {
                let o = (addr - SRAM_I_BASE) as usize;
                (o + len <= SRAM_SIZE).then_some((&mut self.sram[..], o))
            }
            0x50 => {
                let o = (addr.wrapping_sub(RTC_BASE)) as usize;
                (o + len <= RTC_SIZE).then_some((&mut self.rtc[..], o))
            }
            _ => None,
        }
    }

    fn is_periph(addr: u32) -> bool {
        (PERIPH_BASE..PERIPH_BASE + PERIPH_SIZE as u32).contains(&addr)
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

impl MemBus for C3Bus {
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
        self.check(addr, 2, true);
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
