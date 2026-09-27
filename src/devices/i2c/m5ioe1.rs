//! M5Stack's M5IOE1 I/O expander (I2C 0x4f) on the M5Paper (mono): its GPIO3 switches
//! the e-paper panel's 3.3 V LDO and GPIO5 drives the panel's RST (the ESP32-S3 has no
//! RST line to it). bb_epaper's `M5IOE1_SetGPIO()` makes a pin a push-pull output without
//! pulls and sets its level, each step a read-modify-write of a 16-bit little-endian
//! register (bit n = GPIO n+1): 0x03 mode (1 = output), 0x05 output level, 0x09 pull-up,
//! 0x0b pull-down, 0x13 drive (1 = open drain).
//!
//! Byte registers behind an auto-incrementing pointer (the first byte written); the
//! pointer latches across STOP for the following read.

use super::I2cDevice;
use crate::savepoint::{StateReader, StateWriter};

pub const ADDR: u8 = 0x4f;

pub const REG_MODE: u8 = 0x03;
pub const REG_OUT: u8 = 0x05;
pub const REG_PULL_UP: u8 = 0x09;
pub const REG_PULL_DOWN: u8 = 0x0b;
pub const REG_DRIVE: u8 = 0x13;

pub struct M5Ioe1 {
    regs: [u8; 256],
    ptr: u8,
    expect_ptr: bool,
}

impl Default for M5Ioe1 {
    fn default() -> Self {
        Self::new()
    }
}

impl M5Ioe1 {
    pub fn new() -> Self {
        M5Ioe1 { regs: [0; 256], ptr: 0, expect_ptr: false }
    }

    fn reg16(&self, r: u8) -> u16 {
        u16::from_le_bytes([self.regs[r as usize], self.regs[r as usize + 1]])
    }

    /// The level GPIO `n` (1-based, as M5Stack numbers them) drives: `Some(level)` for a
    /// push-pull output (an open-drain output driving low counts too), `None` when it
    /// doesn't drive the line.
    pub fn gpio(&self, n: u8) -> Option<bool> {
        let bit = 1u16 << (n - 1);
        if self.reg16(REG_MODE) & bit == 0 {
            return None;
        }
        let high = self.reg16(REG_OUT) & bit != 0;
        let open_drain = self.reg16(REG_DRIVE) & bit != 0;
        (!(open_drain && high)).then_some(high)
    }
}

impl I2cDevice for M5Ioe1 {
    fn address(&self) -> u8 {
        ADDR
    }

    fn start(&mut self, _now: u64, read: bool) -> bool {
        self.expect_ptr = !read;
        true
    }

    fn write(&mut self, _now: u64, byte: u8) -> bool {
        if self.expect_ptr {
            self.ptr = byte;
            self.expect_ptr = false;
        } else {
            self.regs[self.ptr as usize] = byte;
            self.ptr = self.ptr.wrapping_add(1);
        }
        true
    }

    fn read(&mut self, _now: u64, _ack: bool) -> u8 {
        let v = self.regs[self.ptr as usize];
        self.ptr = self.ptr.wrapping_add(1);
        v
    }

    fn stop(&mut self, _now: u64) {}

    fn save_state(&self, w: &mut StateWriter) {
        w.bytes(&self.regs);
        w.u8(self.ptr);
    }

    fn restore_state(&mut self, r: &mut StateReader) -> anyhow::Result<()> {
        r.fill_u8(&mut self.regs)?;
        self.ptr = r.u8()?;
        self.expect_ptr = false;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devices::i2c::I2cBus;

    fn read16(bus: &mut I2cBus, reg: u8) -> u16 {
        assert!(bus.write_txn(0, ADDR, &[reg]));
        assert!(bus.start(0, ADDR, true));
        let lo = bus.read(0, true);
        let hi = bus.read(0, false);
        bus.stop(0);
        u16::from_le_bytes([lo, hi])
    }

    /// bb_epaper's M5IOE1_SetGPIO().
    fn set_gpio(bus: &mut I2cBus, gpio: u8, on: bool) {
        let bit = 1u16 << (gpio - 1);
        let mut rmw = |reg: u8, f: &dyn Fn(u16) -> u16| {
            let v = f(read16(bus, reg)).to_le_bytes();
            assert!(bus.write_txn(0, ADDR, &[reg, v[0], v[1]]));
        };
        rmw(REG_DRIVE, &|v| v & !bit);
        rmw(REG_PULL_UP, &|v| v & !bit);
        rmw(REG_PULL_DOWN, &|v| v & !bit);
        rmw(REG_MODE, &|v| v | bit);
        rmw(REG_OUT, &|v| if on { v | bit } else { v & !bit });
    }

    #[test]
    fn bb_epaper_drives_the_ldo_and_reset() {
        let mut bus = I2cBus::new();
        bus.add(Box::new(M5Ioe1::new()));
        let io = |bus: &I2cBus, n| bus.device::<M5Ioe1>().unwrap().gpio(n);
        assert_eq!(io(&bus, 3), None);
        set_gpio(&mut bus, 3, true);
        set_gpio(&mut bus, 5, false);
        assert_eq!((io(&bus, 3), io(&bus, 5)), (Some(true), Some(false)));
        set_gpio(&mut bus, 5, true);
        assert_eq!((io(&bus, 3), io(&bus, 5)), (Some(true), Some(true)));
        set_gpio(&mut bus, 13, true); // a pin in the high byte
        assert_eq!((io(&bus, 3), io(&bus, 13)), (Some(true), Some(true)));
    }
}
