//! ESP32-C5 peripheral models. Every peripheral register is backed by a flat register file
//! (so unmodelled registers read back what was written); the models below add side effects
//! and status bits on top. Register offsets are from IDF 5.5's `soc/esp32c5/register`.

use std::collections::HashMap;

use super::bus::{C5Bus, CLIC_BASE, CLIC_SIZE, MMU_ENTRIES, PERIPH_BASE, PERIPH_SIZE, XTAL_HZ};
use super::crypto::{AES, DMA, ECC, ECDSA, RSA};
use crate::periph::i2c::I2c;
use crate::periph::sha::Sha;
use crate::periph::systimer::Systimer;

/// Interrupt sources (soc/interrupts.h).
pub mod src {
    pub const FROM_CPU0: usize = 23;
    pub const GPIO: usize = 31;
    pub const UART0: usize = 47;
    pub const UART1: usize = 48;
    pub const USB_JTAG: usize = 54;
    pub const I2C_EXT0: usize = 56;
    pub const PARL_IO_TX: usize = 67;
    pub const SYSTIMER_TARGET0: usize = 61;
    pub const DMA_IN_CH0: usize = 71;
    pub const DMA_OUT_CH0: usize = 74;
    pub const AES: usize = 78;
    pub const SHA: usize = 79;
    pub const RSA: usize = 80;
    pub const ECC: usize = 81;
    pub const ECDSA: usize = 82;
    pub const COUNT: usize = 84;
}

pub const UART0: u32 = 0x6000_0000;
pub const UART1: u32 = 0x6000_1000;
pub const SPIMEM0: u32 = 0x6000_2000;
pub const SPIMEM1: u32 = 0x6000_3000;
pub const I2C0: u32 = 0x6000_4000;
pub const TIMG0: u32 = 0x6000_8000;
pub const TIMG1: u32 = 0x6000_9000;
pub const SYSTIMER: u32 = 0x6000_A000;
pub const USB_JTAG: u32 = 0x6000_F000;
pub const INTMTX: u32 = 0x6001_0000;
pub const PARL_IO: u32 = 0x6001_5000;
pub const SPI2: u32 = 0x6008_1000;
pub const SHA_BASE: u32 = 0x6008_9000;
pub const IO_MUX: u32 = 0x6009_0000;
pub const GPIO: u32 = 0x6009_1000;
pub const PCR: u32 = 0x6009_6000;
pub const I2C_ANA_MST: u32 = 0x600A_F800;
pub const PMU: u32 = 0x600B_0000;
pub const LP_CLKRST: u32 = 0x600B_0400;
pub const LP_TIMER: u32 = 0x600B_0C00;
pub const LP_AON: u32 = 0x600B_1000;
pub const LPPERI: u32 = 0x600B_2800;
pub const EFUSE: u32 = 0x600B_4800;
pub const INTPRI: u32 = 0x600C_5000;
pub const CACHE: u32 = 0x600C_8000;
/// The low-power domain (PMU .. LP GPIO): kept through deep sleep and software resets.
pub const LP_DOMAIN: (u32, u32) = (0x600B_0000, 0x600B_4800);
/// Number of GPIOs.
pub const GPIO_COUNT: u32 = 29;
const GPIO_MASK: u32 = (1 << GPIO_COUNT) - 1;

/// Why the chip last reset (LP_CLKRST_RESET_CAUSE, soc/reset_reasons.h).
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

/// The interrupt matrix (peripheral source -> CLIC interrupt id) and the CLIC.
#[derive(Debug)]
pub struct Intc {
    /// CLIC id each source is routed to (below 16 = not routed).
    map: [u8; src::COUNT],
    /// Level of every interrupt source.
    pub sources: u128,
    /// CLIC control word per interrupt id: IP (bit 0), IE (8), attr (16..23: SHV 16,
    /// TRIG 17..18, MODE 22..23), CTL (24..31, the level in the top NLBITS).
    ctrl: [u32; 48],
    /// Line levels (bit = CLIC id) at the last evaluation, for edge detection.
    prev_lines: u64,
    /// `mintthresh` of the (unused) memory-mapped threshold register.
    thresh_reg: u32,
}

impl Default for Intc {
    fn default() -> Self {
        Intc { map: [0; src::COUNT], sources: 0, ctrl: [0; 48], prev_lines: 0, thresh_reg: 0 }
    }
}

/// An interrupt the CLIC offers the core.
#[derive(Debug, Clone, Copy)]
pub struct ClicIrq {
    pub id: u32,
    /// The 8-bit interrupt level (CTL with NLBITS = 3, low bits set).
    pub level: u8,
    /// Hardware vectored (through `mtvt`).
    pub shv: bool,
}

impl Intc {
    fn line_levels(&self) -> u64 {
        let mut lines = 0u64;
        let mut s = self.sources;
        while s != 0 {
            let n = s.trailing_zeros() as usize;
            s &= s - 1;
            let l = self.map[n];
            if (16..48).contains(&l) {
                lines |= 1 << l;
            }
        }
        lines
    }

    /// Update pending bits from the source levels and return the highest-priority
    /// enabled pending interrupt, if any.
    pub fn best(&mut self) -> Option<ClicIrq> {
        let lines = self.line_levels();
        let rising = lines & !self.prev_lines;
        let falling = !lines & self.prev_lines;
        self.prev_lines = lines;
        let mut best: Option<ClicIrq> = None;
        for id in 16..48usize {
            let c = &mut self.ctrl[id];
            let trig = (*c >> 17) & 3;
            let bit = 1u64 << id;
            if trig & 1 == 0 {
                // level triggered: pending follows the line
                *c = (*c & !1) | (lines & bit != 0) as u32;
            } else if (trig & 2 == 0 && rising & bit != 0) || (trig & 2 != 0 && falling & bit != 0) {
                *c |= 1;
            }
            if *c & 1 != 0 && *c & 1 << 8 != 0 {
                let level = ((*c >> 24) as u8 & 0xe0) | 0x1f;
                if best.is_none_or(|b| level >= b.level) {
                    best = Some(ClicIrq { id: id as u32, level, shv: *c & 1 << 16 != 0 });
                }
            }
        }
        best
    }

    /// For debug dumps: each routed source with its CLIC id, and the enabled ids with their
    /// level and pending bit.
    pub fn summary(&self) -> String {
        let routed: Vec<String> = (0..src::COUNT)
            .filter(|&n| self.map[n] >= 16)
            .map(|n| format!("{n}->{}{}", self.map[n], if self.sources >> n & 1 != 0 { "*" } else { "" }))
            .collect();
        let enabled: Vec<String> = (16..48)
            .filter(|&id| self.ctrl[id] & 1 << 8 != 0)
            .map(|id| format!("{id}:l{}{}", self.ctrl[id] >> 29, if self.ctrl[id] & 1 != 0 { "*" } else { "" }))
            .collect();
        format!("sources {} (* = active); CLIC enabled {} (* = pending)", routed.join(" "), enabled.join(" "))
    }

    /// The core took `id`: an edge-triggered interrupt stops pending.
    pub fn taken(&mut self, id: u32) {
        let c = &mut self.ctrl[id as usize];
        if (*c >> 17) & 1 != 0 {
            *c &= !1;
        }
    }

    fn read_map(&self, off: u32) -> Option<u32> {
        let i = (off / 4) as usize;
        if i < src::COUNT {
            return Some(self.map[i] as u32);
        }
        match off {
            0x150 => Some(self.sources as u32),
            0x154 => Some((self.sources >> 32) as u32),
            0x158 => Some((self.sources >> 64) as u32),
            _ => None,
        }
    }

    fn write_map(&mut self, off: u32, v: u32) {
        let i = (off / 4) as usize;
        if i < src::COUNT {
            self.map[i] = (v & 63) as u8;
        }
    }

    fn read_clic(&self, off: u32) -> u32 {
        match off {
            // CLICCFG: nlbits = 3; CLICINFO: 48 interrupts, 3 CTL bits
            0x0 => 3,
            0x4 => 48 | 3 << 21,
            0x8 => self.thresh_reg,
            o if (0x1000..0x1000 + 48 * 4).contains(&o) => self.ctrl[((o - 0x1000) / 4) as usize],
            _ => 0,
        }
    }

    fn write_clic(&mut self, off: u32, v: u32) {
        match off {
            0x8 => self.thresh_reg = v,
            o if (0x1000..0x1000 + 48 * 4).contains(&o) => {
                let c = &mut self.ctrl[((o - 0x1000) / 4) as usize];
                let edge = (v >> 17) & 1 != 0;
                let old_ip = *c & 1;
                let ip = if edge {
                    // Writing IP=1 acknowledges an edge interrupt (rv_utils_intr_edge_ack).
                    if v & 1 != 0 { 0 } else { old_ip }
                } else {
                    old_ip
                };
                // Implemented bits: IP, IE, SHV, TRIG, MODE, CTL (top 3 bits, low ones read 1)
                *c = ip | (v & 1 << 8) | (v & 0x00c7_0000) | (v & 0xe000_0000) | 0x1f00_0000;
            }
            _ => {}
        }
    }
}

pub use crate::soc::Console;

pub struct Periph {
    store: Vec<u32>,
    pub intc: Intc,
    pub systimer: Systimer,
    pub sha: Sha,
    pub i2c: I2c,
    pub aes_irq: bool,
    pub rsa_irq: bool,
    pub ecc_irq: bool,
    pub ecdsa_irq: bool,
    pub gpio_out: u32,
    pub gpio_oe: u32,
    pub gpio_in: u32,
    pub gpio_status: u32,
    /// The console: UART0 and USB serial/JTAG output, merged (see [`Console`]).
    pub console_out: Vec<u8>,
    pub console: Console,
    uart_raw: [u32; 2],
    usb_raw: u32,
    usb_last_sof: u64,
    pub reset_request: Option<ResetRequest>,
    pub reset_reason: ResetReason,
    /// PMU_SLP_WAKEUP_STATUS0 bits (PMU_GPIO_WAKEUP_EN = BIT(2), PMU_LP_TIMER_WAKEUP_EN = BIT(4)).
    pub wakeup_cause: u32,
    /// RTC slow clock ticks accumulated before the current boot (survives deep sleep).
    pub rtc_ticks_base: u64,
    rtc_time_latched: u64,
    pub rtc_slow_hz: u64,
    pub mac: [u8; 6],
    /// Die temperature read by the on-chip sensor, °C.
    pub chip_temp_c: f32,
    /// Analog registers behind the regi2c master: (block, register) -> value.
    ana: HashMap<(u8, u8), u8>,
    rng: u64,
    /// Log of unmodelled register accesses (first touch only).
    seen: std::collections::HashSet<u32>,
}

impl Periph {
    pub fn new() -> Self {
        let mut p = Periph {
            store: vec![0; PERIPH_SIZE / 4],
            intc: Intc::default(),
            systimer: Systimer::default(),
            sha: Sha::default(),
            i2c: I2c::default(),
            aes_irq: false,
            rsa_irq: false,
            ecc_irq: false,
            ecdsa_irq: false,
            gpio_out: 0,
            gpio_oe: 0,
            gpio_in: 0,
            gpio_status: 0,
            console_out: Vec::new(),
            console: Console::default(),
            uart_raw: [0; 2],
            usb_raw: 0,
            usb_last_sof: 0,
            reset_request: None,
            reset_reason: ResetReason::PowerOn,
            wakeup_cause: 0,
            rtc_ticks_base: 0,
            rtc_time_latched: 0,
            rtc_slow_hz: 136_000,
            mac: [0x7c, 0xdf, 0xa1, 0x5e, 0x1a, 0x2b],
            chip_temp_c: 25.0,
            ana: HashMap::new(),
            rng: 0x2545_f491_4f6c_dd1d,
            seen: Default::default(),
        };
        p.chip_reset(false);
        p
    }

    #[inline]
    pub fn store_get(&self, a: u32) -> u32 {
        self.store[((a - PERIPH_BASE) / 4) as usize]
    }

    #[inline]
    pub fn store_set(&mut self, a: u32, v: u32) {
        self.store[((a - PERIPH_BASE) / 4) as usize] = v;
    }

    /// The low-power domain's registers (what `chip_reset` keeps through deep sleep).
    pub fn rtc_regs(&self) -> Vec<u32> {
        (LP_DOMAIN.0..LP_DOMAIN.1).step_by(4).map(|a| self.store_get(a)).collect()
    }

    pub fn set_rtc_regs(&mut self, regs: &[u32]) {
        for (i, v) in regs.iter().take(((LP_DOMAIN.1 - LP_DOMAIN.0) / 4) as usize).enumerate() {
            self.store_set(LP_DOMAIN.0 + 4 * i as u32, *v);
        }
    }

    /// Reset register state as a chip reset would (low-power domain kept).
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
        self.ecc_irq = false;
        self.ecdsa_irq = false;
        self.gpio_out = 0;
        self.gpio_oe = 0;
        self.gpio_status = 0;
        self.uart_raw = [0; 2];
        self.usb_raw = 0;
        self.reset_request = None;
        // Reset values software relies on: the CPU runs from XTAL, divider 1.
        self.store_set(PCR + 0x110, 48 << 24);
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
            // RD_REPEAT_DATA4: XTAL_48M_SEL = 0b111 (48 MHz crystal)
            0x40 => 7 << 9,
            // BLK1: MAC (byte order reversed like hardware)
            0x44 => u32::from_le_bytes([m[5], m[4], m[3], m[2]]),
            0x48 => u16::from_le_bytes([m[1], m[0]]) as u32,
            // MAC_SYS2: wafer version 1.0 (production silicon)
            0x4C => 1 << 4 | 2 << 8,
            0x1D4 => 1, // EFUSE_STATUS: state = idle
            _ => return None,
        })
    }
}

impl Default for Periph {
    fn default() -> Self {
        Self::new()
    }
}

impl C5Bus {
    /// The raw stored value of a register (for byte/halfword writes).
    pub fn periph_stored(&self, a: u32) -> u32 {
        if (CLIC_BASE..CLIC_BASE + CLIC_SIZE).contains(&a) {
            return self.p.intc.read_clic(a - CLIC_BASE);
        }
        self.p.store_get(a)
    }

    pub fn periph_read(&mut self, a: u32) -> u32 {
        if (CLIC_BASE..CLIC_BASE + CLIC_SIZE).contains(&a) {
            return self.p.intc.read_clic(a - CLIC_BASE);
        }
        let now = self.clock.ns();
        let stored = self.p.store_get(a);
        match a {
            // ---- UART ----
            UART0 | UART1 => 0, // RX FIFO empty
            _ if a == UART0 + 0x04 || a == UART1 + 0x04 => self.uart_raw_bits(a),
            _ if a == UART0 + 0x08 || a == UART1 + 0x08 => self.uart_raw_bits(a - 4) & self.p.store_get(a + 4),
            _ if a == UART0 + 0x1C || a == UART1 + 0x1C => 0, // STATUS: FIFOs empty
            _ if a == UART0 + 0x70 || a == UART1 + 0x70 => 0, // FSM idle
            _ if a == UART0 + 0x98 || a == UART1 + 0x98 => stored & !1, // REG_UPDATE done

            // ---- SPI_MEM1 (flash controller) ----
            SPIMEM1 => 0, // commands complete instantly, state machines idle
            _ if a == SPIMEM0 => 0,
            _ if a == SPIMEM0 + 0x37C => {
                let i = self.p.store_get(SPIMEM0 + 0x380) as usize;
                if i < MMU_ENTRIES { self.mmu[i] } else { 0 }
            }

            // ---- GPIO ----
            _ if a == GPIO + 0x04 => self.p.gpio_out,
            _ if a == GPIO + 0x34 => self.p.gpio_oe,
            _ if a == GPIO => 0x08, // strapping: SPI boot
            _ if a == GPIO + 0x64 => {
                self.gpio_sample(now);
                self.p.gpio_in
            }
            _ if a == GPIO + 0x74 => self.p.gpio_status,
            _ if a == GPIO + 0xA4 => self.p.gpio_status & self.gpio_int_enabled_mask(),
            _ if a == GPIO + 0xC4 => self.p.gpio_status, // STATUS_NEXT

            // ---- PCR: clocks ----
            _ if a == PCR + 0x110 => stored & !(0x7f << 24) | ((XTAL_HZ / 1_000_000) as u32) << 24,
            _ if a == PCR + 0x144 => stored & !1, // BUS_CLK_UPDATE done
            // Module *_CONF registers: the READY bits (out of reset, clock running)
            _ if (PCR..PCR + 0x180).contains(&a) => match a - PCR {
                0x18 => stored | 1 << 3,
                0x54 | 0x60 => stored | 7 << 2,
                0x74 => stored | 3 << 2,
                0x0 | 0xc | 0x28 | 0x30 | 0x38 | 0x48 | 0x6c | 0x94 | 0x98 | 0x9c | 0xa0 | 0xa4 | 0xac | 0xc4
                | 0xcc | 0xd0 | 0xd4 | 0xdc | 0xe4 | 0xe8 | 0xec | 0x164 | 0x16c | 0x170 => stored | 1 << 2,
                _ => stored,
            },

            // ---- analog I2C master (regi2c) ----
            _ if a == I2C_ANA_MST + 0x18 => stored | 1 << 24, // BBPLL calibration done

            // ---- low-power domain ----
            _ if a == PMU + 0x144 => self.p.wakeup_cause,
            _ if a == LP_CLKRST + 0x10 => stored & !0x1f | self.p.reset_reason as u32,
            _ if a == LP_TIMER + 0x10 => stored & !(1 << 27),
            _ if a == LP_TIMER + 0x14 => self.p.rtc_time_latched as u32,
            _ if a == LP_TIMER + 0x18 => (self.p.rtc_time_latched >> 32) as u32 & 0xffff,
            _ if a == LPPERI + 0x08 || a == LPPERI + 0x28 => self.p.rand(),
            _ if (EFUSE..EFUSE + 0x200).contains(&a) => match self.p.efuse_word(a - EFUSE) {
                Some(v) => v,
                None if a == EFUSE + 0x1D8 => stored & !3, // CMD: read/program done
                None => stored,
            },

            // ---- TIMG: RTC slow clock calibration ----
            _ if a == TIMG0 + 0x68 || a == TIMG1 + 0x68 => stored | 1 << 15, // RTC cal ready
            _ if a == TIMG0 + 0x6C || a == TIMG1 + 0x6C => {
                let cfg = self.p.store_get(a - 4);
                let max = (cfg >> 16) & 0x7fff;
                // XTAL cycles counted during `max` slow clock cycles
                let v = max as u64 * XTAL_HZ / self.p.rtc_slow_hz;
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

            // ---- crypto ----
            _ if a == RSA + 0x808 || a == RSA + 0x818 => 1, // memory clean / operation done
            _ if (DMA..DMA + 0x60).contains(&a) && (a - DMA) % 0x10 == 4 => {
                self.p.store_get(a - 4) & self.p.store_get(a + 4) // INT_ST = RAW & ENA
            }
            _ if (SHA_BASE..SHA_BASE + 0x100).contains(&a) => self.p.sha.read(a - SHA_BASE),
            _ if a == AES + 0x4C => stored, // STATE
            _ if (ECC..ECC + 0x1000).contains(&a) => self.ecc_read(a, stored),
            _ if (ECDSA..ECDSA + 0x1000).contains(&a) => self.ecdsa_read(a, stored),

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

            // ---- USB serial/JTAG: the console ----
            _ if a == USB_JTAG + 0x04 => stored & !1 | 1 << 1, // IN endpoint always has room
            _ if a == USB_JTAG + 0x08 => self.usb_raw(),
            _ if a == USB_JTAG + 0x0C => self.usb_raw() & self.p.store_get(USB_JTAG + 0x10),
            _ if a == USB_JTAG + 0x24 => ((now / 1_000_000) & 0x7ff) as u32, // SOF frame number (1 kHz)

            // ---- PARLIO: TX always ready, INT_ST = RAW & ENA ----
            _ if a == PARL_IO + 0x24 => stored | 1 << 31,
            _ if a == PARL_IO + 0x30 => self.p.store_get(PARL_IO + 0x2C) & self.p.store_get(PARL_IO + 0x28),

            // ---- interrupt matrix ----
            _ if (INTMTX..INTMTX + 0x800).contains(&a) => self.p.intc.read_map(a - INTMTX).unwrap_or(stored),

            // ---- cache: operations complete instantly ----
            _ if a == CACHE + 0x28 => stored & !(1 << 18) | (stored >> 16 & 1) << 18, // freeze done follows ENA
            _ if a == CACHE + 0x84 => stored | 1 << 2,                                // lock done
            _ if a == CACHE + 0x94 => stored | 1 << 4,                                // sync done
            _ if a == CACHE + 0xD4 => stored | 1 << 1,                                // preload done
            _ if a == CACHE + 0x130 => stored | 1 << 1,                               // autoload done
            _ => {
                if log::log_enabled!(log::Level::Trace) && self.p.seen.insert(a) {
                    log::trace!("periph read  {a:#010x} (unmodelled) = {stored:#x}");
                }
                stored
            }
        }
    }

    pub fn periph_write(&mut self, a: u32, v: u32) {
        if (CLIC_BASE..CLIC_BASE + CLIC_SIZE).contains(&a) {
            self.p.intc.write_clic(a - CLIC_BASE, v);
            self.irq_dirty = true;
            return;
        }
        let now = self.clock.ns();
        let parlio_start = a == PARL_IO + 0x14 && v & 1 << 31 != 0 && self.p.store_get(a) & 1 << 31 == 0;
        self.p.store_set(a, v);
        if parlio_start {
            self.parlio_tx_start(now);
        }
        match a {
            UART0 | UART1 => {
                if a == UART0 {
                    self.p.console.push(0, v as u8, &mut self.p.console_out);
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

            SPIMEM1 => {
                self.spi1_command(v);
                // A power-loss fault fired: make the run loop stop right after this store.
                self.irq_dirty |= self.flash.power_lost().is_some();
            }
            _ if a == SPIMEM0 + 0x37C => {
                let i = self.p.store_get(SPIMEM0 + 0x380) as usize;
                if i < MMU_ENTRIES {
                    self.mmu[i] = v & 0x7ff;
                }
                log::trace!("mmu[{i}] = {v:#x}");
            }

            // GPIO
            _ if a == GPIO + 0x04 => self.gpio_set(v, now),
            _ if a == GPIO + 0x08 => self.gpio_set(self.p.gpio_out | v, now),
            _ if a == GPIO + 0x0C => self.gpio_set(self.p.gpio_out & !v, now),
            _ if a == GPIO + 0x34 => self.gpio_set_oe(v, now),
            _ if a == GPIO + 0x38 => self.gpio_set_oe(self.p.gpio_oe | v, now),
            _ if a == GPIO + 0x3C => self.gpio_set_oe(self.p.gpio_oe & !v, now),
            _ if a == GPIO + 0x74 => {
                self.p.gpio_status = v;
                self.irq_dirty = true;
            }
            _ if a == GPIO + 0x78 => {
                self.p.gpio_status |= v;
                self.irq_dirty = true;
            }
            _ if a == GPIO + 0x7C => {
                self.p.gpio_status &= !v;
                self.gpio_sample(now);
                self.irq_dirty = true;
            }
            _ if (GPIO + 0xD4..GPIO + 0xD4 + GPIO_COUNT * 4).contains(&a) => {
                self.gpio_sample(now);
                self.irq_dirty = true;
            }
            _ if (IO_MUX..IO_MUX + GPIO_COUNT * 4).contains(&a) => self.gpio_sample(now),

            // PCR: CPU clock
            _ if a == PCR + 0x110 || a == PCR + 0x118 => self.update_cpu_clock(),

            // regi2c (analog registers)
            _ if a == I2C_ANA_MST || a == I2C_ANA_MST + 4 => {
                let (block, reg) = (v as u8, (v >> 8) as u8);
                if v & 1 << 24 != 0 {
                    self.p.ana.insert((block, reg), (v >> 16) as u8);
                } else {
                    let d = self.p.ana.get(&(block, reg)).copied().unwrap_or(match (block, reg) {
                        // I2C_ULP: o-code calibration done (O_DONE_FLAG, BG_O_DONE_FLAG)
                        (0x61, 3) => 0x09,
                        _ => 0,
                    });
                    self.p.store_set(a, v & !(0xff << 16) | (d as u32) << 16);
                }
            }

            // low-power domain
            _ if a == LP_AON + 0x34 && v & 1 << 31 != 0 => {
                self.p.reset_request = Some(ResetRequest::System);
                self.p.store_set(a, v & !(1 << 31));
            }
            _ if a == LP_AON + 0x38 && v & 1 << 28 != 0 => {
                self.p.reset_request = Some(ResetRequest::Cpu);
                self.p.store_set(a, v & !(1 << 28));
            }
            _ if a == LP_TIMER + 0x10 && v & 1 << 27 != 0 => {
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
            _ if (DMA..DMA + 0x60).contains(&a) && (a - DMA) % 0x10 == 0xC => {
                let raw = self.p.store_get(a - 0xC);
                self.p.store_set(a - 0xC, raw & !v);
                self.irq_dirty = true;
            }
            _ if (DMA..DMA + 0x60).contains(&a) && (a - DMA) % 0x10 == 8 => self.irq_dirty = true,
            _ if (ECC..ECC + 0x1000).contains(&a) => self.ecc_write(a, v),
            _ if (ECDSA..ECDSA + 0x1000).contains(&a) => self.ecdsa_write(a, v),

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

            // USB serial/JTAG console
            _ if a == USB_JTAG => self.p.console.push(1, v as u8, &mut self.p.console_out),
            _ if a == USB_JTAG + 0x14 => {
                self.p.usb_raw &= !v;
                self.irq_dirty = true;
            }
            _ if a == USB_JTAG + 0x10 => self.irq_dirty = true,

            // PARLIO (TX_START's rising edge is handled above)
            _ if a == PARL_IO + 0x34 => {
                let raw = self.p.store_get(PARL_IO + 0x2C);
                self.p.store_set(PARL_IO + 0x2C, raw & !v);
                self.irq_dirty = true;
            }
            _ if a == PARL_IO + 0x28 => self.irq_dirty = true,

            // cross-core (FreeRTOS yield) interrupts
            _ if (INTPRI + 0x90..=INTPRI + 0x9C).contains(&a) => {
                let n = ((a - INTPRI - 0x90) / 4) as usize;
                let bit = 1u128 << (src::FROM_CPU0 + n);
                if v & 1 != 0 {
                    self.p.intc.sources |= bit;
                } else {
                    self.p.intc.sources &= !bit;
                }
                self.irq_dirty = true;
            }

            _ if (INTMTX..INTMTX + 0x800).contains(&a) => {
                self.p.intc.write_map(a - INTMTX, v);
                self.irq_dirty = true;
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

    fn usb_raw(&self) -> u32 {
        // SERIAL_IN_EMPTY (bit 3): the IN FIFO is always drained immediately.
        self.p.usb_raw | 1 << 3
    }

    /// A USB host is attached: start-of-frame every millisecond. Arduino's HWCDC
    /// only sends console output (and `Serial` is only true) while it sees SOF interrupts.
    pub fn usb_sof(&mut self, now: u64) {
        let frame = now / 1_000_000;
        if frame != self.p.usb_last_sof {
            self.p.usb_last_sof = frame;
            self.p.usb_raw |= 1 << 1;
            self.irq_dirty = true;
        }
    }

    /// Recompute interrupt source levels from peripheral state.
    pub fn refresh_sources(&mut self) {
        let mut s = self.p.intc.sources;
        let mut set = |n: usize, on: bool| {
            if on { s |= 1 << n } else { s &= !(1u128 << n) }
        };
        set(src::UART0, self.uart_raw_bits(UART0 + 4) & self.p.store_get(UART0 + 0x0C) != 0);
        set(src::UART1, self.uart_raw_bits(UART1 + 4) & self.p.store_get(UART1 + 0x0C) != 0);
        set(src::USB_JTAG, self.usb_raw() & self.p.store_get(USB_JTAG + 0x10) != 0);
        let st = self.p.systimer.irq_lines();
        for i in 0..3 {
            set(src::SYSTIMER_TARGET0 + i, st & (1 << i) != 0);
        }
        set(src::GPIO, self.p.gpio_status & self.gpio_int_enabled_mask() != 0);
        set(src::SHA, self.p.sha.irq());
        set(src::AES, self.p.aes_irq && self.p.store_get(AES + 0xB0) & 1 != 0);
        set(src::RSA, self.p.rsa_irq && self.p.store_get(RSA + 0x82C) & 1 != 0);
        set(src::ECC, self.p.ecc_irq && self.p.store_get(ECC + 0x14) & 1 != 0);
        set(src::ECDSA, self.p.ecdsa_irq && self.p.store_get(ECDSA + 0x14) & 0xf != 0);
        for ch in 0..3u32 {
            let i = self.p.store_get(DMA + 0x10 * ch) & self.p.store_get(DMA + 8 + 0x10 * ch);
            set(src::DMA_IN_CH0 + ch as usize, i != 0);
            let o = self.p.store_get(DMA + 0x30 + 0x10 * ch) & self.p.store_get(DMA + 0x38 + 0x10 * ch);
            set(src::DMA_OUT_CH0 + ch as usize, o != 0);
        }
        set(src::I2C_EXT0, self.p.i2c.raw & self.p.store_get(I2C0 + 0x28) != 0);
        set(src::PARL_IO_TX, self.p.store_get(PARL_IO + 0x2C) & self.p.store_get(PARL_IO + 0x28) != 0);
        self.p.intc.sources = s;
    }

    // ---- clocks ------------------------------------------------------------------------------

    fn update_cpu_clock(&mut self) {
        let sysclk = self.p.store_get(PCR + 0x110);
        let div = (self.p.store_get(PCR + 0x118) & 0xff) as u64 + 1;
        let src = match (sysclk >> 16) & 3 {
            0 => XTAL_HZ,
            1 => 17_500_000,
            2 => 160_000_000,
            _ => 240_000_000,
        };
        self.clock.set_hz(src / div);
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
        for pin in 0..GPIO_COUNT {
            let r = self.p.store_get(GPIO + 0xD4 + 4 * pin);
            if (r >> 13) & 0x1f != 0 && (r >> 7) & 7 != 0 {
                m |= 1 << pin;
            }
        }
        m
    }

    fn gpio_set(&mut self, new: u32, now: u64) {
        let new = new & GPIO_MASK;
        if new != self.p.gpio_out {
            self.p.gpio_out = new;
            self.board.gpio_out(now, new as u64, self.p.gpio_oe as u64);
        }
    }

    fn gpio_set_oe(&mut self, v: u32, now: u64) {
        let v = v & GPIO_MASK;
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
        for pin in 0..GPIO_COUNT {
            let bit = 1 << pin;
            let level = if driven & bit != 0 {
                lv & bit != 0
            } else if self.p.gpio_oe & bit != 0 {
                self.p.gpio_out & bit != 0
            } else {
                // FUN_PU (bit 8) pulls up. Pins without pull-ups idle low.
                self.p.store_get(IO_MUX + 4 * pin) & (1 << 8) != 0
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
        for pin in 0..GPIO_COUNT {
            let bit = 1 << pin;
            let r = self.p.store_get(GPIO + 0xD4 + 4 * pin);
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
        self.p.store_set(SPI2 + 0x3C, raw | 1 << 12); // TRANS_DONE
        self.irq_dirty = true;
    }

    // ---- SPI_MEM1: flash controller --------------------------------------------------------------

    /// A user command on SPI1 with CS1 selected (MISC: CS0_DIS set, CS1_DIS clear): the
    /// quad PSRAM (an AP Memory 64 Mbit part, as on the ESP32-C5-WROOM-1 N16R8).
    fn psram_command(&mut self) {
        const SPI1: u32 = SPIMEM1;
        let user = self.p.store_get(SPI1 + 0x18);
        let opcode = (self.p.store_get(SPI1 + 0x20) & 0xff) as u8;
        let addr = (self.p.store_get(SPI1 + 0x04) & 0xff_ffff) as usize;
        let bytes =
            |reg: u32, on: bool| if on { ((self.p.store_get(reg) & 0x3ff) as usize + 1).div_ceil(8) } else { 0 };
        let mosi_n = bytes(SPI1 + 0x24, user & 1 << 27 != 0).min(64);
        let miso_n = bytes(SPI1 + 0x28, user & 1 << 28 != 0).min(64);
        let mosi: Vec<u8> =
            (0..mosi_n).map(|i| (self.p.store_get(SPI1 + 0x58 + 4 * (i as u32 / 4)) >> (8 * (i % 4))) as u8).collect();
        let size = self.psram.len();
        let rx: Vec<u8> = match opcode {
            // MFID (AP), KGD, EID[47:40] = density 2 (64 Mbit) | EID[41] (not 2T mode)
            0x9f => [0x0d, 0x5d, 0x42, 0x11, 0x22, 0x33].iter().copied().cycle().take(miso_n).collect(),
            0x02 | 0x38 => {
                for (i, b) in mosi.iter().enumerate() {
                    self.psram[(addr + i) % size] = *b;
                }
                Vec::new()
            }
            0x03 | 0x0b | 0xeb => (0..miso_n).map(|i| self.psram[(addr + i) % size]).collect(),
            _ => vec![0; miso_n], // reset, QPI enter/exit, wrap length
        };
        log::trace!("psram cmd {opcode:#04x} addr {addr:#x} mosi {mosi_n} miso {miso_n}");
        self.spi1_fill(&rx);
    }

    fn spi1_command(&mut self, cmd: u32) {
        const SPI1: u32 = SPIMEM1;
        if cmd & 1 << 18 != 0 && self.p.store_get(SPI1 + 0x34) & 3 == 1 {
            return self.psram_command();
        }
        log::trace!(
            "spi1 cmd {cmd:#x} addr {:#x} user {:#x} user2 {:#x}",
            self.p.store_get(SPI1 + 4),
            self.p.store_get(SPI1 + 0x18),
            self.p.store_get(SPI1 + 0x20)
        );
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
            let mut rx = self.flash.transact(opcode, addr, &mosi, miso_bytes.min(64));
            log::trace!("spi1 usr {opcode:#04x} miso {miso_bytes} -> {:02x?}", &rx[..rx.len().min(16)]);
            if addr.is_some() && !self.mspi_samples_well() {
                // sampled at the wrong moment: every bit a cycle late
                let mut carry = 0;
                for b in rx.iter_mut() {
                    let v = *b;
                    *b = v >> 1 | carry << 7;
                    carry = v & 1;
                }
            }
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

    /// Whether flash data is sampled correctly with the current MSPI input timing (DIN mode
    /// and number, SPI1's extra dummy cycles). IDF sweeps the timing configs of its 80 MHz
    /// table at boot and wants a window of 3 to 4 or 6 working ones (MSPI_TIMING_FLASH_CONSECUTIVE_
    /// LEN_MAX) around the default, as on real silicon; outside the table all timings work.
    fn mspi_samples_well(&self) -> bool {
        const TABLE_80M: [(u32, u32, u32); 14] = [
            (2, 2, 1),
            (2, 1, 1),
            (2, 0, 1),
            (0, 0, 0),
            (3, 1, 2),
            (2, 3, 2),
            (2, 2, 2),
            (2, 1, 2),
            (2, 0, 2),
            (0, 0, 1),
            (3, 1, 3),
            (2, 3, 3),
            (2, 2, 3),
            (2, 1, 3),
        ];
        let cali = self.p.store_get(SPIMEM1 + 0x180);
        let dummy = if cali & 2 != 0 { (cali >> 2) & 7 } else { 0 };
        let t = (self.p.store_get(SPIMEM0 + 0x184) & 7, self.p.store_get(SPIMEM0 + 0x188) & 3, dummy);
        TABLE_80M.iter().position(|c| *c == t).is_none_or(|i| (3..=6).contains(&i))
    }

    fn spi1_fill(&mut self, rx: &[u8]) {
        for (i, chunk) in rx.chunks(4).enumerate() {
            let mut w = [0u8; 4];
            w[..chunk.len()].copy_from_slice(chunk);
            self.p.store_set(SPIMEM1 + 0x58 + 4 * i as u32, u32::from_le_bytes(w));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn console_shows_a_line_sent_to_both_channels_once() {
        let (mut c, mut out) = (Console::default(), Vec::new());
        for b in b"I (5) boot: hi\r\n" {
            c.push(0, *b, &mut out);
        }
        for b in b"arduino only\r\nI (5) boot: hi\r\n" {
            c.push(1, *b, &mut out);
        }
        assert_eq!(out, b"I (5) boot: hi\r\narduino only\r\n");
    }

    #[test]
    fn clic_offers_the_highest_level_enabled_pending_interrupt() {
        let mut i = Intc::default();
        i.write_map(4 * src::GPIO as u32, 17);
        i.write_map(4 * src::UART0 as u32, 18);
        // id 17: enabled, level 1; id 18: enabled, level 3; both level triggered
        i.write_clic(0x1000 + 4 * 17, 1 << 8 | 0x20 << 24);
        i.write_clic(0x1000 + 4 * 18, 1 << 8 | 0x60 << 24);
        assert!(i.best().is_none());
        i.sources = 1 << src::GPIO | 1 << src::UART0;
        let b = i.best().unwrap();
        assert_eq!((b.id, b.level), (18, 0x7f));
        // disabled interrupts don't pend the core
        i.write_clic(0x1000 + 4 * 18, 0x60 << 24);
        assert_eq!(i.best().unwrap().id, 17);
        // the line dropping clears a level-triggered interrupt
        i.sources = 0;
        assert!(i.best().is_none());
    }

    #[test]
    fn clic_edge_interrupt_pends_until_taken() {
        let mut i = Intc::default();
        i.write_map(4 * src::GPIO as u32, 20);
        i.write_clic(0x1000 + 4 * 20, 1 << 8 | 1 << 17 | 0x40 << 24); // rising edge
        i.sources = 1 << src::GPIO;
        assert_eq!(i.best().unwrap().id, 20);
        i.sources = 0;
        assert_eq!(i.best().unwrap().id, 20, "latched");
        i.taken(20);
        assert!(i.best().is_none());
    }
}
