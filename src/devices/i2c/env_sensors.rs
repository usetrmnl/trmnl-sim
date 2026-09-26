//! Environment sensors a TRMNL OG can have on its I2C header (`--sensor`), as the
//! firmware's `EnvironmentSensor` finds them through bb_scd41 and bb_temperature:
//!
//! - [`Scd41`] (0x62): Sensirion CO2 sensor. 16-bit big-endian commands; reads return
//!   words, each followed by its CRC-8 (poly 0x31, init 0xFF). GET_DATA_READY_STATUS
//!   reports a sample once SINGLE_SHOT_MEASUREMENT (or periodic mode) was started;
//!   READ_MEASUREMENT returns CO2 (ppm), temperature and humidity.
//! - [`Aht20`] (0x38): ASAIR temperature/humidity sensor. Every read starts with the
//!   status byte (0x18: calibrated, idle); after a 0xAC measure command it is followed
//!   by 20-bit humidity and 20-bit temperature.

use super::I2cDevice;

pub const SCD41_ADDR: u8 = 0x62;
pub const AHT20_ADDR: u8 = 0x38;

const CMD_SINGLE_SHOT: u16 = 0x219d;
const CMD_START_PERIODIC: u16 = 0x21b1;
const CMD_START_LP_PERIODIC: u16 = 0x21ac;
const CMD_READ_MEASUREMENT: u16 = 0xec05;
const CMD_GET_DATA_READY: u16 = 0xe4b8;

/// The readings the sensors report.
#[derive(Clone, Copy, Debug)]
pub struct Climate {
    pub co2_ppm: u16,
    pub temperature_c: f32,
    pub humidity_pct: f32,
}

impl Default for Climate {
    fn default() -> Self {
        Climate { co2_ppm: 812, temperature_c: 22.5, humidity_pct: 45.0 }
    }
}

fn crc8(data: &[u8]) -> u8 {
    let mut crc = 0xffu8;
    for &b in data {
        crc ^= b;
        for _ in 0..8 {
            crc = if crc & 0x80 != 0 { (crc << 1) ^ 0x31 } else { crc << 1 };
        }
    }
    crc
}

pub struct Scd41 {
    pub climate: Climate,
    cmd: Vec<u8>,
    out: Vec<u8>,
    pos: usize,
    measuring: bool,
}

impl Scd41 {
    pub fn new(climate: Climate) -> Self {
        Scd41 { climate, cmd: Vec::new(), out: Vec::new(), pos: 0, measuring: false }
    }

    fn words(words: &[u16]) -> Vec<u8> {
        words
            .iter()
            .flat_map(|w| {
                let b = w.to_be_bytes();
                [b[0], b[1], crc8(&b)]
            })
            .collect()
    }

    fn command(&mut self, cmd: u16) {
        match cmd {
            CMD_SINGLE_SHOT | CMD_START_PERIODIC | CMD_START_LP_PERIODIC => self.measuring = true,
            CMD_GET_DATA_READY => self.out = Self::words(&[if self.measuring { 0x8006 } else { 0x8000 }]),
            CMD_READ_MEASUREMENT => {
                let c = self.climate;
                let t = ((c.temperature_c + 45.0) * 65536.0 / 175.0).clamp(0.0, 65535.0) as u16;
                let h = (c.humidity_pct * 65536.0 / 100.0).clamp(0.0, 65535.0) as u16;
                self.out = Self::words(&[c.co2_ppm, t, h]);
            }
            _ => {}
        }
        self.pos = 0;
    }
}

impl I2cDevice for Scd41 {
    fn address(&self) -> u8 {
        SCD41_ADDR
    }

    fn start(&mut self, _now: u64, read: bool) -> bool {
        if !read {
            self.cmd.clear();
        }
        true
    }

    fn write(&mut self, _now: u64, byte: u8) -> bool {
        self.cmd.push(byte);
        if self.cmd.len() == 2 {
            self.command(u16::from_be_bytes([self.cmd[0], self.cmd[1]]));
        }
        true
    }

    fn read(&mut self, _now: u64, _ack: bool) -> u8 {
        let b = self.out.get(self.pos).copied().unwrap_or(0xff);
        self.pos += 1;
        b
    }

    fn stop(&mut self, _now: u64) {}
}

pub struct Aht20 {
    pub climate: Climate,
    cmd: Vec<u8>,
    pos: usize,
    measured: bool,
}

impl Aht20 {
    pub fn new(climate: Climate) -> Self {
        Aht20 { climate, cmd: Vec::new(), pos: 0, measured: false }
    }

    fn data(&self) -> [u8; 6] {
        let c = self.climate;
        let h = ((c.humidity_pct / 100.0) * (1u32 << 20) as f32).clamp(0.0, 1048575.0) as u32;
        let t = (((c.temperature_c + 50.0) / 200.0) * (1u32 << 20) as f32).clamp(0.0, 1048575.0) as u32;
        [0x18, (h >> 12) as u8, (h >> 4) as u8, ((h & 0xf) << 4 | t >> 16) as u8, (t >> 8) as u8, t as u8]
    }
}

impl I2cDevice for Aht20 {
    fn address(&self) -> u8 {
        AHT20_ADDR
    }

    fn start(&mut self, _now: u64, read: bool) -> bool {
        if read {
            self.pos = 0;
        } else {
            self.cmd.clear();
        }
        true
    }

    fn write(&mut self, _now: u64, byte: u8) -> bool {
        self.cmd.push(byte);
        if self.cmd == [0xac, 0x33, 0x00] {
            self.measured = true;
        }
        true
    }

    fn read(&mut self, _now: u64, _ack: bool) -> u8 {
        let data = self.data();
        let b = if self.measured {
            data.get(self.pos).copied().unwrap_or(0)
        } else if self.pos == 0 {
            0x18
        } else {
            0
        };
        self.pos += 1;
        b
    }

    fn stop(&mut self, _now: u64) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc_matches_the_datasheet_example() {
        assert_eq!(crc8(&[0xbe, 0xef]), 0x92);
    }

    #[test]
    fn scd41_reports_a_sample_after_a_single_shot() {
        let mut s = Scd41::new(Climate::default());
        let mut cmd = |s: &mut Scd41, c: u16| {
            s.start(0, false);
            for b in c.to_be_bytes() {
                s.write(0, b);
            }
            s.stop(0);
        };
        let read = |s: &mut Scd41, n: usize| -> Vec<u8> {
            s.start(0, true);
            (0..n).map(|_| s.read(0, true)).collect()
        };
        cmd(&mut s, CMD_GET_DATA_READY);
        assert_eq!(read(&mut s, 3)[1] & 0x07, 0);
        cmd(&mut s, CMD_SINGLE_SHOT);
        cmd(&mut s, CMD_GET_DATA_READY);
        assert_ne!(read(&mut s, 3)[1] & 0x07, 0);
        cmd(&mut s, CMD_READ_MEASUREMENT);
        let d = read(&mut s, 9);
        assert_eq!(u16::from_be_bytes([d[0], d[1]]), 812);
        assert_eq!(d[2], crc8(&d[0..2]));
        // bb_scd41: T = -45 + 175 * raw / 65536 (in tenths)
        let t = -450 + (u16::from_be_bytes([d[3], d[4]]) as i32 * 1750 / 65536);
        assert_eq!(t, 224);
    }

    #[test]
    fn aht20_status_then_measurement() {
        let mut a = Aht20::new(Climate::default());
        a.start(0, true);
        assert_eq!(a.read(0, false), 0x18);
        a.start(0, false);
        for b in [0xac, 0x33, 0x00] {
            a.write(0, b);
        }
        a.start(0, true);
        let d: Vec<u8> = (0..6).map(|_| a.read(0, true)).collect();
        // bb_temperature's conversion
        let h = ((d[1] as u32) << 12 | (d[2] as u32) << 4 | (d[3] as u32) >> 4) * 100 >> 20;
        let t = (((d[3] as u32 & 0xf) << 16 | (d[4] as u32) << 8 | d[5] as u32) >> 10) as i32;
        assert_eq!(h, 44);
        assert_eq!((t * 2000 >> 10) - 500, 224);
    }
}
