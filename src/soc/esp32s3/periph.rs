//! ESP32-S3 peripheral models. Like the C3, every register is backed by a flat
//! register file; models add side effects and status bits. Chip-independent IP
//! (SYSTIMER, SHA, I2C engine, crypto math) lives in `crate::periph`.

use std::collections::VecDeque;

use super::bus::{MMU_ENTRIES, MMU_TABLE, PERIPH_BASE, PERIPH_SIZE, S3Bus};
use crate::periph::crypto_math::{aes_block, aes_blocks, rsa_op};
use crate::periph::i2c::I2c;
use crate::periph::sha::Sha;
use crate::periph::systimer::Systimer;

/// Interrupt sources (soc/interrupts.h for the S3), = map register offset / 4.
pub mod src {
    pub const GPIO: usize = 16;
    pub const LCD_CAM: usize = 24;
    pub const UART0: usize = 27;
    pub const I2C_EXT0: usize = 42;
    pub const SYSTIMER_TARGET0: usize = 57;
    pub const DMA_IN_CH0: usize = 66;
    pub const DMA_OUT_CH0: usize = 71;
    pub const RSA: usize = 76;
    pub const AES: usize = 77;
    pub const SHA: usize = 78;
    pub const FROM_CPU0: usize = 79;
    pub const USB_DEVICE: usize = 96;
}

pub const UART0: u32 = 0x6000_0000;
pub const UART1: u32 = 0x6001_0000;
pub const UART2: u32 = 0x6002_E000;
const SPI1: u32 = 0x6000_2000;
const SPI0: u32 = 0x6000_3000;
pub const GPIO: u32 = 0x6000_4000;
const EFUSE: u32 = 0x6000_7000;
const RTC_CNTL: u32 = 0x6000_8000;
const IO_MUX: u32 = 0x6000_9000;
const I2C0: u32 = 0x6001_3000;
const TIMG0: u32 = 0x6001_F000;
const TIMG1: u32 = 0x6002_0000;
const SYSTIMER: u32 = 0x6002_3000;
const SPI2: u32 = 0x6002_4000;
const RNG: u32 = 0x6003_507C;
const USB_JTAG: u32 = 0x6003_8000;
const AES: u32 = 0x6003_A000;
const SHA_BASE: u32 = 0x6003_B000;
const RSA: u32 = 0x6003_C000;
const GDMA: u32 = 0x6003_F000;
const LCD_CAM: u32 = 0x6004_1000;
const SYSTEM: u32 = 0x600C_0000;
const INTC: u32 = 0x600C_2000;
const EXTMEM: u32 = 0x600C_4000;

const GDMA_CH_STRIDE: u32 = 0xC0;
const GDMA_CHANNELS: u32 = 5;
const PERI_LCD_CAM: u32 = 5;
const PERI_AES: u32 = 6;
const PERI_SHA: u32 = 7;

#[derive(Clone, Copy, Debug, PartialEq)]
#[allow(dead_code)]
pub enum ResetReason {
    PowerOn = 1,
    SwSys = 3,
    DeepSleep = 5,
    SwCpu = 12,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ResetRequest {
    System,
    ProCpu,
    AppCpu,
}

/// Per-core interrupt matrix: source → CPU interrupt line.
pub struct Intc {
    map: [[u8; 128]; 2],
    /// Level of every interrupt source.
    pub sources: u128,
}

impl Default for Intc {
    fn default() -> Self {
        // Unmapped sources point at line 16 (a CPU-internal timer line on the S3,
        // which the matrix cannot drive), matching IDF's "disabled" convention.
        Intc { map: [[16; 128]; 2], sources: 0 }
    }
}

impl Intc {
    /// Bitmask of external CPU interrupt lines asserted for `core`.
    pub fn lines(&self, core: usize) -> u32 {
        let mut lines = 0u32;
        let mut s = self.sources;
        while s != 0 {
            let n = s.trailing_zeros() as usize;
            s &= s - 1;
            lines |= 1 << (self.map[core][n] & 31);
        }
        lines
    }

    fn read(&self, core: usize, off: u32) -> Option<u32> {
        Some(match off {
            0..=0x188 => self.map[core][(off / 4) as usize] as u32,
            0x18C => self.sources as u32,
            0x190 => (self.sources >> 32) as u32,
            0x194 => (self.sources >> 64) as u32,
            0x198 => (self.sources >> 96) as u32,
            _ => return None,
        })
    }

    fn write(&mut self, core: usize, off: u32, v: u32) {
        if off <= 0x188 {
            self.map[core][(off / 4) as usize] = (v & 31) as u8;
        }
    }
}

#[derive(Default)]
pub struct Uart {
    /// The 128-byte hardware RX FIFO.
    pub rx: VecDeque<u8>,
    /// Bytes the peer has sent that don't fit the FIFO yet (held off by RTS flow control).
    pub backlog: VecDeque<u8>,
    pub raw: u32,
}

/// SOC_UART_FIFO_LEN: the IDF driver's ISR copies at most this many bytes into a
/// buffer of this size, trusting RXFIFO_CNT.
const UART_FIFO_LEN: usize = 128;

impl Uart {
    fn refill(&mut self) {
        while self.rx.len() < UART_FIFO_LEN {
            match self.backlog.pop_front() {
                Some(b) => self.rx.push_back(b),
                None => break,
            }
        }
    }
}

pub struct Periph {
    store: Vec<u32>,
    pub intc: Intc,
    pub systimer: Systimer,
    pub sha: Sha,
    pub i2c: I2c,
    pub uart: [Uart; 3],
    pub aes_irq: bool,
    pub rsa_irq: bool,
    pub gpio_out: u64,
    pub gpio_oe: u64,
    pub gpio_in: u64,
    pub gpio_status: u64,
    /// Bytes the firmware wrote to the USB serial/JTAG console.
    pub console_out: Vec<u8>,
    usb_raw: u32,
    usb_last_sof: u64,
    pub lcd_raw: u32,
    pub reset_request: Option<ResetRequest>,
    pub reset_reason: ResetReason,
    pub wakeup_cause: u32,
    pub rtc_ticks_base: u64,
    rtc_time_latched: u64,
    pub rtc_slow_hz: u64,
    pub mac: [u8; 6],
    rng: u64,
    /// OPI PSRAM mode registers (MR0..MR8).
    psram_mr: [u8; 9],
    seen: std::collections::HashSet<u32>,
}

impl Periph {
    pub fn new() -> Self {
        let mut mr = [0u8; 9];
        mr[1] = 0x0D; // vendor: AP Memory
        mr[2] = 0x03; // density 64 Mbit (8 MB), generation bits 0
        Periph {
            store: vec![0; PERIPH_SIZE / 4],
            intc: Intc::default(),
            systimer: Systimer::default(),
            sha: Sha::default(),
            i2c: I2c::new(0),
            uart: Default::default(),
            aes_irq: false,
            rsa_irq: false,
            gpio_out: 0,
            gpio_oe: 0,
            gpio_in: 0,
            gpio_status: 0,
            console_out: Vec::new(),
            usb_raw: 0,
            usb_last_sof: 0,
            lcd_raw: 0,
            reset_request: None,
            reset_reason: ResetReason::PowerOn,
            wakeup_cause: 0,
            rtc_ticks_base: 0,
            rtc_time_latched: 0,
            rtc_slow_hz: 136_000,
            mac: [0xd8, 0x3b, 0xda, 0x5e, 0x1a, 0x2b],
            rng: 0x9E37_79B9_7F4A_7C15,
            psram_mr: mr,
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
        (RTC_CNTL..RTC_CNTL + 0x400).step_by(4).map(|a| self.store_get(a)).collect()
    }

    pub fn set_rtc_regs(&mut self, regs: &[u32]) {
        for (i, v) in regs.iter().take(0x400 / 4).enumerate() {
            self.store_set(RTC_CNTL + 4 * i as u32, *v);
        }
    }

    /// Reset register state as a chip reset would (RTC domain kept unless power-on).
    pub fn chip_reset(&mut self, keep_rtc: bool) {
        let rtc = self.rtc_regs();
        self.store.fill(0);
        if keep_rtc {
            self.set_rtc_regs(&rtc);
        }
        self.intc = Intc::default();
        self.systimer = Systimer::default();
        self.sha = Sha::default();
        self.i2c = I2c::new(0);
        self.uart = Default::default();
        self.aes_irq = false;
        self.rsa_irq = false;
        self.gpio_out = 0;
        self.gpio_oe = 0;
        self.gpio_status = 0;
        self.usb_raw = 0;
        self.lcd_raw = 0;
        self.reset_request = None;
        // Core 1 comes out of reset held in reset, clock-gated.
        self.store_set(SYSTEM, 1 << 2);
        self.store_set(SYSTEM + 0x60, 1); // SYSCLK_CONF: XTAL
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
            0x44 => u32::from_le_bytes([m[5], m[4], m[3], m[2]]),
            0x48 => u16::from_le_bytes([m[1], m[0]]) as u32,
            // wafer version minor = 2 (v0.2), blk version minor = 1
            0x50 => 2 << 18 | 1 << 24,
            0x1D0 => 1, // STATUS: idle
            0x1D4 => 0, // CMD: read/program complete
            _ => return None,
        })
    }

    /// Whether the APP CPU is allowed to run (SYSTEM_CORE_1_CONTROL_0).
    pub fn core1_running(&self) -> bool {
        let c = self.store_get(SYSTEM);
        c & (1 << 2) == 0 && c & (1 << 1) != 0 && c & 1 == 0
    }
}

impl Default for Periph {
    fn default() -> Self {
        Self::new()
    }
}

fn uart_index(a: u32) -> Option<(usize, u32)> {
    match a & !0xFFF {
        UART0 => Some((0, a - UART0)),
        UART1 => Some((1, a - UART1)),
        UART2 => Some((2, a - UART2)),
        _ => None,
    }
}

impl S3Bus {
    pub fn periph_read(&mut self, a: u32) -> u32 {
        let now = self.clock.ns();
        let stored = self.p.store_get(a);
        if let Some((u, off)) = uart_index(a) {
            return self.uart_read(u, off, stored);
        }
        match a {
            // ---- SPI flash controller ----
            _ if a == SPI1 || a == SPI0 => 0,
            _ if a == SPI1 + 0x54 || a == SPI0 + 0x54 => 0,
            _ if a == SPI1 + 0xA4 => 0,

            // ---- GPSPI2: transfers complete instantly ----
            _ if a == SPI2 => stored & !(1 << 24 | 1 << 23), // USR/UPDATE done

            // ---- GPIO ----
            _ if a == GPIO + 0x04 => self.p.gpio_out as u32,
            _ if a == GPIO + 0x10 => (self.p.gpio_out >> 32) as u32,
            _ if a == GPIO + 0x20 => self.p.gpio_oe as u32,
            _ if a == GPIO + 0x2C => (self.p.gpio_oe >> 32) as u32,
            _ if a == GPIO + 0x38 => 0x08, // strapping: SPI boot (GPIO0 high)
            _ if a == GPIO + 0x3C || a == GPIO + 0x40 => {
                self.gpio_sample(now);
                if a == GPIO + 0x3C { self.p.gpio_in as u32 } else { (self.p.gpio_in >> 32) as u32 }
            }
            _ if a == GPIO + 0x44 => self.p.gpio_status as u32,
            _ if a == GPIO + 0x50 => (self.p.gpio_status >> 32) as u32,
            _ if a == GPIO + 0x5C => (self.p.gpio_status & self.gpio_int_mask(0)) as u32,
            _ if a == GPIO + 0x60 => ((self.p.gpio_status & self.gpio_int_mask(0)) >> 32) as u32,
            _ if a == GPIO + 0x68 => (self.p.gpio_status & self.gpio_int_mask(1)) as u32,
            _ if a == GPIO + 0x6C => ((self.p.gpio_status & self.gpio_int_mask(1)) >> 32) as u32,

            // ---- RTC_CNTL / eFuse ----
            _ if a == RTC_CNTL + 0x0C => stored & !(1 << 31) | 1 << 30,
            _ if a == RTC_CNTL + 0x10 => self.p.rtc_time_latched as u32,
            _ if a == RTC_CNTL + 0x14 => (self.p.rtc_time_latched >> 32) as u32,
            _ if a == RTC_CNTL + 0x38 => {
                let r = self.p.reset_reason as u32;
                stored & !0xfff | r | r << 6
            }
            _ if a == RTC_CNTL + 0x130 => self.p.wakeup_cause,
            _ if a == RTC_CNTL + 0x44 || a == RTC_CNTL + 0x48 => 0,
            _ if (EFUSE..EFUSE + 0x200).contains(&a) => self.p.efuse_word(a - EFUSE).unwrap_or(stored),

            // ---- TIMG: RTC slow clock calibration ----
            _ if a == TIMG0 + 0x68 || a == TIMG1 + 0x68 => stored | 1 << 15,
            _ if a == TIMG0 + 0x6C || a == TIMG1 + 0x6C => {
                let max = (self.p.store_get(a - 4) >> 16) & 0x7fff;
                ((max as u64 * 40_000_000 / self.p.rtc_slow_hz) as u32) << 7
            }

            // ---- SYSTIMER ----
            _ if (SYSTIMER..SYSTIMER + 0x100).contains(&a) => {
                let v = self.p.systimer.read(a - SYSTIMER, now);
                self.irq_dirty = true;
                v
            }

            // ---- I2C0 ----
            _ if a == I2C0 + 0x08 => {
                let i = &self.p.i2c;
                (i.rx.len() as u32 & 0x3f) << 8 | (i.tx.len() as u32 & 0x3f) << 18
            }
            _ if a == I2C0 + 0x1C => self.p.i2c.rx.pop_front().unwrap_or(0) as u32,
            _ if a == I2C0 + 0x20 => self.p.i2c.raw,
            _ if a == I2C0 + 0x2C => self.p.i2c.raw & self.p.store_get(I2C0 + 0x28),
            _ if a == I2C0 + 0x80 => stored & !1,
            _ if a == I2C0 + 0x04 => stored & !(1 << 5 | 1 << 11),

            _ if a == RNG => self.p.rand(),
            // SENS SAR_MEAS1/2_CTRL2: RTC ADC one-shot conversions finish instantly (mid-scale)
            0x6000_880C | 0x6000_8830 => stored & !0xffff | 1 << 16 | 0x800,
            // I2C_MST_ANA_CONF0: BBPLL calibration finishes instantly
            0x6000_E040 => stored | 1 << 24,

            // ---- USB serial/JTAG: the console ----
            _ if a == USB_JTAG + 0x04 => stored & !1 | 1 << 1, // IN endpoint always has room
            _ if a == USB_JTAG + 0x08 => self.usb_raw(),
            _ if a == USB_JTAG + 0x0C => self.usb_raw() & self.p.store_get(USB_JTAG + 0x10),
            _ if a == USB_JTAG + 0x24 => ((now / 1_000_000) & 0x7ff) as u32, // SOF frame number (1 kHz)

            // ---- crypto ----
            _ if (SHA_BASE..SHA_BASE + 0x100).contains(&a) => self.p.sha.read(a - SHA_BASE),
            _ if a == RSA + 0x808 || a == RSA + 0x818 => 1,
            _ if (GDMA..GDMA + GDMA_CH_STRIDE * GDMA_CHANNELS).contains(&a) => {
                let off = (a - GDMA) % GDMA_CH_STRIDE;
                match off {
                    0x0C | 0x6C => self.p.store_get(a - 4) & self.p.store_get(a + 4), // INT_ST = RAW & ENA
                    _ => stored,
                }
            }

            // ---- LCD_CAM ----
            _ if a == LCD_CAM + 0x14 => stored & !(1 << 27 | 1 << 20 | 1 << 28),
            _ if a == LCD_CAM + 0x68 => self.p.lcd_raw,
            _ if a == LCD_CAM + 0x6C => self.p.lcd_raw & self.p.store_get(LCD_CAM + 0x64),

            // ---- SYSTEM ----
            _ if a == SYSTEM + 0x50 => stored | 1 << 31,

            // ---- interrupt matrix (core 0 at +0, core 1 at +0x800) ----
            _ if (INTC..INTC + 0x800).contains(&a) => self.p.intc.read(0, a - INTC).unwrap_or(stored),
            _ if (INTC + 0x800..INTC + 0x1000).contains(&a) => self.p.intc.read(1, a - INTC - 0x800).unwrap_or(stored),

            // ---- EXTMEM: cache operations complete instantly ----
            _ if (MMU_TABLE..MMU_TABLE + 4 * MMU_ENTRIES as u32).contains(&a) => {
                self.mmu[((a - MMU_TABLE) / 4) as usize]
            }
            _ if (EXTMEM..EXTMEM + 0x1000).contains(&a) => match a - EXTMEM {
                0x1C | 0x7C => stored | 1 << 2,
                0x28 => stored | 1 << 3,
                0x34 | 0x40 | 0x88 | 0x94 => stored | 1 << 1,
                0x4C | 0xA0 => stored | 1 << 3,
                0x150 | 0x154 => stored & !4 | (stored & 1) << 2, // FREEZE_DONE follows ENA
                0x130 => 0x1 << 12 | 0x1,                         // CACHE_STATE: both caches idle
                _ => stored,
            },
            _ => {
                if log::log_enabled!(log::Level::Trace) && self.p.seen.insert(a) {
                    log::trace!("s3 periph read  {a:#010x} (unmodelled) = {stored:#x}");
                }
                stored
            }
        }
    }

    pub fn periph_write(&mut self, a: u32, v: u32) {
        let now = self.clock.ns();
        self.p.store_set(a, v);
        if let Some((u, off)) = uart_index(a) {
            return self.uart_write(u, off, v, now);
        }
        match a {
            _ if a == SPI1 => self.spi1_command(v),
            _ if a == SPI2 && v & (1 << 24) != 0 => self.spi2_transfer(now),

            // GPIO
            _ if a == GPIO + 0x04 => self.gpio_set_out(self.p.gpio_out & !0xffff_ffff | v as u64, now),
            _ if a == GPIO + 0x08 => self.gpio_set_out(self.p.gpio_out | v as u64, now),
            _ if a == GPIO + 0x0C => self.gpio_set_out(self.p.gpio_out & !(v as u64), now),
            _ if a == GPIO + 0x10 => self.gpio_set_out(self.p.gpio_out & 0xffff_ffff | (v as u64) << 32, now),
            _ if a == GPIO + 0x14 => self.gpio_set_out(self.p.gpio_out | (v as u64) << 32, now),
            _ if a == GPIO + 0x18 => self.gpio_set_out(self.p.gpio_out & !((v as u64) << 32), now),
            _ if a == GPIO + 0x20 => self.gpio_set_oe(self.p.gpio_oe & !0xffff_ffff | v as u64, now),
            _ if a == GPIO + 0x24 => self.gpio_set_oe(self.p.gpio_oe | v as u64, now),
            _ if a == GPIO + 0x28 => self.gpio_set_oe(self.p.gpio_oe & !(v as u64), now),
            _ if a == GPIO + 0x2C => self.gpio_set_oe(self.p.gpio_oe & 0xffff_ffff | (v as u64) << 32, now),
            _ if a == GPIO + 0x30 => self.gpio_set_oe(self.p.gpio_oe | (v as u64) << 32, now),
            _ if a == GPIO + 0x34 => self.gpio_set_oe(self.p.gpio_oe & !((v as u64) << 32), now),
            _ if a == GPIO + 0x44 => self.gpio_set_status(self.p.gpio_status & !0xffff_ffff | v as u64),
            _ if a == GPIO + 0x48 => self.gpio_set_status(self.p.gpio_status | v as u64),
            _ if a == GPIO + 0x4C => self.gpio_set_status(self.p.gpio_status & !(v as u64)),
            _ if a == GPIO + 0x50 => self.gpio_set_status(self.p.gpio_status & 0xffff_ffff | (v as u64) << 32),
            _ if a == GPIO + 0x54 => self.gpio_set_status(self.p.gpio_status | (v as u64) << 32),
            _ if a == GPIO + 0x58 => self.gpio_set_status(self.p.gpio_status & !((v as u64) << 32)),
            _ if (GPIO + 0x74..=GPIO + 0x134).contains(&a) => {
                // PINn: open-drain / interrupt config may change what the board sees
                self.board.gpio_out(now, self.gpio_effective_out(), self.gpio_effective_oe());
                self.gpio_sample(now);
                self.irq_dirty = true;
            }

            // RTC_CNTL
            _ if a == RTC_CNTL => {
                if v & (1 << 31) != 0 {
                    self.p.reset_request = Some(ResetRequest::System);
                } else if v & (1 << 5) != 0 {
                    self.p.reset_request = Some(ResetRequest::ProCpu);
                } else if v & (1 << 4) != 0 {
                    self.p.reset_request = Some(ResetRequest::AppCpu);
                }
                self.p.store_set(a, v & !(1 << 31 | 1 << 5 | 1 << 4)); // self-clearing
            }
            _ if a == RTC_CNTL + 0x0C && v & (1 << 31) != 0 => {
                self.p.rtc_time_latched = self.rtc_ticks(now);
            }

            _ if (SYSTIMER..SYSTIMER + 0x100).contains(&a) => {
                if self.p.systimer.write(a - SYSTIMER, v, now) {
                    self.irq_dirty = true;
                }
            }

            // I2C0
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

            // USB serial/JTAG console
            _ if a == USB_JTAG => self.p.console_out.push(v as u8),
            _ if a == USB_JTAG + 0x14 => {
                self.p.usb_raw &= !v;
                self.irq_dirty = true;
            }
            _ if a == USB_JTAG + 0x10 => self.irq_dirty = true,

            // crypto
            _ if (SHA_BASE..SHA_BASE + 0x100).contains(&a) => {
                self.p.sha.write(a - SHA_BASE, v);
                if a - SHA_BASE == 0x1C || a - SHA_BASE == 0x20 {
                    self.sha_dma(a - SHA_BASE == 0x1C);
                }
                self.irq_dirty = true;
            }
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
            _ if (GDMA..GDMA + GDMA_CH_STRIDE * GDMA_CHANNELS).contains(&a) => {
                let off = (a - GDMA) % GDMA_CH_STRIDE;
                if off == 0x14 || off == 0x74 {
                    // INT_CLR
                    let raw = self.p.store_get(a - 0xC);
                    self.p.store_set(a - 0xC, raw & !v);
                }
                self.irq_dirty = true;
            }

            // LCD_CAM
            _ if a == LCD_CAM + 0x14 => {
                if v & (1 << 27) != 0 {
                    self.lcd_start(now);
                }
            }
            _ if a == LCD_CAM + 0x70 => {
                self.p.lcd_raw &= !v;
                self.irq_dirty = true;
            }
            _ if a == LCD_CAM + 0x64 => self.irq_dirty = true,

            // SYSTEM
            _ if a == SYSTEM + 0x10 || a == SYSTEM + 0x60 => self.update_cpu_clock(),
            _ if (SYSTEM + 0x30..=SYSTEM + 0x3C).contains(&a) => {
                let n = ((a - SYSTEM - 0x30) / 4) as usize;
                let bit = 1u128 << (src::FROM_CPU0 + n);
                if v & 1 != 0 {
                    self.p.intc.sources |= bit;
                } else {
                    self.p.intc.sources &= !bit;
                }
                self.irq_dirty = true;
            }

            _ if (INTC..INTC + 0x800).contains(&a) => {
                self.p.intc.write(0, a - INTC, v);
                self.irq_dirty = true;
            }
            _ if (INTC + 0x800..INTC + 0x1000).contains(&a) => {
                self.p.intc.write(1, a - INTC - 0x800, v);
                self.irq_dirty = true;
            }
            _ if (MMU_TABLE..MMU_TABLE + 4 * MMU_ENTRIES as u32).contains(&a) => {
                self.mmu[((a - MMU_TABLE) / 4) as usize] = v & 0xffff;
            }
            _ => {
                if log::log_enabled!(log::Level::Trace) && self.p.seen.insert(a | 1) {
                    log::trace!("s3 periph write {a:#010x} (unmodelled) = {v:#x}");
                }
            }
        }
    }

    // ---- interrupts --------------------------------------------------------------------------

    pub fn refresh_sources(&mut self) {
        let mut s = self.p.intc.sources;
        let mut set = |n: usize, on: bool| {
            if on { s |= 1 << n } else { s &= !(1 << n) }
        };
        for u in 0..2 {
            let base = [UART0, UART1][u];
            set(src::UART0 + u, self.uart_raw(u) & self.p.store_get(base + 0x0C) != 0);
        }
        let st = self.p.systimer.irq_lines();
        for i in 0..3 {
            set(src::SYSTIMER_TARGET0 + i, st & (1 << i) != 0);
        }
        set(src::GPIO, self.p.gpio_status & (self.gpio_int_mask(0) | self.gpio_int_mask(1)) != 0);
        set(src::SHA, self.p.sha.irq());
        set(src::AES, self.p.aes_irq && self.p.store_get(AES + 0xB0) & 1 != 0);
        set(src::RSA, self.p.rsa_irq && self.p.store_get(RSA + 0x82C) & 1 != 0);
        set(src::I2C_EXT0, self.p.i2c.raw & self.p.store_get(I2C0 + 0x28) != 0);
        set(src::LCD_CAM, self.p.lcd_raw & self.p.store_get(LCD_CAM + 0x64) != 0);
        set(src::USB_DEVICE, self.usb_raw() & self.p.store_get(USB_JTAG + 0x10) != 0);
        for ch in 0..GDMA_CHANNELS {
            let b = GDMA + ch * GDMA_CH_STRIDE;
            set(src::DMA_IN_CH0 + ch as usize, self.p.store_get(b + 0x08) & self.p.store_get(b + 0x10) != 0);
            set(src::DMA_OUT_CH0 + ch as usize, self.p.store_get(b + 0x68) & self.p.store_get(b + 0x70) != 0);
        }
        self.p.intc.sources = s;
    }

    // ---- UART (with receive path: the modem on TRMNL X) ---------------------------------------

    fn uart_raw(&self, u: usize) -> u32 {
        let base = [UART0, UART1, UART2][u];
        let full_thrhd = (self.p.store_get(base + 0x24) & 0x3ff).max(1) as usize;
        let mut raw = self.p.uart[u].raw | 1 << 1; // TXFIFO_EMPTY
        if self.p.uart[u].rx.len() >= full_thrhd {
            raw |= 1; // RXFIFO_FULL
        }
        raw
    }

    fn uart_read(&mut self, u: usize, off: u32, stored: u32) -> u32 {
        match off {
            0x00 => {
                let b = self.p.uart[u].rx.pop_front().unwrap_or(0) as u32;
                self.p.uart[u].refill();
                if self.p.uart[u].rx.is_empty() {
                    self.p.uart[u].raw &= !(1 << 8); // RXFIFO_TOUT cleared once drained
                }
                self.irq_dirty = true;
                b
            }
            0x04 => self.uart_raw(u),
            0x08 => {
                let base = [UART0, UART1, UART2][u];
                self.uart_raw(u) & self.p.store_get(base + 0x0C)
            }
            0x1C => self.p.uart[u].rx.len() as u32 | 1 << 29, // rxfifo_cnt, tx empty
            0x80 => stored & !(1 << 31),
            0x6C | 0x68 => 0,
            _ => stored,
        }
    }

    fn uart_write(&mut self, u: usize, off: u32, v: u32, now: u64) {
        match off {
            0x00 => {
                self.board.uart_tx(now, u as u8, &[v as u8]);
                self.p.uart[u].raw |= 1 << 14; // TX_DONE
                self.irq_dirty = true;
            }
            0x10 => {
                self.p.uart[u].raw &= !v;
                self.irq_dirty = true;
            }
            0x0C => self.irq_dirty = true,
            0x20
                // CONF0: RXFIFO_RST (bit 17) / TXFIFO_RST (bit 18)
                if v & (1 << 17) != 0 => {
                    self.p.uart[u].rx.clear();
                    self.p.uart[u].refill();
                }
            _ => {}
        }
    }

    /// Pull bytes the board delivers to the UART RX pins.
    pub fn uart_poll(&mut self, now: u64) {
        for u in 0..3 {
            let data = self.board.uart_rx(now, u as u8);
            let uart = &mut self.p.uart[u];
            uart.backlog.extend(data);
            uart.refill();
            // Without a precise idle model, (re)raise RX timeout whenever the FIFO holds
            // data: the driver clears it after each 128-byte batch while more is queued.
            if !uart.rx.is_empty() && uart.raw & 1 << 8 == 0 {
                uart.raw |= 1 << 8;
                self.irq_dirty = true;
            }
        }
    }

    // ---- USB serial/JTAG ----------------------------------------------------------------------

    /// A USB host is attached: start-of-frame every millisecond. Arduino's HWCDC
    /// only sends console output while it sees SOF interrupts.
    pub fn usb_sof(&mut self, now: u64) {
        let frame = now / 1_000_000;
        if frame != self.p.usb_last_sof {
            self.p.usb_last_sof = frame;
            self.p.usb_raw |= 1 << 1;
            self.irq_dirty = true;
        }
    }

    fn usb_raw(&self) -> u32 {
        // SERIAL_IN_EMPTY (bit 3): the IN FIFO is always drained immediately.
        self.p.usb_raw | 1 << 3
    }

    // ---- clocks -------------------------------------------------------------------------------

    fn update_cpu_clock(&mut self) {
        let sysclk = self.p.store_get(SYSTEM + 0x60);
        let per = self.p.store_get(SYSTEM + 0x10);
        let hz = match (sysclk >> 10) & 3 {
            0 => 40_000_000 / ((sysclk & 0x3ff) as u64 + 1),
            1 => match per & 3 {
                0 => 80_000_000,
                1 => 160_000_000,
                _ => 240_000_000,
            },
            _ => 17_500_000 / ((sysclk & 0x3ff) as u64 + 1),
        };
        self.clock.set_hz(hz);
    }

    /// Absolute ns at which the RTC sleep timer (SLP_TIMER0/1, in RTC ticks) fires.
    pub fn rtc_sleep_target_ns(&self, now: u64) -> Option<u64> {
        let target =
            (self.p.store_get(RTC_CNTL + 0x08) as u64 & 0xffff) << 32 | self.p.store_get(RTC_CNTL + 0x04) as u64;
        let cur = self.rtc_ticks(now);
        let dt = target.checked_sub(cur)?;
        Some(now + (dt as u128 * 1_000_000_000 / self.p.rtc_slow_hz as u128) as u64)
    }

    /// A pin configured as a light-sleep GPIO wake source (PINn WAKEUP_ENABLE with a
    /// level interrupt type) is at its wake level.
    pub fn gpio_wake_triggered(&self) -> bool {
        (0..49u32).any(|pin| {
            let r = self.pin_reg(pin);
            let level = self.p.gpio_in >> pin & 1 != 0;
            r & (1 << 10) != 0
                && match (r >> 7) & 7 {
                    4 => !level,
                    5 => level,
                    _ => false,
                }
        })
    }

    pub fn rtc_ticks(&self, now: u64) -> u64 {
        self.p.rtc_ticks_base + (now as u128 * self.p.rtc_slow_hz as u128 / 1_000_000_000) as u64
    }

    pub fn systimer_update(&mut self, now: u64) {
        if self.p.systimer.update(now) {
            self.irq_dirty = true;
        }
    }

    // ---- GPIO (49 pins, two banks) --------------------------------------------------------------

    fn pin_reg(&self, pin: u32) -> u32 {
        self.p.store_get(GPIO + 0x74 + 4 * pin)
    }

    /// Pins whose interrupt is enabled for `core` (PINn INT_ENA: bit 13 = PRO, bit 15 = APP).
    fn gpio_int_mask(&self, core: usize) -> u64 {
        let mut m = 0;
        for pin in 0..49u32 {
            let r = self.pin_reg(pin);
            let ena = (r >> 13) & 0x1f;
            let wants = if core == 0 { ena & 0b00101 != 0 } else { ena & 0b01010 != 0 };
            if wants && (r >> 7) & 7 != 0 {
                m |= 1 << pin;
            }
        }
        m
    }

    /// Open-drain pins (PINn PAD_DRIVER, bit 2) only ever drive low.
    fn open_drain_mask(&self) -> u64 {
        let mut m = 0;
        for pin in 0..49u32 {
            if self.pin_reg(pin) & (1 << 2) != 0 {
                m |= 1 << pin;
            }
        }
        m
    }

    fn gpio_effective_oe(&self) -> u64 {
        // An open-drain pin outputting 1 is released (not driven).
        self.p.gpio_oe & !(self.open_drain_mask() & self.p.gpio_out)
    }

    fn gpio_effective_out(&self) -> u64 {
        self.p.gpio_out
    }

    fn gpio_set_out(&mut self, v: u64, now: u64) {
        let v = v & ((1 << 49) - 1);
        if v != self.p.gpio_out {
            self.p.gpio_out = v;
            self.board.gpio_out(now, self.gpio_effective_out(), self.gpio_effective_oe());
        }
    }

    fn gpio_set_oe(&mut self, v: u64, now: u64) {
        let v = v & ((1 << 49) - 1);
        if v != self.p.gpio_oe {
            self.p.gpio_oe = v;
            self.board.gpio_out(now, self.gpio_effective_out(), self.gpio_effective_oe());
        }
    }

    fn gpio_set_status(&mut self, v: u64) {
        self.p.gpio_status = v;
        let now = self.clock.ns();
        self.gpio_sample(now);
        self.irq_dirty = true;
    }

    /// Sample input levels and latch GPIO interrupt status.
    pub fn gpio_sample(&mut self, now: u64) {
        let (lv, driven) = self.board.gpio_in(now);
        let oe = self.gpio_effective_oe();
        let mut input = 0u64;
        for pin in 0..49u32 {
            let bit = 1u64 << pin;
            let level = if driven & bit != 0 {
                // Open-drain wired-AND: a device pulling low wins over a released pin.
                lv & bit != 0 && !(oe & bit != 0 && self.p.gpio_out & bit == 0)
            } else if oe & bit != 0 {
                self.p.gpio_out & bit != 0
            } else {
                // IO_MUX FUN_WPU (bit 8) pulls up; otherwise idle low.
                self.p.store_get(IO_MUX + 0x04 + 4 * pin) & (1 << 8) != 0 || self.open_drain_mask() & bit != 0
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
        for pin in 0..49u32 {
            let bit = 1u64 << pin;
            let hit = match (self.pin_reg(pin) >> 7) & 7 {
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

    // ---- GDMA (S3 layout: in/out register sets per channel) -------------------------------------

    fn gdma_channel_for(&self, peri: u32, out_dir: bool) -> Option<u32> {
        let sel = if out_dir { 0xA8 } else { 0x48 };
        (0..GDMA_CHANNELS).find(|&ch| self.p.store_get(GDMA + ch * GDMA_CH_STRIDE + sel) & 0x3f == peri)
    }

    fn gdma_desc_addr(link_reg: u32) -> u32 {
        // Link registers hold the low 20 bits of an internal-RAM descriptor address.
        0x3FC0_0000 | (link_reg & 0xfffff)
    }

    /// Collect the bytes of an out-link (memory -> peripheral).
    fn gdma_gather(&mut self, ch: u32) -> Vec<u8> {
        let b = GDMA + ch * GDMA_CH_STRIDE;
        let mut addr = Self::gdma_desc_addr(self.p.store_get(b + 0x80));
        let mut out = Vec::new();
        for _ in 0..4096 {
            let Some(w0) = self.peek32(addr) else { break };
            let buf = self.peek32(addr + 4).unwrap_or(0);
            let next = self.peek32(addr + 8).unwrap_or(0);
            let len = ((w0 >> 12) & 0xfff) as usize;
            if let Some(d) = self.peek_bytes(buf, len) {
                out.extend_from_slice(&d);
            }
            self.poke32(addr, w0 & !(1 << 31));
            if w0 & (1 << 30) != 0 || next == 0 {
                break;
            }
            addr = next;
        }
        // OUT_DONE (bit 0), OUT_EOF (bit 1), OUT_TOTAL_EOF (bit 3)
        let raw = self.p.store_get(b + 0x68);
        self.p.store_set(b + 0x68, raw | 1 | 1 << 1 | 1 << 3);
        self.irq_dirty = true;
        out
    }

    /// Scatter bytes into an in-link (peripheral -> memory).
    fn gdma_scatter(&mut self, ch: u32, data: &[u8]) {
        let b = GDMA + ch * GDMA_CH_STRIDE;
        let mut addr = Self::gdma_desc_addr(self.p.store_get(b + 0x20));
        let mut pos = 0;
        for _ in 0..4096 {
            let Some(w0) = self.peek32(addr) else { break };
            let buf = self.peek32(addr + 4).unwrap_or(0);
            let next = self.peek32(addr + 8).unwrap_or(0);
            let size = (w0 & 0xfff) as usize;
            let n = size.min(data.len() - pos);
            self.load_bytes(buf, &data[pos..pos + n]);
            pos += n;
            let last = pos >= data.len() || next == 0;
            let mut nw = (w0 & 0xfff) | (n as u32) << 12;
            if last {
                nw |= 1 << 30;
            }
            self.poke32(addr, nw);
            if last {
                break;
            }
            addr = next;
        }
        // IN_DONE (bit 0), IN_SUC_EOF (bit 1)
        let raw = self.p.store_get(b + 0x08);
        self.p.store_set(b + 0x08, raw | 1 | 1 << 1);
        self.irq_dirty = true;
    }

    // ---- LCD_CAM (i80 mode, fed by GDMA) --------------------------------------------------------

    fn lcd_start(&mut self, now: u64) {
        let user = self.p.store_get(LCD_CAM + 0x14);
        if user & (1 << 24) != 0 {
            // Data phase from DMA
            if let Some(ch) = self.gdma_channel_for(PERI_LCD_CAM, true) {
                let data = self.gdma_gather(ch);
                // With LCD_ALWAYS_OUT_EN the data phase lasts until the DMA chain's EOF;
                // otherwise it is DOUT_CYCLELEN+1 bus cycles.
                let n = if user & (1 << 13) != 0 {
                    data.len()
                } else {
                    let cycles = (user & 0x1fff) as usize + 1;
                    data.len().min(if user & (1 << 23) != 0 { cycles * 2 } else { cycles })
                };
                self.board.lcd_transfer(now, &data[..n]);
            }
        }
        // TRANS_DONE: the transfer takes microseconds of guest time; report it done
        // right away (the firmware waits for it via the interrupt / raw status).
        self.p.lcd_raw |= 1 << 1;
        self.irq_dirty = true;
    }

    // ---- crypto (same engines as C3, S3 GDMA wiring) ---------------------------------------------

    fn reg_bytes(&self, base: u32, n: usize) -> Vec<u8> {
        (0..n as u32 / 4).flat_map(|i| self.p.store_get(base + 4 * i).to_le_bytes()).collect()
    }

    fn set_reg_bytes(&mut self, base: u32, b: &[u8]) {
        for (i, c) in b.chunks(4).enumerate() {
            let mut w = [0u8; 4];
            w[..c.len()].copy_from_slice(c);
            self.p.store_set(base + 4 * i as u32, u32::from_le_bytes(w));
        }
    }

    fn aes_trigger(&mut self) {
        let mode = self.p.store_get(AES + 0x40) & 7;
        let key_len = if mode & 2 != 0 { 32 } else { 16 };
        let decrypt = mode & 4 != 0;
        let key = self.reg_bytes(AES, key_len);
        if self.p.store_get(AES + 0x90) & 1 == 0 {
            let mut block = self.reg_bytes(AES + 0x20, 16);
            aes_block(&key, decrypt, &mut block);
            self.set_reg_bytes(AES + 0x30, &block);
            self.p.store_set(AES + 0x4C, 0);
            return;
        }
        let block_mode = self.p.store_get(AES + 0x94) & 7;
        let nblocks = self.p.store_get(AES + 0x98) as usize;
        let inc32 = self.p.store_get(AES + 0x9C) & 1 == 0;
        let (Some(out_ch), Some(in_ch)) =
            (self.gdma_channel_for(PERI_AES, true), self.gdma_channel_for(PERI_AES, false))
        else {
            log::warn!("AES DMA started without GDMA channels");
            self.p.store_set(AES + 0x4C, 2);
            return;
        };
        let mut data = self.gdma_gather(out_ch);
        data.resize(nblocks * 16, 0);
        let mut iv: [u8; 16] = self.reg_bytes(AES + 0x50, 16).try_into().unwrap();
        aes_blocks(&key, decrypt, block_mode, inc32, &mut iv, &mut data);
        self.set_reg_bytes(AES + 0x50, &iv);
        self.gdma_scatter(in_ch, &data);
        self.p.store_set(AES + 0x4C, 2);
        self.p.aes_irq = true;
        self.irq_dirty = true;
    }

    fn sha_dma(&mut self, start: bool) {
        let Some(ch) = self.gdma_channel_for(PERI_SHA, true) else {
            log::warn!("SHA DMA started without a GDMA channel");
            return;
        };
        let data = self.gdma_gather(ch);
        let n = self.p.sha.block_num as usize;
        if start {
            self.p.sha.init();
        }
        let block = self.p.sha.block_len();
        for b in data.chunks_exact(block).take(n) {
            self.p.sha.compress_bytes(b);
        }
        self.irq_dirty = true;
    }

    fn rsa_start(&mut self, which: u32) {
        let len = (self.p.store_get(RSA + 0x804) & 0x7f) as usize + 1;
        let mut mem = [0u32; 0x200];
        for (i, w) in mem.iter_mut().enumerate() {
            *w = self.p.store_get(RSA + 4 * i as u32);
        }
        rsa_op(which, len, &mut mem);
        for (i, w) in mem.iter().enumerate() {
            self.p.store_set(RSA + 4 * i as u32, *w);
        }
        self.p.rsa_irq = true;
        self.irq_dirty = true;
    }

    // ---- SPI1: flash (CS0) and octal PSRAM (CS1) --------------------------------------------------

    fn spi1_command(&mut self, cmd: u32) {
        let addr_reg = self.p.store_get(SPI1 + 0x04);
        let w = |p: &Periph, i: usize| p.store_get(SPI1 + 0x58 + 4 * i as u32);
        let buf = |p: &Periph, n: usize| -> Vec<u8> {
            (0..n.min(64)).map(|i| (w(p, i / 4) >> (8 * (i % 4))) as u8).collect()
        };
        // SPI_MEM_MISC_REG (0x34): CS0_DIS bit 0, CS1_DIS bit 1
        let misc = self.p.store_get(SPI1 + 0x34);
        let psram_selected = misc & 1 != 0 && misc & 2 == 0;
        if cmd & (1 << 18) != 0 {
            let user = self.p.store_get(SPI1 + 0x18);
            let user1 = self.p.store_get(SPI1 + 0x1C);
            let user2 = self.p.store_get(SPI1 + 0x20);
            let opcode = user2 & 0xffff;
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
            let _ = user1;
            let rx = if psram_selected {
                self.psram_transact(opcode, addr.unwrap_or(0), &mosi, miso_bytes.min(64))
            } else {
                self.flash.transact(opcode as u8, addr, &mosi, miso_bytes.min(64))
            };
            self.spi1_fill(&rx);
            return;
        }
        let f = &mut self.flash;
        if cmd & (1 << 31) != 0 {
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
        }
    }

    // ---- GPSPI2 (CPU-driven transfers through W0..W15, as on the ESP32-C3) ----------------

    fn spi2_transfer(&mut self, now: u64) {
        let bits = (self.p.store_get(SPI2 + 0x1C) & 0x3ffff) + 1;
        let user = self.p.store_get(SPI2 + 0x10);
        let n = (bits as usize).div_ceil(8).min(64);
        let out: Vec<u8> =
            (0..n).map(|i| (self.p.store_get(SPI2 + 0x98 + 4 * (i as u32 / 4)) >> (8 * (i % 4))) as u8).collect();
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
        self.p.store_set(SPI2 + 0x3C, raw | 1 << 12); // TRANS_DONE
    }

    /// AP Memory octal PSRAM command set (16-bit opcodes in OPI mode).
    fn psram_transact(&mut self, opcode: u32, addr: u32, mosi: &[u8], miso_len: usize) -> Vec<u8> {
        let a = addr as usize & (self.psram.len() - 1);
        match opcode & 0xff {
            0x40 => {
                // mode register read from register `addr` on: IDF reads MR0 and MR1 (then
                // MR2 and MR3) as one 16-bit transfer
                let r = (addr as usize) & 0xff;
                (0..miso_len).map(|i| *self.p.psram_mr.get(r + i).unwrap_or(&0)).collect()
            }
            0xC0 => {
                if let Some(&v) = mosi.first() {
                    let r = (addr as usize) & 0xff;
                    if r < self.p.psram_mr.len() && r != 1 && r != 2 {
                        self.p.psram_mr[r] = v;
                    }
                }
                vec![]
            }
            0x00 | 0x20 => (0..miso_len).map(|i| self.psram[(a + i) & (self.psram.len() - 1)]).collect(),
            0x80 | 0xA0 => {
                for (i, &b) in mosi.iter().enumerate() {
                    let k = (a + i) & (self.psram.len() - 1);
                    self.psram[k] = b;
                }
                vec![]
            }
            _ => vec![0; miso_len],
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
