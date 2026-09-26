//! ESP32-C3 peripheral models. Every peripheral register is backed by a flat
//! register file (so unmodelled registers read back what was written); the
//! models below add side effects and status bits on top.

use super::bus::{C3Bus, MMU_ENTRIES, MMU_TABLE, PERIPH_BASE, PERIPH_SIZE};
use super::crypto::{AES, GDMA, RSA};
use crate::periph::i2c::I2c;
use crate::periph::sha::Sha;
use crate::periph::systimer::Systimer;

// Interrupt sources (periph_defs.h)
pub mod src {
    pub const GPIO: usize = 16;
    pub const I2C_EXT0: usize = 29;
    pub const UART0: usize = 21;
    pub const SYSTIMER_TARGET0: usize = 37;
    pub const DMA_CH0: usize = 44;
    pub const RSA: usize = 47;
    pub const AES: usize = 48;
    pub const SHA: usize = 49;
    pub const FROM_CPU0: usize = 50;
}

const UART0: u32 = 0x6000_0000;
const UART1: u32 = 0x6001_0000;
const SPI1: u32 = 0x6000_2000;
const GPIO: u32 = 0x6000_4000;
const RTC_CNTL: u32 = 0x6000_8000;
const EFUSE: u32 = 0x6000_8800;
const TIMG0: u32 = 0x6001_F000;
const TIMG1: u32 = 0x6002_0000;
const SYSTIMER: u32 = 0x6002_3000;
const I2C0: u32 = 0x6001_3000;
const SPI2: u32 = 0x6002_4000;
const SYSCON: u32 = 0x6002_6000;
const SHA_BASE: u32 = 0x6003_B000;
const USB_JTAG: u32 = 0x6004_3000;
const SARADC: u32 = 0x6004_0000;
const SYSTEM: u32 = 0x600C_0000;
const INTC: u32 = 0x600C_2000;
const EXTMEM: u32 = 0x600C_4000;

/// Why the chip last reset (RTC_CNTL_RESET_STATE, esp_rom reset reasons).
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ResetReason {
    PowerOn = 1,
    SwSys = 3,
    DeepSleep = 5,
    Tg0Wdt = 7,
    RtcWdt = 9,
    SwCpu = 12,
}

/// Reset requests raised by register writes, handled by the machine.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ResetRequest {
    System,
    Cpu,
}

#[derive(Debug)]
pub struct Intc {
    /// CPU interrupt line each source is routed to (0 = none).
    map: [u8; 64],
    enable: u32,
    edge: u32,
    pri: [u8; 32],
    thresh: u8,
    edge_latched: u32,
    /// Level of every interrupt source.
    pub sources: u64,
    prev_lines: u32,
}

impl Default for Intc {
    fn default() -> Self {
        Intc { map: [0; 64], enable: 0, edge: 0, pri: [0; 32], thresh: 0, edge_latched: 0, sources: 0, prev_lines: 0 }
    }
}

impl Intc {
    /// Recompute latched edges and return the highest-priority line to take, if any.
    pub fn best(&mut self) -> Option<(u32, u32)> {
        let mut lines = 0u32;
        let mut s = self.sources;
        while s != 0 {
            let n = s.trailing_zeros() as usize;
            s &= s - 1;
            let l = self.map[n];
            if l != 0 {
                lines |= 1 << l;
            }
        }
        let rising = lines & !self.prev_lines;
        self.prev_lines = lines;
        self.edge_latched |= rising & self.edge;
        let pending = ((lines & !self.edge) | self.edge_latched) & self.enable;
        let mut best: Option<(u32, u32)> = None;
        let mut p = pending;
        while p != 0 {
            let l = p.trailing_zeros();
            p &= p - 1;
            let pr = self.pri[l as usize] as u32;
            if pr == 0 || pr < self.thresh as u32 {
                continue;
            }
            if best.is_none_or(|(_, bp)| pr > bp) {
                best = Some((l, pr));
            }
        }
        best
    }

    fn read(&self, off: u32) -> Option<u32> {
        Some(match off {
            0..=0xF4 => self.map[(off / 4) as usize] as u32,
            0xF8 => self.sources as u32,
            0xFC => (self.sources >> 32) as u32,
            0x104 => self.enable,
            0x108 => self.edge,
            0x110 => ((self.prev_lines & !self.edge) | self.edge_latched) & self.enable,
            0x114..=0x190 => self.pri[((off - 0x114) / 4) as usize] as u32,
            0x194 => self.thresh as u32,
            _ => return None,
        })
    }

    fn write(&mut self, off: u32, v: u32) {
        match off {
            0..=0xF4 => self.map[(off / 4) as usize] = (v & 31) as u8,
            0x104 => self.enable = v,
            0x108 => self.edge = v,
            0x10C => self.edge_latched &= !v,
            0x114..=0x190 => self.pri[((off - 0x114) / 4) as usize] = (v & 15) as u8,
            0x194 => self.thresh = (v & 15) as u8,
            _ => {}
        }
    }
}

pub struct Periph {
    store: Vec<u32>,
    pub intc: Intc,
    pub systimer: Systimer,
    pub sha: Sha,
    pub i2c: I2c,
    pub aes_irq: bool,
    pub rsa_irq: bool,
    pub gpio_out: u32,
    pub gpio_oe: u32,
    pub gpio_in: u32,
    pub gpio_status: u32,
    pub uart_out: Vec<u8>,
    uart_raw: [u32; 2],
    pub reset_request: Option<ResetRequest>,
    pub reset_reason: ResetReason,
    pub wakeup_cause: u32,
    /// RTC slow clock ticks accumulated before the current boot (survives deep sleep).
    pub rtc_ticks_base: u64,
    rtc_time_latched: u64,
    pub rtc_slow_hz: u64,
    pub mac: [u8; 6],
    rng: u64,
    /// Log of unmodelled register accesses (first touch only).
    seen: std::collections::HashSet<u32>,
}

impl Periph {
    pub fn new() -> Self {
        Periph {
            store: vec![0; PERIPH_SIZE / 4],
            intc: Intc::default(),
            systimer: Systimer::default(),
            sha: Sha::default(),
            i2c: I2c::default(),
            aes_irq: false,
            rsa_irq: false,
            gpio_out: 0,
            gpio_oe: 0,
            gpio_in: 0,
            gpio_status: 0,
            uart_out: Vec::new(),
            uart_raw: [0; 2],
            reset_request: None,
            reset_reason: ResetReason::PowerOn,
            wakeup_cause: 0,
            rtc_ticks_base: 0,
            rtc_time_latched: 0,
            rtc_slow_hz: 136_000,
            mac: [0x7c, 0xdf, 0xa1, 0x5e, 0x1a, 0x2b],
            rng: 0x2545_f491_4f6c_dd1d,
            seen: Default::default(),
        }
    }

    #[inline]
    pub fn store_get(&self, a: u32) -> u32 {
        self.store[((a - PERIPH_BASE) / 4) as usize]
    }

    #[inline]
    pub fn store_set(&mut self, a: u32, v: u32) {
        self.store[((a - PERIPH_BASE) / 4) as usize] = v;
    }

    /// The RTC_CNTL register block (what `chip_reset` keeps through deep sleep).
    pub fn rtc_regs(&self) -> Vec<u32> {
        (RTC_CNTL..RTC_CNTL + 0x800).step_by(4).map(|a| self.store_get(a)).collect()
    }

    pub fn set_rtc_regs(&mut self, regs: &[u32]) {
        for (i, v) in regs.iter().take(0x800 / 4).enumerate() {
            self.store_set(RTC_CNTL + 4 * i as u32, *v);
        }
    }

    /// Reset register state as a chip reset would (RTC domain kept).
    pub fn chip_reset(&mut self, keep_rtc: bool) {
        let rtc = self.rtc_regs();
        self.store.fill(0);
        if keep_rtc {
            self.set_rtc_regs(&rtc);
        }
        self.intc = Intc::default();
        self.systimer = Systimer::default();
        self.sha = Sha::default();
        self.i2c = I2c::default();
        self.aes_irq = false;
        self.rsa_irq = false;
        self.gpio_out = 0;
        self.gpio_oe = 0;
        self.gpio_status = 0;
        self.uart_raw = [0; 2];
        self.reset_request = None;
        // Default register values that software relies on
        self.store_set(SYSTEM + 0x58, 1); // SYSCLK_CONF: XTAL, div 1 -> 40 MHz
    }

    fn rand(&mut self) -> u32 {
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 7;
        self.rng ^= self.rng << 17;
        (self.rng >> 16) as u32
    }

    fn efuse_word(&self, off: u32) -> Option<u32> {
        let m = &self.mac;
        Some(match off {
            // BLK1: MAC (little-endian byte order reversed like hardware)
            0x44 => u32::from_le_bytes([m[5], m[4], m[3], m[2]]),
            0x48 => u16::from_le_bytes([m[1], m[0]]) as u32,
            // SYS_3: wafer version minor low = 3 (ECO3), blk version minor = 1
            0x50 => 3 << 18 | 1 << 24,
            0x58 => 0,
            0x1D0 => 1, // EFUSE_STATUS: state = idle
            0x1FC => 0x2007_3000,
            _ => return None,
        })
    }
}

impl Default for Periph {
    fn default() -> Self {
        Self::new()
    }
}

impl C3Bus {
    pub fn periph_read(&mut self, a: u32) -> u32 {
        let now = self.clock.ns();
        let stored = self.p.store_get(a);
        match a {
            // ---- UART ----
            UART0 | UART1 => 0, // RX FIFO empty
            _ if a == UART0 + 0x04 || a == UART1 + 0x04 => self.uart_raw_bits(a),
            _ if a == UART0 + 0x08 || a == UART1 + 0x08 => self.uart_raw_bits(a - 4) & self.p.store_get(a + 4),
            _ if a == UART0 + 0x1C || a == UART1 + 0x1C => 0, // STATUS: fifos empty
            _ if a == UART0 + 0x80 || a == UART1 + 0x80 => stored & !(1 << 31), // ID: UPDATE done
            _ if a == UART0 + 0x68 || a == UART1 + 0x68 => 0, // FSM status idle

            // ---- SPI1 (flash controller) ----
            _ if a == SPI1 => 0,                               // all commands complete instantly
            _ if (SPI1 + 0x54..SPI1 + 0x58).contains(&a) => 0, // FSM idle
            _ if a == SPI1 + 0xA4 => 0,                        // SUS_STATUS

            // ---- GPIO ----
            _ if a == GPIO + 0x04 => self.p.gpio_out,
            _ if a == GPIO + 0x20 => self.p.gpio_oe,
            _ if a == GPIO + 0x38 => 0x0c, // strapping: SPI boot (GPIO8=1, GPIO9=1 -> 0b1100)
            _ if a == GPIO + 0x3C => {
                self.gpio_sample(now);
                self.p.gpio_in
            }
            _ if a == GPIO + 0x44 => self.p.gpio_status,
            _ if a == GPIO + 0x5C => self.p.gpio_status & self.gpio_int_enabled_mask(),
            _ if a == GPIO + 0x14C => self.p.gpio_status, // STATUS_NEXT

            // ---- RTC_CNTL ----
            _ if a == RTC_CNTL + 0x0C => stored & !(1 << 31) | 1 << 30, // TIME_UPDATE: valid
            _ if a == RTC_CNTL + 0x10 => self.p.rtc_time_latched as u32,
            _ if a == RTC_CNTL + 0x14 => (self.p.rtc_time_latched >> 32) as u32,
            _ if a == RTC_CNTL + 0x38 => {
                let r = self.p.reset_reason as u32;
                stored & !0xfff | r | r << 6
            }
            _ if a == RTC_CNTL + 0xF8 => self.p.wakeup_cause,
            _ if a == RTC_CNTL + 0x44 || a == RTC_CNTL + 0x48 => 0, // no RTC interrupts pending
            _ if (EFUSE..EFUSE + 0x200).contains(&a) => self.p.efuse_word(a - EFUSE).unwrap_or(stored),

            // ---- TIMG ----
            _ if a == TIMG0 + 0x68 || a == TIMG1 + 0x68 => stored | 1 << 15, // RTC cal ready
            _ if a == TIMG0 + 0x6C || a == TIMG1 + 0x6C => {
                let cfg = self.p.store_get(a - 4);
                let max = (cfg >> 16) & 0x7fff;
                // XTAL (40 MHz) cycles counted during `max` slow clock cycles
                let v = max as u64 * 40_000_000 / self.p.rtc_slow_hz;
                (v as u32) << 7
            }

            // ---- SYSTIMER ----
            _ if (SYSTIMER..SYSTIMER + 0x100).contains(&a) => {
                let v = self.p.systimer.read(a - SYSTIMER, now);
                self.irq_dirty = true;
                v
            }

            // ---- GPSPI2 ----
            _ if a == SPI2 => stored & !(1 << 24 | 1 << 23), // USR/UPDATE complete

            // ---- APB_SARADC: one-shot conversions complete instantly ----
            _ if a == SARADC + 0x44 => stored | 3 << 30,
            _ if a == SARADC + 0x2C || a == SARADC + 0x30 => {
                let cfg = self.p.store_get(SARADC + 0x20);
                let ch = (cfg >> 25) & 0xf; // bit 3 selects ADC2
                let atten = (cfg >> 23) & 3;
                let full_scale_mv = [950, 1250, 1750, 2500][atten as usize];
                let mv = if ch < 5 { self.board.adc_millivolts(ch as u8) } else { 0 };
                (mv * 4095 / full_scale_mv).min(4095)
            }

            // ---- crypto ----
            _ if a == RSA + 0x808 || a == RSA + 0x818 => 1, // memory clean / operation done
            _ if (GDMA..GDMA + 0x30).contains(&a) && (a - GDMA) % 0x10 == 8 => {
                self.p.store_get(a - 8) & self.p.store_get(a - 4) // INT_ST = RAW & ENA
            }

            // ---- I2C ----
            _ if a == I2C0 + 0x08 => {
                let i = &self.p.i2c;
                (i.rx.len() as u32 & 0x3f) << 8 | (i.tx.len() as u32 & 0x3f) << 18
            }
            _ if a == I2C0 + 0x1C => self.p.i2c.rx.pop_front().unwrap_or(0) as u32,
            _ if a == I2C0 + 0x20 => self.p.i2c.raw,
            _ if a == I2C0 + 0x2C => self.p.i2c.raw & self.p.store_get(I2C0 + 0x28),
            _ if a == I2C0 + 0x80 => stored & !1, // SCL_RST_SLV_EN self-clears
            _ if a == I2C0 + 0x04 => stored & !(1 << 5 | 1 << 11),

            // ---- SYSCON: RNG ----
            _ if a == SYSCON + 0xB0 => self.p.rand(),

            // ---- SHA ----
            _ if (SHA_BASE..SHA_BASE + 0x100).contains(&a) => self.p.sha.read(a - SHA_BASE),

            // ---- USB serial/JTAG: always ready, output discarded ----
            _ if a == USB_JTAG + 0x04 => stored & !1 | 2,
            _ if a == USB_JTAG + 0x08 => stored | 1 << 1 | 1 << 3, // int raw: in empty

            // ---- SYSTEM ----
            _ if a == SYSTEM + 0x48 => stored | 1 << 31, // RTC mem CRC finished

            // ---- INTC ----
            _ if (INTC..INTC + 0x800).contains(&a) => self.p.intc.read(a - INTC).unwrap_or(stored),

            // ---- EXTMEM: cache operations complete instantly ----
            _ if a == EXTMEM + 0x1C => stored | 1 << 2,
            _ if a == EXTMEM + 0x28 => stored & !1 | 1 << 1,
            _ if a == EXTMEM + 0x34 => stored & !1 | 1 << 1,
            _ if a == EXTMEM + 0x40 => stored | 1 << 3,
            _ if (MMU_TABLE..MMU_TABLE + 4 * MMU_ENTRIES as u32).contains(&a) => {
                self.mmu[((a - MMU_TABLE) / 4) as usize]
            }
            _ if (EXTMEM..EXTMEM + 0x1000).contains(&a) => {
                // Other EXTMEM status registers: report idle/done where it matters.
                match a - EXTMEM {
                    0xCC => stored & !4 | (stored & 1) << 2, // ICACHE_FREEZE_DONE follows ENA
                    _ => stored,
                }
            }
            _ => {
                if log::log_enabled!(log::Level::Trace) && self.p.seen.insert(a) {
                    log::trace!("periph read  {a:#010x} (unmodelled) = {stored:#x}");
                }
                stored
            }
        }
    }

    pub fn periph_write(&mut self, a: u32, v: u32) {
        let now = self.clock.ns();
        self.p.store_set(a, v);
        match a {
            UART0 | UART1 => {
                if a == UART0 {
                    self.p.uart_out.push(v as u8);
                }
                let u = (a == UART1) as usize;
                self.p.uart_raw[u] |= 1 << 14; // TX_DONE
                self.irq_dirty = true;
            }
            _ if a == UART0 + 0x10 || a == UART1 + 0x10 => {
                let u = (a == UART1 + 0x10) as usize;
                self.p.uart_raw[u] &= !v;
                self.irq_dirty = true;
            }
            _ if a == UART0 + 0x0C || a == UART1 + 0x0C => self.irq_dirty = true,

            _ if a == SPI1 => {
                self.spi1_command(v);
                // A power-loss fault fired: make the run loop stop right after this store.
                self.irq_dirty |= self.flash.power_lost().is_some();
            }

            // GPIO
            _ if a == GPIO + 0x04 => self.gpio_set(self.p.gpio_out, v, now),
            _ if a == GPIO + 0x08 => self.gpio_set(self.p.gpio_out, self.p.gpio_out | v, now),
            _ if a == GPIO + 0x0C => self.gpio_set(self.p.gpio_out, self.p.gpio_out & !v, now),
            _ if a == GPIO + 0x20 => self.gpio_set_oe(v, now),
            _ if a == GPIO + 0x24 => self.gpio_set_oe(self.p.gpio_oe | v, now),
            _ if a == GPIO + 0x28 => self.gpio_set_oe(self.p.gpio_oe & !v, now),
            _ if a == GPIO + 0x44 => {
                self.p.gpio_status = v;
                self.irq_dirty = true;
            }
            _ if a == GPIO + 0x48 => {
                self.p.gpio_status |= v;
                self.irq_dirty = true;
            }
            _ if a == GPIO + 0x4C => {
                self.p.gpio_status &= !v;
                self.gpio_level_ints();
                self.irq_dirty = true;
            }
            _ if (GPIO + 0x74..GPIO + 0x74 + 22 * 4).contains(&a) => {
                self.gpio_level_ints();
                self.irq_dirty = true;
            }

            // RTC_CNTL
            _ if a == RTC_CNTL => {
                if v & (1 << 31) != 0 {
                    self.p.reset_request = Some(ResetRequest::System);
                } else if v & (1 << 5) != 0 {
                    self.p.reset_request = Some(ResetRequest::Cpu);
                }
                // SW_SYS_RST / SW_PROCPU_RST are self-clearing.
                self.p.store_set(a, v & !(1 << 31 | 1 << 5));
            }
            _ if a == RTC_CNTL + 0x0C && v & (1 << 31) != 0 => {
                self.p.rtc_time_latched = self.rtc_ticks(now);
            }

            // SYSTIMER
            _ if (SYSTIMER..SYSTIMER + 0x100).contains(&a) => {
                if self.p.systimer.write(a - SYSTIMER, v, now) {
                    self.irq_dirty = true;
                }
            }

            // crypto
            _ if a == AES + 0x48 => self.aes_trigger(),
            _ if a == AES + 0xB8 => self.p.store_set(AES + 0x4C, 0),
            _ if a == AES + 0xAC => {
                self.p.aes_irq = false;
                self.irq_dirty = true;
            }
            _ if a == RSA + 0x80C || a == RSA + 0x810 || a == RSA + 0x814 => self.rsa_start(a - RSA),
            _ if a == RSA + 0x81C => {
                self.p.rsa_irq = false;
                self.irq_dirty = true;
            }
            _ if (GDMA..GDMA + 0x30).contains(&a) && (a - GDMA) % 0x10 == 0xC => {
                let raw = self.p.store_get(a - 0xC);
                self.p.store_set(a - 0xC, raw & !v);
                self.irq_dirty = true;
            }

            // I2C
            _ if a == I2C0 + 0x04 && v & (1 << 5) != 0 => {
                let mut cmds = [0u32; 8];
                for (k, c) in cmds.iter_mut().enumerate() {
                    *c = self.p.store_get(I2C0 + 0x58 + 4 * k as u32);
                }
                self.p.i2c.execute(now, &mut cmds, self.board.as_mut());
                for (k, c) in cmds.iter().enumerate() {
                    self.p.store_set(I2C0 + 0x58 + 4 * k as u32, *c);
                }
                self.irq_dirty = true;
            }
            _ if a == I2C0 + 0x1C => self.p.i2c.tx.push_back(v as u8),
            _ if a == I2C0 + 0x18 => {
                if v & (1 << 13) != 0 {
                    self.p.i2c.tx.clear();
                }
                if v & (1 << 12) != 0 {
                    self.p.i2c.rx.clear();
                }
            }
            _ if a == I2C0 + 0x24 => {
                self.p.i2c.raw &= !v;
                self.irq_dirty = true;
            }
            _ if a == I2C0 + 0x28 => self.irq_dirty = true,

            // GPSPI2
            _ if a == SPI2 => {
                if v & (1 << 24) != 0 {
                    self.spi2_transfer(now);
                }
            }

            // SHA
            _ if (SHA_BASE..SHA_BASE + 0x100).contains(&a) => {
                self.p.sha.write(a - SHA_BASE, v);
                if a - SHA_BASE == 0x1C || a - SHA_BASE == 0x20 {
                    self.sha_dma(a - SHA_BASE == 0x1C);
                }
                self.irq_dirty = true;
            }

            // SYSTEM
            _ if a == SYSTEM + 0x08 || a == SYSTEM + 0x58 => self.update_cpu_clock(),
            _ if (SYSTEM + 0x28..=SYSTEM + 0x34).contains(&a) => {
                let n = ((a - SYSTEM - 0x28) / 4) as usize;
                let bit = 1u64 << (src::FROM_CPU0 + n);
                if v & 1 != 0 {
                    self.p.intc.sources |= bit;
                } else {
                    self.p.intc.sources &= !bit;
                }
                self.irq_dirty = true;
            }

            _ if (INTC..INTC + 0x800).contains(&a) => {
                self.p.intc.write(a - INTC, v);
                self.irq_dirty = true;
            }
            _ if (MMU_TABLE..MMU_TABLE + 4 * MMU_ENTRIES as u32).contains(&a) => {
                self.mmu[((a - MMU_TABLE) / 4) as usize] = v & 0x1ff;
            }
            _ => {
                if log::log_enabled!(log::Level::Trace) && self.p.seen.insert(a | 1) {
                    log::trace!("periph write {a:#010x} (unmodelled) = {v:#x}");
                }
            }
        }
    }

    fn uart_raw_bits(&self, raw_reg: u32) -> u32 {
        let u = (raw_reg == UART1 + 0x04) as usize;
        self.p.uart_raw[u] | 1 << 1 // TXFIFO_EMPTY
    }

    /// Recompute interrupt source levels from peripheral state.
    pub fn refresh_sources(&mut self) {
        let mut s = self.p.intc.sources;
        let mut set = |n: usize, on: bool| {
            if on { s |= 1 << n } else { s &= !(1 << n) }
        };
        let uart_st = self.uart_raw_bits(UART0 + 4) & self.p.store_get(UART0 + 0x0C);
        set(src::UART0, uart_st != 0);
        set(src::UART0 + 1, (self.uart_raw_bits(UART1 + 4) & self.p.store_get(UART1 + 0x0C)) != 0);
        let st = self.p.systimer.irq_lines();
        for i in 0..3 {
            set(src::SYSTIMER_TARGET0 + i, st & (1 << i) != 0);
        }
        set(src::GPIO, self.p.gpio_status & self.gpio_int_enabled_mask() != 0);
        set(src::SHA, self.p.sha.irq());
        set(src::AES, self.p.aes_irq && self.p.store_get(AES + 0xB0) & 1 != 0);
        set(src::RSA, self.p.rsa_irq && self.p.store_get(RSA + 0x82C) & 1 != 0);
        for ch in 0..3u32 {
            let st = self.p.store_get(GDMA + 0x10 * ch) & self.p.store_get(GDMA + 4 + 0x10 * ch);
            set(src::DMA_CH0 + ch as usize, st != 0);
        }
        set(src::I2C_EXT0, self.p.i2c.raw & self.p.store_get(I2C0 + 0x28) != 0);
        self.p.intc.sources = s;
    }

    // ---- clocks ------------------------------------------------------------------------------

    fn update_cpu_clock(&mut self) {
        let sysclk = self.p.store_get(SYSTEM + 0x58);
        let per = self.p.store_get(SYSTEM + 0x08);
        let hz = match (sysclk >> 10) & 3 {
            0 => 40_000_000 / ((sysclk & 0x3ff) as u64 + 1),
            1 => {
                if per & 3 == 1 {
                    160_000_000
                } else {
                    80_000_000
                }
            }
            _ => 17_500_000 / ((sysclk & 0x3ff) as u64 + 1),
        };
        self.clock.set_hz(hz);
    }

    pub fn rtc_ticks(&self, now: u64) -> u64 {
        self.p.rtc_ticks_base + (now as u128 * self.p.rtc_slow_hz as u128 / 1_000_000_000) as u64
    }

    // ---- systimer ----------------------------------------------------------------------------

    pub fn systimer_update(&mut self, now: u64) {
        if self.p.systimer.update(now) {
            self.irq_dirty = true;
        }
    }

    pub fn systimer_next_ns(&self, now: u64) -> Option<u64> {
        self.p.systimer.next_ns(now)
    }

    // ---- GPIO --------------------------------------------------------------------------------

    fn gpio_int_enabled_mask(&self) -> u32 {
        let mut m = 0;
        for pin in 0..22 {
            let r = self.p.store_get(GPIO + 0x74 + 4 * pin);
            if (r >> 13) & 0x1f != 0 && (r >> 7) & 7 != 0 {
                m |= 1 << pin;
            }
        }
        m
    }

    fn gpio_set(&mut self, _old: u32, new: u32, now: u64) {
        let new = new & 0x3f_ffff;
        if new != self.p.gpio_out {
            self.p.gpio_out = new;
            self.board.gpio_out(now, new as u64, self.p.gpio_oe as u64);
        }
    }

    fn gpio_set_oe(&mut self, v: u32, now: u64) {
        let v = v & 0x3f_ffff;
        if v != self.p.gpio_oe {
            self.p.gpio_oe = v;
            self.board.gpio_out(now, self.p.gpio_out as u64, v as u64);
        }
    }

    /// Sample input levels and latch GPIO interrupt status.
    pub fn gpio_sample(&mut self, now: u64) {
        let (lv, driven) = self.board.gpio_in(now);
        let (lv, driven) = (lv as u32, driven as u32);
        let mut input = 0u32;
        for pin in 0..22u32 {
            let bit = 1 << pin;
            let level = if driven & bit != 0 {
                lv & bit != 0
            } else if self.p.gpio_oe & bit != 0 {
                self.p.gpio_out & bit != 0
            } else {
                let mux = self.p.store_get(0x6000_9004 + 4 * pin);
                // FUN_WPU (bit 8) pulls up. Pins without pull-ups idle low.
                mux & (1 << 8) != 0
            };
            if level {
                input |= bit;
            }
        }
        let old = self.p.gpio_in;
        self.p.gpio_in = input;
        let rising = input & !old;
        let falling = !input & old;
        let mut status = self.p.gpio_status;
        for pin in 0..22u32 {
            let bit = 1 << pin;
            let r = self.p.store_get(GPIO + 0x74 + 4 * pin);
            let hit = match (r >> 7) & 7 {
                1 => rising & bit != 0,
                2 => falling & bit != 0,
                3 => (rising | falling) & bit != 0,
                4 => input & bit == 0,
                5 => input & bit != 0,
                _ => false,
            };
            if hit {
                status |= bit;
            }
        }
        if status != self.p.gpio_status {
            self.p.gpio_status = status;
            self.irq_dirty = true;
        }
    }

    fn gpio_level_ints(&mut self) {
        let now = self.clock.ns();
        self.gpio_sample(now);
    }

    // ---- GPSPI2 ------------------------------------------------------------------------------

    fn spi2_transfer(&mut self, now: u64) {
        let bits = (self.p.store_get(SPI2 + 0x1C) & 0x3ffff) + 1;
        let user = self.p.store_get(SPI2 + 0x10);
        let n = (bits as usize).div_ceil(8).min(64);
        let mut out = Vec::with_capacity(n);
        for i in 0..n {
            let w = self.p.store_get(SPI2 + 0x98 + 4 * (i as u32 / 4));
            out.push((w >> (8 * (i % 4))) as u8);
        }
        let mosi = user & (1 << 27) != 0 || user & 1 != 0;
        let miso = user & (1 << 28) != 0 || user & 1 != 0;
        let rx = self.board.spi_transfer(now, 2, if mosi { &out } else { &[] }, if miso { n } else { 0 });
        if miso {
            for (i, chunk) in rx.chunks(4).enumerate() {
                let mut w = [0u8; 4];
                w[..chunk.len()].copy_from_slice(chunk);
                self.p.store_set(SPI2 + 0x98 + 4 * i as u32, u32::from_le_bytes(w));
            }
        }
        let raw = self.p.store_get(SPI2 + 0x3C);
        self.p.store_set(SPI2 + 0x3C, raw | 1 << 12);
    }

    // ---- SPI1: flash controller ----------------------------------------------------------------

    fn spi1_command(&mut self, cmd: u32) {
        let addr_reg = self.p.store_get(SPI1 + 0x04);
        let w = |p: &Periph, i: usize| p.store_get(SPI1 + 0x58 + 4 * i as u32);
        let buf = |p: &Periph, n: usize| -> Vec<u8> {
            (0..n.min(64)).map(|i| (w(p, i / 4) >> (8 * (i % 4))) as u8).collect()
        };
        let f = &mut self.flash;
        if cmd & (1 << 18) != 0 {
            // User-defined transaction
            let user = self.p.store_get(SPI1 + 0x18);
            let user2 = self.p.store_get(SPI1 + 0x20);
            let opcode = (user2 & 0xffff) as u8;
            let addr = (user & (1 << 30) != 0).then_some(addr_reg);
            let mosi_bytes = if user & (1 << 27) != 0 {
                ((self.p.store_get(SPI1 + 0x24) & 0x3ff) as usize + 1).div_ceil(8)
            } else {
                0
            };
            let miso_bytes = if user & (1 << 28) != 0 {
                ((self.p.store_get(SPI1 + 0x28) & 0x3ff) as usize + 1).div_ceil(8)
            } else {
                0
            };
            let mosi = buf(&self.p, mosi_bytes);
            let rx = self.flash.transact(opcode, addr, &mosi, miso_bytes.min(64));
            self.spi1_fill(&rx);
        } else if cmd & (1 << 31) != 0 {
            let len = ((addr_reg >> 24) as usize).min(64);
            let rx = f.transact(0x03, Some(addr_reg & 0xff_ffff), &[], len);
            self.spi1_fill(&rx);
        } else if cmd & (1 << 30) != 0 {
            f.transact(0x06, None, &[], 0);
        } else if cmd & (1 << 29) != 0 {
            f.transact(0x04, None, &[], 0);
        } else if cmd & (1 << 28) != 0 {
            let rx = f.transact(0x9f, None, &[], 3);
            self.spi1_fill(&rx);
        } else if cmd & (1 << 27) != 0 {
            let s = f.status();
            self.p.store_set(SPI1 + 0x2C, s & 0xffff);
        } else if cmd & (1 << 26) != 0 {
            let s = self.p.store_get(SPI1 + 0x2C);
            f.transact(0x01, None, &[s as u8, (s >> 8) as u8], 0);
        } else if cmd & (1 << 25) != 0 {
            let len = ((addr_reg >> 24) as usize).min(64);
            let data = buf(&self.p, len);
            self.flash.transact(0x02, Some(addr_reg & 0xff_ffff), &data, 0);
        } else if cmd & (1 << 24) != 0 {
            f.transact(0x20, Some(addr_reg & 0xff_ffff), &[], 0);
        } else if cmd & (1 << 23) != 0 {
            f.transact(0xd8, Some(addr_reg & 0xff_ffff), &[], 0);
        } else if cmd & (1 << 22) != 0 {
            f.transact(0x60, None, &[], 0);
        } else if cmd & (1 << 21) != 0 {
            f.transact(0xb9, None, &[], 0);
        } else if cmd & (1 << 20) != 0 {
            f.transact(0xab, None, &[], 0);
        }
    }

    fn spi1_fill(&mut self, rx: &[u8]) {
        for (i, chunk) in rx.chunks(4).enumerate() {
            let mut w = [0u8; 4];
            w[..chunk.len()].copy_from_slice(chunk);
            self.p.store_set(SPI1 + 0x58 + 4 * i as u32, u32::from_le_bytes(w));
        }
    }
}
