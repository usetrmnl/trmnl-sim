//! The PY32 microcontroller M5Stack uses as a power-management / I/O chip (I2C 0x6e) on
//! the M5Paper Color: its GPIO0 switches the e-paper panel's supply. bb_epaper's
//! `begin(EPD_M5_PAPER_COLOR)` disables its watchdog (0x0a) and I2C idle sleep (0x09) and
//! makes GPIO0 a push-pull output driven high, each with a read-modify-write of one bit:
//! 0x16 function (1 = alternate function), 0x10 direction (1 = output), 0x13 drive
//! (1 = open drain), 0x11 output level.
//!
//! Byte registers behind an auto-incrementing pointer (the first byte written); the
//! pointer latches across STOP for the following read. Only the registers bb_epaper
//! touches mean anything here; the rest read back what was written.

use super::I2cDevice;
use crate::savepoint::{StateReader, StateWriter};

pub const ADDR: u8 = 0x6e;

/// Chip ID (register 0x00), as bb_epaper's (disabled) board check expects of the Paper Color.
const ID: u8 = 0x50;
pub const REG_I2C_SLEEP: u8 = 0x09;
pub const REG_WATCHDOG: u8 = 0x0a;
pub const REG_GPIO_DIR: u8 = 0x10;
pub const REG_GPIO_OUT: u8 = 0x11;
pub const REG_GPIO_DRIVE: u8 = 0x13;
pub const REG_GPIO_FUNC: u8 = 0x16;

pub struct M5Py32 {
    regs: [u8; 256],
    ptr: u8,
    /// Next written byte is the register pointer.
    expect_ptr: bool,
}

impl Default for M5Py32 {
    fn default() -> Self {
        Self::new()
    }
}

impl M5Py32 {
    pub fn new() -> Self {
        let mut regs = [0u8; 256];
        regs[0] = ID;
        M5Py32 { regs, ptr: 0, expect_ptr: false }
    }

    pub fn reg(&self, r: u8) -> u8 {
        self.regs[r as usize]
    }

    /// Is GPIO `n` driving high (a push-pull GPIO output set high)?
    pub fn gpio_high(&self, n: u8) -> bool {
        let bit = |r: u8| self.regs[r as usize] >> n & 1 != 0;
        !bit(REG_GPIO_FUNC) && bit(REG_GPIO_DIR) && !bit(REG_GPIO_DRIVE) && bit(REG_GPIO_OUT)
    }
}

impl I2cDevice for M5Py32 {
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
            if self.ptr != 0 {
                self.regs[self.ptr as usize] = byte;
            }
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

    /// bb_epaper's setI2CBit(): write the pointer, read the register, write it back.
    fn set_bit(bus: &mut I2cBus, reg: u8, bit: u8, on: bool) {
        assert!(bus.write_txn(0, ADDR, &[reg]));
        assert!(bus.start(0, ADDR, true));
        let v = bus.read(0, false);
        bus.stop(0);
        let v = if on { v | 1 << bit } else { v & !(1 << bit) };
        assert!(bus.write_txn(0, ADDR, &[reg, v]));
    }

    #[test]
    fn bb_epaper_power_up_drives_gpio0_high() {
        let mut bus = I2cBus::new();
        bus.add(Box::new(M5Py32::new()));
        let py = |bus: &I2cBus| bus.device::<M5Py32>().unwrap().gpio_high(0);
        assert!(!py(&bus));
        assert!(bus.write_txn(0, ADDR, &[REG_WATCHDOG, 0]));
        set_bit(&mut bus, REG_GPIO_FUNC, 0, false);
        set_bit(&mut bus, REG_GPIO_DIR, 0, true);
        assert!(!py(&bus), "still low until the output is set");
        set_bit(&mut bus, REG_GPIO_DRIVE, 0, false);
        set_bit(&mut bus, REG_GPIO_OUT, 0, true);
        assert!(py(&bus));
        set_bit(&mut bus, REG_GPIO_DRIVE, 0, true);
        assert!(!py(&bus), "open drain doesn't drive high");
        // The ID register is read-only.
        assert!(bus.write_txn(0, ADDR, &[0, 0x12]));
        assert_eq!(bus.device::<M5Py32>().unwrap().reg(0), ID);
    }
}
