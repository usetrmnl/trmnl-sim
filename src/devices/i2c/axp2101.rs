//! X-Powers AXP2101 PMIC (0x34), as read by the Waveshare ESP32-S3 3.97" board: the
//! firmware reads the battery voltage from VBAT_H/VBAT_L (0x34/0x35, big-endian, 14 bits
//! in mV).
//!
//! A register file with an auto-incrementing pointer: writes are stored (power rails,
//! ADC enables) and read back; the status and ADC registers reflect the battery.

use super::I2cDevice;
use crate::savepoint::{StateReader, StateWriter};

pub const ADDR: u8 = 0x34;

const CHIP_ID: u8 = 0x03;
const STATUS1: u8 = 0x00;
const STATUS2: u8 = 0x01;
const VBAT_H: u8 = 0x34;
const VBAT_L: u8 = 0x35;
const BAT_PERCENT: u8 = 0xa4;

pub struct Axp2101 {
    regs: [u8; 256],
    ptr: u8,
    have_ptr: bool,
    pub mv: u16,
    pub charging: bool,
    pub soc: u8,
}

impl Default for Axp2101 {
    fn default() -> Self {
        Self::new()
    }
}

impl Axp2101 {
    pub fn new() -> Self {
        let mut regs = [0u8; 256];
        regs[CHIP_ID as usize] = 0x4a;
        Axp2101 { regs, ptr: 0, have_ptr: false, mv: 4100, charging: false, soc: 85 }
    }

    pub fn set_battery(&mut self, mv: u16, charging: bool, soc: u8) {
        self.mv = mv;
        self.charging = charging;
        self.soc = soc.min(100);
    }

    fn reg(&self, r: u8) -> u8 {
        match r {
            // STATUS1: battery present (bit 3), VBUS good (bit 5) while charging
            STATUS1 => 0x08 | if self.charging { 0x20 } else { 0 },
            // STATUS2: charge direction (bits 6:5 = 01 charging, 10 discharging)
            STATUS2 => {
                if self.charging {
                    0x20
                } else {
                    0x40
                }
            }
            VBAT_H => (self.mv >> 8) as u8 & 0x3f,
            VBAT_L => self.mv as u8,
            BAT_PERCENT => self.soc,
            _ => self.regs[r as usize],
        }
    }
}

impl I2cDevice for Axp2101 {
    fn address(&self) -> u8 {
        ADDR
    }

    fn start(&mut self, _now: u64, read: bool) -> bool {
        if !read {
            self.have_ptr = false;
        }
        true
    }

    fn write(&mut self, _now: u64, byte: u8) -> bool {
        if !self.have_ptr {
            self.ptr = byte;
            self.have_ptr = true;
        } else {
            self.regs[self.ptr as usize] = byte;
            self.ptr = self.ptr.wrapping_add(1);
        }
        true
    }

    fn read(&mut self, _now: u64, _ack: bool) -> u8 {
        let b = self.reg(self.ptr);
        self.ptr = self.ptr.wrapping_add(1);
        b
    }

    fn stop(&mut self, _now: u64) {}

    fn save_state(&self, w: &mut StateWriter) {
        w.bytes(&self.regs);
        w.u8(self.ptr);
        w.u32(self.mv as u32);
        w.bool(self.charging);
        w.u8(self.soc);
    }

    fn restore_state(&mut self, r: &mut StateReader) -> anyhow::Result<()> {
        r.fill_u8(&mut self.regs)?;
        self.ptr = r.u8()?;
        self.mv = r.u32()? as u16;
        self.charging = r.bool()?;
        self.soc = r.u8()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devices::i2c::I2cBus;

    #[test]
    fn battery_voltage_is_big_endian_14_bits() {
        let mut bus = I2cBus::new();
        bus.add(Box::new(Axp2101::new()));
        bus.device_mut::<Axp2101>().unwrap().set_battery(3876, false, 60);
        assert_eq!(bus.write_read(0, ADDR, &[VBAT_H], 2), Some(vec![(3876u16 >> 8) as u8, 3876u16 as u8]));
        assert_eq!(bus.write_read(0, ADDR, &[CHIP_ID], 1), Some(vec![0x4a]));
        // writes read back
        assert!(bus.write_txn(0, ADDR, &[0x90, 0xbf]));
        assert_eq!(bus.write_read(0, ADDR, &[0x90], 1), Some(vec![0xbf]));
    }
}
