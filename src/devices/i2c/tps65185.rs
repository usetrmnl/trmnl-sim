//! TI TPS65185 e-paper PMIC (TRMNL X: 0x68), as driven by FastEPD's
//! `EPDiyV7EinkPower()` (FastEPD.inl:806-870):
//!
//! power on:  TCA OE, GMOD, WAKEUP, PWRUP, VCOM_CTRL high; spin until TCA P1_6
//!            (PWR_GOOD pin) reads 1; write ENABLE (0x01) = 0x3F; write
//!            `[0x03, vcom_lo, vcom_hi]`; poll PG (0x0F) until `(pg & 0xFA) == 0xFA`
//!            (400 tries, 1 tick apart) — register read = write `[0x0F]` + STOP, read 1.
//! power off: PWRUP low, VCOM_CTRL low, ..., WAKEUP low.
//!
//! The WAKEUP / PWRUP pins live on the TCA9535; the board forwards them with
//! [`Tps65185::set_pins`]. Register pointer auto-increments; it latches across
//! STOP (write-then-read in separate transactions) and repeated START.

use super::I2cDevice;

pub const ADDR: u8 = 0x68;

pub const REG_TMST_VALUE: u8 = 0x00;
pub const REG_ENABLE: u8 = 0x01;
pub const REG_VADJ: u8 = 0x02;
pub const REG_VCOM1: u8 = 0x03;
pub const REG_VCOM2: u8 = 0x04;
pub const REG_INT1: u8 = 0x07;
pub const REG_INT2: u8 = 0x08;
pub const REG_UPSEQ0: u8 = 0x09;
pub const REG_PG: u8 = 0x0F;
pub const REG_REVID: u8 = 0x10;

/// All rails good: VNEG, VEE, VPOS, VDDH and VCOM power-good bits as FastEPD expects.
pub const PG_ALL_GOOD: u8 = 0xFA;

const ENABLE_ACTIVE: u8 = 0x80;
const ENABLE_STANDBY: u8 = 0x40;
const NREGS: usize = 0x11;

/// Datasheet reset values (TMST_VALUE = 25 C, REVID = TPS65185 rev 1p2).
const DEFAULTS: [u8; NREGS] =
    [25, 0x00, 0x23, 0x7D, 0x00, 0x7F, 0x55, 0x00, 0x00, 0xE4, 0x55, 0x1E, 0xE0, 0x20, 0x78, 0x00, 0x65];

pub struct Tps65185 {
    regs: [u8; NREGS],
    ptr: u8,
    expect_ptr: bool,
    wakeup: bool,
    pwrup: bool,
    /// Power-up requested over I2C (ENABLE.ACTIVE) rather than the PWRUP pin.
    i2c_active: bool,
    /// Rails are ramping / up since this time.
    rails_since: Option<u64>,
    /// Time from power-up to all rails in regulation.
    pub pg_delay_ns: u64,
}

impl Default for Tps65185 {
    fn default() -> Self {
        Self::new()
    }
}

impl Tps65185 {
    pub fn new() -> Self {
        Tps65185 {
            regs: DEFAULTS,
            ptr: 0,
            expect_ptr: false,
            wakeup: false,
            pwrup: false,
            i2c_active: false,
            rails_since: None,
            pg_delay_ns: 1_000_000,
        }
    }

    /// WAKEUP / PWRUP pin levels (from the TCA9535 outputs P1_5 / P1_3).
    /// WAKEUP low puts the chip in SLEEP (registers back to defaults).
    pub fn set_pins(&mut self, now: u64, wakeup: bool, pwrup: bool) {
        if self.wakeup && !wakeup {
            self.regs = DEFAULTS;
            self.i2c_active = false;
        }
        self.wakeup = wakeup;
        self.pwrup = pwrup;
        self.refresh(now);
    }

    fn powered(&self) -> bool {
        self.wakeup && (self.pwrup || self.i2c_active)
    }

    fn refresh(&mut self, now: u64) {
        match (self.powered(), self.rails_since) {
            (true, None) => self.rails_since = Some(now),
            (false, Some(_)) => self.rails_since = None,
            _ => {}
        }
    }

    /// High-voltage panel rails (VPOS/VNEG/VDDH/VEE, VCOM) are up and in regulation.
    pub fn rails_on(&self, now: u64) -> bool {
        self.rails_since.is_some_and(|t| now >= t + self.pg_delay_ns)
    }

    /// Level of the PWR_GOOD pin (to TCA9535 P1_6).
    pub fn pwr_good(&self, now: u64) -> bool {
        self.rails_on(now)
    }

    /// Programmed VCOM in millivolts (negative), e.g. -1100.
    pub fn vcom_mv(&self) -> i32 {
        let raw = self.regs[REG_VCOM1 as usize] as i32 | ((self.regs[REG_VCOM2 as usize] as i32 & 1) << 8);
        -10 * raw
    }

    pub fn reg(&self, r: u8) -> u8 {
        self.regs.get(r as usize).copied().unwrap_or(0)
    }

    fn write_reg(&mut self, now: u64, r: u8, v: u8) {
        match r {
            REG_TMST_VALUE | REG_INT1 | REG_INT2 | REG_PG | REG_REVID => {} // read-only
            REG_ENABLE => {
                if v & ENABLE_STANDBY != 0 {
                    self.i2c_active = false;
                } else if v & ENABLE_ACTIVE != 0 {
                    self.i2c_active = true;
                }
                self.regs[REG_ENABLE as usize] = v & !(ENABLE_ACTIVE | ENABLE_STANDBY);
                self.refresh(now);
            }
            r if (r as usize) < NREGS => self.regs[r as usize] = v,
            _ => {}
        }
    }

    fn read_reg(&mut self, now: u64, r: u8) -> u8 {
        match r {
            REG_PG if self.rails_on(now) => PG_ALL_GOOD,
            REG_PG => 0,
            REG_INT1 | REG_INT2 => std::mem::take(&mut self.regs[r as usize]),
            r if (r as usize) < NREGS => self.regs[r as usize],
            _ => 0,
        }
    }
}

impl I2cDevice for Tps65185 {
    fn address(&self) -> u8 {
        ADDR
    }

    fn start(&mut self, _now: u64, read: bool) -> bool {
        self.expect_ptr = !read;
        true
    }

    fn write(&mut self, now: u64, byte: u8) -> bool {
        if self.expect_ptr {
            self.expect_ptr = false;
            self.ptr = byte;
        } else {
            self.write_reg(now, self.ptr, byte);
            self.ptr = self.ptr.wrapping_add(1);
        }
        true
    }

    fn read(&mut self, now: u64, _ack: bool) -> u8 {
        let v = self.read_reg(now, self.ptr);
        self.ptr = self.ptr.wrapping_add(1);
        v
    }

    fn stop(&mut self, _now: u64) {
        self.expect_ptr = false;
    }

    fn next_event_ns(&self, now: u64) -> Option<u64> {
        self.rails_since.map(|t| t + self.pg_delay_ns).filter(|&t| t > now)
    }
}

#[cfg(test)]
mod tests {
    use super::super::I2cBus;
    use super::*;

    const MS: u64 = 1_000_000;

    fn tps(bus: &mut I2cBus) -> &mut Tps65185 {
        bus.device_mut::<Tps65185>().unwrap()
    }

    /// bbepI2CReadRegister(0x68, 0x0F, &v, 1)
    fn read_pg(bus: &mut I2cBus, now: u64) -> u8 {
        assert!(bus.write_txn(now, 0x68, &[REG_PG]));
        bus.read_txn(now, 0x68, 1).unwrap()[0]
    }

    #[test]
    fn fastepd_power_on_pg_poll_and_power_off() {
        let mut bus = I2cBus::new();
        bus.add(Box::new(Tps65185::new()));
        let mut t = 0;
        assert_eq!(read_pg(&mut bus, t), 0);
        // WAKEUP then PWRUP via the expander.
        tps(&mut bus).set_pins(t, true, false);
        tps(&mut bus).set_pins(t, true, true);
        t += 3 * MS; // vTaskDelay(3) + PWR_GOOD pin spin
        assert!(tps(&mut bus).pwr_good(t));
        assert!(bus.write_txn(t, 0x68, &[REG_ENABLE, 0x3f]));
        // iVCOM = -1100 -> vcom = 110 -> [3, 110, 0]
        assert!(bus.write_txn(t, 0x68, &[REG_VCOM1, 110, 0]));
        assert_eq!(tps(&mut bus).vcom_mv(), -1100);
        // PG poll loop: up to 400 reads 1 ms apart.
        let mut tries = 0;
        let mut v = 0;
        while tries < 400 && v & 0xfa != 0xfa {
            v = read_pg(&mut bus, t);
            tries += 1;
            t += MS;
        }
        assert_eq!(v, PG_ALL_GOOD);
        assert_eq!(tries, 1);
        assert!(tps(&mut bus).rails_on(t));
        // Repeated-start read of VCOM1/VCOM2 (pointer auto-increments).
        assert_eq!(bus.write_read(t, 0x68, &[REG_VCOM1], 2), Some(vec![110, 0]));
        // Power off: PWRUP low -> rails down; WAKEUP low -> sleep resets registers.
        tps(&mut bus).set_pins(t, true, false);
        assert!(!tps(&mut bus).rails_on(t));
        assert_eq!(read_pg(&mut bus, t), 0);
        tps(&mut bus).set_pins(t, false, false);
        assert_eq!(tps(&mut bus).reg(REG_VCOM1), 0x7D);
    }

    #[test]
    fn pg_needs_ramp_time_and_i2c_active_bit() {
        let mut bus = I2cBus::new();
        bus.add(Box::new(Tps65185::new()));
        tps(&mut bus).set_pins(0, true, true);
        assert_eq!(read_pg(&mut bus, 0), 0);
        assert_eq!(tps(&mut bus).next_event_ns(0), Some(MS));
        assert_eq!(read_pg(&mut bus, MS), PG_ALL_GOOD);
        // ENABLE.ACTIVE powers up without the PWRUP pin; STANDBY powers down.
        tps(&mut bus).set_pins(2 * MS, true, false);
        assert!(bus.write_txn(2 * MS, 0x68, &[REG_ENABLE, 0xBF]));
        assert_eq!(read_pg(&mut bus, 4 * MS), PG_ALL_GOOD);
        assert!(bus.write_txn(4 * MS, 0x68, &[REG_ENABLE, 0x7F]));
        assert_eq!(read_pg(&mut bus, 6 * MS), 0);
        // Revision ID register reads back.
        assert_eq!(bus.write_read(0, 0x68, &[REG_REVID], 1), Some(vec![0x65]));
    }
}
