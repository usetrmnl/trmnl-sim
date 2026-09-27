//! TI BQ27220 single-cell fuel gauge (0x55), as read by BYOD boards (Xteink X3, Seeed
//! Sticky, LilyGo T5 Pro): the firmware reads Voltage() (command 0x08) directly.
//!
//! Standard commands are 16-bit little-endian registers at even command codes; a read
//! starts at the command written last and auto-increments. Control() (0x00) subcommands
//! are accepted and ignored.

use super::I2cDevice;
use crate::savepoint::{StateReader, StateWriter};

pub const ADDR: u8 = 0x55;

pub struct Bq27220 {
    ptr: u8,
    /// A write transaction has set the pointer (the next bytes are data).
    have_ptr: bool,
    pub mv: u16,
    pub current_ma: i16,
    pub soc: u8,
    pub temp_c: f32,
}

impl Default for Bq27220 {
    fn default() -> Self {
        Self::new()
    }
}

impl Bq27220 {
    pub fn new() -> Self {
        Bq27220 { ptr: 0, have_ptr: false, mv: 4100, current_ma: 0, soc: 85, temp_c: 25.0 }
    }

    /// Battery state as the gauge would measure it.
    pub fn set_battery(&mut self, mv: u16, charging: bool, soc: u8) {
        self.mv = mv;
        self.current_ma = if charging { 400 } else { -12 };
        self.soc = soc.min(100);
    }

    fn reg16(&self, cmd: u8) -> u16 {
        const DESIGN_MAH: u16 = 1500;
        let remaining = (DESIGN_MAH as u32 * self.soc as u32 / 100) as u16;
        match cmd {
            0x06 => ((self.temp_c + 273.15) * 10.0) as u16, // Temperature, 0.1 K
            0x08 => self.mv,                                // Voltage, mV
            0x0a => {
                // BatteryStatus: DSG while discharging
                if self.current_ma < 0 { 0x0001 } else { 0x0000 }
            }
            0x0c | 0x14 => self.current_ma as u16, // Current / AverageCurrent, mA
            0x10 => remaining,                     // RemainingCapacity, mAh
            0x12 | 0x3c => DESIGN_MAH,             // FullChargeCapacity / DesignCapacity
            0x16 => {
                // TimeToEmpty, minutes (65535 = not discharging)
                if self.current_ma < 0 {
                    (remaining as u32 * 60 / self.current_ma.unsigned_abs() as u32).min(65534) as u16
                } else {
                    0xffff
                }
            }
            0x2c => self.soc as u16, // StateOfCharge, %
            0x2e => 100,             // StateOfHealth, %
            _ => 0,
        }
    }

    fn byte_at(&self, addr: u8) -> u8 {
        let v = self.reg16(addr & !1);
        if addr & 1 == 0 { v as u8 } else { (v >> 8) as u8 }
    }
}

impl I2cDevice for Bq27220 {
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
            // Control() subcommands and data memory writes: accepted, no effect.
            self.ptr = self.ptr.wrapping_add(1);
        }
        true
    }

    fn read(&mut self, _now: u64, _ack: bool) -> u8 {
        let b = self.byte_at(self.ptr);
        self.ptr = self.ptr.wrapping_add(1);
        b
    }

    fn stop(&mut self, _now: u64) {}

    fn save_state(&self, w: &mut StateWriter) {
        w.u8(self.ptr);
        w.u32(self.mv as u32);
        w.u8(self.soc);
        w.u32(self.current_ma as u16 as u32);
    }

    fn restore_state(&mut self, r: &mut StateReader) -> anyhow::Result<()> {
        self.ptr = r.u8()?;
        self.mv = r.u32()? as u16;
        self.soc = r.u8()?;
        self.current_ma = r.u32()? as u16 as i16;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devices::i2c::I2cBus;

    #[test]
    fn voltage_reads_little_endian_like_the_firmware() {
        let mut bus = I2cBus::new();
        bus.add(Box::new(Bq27220::new()));
        bus.device_mut::<Bq27220>().unwrap().set_battery(3876, false, 60);
        // readReg16(0x55, 8): write the command, STOP, read two bytes
        assert_eq!(bus.write_read(0, ADDR, &[0x08], 2), Some(vec![3876u16 as u8, (3876u16 >> 8) as u8]));
        assert_eq!(bus.write_read(0, ADDR, &[0x2c], 2), Some(vec![60, 0]));
        let cur = bus.write_read(0, ADDR, &[0x0c], 2).unwrap();
        assert!(i16::from_le_bytes([cur[0], cur[1]]) < 0);
    }
}
