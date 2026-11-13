//! Bosch BMA530 accelerometer (TRMNL X: 0x18), as the firmware reads it through
//! Bosch's BMA5 Sensor API (lib/BMA530_SensorAPI, lib/trmnl_x/src/accelerometer.cpp).
//!
//! Registers (8-bit pointer, auto-incrementing on reads and writes; the pointer
//! latches across STOP):
//! - 0x00 CHIP_ID = 0xC2.
//! - 0x11 SENSOR_STATUS: bit 0 acc_data_rdy (a sample is valid), bit 2 sensor_rdy.
//! - 0x18..0x1D ACC_DATA x/y/z, 16-bit little-endian two's complement, full scale =
//!   the configured range (65536 LSB over ±range).
//! - 0x30 ACC_CONF_0: sensor_ctrl (bits 0-3, nonzero enables sampling; the firmware
//!   writes BMA5_SENSOR_CTRL_ENABLE = 0x0F).
//! - 0x31 ACC_CONF_1: acc_odr (bits 0-3, 1.5625 Hz << n), acc_bwp, power_mode.
//! - 0x32 ACC_CONF_2: acc_range (bits 0-1, ±2/4/8/16 g), noise mode, DRDY auto clear.
//!
//! The first transfer after power-on NACKs (Bosch's `bma530_init` does a dummy chip-ID
//! read for that reason, bma530_features.c). Sampling starts when ACC_CONF_0 enables
//! it; the first sample is valid one ODR period later (the firmware polls
//! acc_data_rdy for 50 ms after enabling, `bma530_wait_for_data_ready`). Until then the
//! data registers read zero.
//!
//! The reading is gravity in the die's frame, from the device [`Pose`]: the in-plane
//! component lies at the angle the firmware's `bma530_compute_orientation` maps back to
//! the pose's rotation (the die sits 45° to the panel edges, BMA530_MOUNT_OFFSET_DEG),
//! and the tilt moves gravity onto the Z axis (flat on its back reads +1 g on Z).

use sim_api::Pose;

use super::I2cDevice;
use crate::savepoint::{StateReader, StateWriter};

pub const ADDR: u8 = 0x18;

pub const CHIP_ID: u8 = 0xc2;

const REG_CHIP_ID: u8 = 0x00;
const REG_SENSOR_STATUS: u8 = 0x11;
const REG_ACC_DATA_0: u8 = 0x18;
const REG_ACC_CONF_0: u8 = 0x30;
const REG_ACC_CONF_1: u8 = 0x31;
const REG_ACC_CONF_2: u8 = 0x32;

/// Rotation 0 (landscape, as shipped) puts gravity's in-plane component at this angle
/// of `atan2(-ay, ax)`: the firmware adds BMA530_MOUNT_OFFSET_DEG (45°) and calls
/// 180° LANDSCAPE_RIGHT, which it draws at rotation 0.
const PHI_AT_ROTATION_0: f32 = 135.0;

pub struct Bma530 {
    pub pose: Pose,
    regs: [u8; 0x80],
    ptr: u8,
    /// Next written byte is the register pointer.
    expect_ptr: bool,
    /// The interface has not been accessed since power-on (that first access NACKs).
    asleep: bool,
    /// Sampling was enabled at this time (None = sensor_ctrl is 0).
    enabled_at: Option<u64>,
}

impl Default for Bma530 {
    fn default() -> Self {
        Self::new()
    }
}

impl Bma530 {
    pub fn new() -> Self {
        let mut regs = [0u8; 0x80];
        regs[REG_CHIP_ID as usize] = CHIP_ID;
        regs[REG_ACC_CONF_1 as usize] = 0x06; // 100 Hz
        Bma530 { pose: Pose::default(), regs, ptr: 0, expect_ptr: false, asleep: true, enabled_at: None }
    }

    fn odr_period_ns(&self) -> u64 {
        let code = (self.regs[REG_ACC_CONF_1 as usize] & 0x0f).min(0x0c);
        (1e9 / (1.5625 * (1u32 << code) as f64)) as u64
    }

    fn sample_valid(&self, now: u64) -> bool {
        self.enabled_at.is_some_and(|t| now >= t + self.odr_period_ns())
    }

    /// Gravity in the die's frame, in g.
    pub fn gravity(&self) -> [f32; 3] {
        let (rot, tilt) = (self.pose.rotation_deg, self.pose.tilt_deg.to_radians());
        let phi = (PHI_AT_ROTATION_0 + rot).to_radians();
        let inplane = tilt.cos();
        [inplane * phi.cos(), -inplane * phi.sin(), tilt.sin()]
    }

    fn raw_axes(&self) -> [i16; 3] {
        let range_g = 2.0 * (1u32 << (self.regs[REG_ACC_CONF_2 as usize] & 0x03)) as f32;
        let lsb_per_g = 32768.0 / range_g;
        self.gravity().map(|g| (g * lsb_per_g).round().clamp(-32768.0, 32767.0) as i16)
    }

    fn read_reg(&self, now: u64, reg: u8) -> u8 {
        let valid = self.sample_valid(now);
        match reg {
            REG_SENSOR_STATUS => 0x04 | valid as u8,
            REG_ACC_DATA_0..=0x1d => {
                if !valid {
                    return 0;
                }
                let i = (reg - REG_ACC_DATA_0) as usize;
                self.raw_axes()[i / 2].to_le_bytes()[i % 2]
            }
            r => self.regs[(r & 0x7f) as usize],
        }
    }

    fn write_reg(&mut self, now: u64, reg: u8, v: u8) {
        match reg {
            REG_CHIP_ID | REG_SENSOR_STATUS | REG_ACC_DATA_0..=0x1d => {}
            REG_ACC_CONF_0 => {
                let on = v & 0x0f != 0;
                if !on {
                    self.enabled_at = None;
                } else if self.enabled_at.is_none() {
                    self.enabled_at = Some(now);
                }
                self.regs[reg as usize] = v;
            }
            r => self.regs[(r & 0x7f) as usize] = v,
        }
    }
}

impl I2cDevice for Bma530 {
    fn address(&self) -> u8 {
        ADDR
    }

    fn start(&mut self, _now: u64, read: bool) -> bool {
        if std::mem::take(&mut self.asleep) {
            return false;
        }
        if !read {
            self.expect_ptr = true;
        }
        true
    }

    fn write(&mut self, now: u64, byte: u8) -> bool {
        if std::mem::take(&mut self.expect_ptr) {
            self.ptr = byte & 0x7f;
        } else {
            self.write_reg(now, self.ptr, byte);
            self.ptr = (self.ptr + 1) & 0x7f;
        }
        true
    }

    fn read(&mut self, now: u64, _ack: bool) -> u8 {
        let b = self.read_reg(now, self.ptr);
        self.ptr = (self.ptr + 1) & 0x7f;
        b
    }

    fn stop(&mut self, _now: u64) {
        self.expect_ptr = false;
    }

    fn save_state(&self, w: &mut StateWriter) {
        w.bytes(&self.regs);
        w.u8(self.ptr);
        w.bool(self.asleep);
        w.bool(self.enabled_at.is_some());
        w.u64(self.enabled_at.unwrap_or(0));
    }

    fn restore_state(&mut self, r: &mut StateReader) -> anyhow::Result<()> {
        let regs = r.bytes()?;
        anyhow::ensure!(regs.len() == self.regs.len(), "bma530: {} registers", regs.len());
        self.regs.copy_from_slice(regs);
        self.ptr = r.u8()?;
        self.asleep = r.bool()?;
        let enabled = r.bool()?;
        let at = r.u64()?;
        self.enabled_at = enabled.then_some(at);
        self.expect_ptr = false;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: u64 = 1_000_000;

    fn write(b: &mut Bma530, now: u64, reg: u8, data: &[u8]) -> bool {
        let ack = b.start(now, false);
        if ack {
            b.write(now, reg);
            for &d in data {
                b.write(now, d);
            }
        }
        b.stop(now);
        ack
    }

    fn read(b: &mut Bma530, now: u64, reg: u8, n: usize) -> Option<Vec<u8>> {
        if !b.start(now, false) {
            b.stop(now);
            return None;
        }
        b.write(now, reg);
        b.start(now, true);
        let v = (0..n).map(|i| b.read(now, i + 1 < n)).collect();
        b.stop(now);
        Some(v)
    }

    /// The firmware's bring-up: dummy read, chip ID, LPM 25 Hz ±4 g, enable.
    fn bring_up(b: &mut Bma530, now: u64) {
        assert_eq!(read(b, now, REG_CHIP_ID, 1), None, "first access after power-on NACKs");
        assert_eq!(read(b, now, REG_CHIP_ID, 1), Some(vec![CHIP_ID]));
        write(b, now, REG_ACC_CONF_1, &[0x04, 0x01]);
        write(b, now, REG_ACC_CONF_0, &[0x0f]);
    }

    fn axes(b: &mut Bma530, now: u64) -> [i16; 3] {
        let d = read(b, now, REG_ACC_DATA_0, 6).unwrap();
        [0, 1, 2].map(|i| i16::from_le_bytes([d[2 * i], d[2 * i + 1]]))
    }

    /// accelerometer.cpp `bma530_compute_orientation`: (orientation, is_flat).
    fn firmware_orientation(a: [i16; 3]) -> (u8, bool) {
        let [x, y, z] = a.map(|v| v as i64);
        let flat = 64 * (x * x + y * y) < 0x27 * z * z;
        let mut phi = (-(y as f32)).atan2(x as f32).to_degrees();
        if phi < 0.0 {
            phi += 360.0;
        }
        let mut c = phi + 45.0;
        if c >= 360.0 {
            c -= 360.0;
        }
        let o = if !(45.0..315.0).contains(&c) {
            1 // LANDSCAPE_LEFT
        } else if c < 135.0 {
            2 // PORTRAIT_UP_DOWN
        } else if c < 225.0 {
            3 // LANDSCAPE_RIGHT
        } else {
            0 // PORTRAIT_UP_RIGHT
        };
        (o, flat)
    }

    #[test]
    fn data_ready_one_odr_period_after_enable() {
        let mut b = Bma530::new();
        b.pose.tilt_deg = 0.0;
        bring_up(&mut b, 0);
        assert_eq!(read(&mut b, 0, REG_SENSOR_STATUS, 1).unwrap()[0] & 1, 0);
        assert_eq!(axes(&mut b, 0), [0, 0, 0]);
        assert_eq!(read(&mut b, 39 * MS, REG_SENSOR_STATUS, 1).unwrap()[0] & 1, 0);
        assert_eq!(read(&mut b, 40 * MS, REG_SENSOR_STATUS, 1).unwrap()[0] & 1, 1);
        // ±4 g: 8192 LSB/g; upright landscape has no Z component.
        let a = axes(&mut b, 40 * MS);
        assert_eq!(a[2], 0);
        assert!(((a[0] as f32).hypot(a[1] as f32) - 8192.0).abs() < 2.0);
        write(&mut b, 41 * MS, REG_ACC_CONF_0, &[0]);
        assert_eq!(read(&mut b, 100 * MS, REG_SENSOR_STATUS, 1).unwrap()[0] & 1, 0);
    }

    #[test]
    fn conf_reads_back_and_pointer_auto_increments() {
        let mut b = Bma530::new();
        bring_up(&mut b, 0);
        assert_eq!(read(&mut b, 0, REG_ACC_CONF_1, 2), Some(vec![0x04, 0x01]));
    }

    #[test]
    fn firmware_maps_each_pose_back() {
        // (rotation, firmware constant) per display_set_orientation.
        for (rot, want) in [(0.0, 3), (90.0, 0), (180.0, 1), (270.0, 2), (30.0, 3), (-30.0, 3), (100.0, 0)] {
            let mut b = Bma530::new();
            b.pose = Pose { rotation_deg: rot, tilt_deg: 20.0 };
            bring_up(&mut b, 0);
            assert_eq!(firmware_orientation(axes(&mut b, 50 * MS)), (want, false), "rotation {rot}");
        }
    }

    #[test]
    fn lying_flat_is_flat() {
        for (tilt, flat) in [(0.0, false), (45.0, false), (60.0, true), (90.0, true)] {
            let mut b = Bma530::new();
            b.pose = Pose { rotation_deg: 0.0, tilt_deg: tilt };
            bring_up(&mut b, 0);
            assert_eq!(firmware_orientation(axes(&mut b, 50 * MS)).1, flat, "tilt {tilt}");
        }
    }

    #[test]
    fn save_state_round_trip() {
        let mut b = Bma530::new();
        bring_up(&mut b, 5 * MS);
        let mut w = StateWriter::new();
        b.save_state(&mut w);
        let bytes = w.into_bytes();
        let mut c = Bma530::new();
        c.restore_state(&mut StateReader::new(&bytes)).unwrap();
        assert_eq!(axes(&mut c, 50 * MS), axes(&mut b, 50 * MS));
    }
}
