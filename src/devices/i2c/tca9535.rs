//! TI TCA9535 / NXP PCA9535 16-bit I/O expander (TRMNL X: 0x20), as driven by
//! FastEPD's `bbepPCA9535*` helpers and the firmware's `bbep.io*()` calls.
//!
//! Registers (command byte = register pointer, low 3 bits):
//! 0/1 input port 0/1, 2/3 output, 4/5 polarity inversion, 6/7 configuration
//! (1 = input). After each data byte the pointer toggles within its register
//! pair (0<->1, 2<->3, ...). The pointer latches across STOP, so FastEPD's
//! "write [reg] + STOP, then a separate read" works.
//!
//! The input register reflects the pin *level*: the driven output value for
//! output pins, the external level for input pins. `/INT` (active low, to S3
//! GPIO38) asserts while any input pin differs from its value when its port was
//! last read, and clears when that port is read (or the pin changes back) —
//! PCA9535 datasheet behaviour, including the "false" interrupt when a pin is
//! switched from output to input at a different level.
//!
//! Pin numbering is FastEPD's: 0..7 = P0_0..P0_7, 8..15 = P1_0..P1_7.

use super::I2cDevice;
use crate::savepoint::{StateReader, StateWriter};

pub const ADDR: u8 = 0x20;

/// TRMNL X expander pin assignments (bbep numbering).
pub mod pins {
    /// BQ25616 PG: input, LOW = VBUS present (docked).
    pub const CHG_PG: u8 = 0;
    /// OTG enable (output).
    pub const OTG_EN: u8 = 1;
    /// BQ25616 STAT: input, LOW = charging.
    pub const CHG_STAT: u8 = 2;
    /// BMA530 INT1: input, idle high.
    pub const BMA_INT1: u8 = 3;
    /// ESP32-C5 modem SPI_BOOT / USB_BOOT / EN (outputs).
    pub const C5_SPI_BOOT: u8 = 4;
    pub const C5_USB_BOOT: u8 = 5;
    pub const C5_EN: u8 = 6;
    /// Battery-count RC network (driven high, then released and timed; src/display.cpp).
    pub const BAT_DET: u8 = 7;
    /// EPD source driver OE, gate driver GMOD (outputs).
    pub const EPD_OE: u8 = 8;
    pub const EPD_GMOD: u8 = 9;
    /// BQ27427 reset (output).
    pub const BQ_RESET: u8 = 10;
    /// TPS65185 PWRUP, VCOM_CTRL, WAKEUP (outputs).
    pub const TPS_PWRUP: u8 = 11;
    pub const TPS_VCOM_CTRL: u8 = 12;
    pub const TPS_WAKEUP: u8 = 13;
    /// TPS65185 PWR_GOOD (input; FastEPD spins until it reads 1).
    pub const TPS_PWR_GOOD: u8 = 14;
    /// TPS65185 nINT (input, idle high).
    pub const TPS_NINT: u8 = 15;
}

const MS: u64 = 1_000_000;
const US: u64 = 1_000;

/// External pin levels when nothing else is specified: charger absent and not
/// charging (PG/STAT high), BMA INT1 idle high, TPS nINT high, modem boot
/// straps pulled high, everything else low.
const DEFAULT_EXTERNAL: u16 = 1 << pins::CHG_PG
    | 1 << pins::CHG_STAT
    | 1 << pins::BMA_INT1
    | 1 << pins::C5_SPI_BOOT
    | 1 << pins::C5_USB_BOOT
    | 1 << pins::BQ_RESET
    | 1 << pins::TPS_NINT;

pub struct Tca9535 {
    ptr: u8,
    /// Next written byte is the command byte.
    expect_cmd: bool,
    output: [u8; 2],
    polarity: [u8; 2],
    config: [u8; 2],
    /// Levels the board drives onto input pins (bit n = pin n).
    external: u16,
    /// Raw pin levels captured when each port was last read (for /INT).
    last_read: [u8; 2],

    /// BAT_DET: after being released from output-high, reads high until this time.
    rc_high_until: Option<u64>,
    /// RC discharge time after release. Firmware thresholds: <=750 us = 2 cells,
    /// >750 us = 1 cell, still high at 6000 us = no battery.
    pub rc_decay_ns: u64,

    /// Model TPS PWR_GOOD internally: reads 1 once PWRUP and WAKEUP have both
    /// been driven high for `pwr_good_delay_ns`. Disable to drive P1_6 from the
    /// board (e.g. from `Tps65185::pwr_good`) with `set_input`.
    pub auto_pwr_good: bool,
    pub pwr_good_delay_ns: u64,
    pwrup_wakeup_since: Option<u64>,

    /// (levels, output-enable) as last reported by `take_output_changes`.
    reported_outputs: Option<(u16, u16)>,
}

impl Default for Tca9535 {
    fn default() -> Self {
        Self::new()
    }
}

impl Tca9535 {
    pub fn new() -> Self {
        let mut t = Tca9535 {
            ptr: 0,
            expect_cmd: false,
            output: [0xff; 2],
            polarity: [0; 2],
            config: [0xff; 2],
            external: DEFAULT_EXTERNAL,
            last_read: [0; 2],
            rc_high_until: None,
            rc_decay_ns: 2 * MS,
            auto_pwr_good: true,
            pwr_good_delay_ns: MS,
            pwrup_wakeup_since: None,
            reported_outputs: None,
        };
        t.last_read = t.raw_ports(0);
        t
    }

    /// Power-on reset (register defaults; external levels are kept).
    pub fn reset(&mut self, now: u64) {
        let (ext, decay, auto, delay) = (self.external, self.rc_decay_ns, self.auto_pwr_good, self.pwr_good_delay_ns);
        *self = Self::new();
        self.external = ext;
        self.rc_decay_ns = decay;
        self.auto_pwr_good = auto;
        self.pwr_good_delay_ns = delay;
        self.last_read = self.raw_ports(now);
    }

    /// Battery pack for the P0_7 RC detect: 0 = none, 1 = one cell, 2 = two cells.
    pub fn set_battery_cells(&mut self, cells: u8) {
        self.rc_decay_ns = match cells {
            0 => 20 * MS,
            1 => 2 * MS,
            _ => 400 * US,
        };
    }

    /// Board drives an input pin (ignored while the pin is configured as output,
    /// but remembered for when it becomes an input).
    pub fn set_input(&mut self, pin: u8, level: bool) {
        let bit = 1u16 << (pin & 15);
        if level {
            self.external |= bit;
        } else {
            self.external &= !bit;
        }
    }

    /// Charger inputs: `docked` = VBUS present (PG low), `charging` = STAT low.
    pub fn set_charger(&mut self, docked: bool, charging: bool) {
        self.set_input(pins::CHG_PG, !docked);
        self.set_input(pins::CHG_STAT, !charging);
    }

    fn is_output(&self, pin: u8) -> bool {
        self.config[(pin >> 3) as usize] >> (pin & 7) & 1 == 0
    }

    fn out_bit(&self, pin: u8) -> bool {
        self.output[(pin >> 3) as usize] >> (pin & 7) & 1 != 0
    }

    /// Pin driven high by the expander (configured as output and output bit set).
    pub fn driven_high(&self, pin: u8) -> bool {
        self.is_output(pin) && self.out_bit(pin)
    }

    /// Driven output levels and the output-enable mask (bit n = pin n).
    pub fn outputs(&self) -> (u16, u16) {
        let oe = !(u16::from_le_bytes(self.config));
        (u16::from_le_bytes(self.output) & oe, oe)
    }

    /// Returns the output state if it changed since the previous call (the first
    /// call always reports). Poll after each I2C transaction / in `update`.
    pub fn take_output_changes(&mut self) -> Option<(u16, u16)> {
        let now = self.outputs();
        if self.reported_outputs == Some(now) {
            return None;
        }
        self.reported_outputs = Some(now);
        Some(now)
    }

    /// Level of a pin as the input register sees it (before polarity inversion).
    pub fn pin_level(&self, now: u64, pin: u8) -> bool {
        let pin = pin & 15;
        if self.is_output(pin) {
            return self.out_bit(pin);
        }
        match pin {
            pins::BAT_DET => self.rc_high_until.is_some_and(|t| now < t),
            pins::TPS_PWR_GOOD if self.auto_pwr_good => {
                self.pwrup_wakeup_since.is_some_and(|t| now >= t + self.pwr_good_delay_ns)
            }
            _ => self.external >> pin & 1 != 0,
        }
    }

    fn raw_ports(&self, now: u64) -> [u8; 2] {
        let mut p = [0u8; 2];
        for pin in 0..16u8 {
            if self.pin_level(now, pin) {
                p[(pin >> 3) as usize] |= 1 << (pin & 7);
            }
        }
        p
    }

    /// /INT output (to S3 GPIO38) is asserted (low).
    pub fn int_low(&self, now: u64) -> bool {
        let raw = self.raw_ports(now);
        (0..2).any(|p| (raw[p] ^ self.last_read[p]) & self.config[p] != 0)
    }

    fn write_reg(&mut self, now: u64, reg: u8, v: u8) {
        let port = (reg & 1) as usize;
        let before_bat = (self.driven_high(pins::BAT_DET), self.is_output(pins::BAT_DET));
        match reg & 7 {
            0 | 1 => {} // input port: read-only
            2 | 3 => self.output[port] = v,
            4 | 5 => self.polarity[port] = v,
            _ => self.config[port] = v,
        }
        // BAT_DET released from driving high: the RC network starts discharging.
        if before_bat == (true, true) && !self.is_output(pins::BAT_DET) {
            self.rc_high_until = Some(now + self.rc_decay_ns);
        } else if self.is_output(pins::BAT_DET) {
            self.rc_high_until = None;
        }
        let both = self.driven_high(pins::TPS_PWRUP) && self.driven_high(pins::TPS_WAKEUP);
        match (both, self.pwrup_wakeup_since) {
            (true, None) => self.pwrup_wakeup_since = Some(now),
            (false, Some(_)) => self.pwrup_wakeup_since = None,
            _ => {}
        }
    }

    fn read_reg(&mut self, now: u64, reg: u8) -> u8 {
        let port = (reg & 1) as usize;
        match reg & 7 {
            0 | 1 => {
                let raw = self.raw_ports(now)[port];
                self.last_read[port] = raw; // reading the port clears its interrupt
                raw ^ self.polarity[port]
            }
            2 | 3 => self.output[port],
            4 | 5 => self.polarity[port],
            _ => self.config[port],
        }
    }
}

impl I2cDevice for Tca9535 {
    fn address(&self) -> u8 {
        ADDR
    }

    fn save_state(&self, w: &mut StateWriter) {
        w.u8(self.ptr);
        w.bool(self.expect_cmd);
        for a in [self.output, self.polarity, self.config, self.last_read] {
            w.bytes(&a);
        }
        w.u16(self.external);
        w.opt_u64(self.rc_high_until);
        w.opt_u64(self.pwrup_wakeup_since);
        w.bool(self.reported_outputs.is_some());
        let (lv, oe) = self.reported_outputs.unwrap_or_default();
        w.u16(lv);
        w.u16(oe);
    }

    fn restore_state(&mut self, r: &mut StateReader) -> anyhow::Result<()> {
        self.ptr = r.u8()?;
        self.expect_cmd = r.bool()?;
        for a in [&mut self.output, &mut self.polarity, &mut self.config, &mut self.last_read] {
            *a = r.array()?;
        }
        self.external = r.u16()?;
        self.rc_high_until = r.opt_u64()?;
        self.pwrup_wakeup_since = r.opt_u64()?;
        let some = r.bool()?;
        let outputs = (r.u16()?, r.u16()?);
        self.reported_outputs = some.then_some(outputs);
        Ok(())
    }

    fn start(&mut self, _now: u64, read: bool) -> bool {
        self.expect_cmd = !read;
        true
    }

    fn write(&mut self, now: u64, byte: u8) -> bool {
        if self.expect_cmd {
            self.expect_cmd = false;
            self.ptr = byte & 7;
        } else {
            self.write_reg(now, self.ptr, byte);
            self.ptr ^= 1;
        }
        true
    }

    fn read(&mut self, now: u64, _ack: bool) -> u8 {
        let v = self.read_reg(now, self.ptr);
        self.ptr ^= 1;
        v
    }

    fn stop(&mut self, _now: u64) {
        self.expect_cmd = false;
    }

    fn next_event_ns(&self, now: u64) -> Option<u64> {
        let rc = self.rc_high_until.filter(|&t| t > now);
        let pg = self.pwrup_wakeup_since.map(|t| t + self.pwr_good_delay_ns).filter(|&t| t > now && self.auto_pwr_good);
        rc.into_iter().chain(pg).min()
    }
}

#[cfg(test)]
mod tests {
    use super::super::I2cBus;
    use super::pins::*;
    use super::*;

    const US: u64 = 1_000;

    fn bus() -> I2cBus {
        let mut b = I2cBus::new();
        b.add(Box::new(Tca9535::new()));
        b
    }

    /// FastEPD keeps a RAM copy (`ioRegs`) and rewrites a whole port per call.
    struct FastEpd {
        io_regs: [u8; 8],
    }

    impl FastEpd {
        fn new() -> Self {
            FastEpd { io_regs: [0; 8] } // EPDiyV7IOInit(): memset(ioRegs, 0)
        }
        /// bbepPCA9535PinMode()
        fn pin_mode(&mut self, bus: &mut I2cBus, now: u64, pin: u8, input: bool) {
            let port = (pin / 8) as usize;
            let bit = 1 << (pin & 7);
            if input {
                self.io_regs[6 + port] |= bit;
            } else {
                self.io_regs[6 + port] &= !bit;
            }
            assert!(bus.write_txn(now, 0x20, &[6 + port as u8, self.io_regs[6 + port]]));
        }
        /// bbepPCA9535DigitalWrite()
        fn digital_write(&mut self, bus: &mut I2cBus, now: u64, pin: u8, v: bool) {
            let port = (pin / 8) as usize;
            let bit = 1 << (pin & 7);
            if v {
                self.io_regs[2 + port] |= bit;
            } else {
                self.io_regs[2 + port] &= !bit;
            }
            assert!(bus.write_txn(now, 0x20, &[2 + port as u8, self.io_regs[2 + port]]));
        }
        /// bbepPCA9535DigitalRead(): bbepI2CReadRegister = write [port] + STOP, then read.
        fn digital_read(&mut self, bus: &mut I2cBus, now: u64, pin: u8) -> bool {
            assert!(bus.write_txn(now, 0x20, &[pin / 8]));
            let v = bus.read_txn(now, 0x20, 1).unwrap()[0];
            v >> (pin & 7) & 1 != 0
        }
    }

    fn tca(bus: &mut I2cBus) -> &mut Tca9535 {
        bus.device_mut::<Tca9535>().unwrap()
    }

    #[test]
    fn reset_defaults_and_pointer_pairs() {
        let mut bus = bus();
        // Read all 8 registers from 0 in one go: the pointer toggles 0<->1 only.
        assert!(bus.write_txn(0, 0x20, &[0x06]));
        assert_eq!(bus.read_txn(0, 0x20, 4), Some(vec![0xff, 0xff, 0xff, 0xff]));
        assert!(bus.write_txn(0, 0x20, &[0x02]));
        assert_eq!(bus.read_txn(0, 0x20, 2), Some(vec![0xff, 0xff]));
        assert!(bus.write_txn(0, 0x20, &[0x04]));
        assert_eq!(bus.read_txn(0, 0x20, 2), Some(vec![0x00, 0x00]));
        // Pair write: [6, cfg0, cfg1]; pointer latched at 6 afterwards (toggled twice).
        assert!(bus.write_txn(0, 0x20, &[0x06, 0x0f, 0xf0]));
        assert_eq!(bus.read_txn(0, 0x20, 3), Some(vec![0x0f, 0xf0, 0x0f]));
        // Polarity inversion applies to the input register.
        assert!(bus.write_txn(0, 0x20, &[0x04, 0x01]));
        assert!(bus.write_txn(0, 0x20, &[0x00]));
        let p0 = bus.read_txn(0, 0x20, 1).unwrap()[0];
        assert_eq!(p0 & 1, 0, "PG (external high) reads inverted");
    }

    #[test]
    fn fastepd_io_init_and_power_good_follows_pwrup_wakeup() {
        let mut bus = bus();
        let mut f = FastEpd::new();
        let mut t = 0;
        // EPDiyV7IOInit(): pins 8..13 outputs, 14/15 inputs.
        for pin in 8..14 {
            f.pin_mode(&mut bus, t, pin, false);
        }
        f.pin_mode(&mut bus, t, 14, true);
        f.pin_mode(&mut bus, t, 15, true);
        // The chip's output register still holds its 0xFF reset value, so all new
        // outputs drive high until first written (FastEPD's RAM copy says 0).
        let (lv, oe) = tca(&mut bus).outputs();
        assert_eq!(oe, 0x3f00);
        assert_eq!(lv, 0x3f00);
        // EPDiyV7EinkPower(0) path first: everything low.
        for pin in [11, 12, 8, 9, 13] {
            f.digital_write(&mut bus, t, pin, false);
        }
        t += 5_000 * US;
        assert!(!f.digital_read(&mut bus, t, TPS_PWR_GOOD));
        assert!(f.digital_read(&mut bus, t, TPS_NINT));
        // EPDiyV7EinkPower(1): OE, GMOD, WAKEUP, PWRUP, VCOM, vTaskDelay(3), spin on PWR_GOOD.
        for pin in [8, 9, 13, 11, 12] {
            f.digital_write(&mut bus, t, pin, true);
        }
        assert!(!f.digital_read(&mut bus, t, TPS_PWR_GOOD), "not instantly good");
        assert_eq!(tca(&mut bus).next_event_ns(t), Some(t + MS));
        t += 3_000 * US;
        assert!(f.digital_read(&mut bus, t, TPS_PWR_GOOD));
        // Power off: PWRUP low drops PWR_GOOD.
        f.digital_write(&mut bus, t, TPS_PWRUP, false);
        assert!(!f.digital_read(&mut bus, t, TPS_PWR_GOOD));
        let (lv, _) = tca(&mut bus).outputs();
        assert_eq!(lv >> 8, 0b0011_1011 & !(1 << 3));
    }

    /// src/display.cpp measure_battery_once(): drive P0_7 high 2 ms, switch to input,
    /// poll ioRead until it reads 0; >6000 us none, >750 us one cell, else two.
    fn measure_battery_once(bus: &mut I2cBus, f: &mut FastEpd, t: &mut u64) -> u8 {
        f.pin_mode(bus, *t, BAT_DET, false);
        f.digital_write(bus, *t, BAT_DET, true);
        *t += 2_000 * US;
        f.pin_mode(bus, *t, BAT_DET, true);
        let start = *t;
        loop {
            let high = f.digital_read(bus, *t, BAT_DET);
            if !high {
                break;
            }
            if *t - start >= 6_000 * US {
                return 0;
            }
            *t += 350 * US; // one 100 kHz ioRead() (2 transactions) is ~0.35 ms
        }
        if *t - start > 750 * US { 1 } else { 2 }
    }

    #[test]
    fn battery_count_rc_timing() {
        for (cells, expect) in [(1u8, 1u8), (2, 2), (0, 0)] {
            let mut bus = bus();
            tca(&mut bus).set_battery_cells(cells);
            let mut f = FastEpd::new();
            let mut t = 0;
            let a = measure_battery_once(&mut bus, &mut f, &mut t);
            t += 10_000 * US;
            let b = measure_battery_once(&mut bus, &mut f, &mut t);
            assert_eq!((a, b), (expect, expect), "cells={cells}");
        }
        // Default decay is ~2 ms = one cell.
        let mut bus = bus();
        let mut f = FastEpd::new();
        let mut t = 0;
        assert_eq!(measure_battery_once(&mut bus, &mut f, &mut t), 1);
        // Released from output-low: reads 0 immediately.
        f.pin_mode(&mut bus, t, BAT_DET, false);
        f.digital_write(&mut bus, t, BAT_DET, false);
        f.pin_mode(&mut bus, t, BAT_DET, true);
        assert!(!f.digital_read(&mut bus, t, BAT_DET));
    }

    #[test]
    fn interrupt_on_input_change_cleared_by_port_read() {
        let mut bus = bus();
        let mut f = FastEpd::new();
        assert!(!tca(&mut bus).int_low(0));
        // Like the firmware, every pin is made an input right before it is read
        // (FastEPD's RAM copy starts at 0, so untouched port-0 pins become outputs).
        for pin in [CHG_PG, CHG_STAT, BMA_INT1] {
            f.pin_mode(&mut bus, 0, pin, true);
        }
        let _ = f.digital_read(&mut bus, 0, CHG_PG);
        assert!(!tca(&mut bus).int_low(0));
        // Dock + charging: PG and STAT go low -> INT asserts.
        tca(&mut bus).set_charger(true, true);
        assert!(tca(&mut bus).int_low(0));
        // Reading port 1 does not clear a port-0 interrupt.
        f.digital_read(&mut bus, 0, 8);
        assert!(tca(&mut bus).int_low(0));
        // TCA9535Power::usbStatus()/chargingStatus(): 0 = connected / charging; clears INT.
        assert!(!f.digital_read(&mut bus, 0, CHG_PG));
        assert!(!f.digital_read(&mut bus, 0, CHG_STAT));
        assert!(!tca(&mut bus).int_low(0));
        // A change that reverts before being read deasserts by itself.
        tca(&mut bus).set_input(BMA_INT1, false);
        assert!(tca(&mut bus).int_low(0));
        tca(&mut bus).set_input(BMA_INT1, true);
        assert!(!tca(&mut bus).int_low(0));
        // Output pins never interrupt.
        f.pin_mode(&mut bus, 0, OTG_EN, false);
        f.digital_write(&mut bus, 0, OTG_EN, false);
        let _ = f.digital_read(&mut bus, 0, 0);
        f.digital_write(&mut bus, 0, OTG_EN, true);
        assert!(!tca(&mut bus).int_low(0));
        // Switching an output to input at a different level is a (datasheet) false interrupt.
        tca(&mut bus).set_input(OTG_EN, true); // last read as 0 (driven low then)
        f.pin_mode(&mut bus, 0, OTG_EN, true);
        assert!(tca(&mut bus).int_low(0));
    }

    #[test]
    fn output_change_notifications() {
        let mut bus = bus();
        let mut f = FastEpd::new();
        assert_eq!(tca(&mut bus).take_output_changes(), Some((0, 0)));
        assert_eq!(tca(&mut bus).take_output_changes(), None);
        // modem_enter_bootloader(): ioWrite(EN, 0) first (output register only, the
        // pin is still an input), then pinMode OUTPUT. FastEPD's RAM copy starts at
        // 0, so the whole port becomes outputs.
        f.digital_write(&mut bus, 0, C5_EN, false);
        assert_eq!(tca(&mut bus).take_output_changes(), None);
        f.pin_mode(&mut bus, 0, C5_EN, false);
        assert_eq!(tca(&mut bus).take_output_changes(), Some((0, 0x00ff)));
        f.digital_write(&mut bus, 0, C5_EN, true);
        assert_eq!(tca(&mut bus).take_output_changes(), Some((1 << C5_EN, 0x00ff)));
        assert!(tca(&mut bus).driven_high(C5_EN));
        f.digital_write(&mut bus, 0, C5_EN, false);
        assert!(!tca(&mut bus).driven_high(C5_EN));
    }
}
