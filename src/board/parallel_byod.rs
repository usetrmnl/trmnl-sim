//! BYOD boards with a directly driven parallel e-paper panel: the firmware's
//! `#ifdef PARALLEL_EPD` `device_list[]` rows other than the TRMNL X, as FastEPD drives them.
//! Each is an ESP32-S3 feeding the panel's source drivers over LCD_CAM (i80, 8-bit bus) and
//! stepping its gate driver with GPIOs, like the X (see `devices::parallel_epd`); they differ
//! in how the panel's high voltages are switched, the button and the battery:
//!
//! * M5Stack PaperS3 (`m5_papers3`, `BB_PANEL_M5PAPERS3`): 4.7" ED047TC1 960x540. No PMIC on
//!   I2C: FastEPD's `PaperS3EinkPower` raises OE (GPIO45) and then the DC/DC enable PWR
//!   (GPIO46). Battery through a 1:2 divider on GPIO3. `device_list[]` gives no wake button
//!   (`interrupt_pin` 0xff), so it only wakes on its timer.
//! * LilyGo T5 4.7" S3 Pro (`lilygo_t5pro`, `BB_PANEL_EPDIY_V7` + `BBEP_DISPLAY_ED047TC1`):
//!   EPDiy V7 wiring, the same power path as the X: a TCA9535/PCA9535 expander (0x20: OE,
//!   GMOD, TPS PWRUP / VCOM_CTRL / WAKEUP, PWR_GOOD) and a TPS65185 PMIC (0x68) on I2C
//!   SDA 39 / SCL 40, plus a BQ27220 fuel gauge (0x55) there. Button on GPIO0.
//! * Sensoria C5 (`sensoria_c5`, `BB_PANEL_SENSORIA_C5`): an ESP32-C5 feeding a 1280x720 panel
//!   over PARLIO (8-bit bus). A PCA9535 (0x20) on SDA 7 / SCL 6 carries OE, GMOD, the gate
//!   driver's SPV and the TPS65185's PWRUP / VCOM_CTRL / WAKEUP (pins 0-5, PWR_GOOD on 6);
//!   the TPS65185 (0x68) is on the same bus. Button on GPIO0; `batt_pin` 0xff (reads 0 V).

use sim_api::BoardInfo;

use super::Board;
use super::spi_epd::Chip;
use crate::devices::i2c::I2cBus;
use crate::devices::i2c::bq27220::Bq27220;
use crate::devices::i2c::tca9535::{Tca9535, pins};
use crate::devices::i2c::tps65185::Tps65185;
use crate::devices::parallel_epd::{PanelGeometry, ParallelEpd};
use crate::savepoint::{StateReader, StateWriter};

/// How the panel's high voltages are switched.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Power {
    /// DC/DC enable and source-driver OE on GPIOs (M5Stack PaperS3).
    Gpio { pwr: u8, oe: u8 },
    /// EPDiy V7: TPS65185 sequenced through a TCA9535 expander on I2C.
    Epdiy,
    /// Sensoria C5: the same chips, on the expander's port 0 (and SPV there too).
    Sensoria,
}

/// The Sensoria's PCA9535 pins (FastEPD `SensoriaEinkPower` / `SensoriaRowControl`).
mod sensoria {
    pub const OE: u8 = 0;
    pub const SPV: u8 = 2;
    pub const PWRUP: u8 = 3;
    pub const WAKEUP: u8 = 5;
    pub const PWR_GOOD: u8 = 6;
}

/// How the firmware measures the battery (`batt_type`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Battery {
    /// The LiPo through a 1:2 divider on an ADC pin.
    Adc { pin: u8 },
    /// A BQ27220 fuel gauge on the I2C bus (0x55).
    Bq27220,
    /// `BATT_ADC` with no ADC pin (`batt_pin` 0xff): reads as 0 V.
    Unwired,
}

/// One supported board.
#[derive(Debug)]
pub struct ParallelSpec {
    /// The firmware's `DEVICE_MODEL` (its `device_list[]` row).
    pub model: &'static str,
    /// The PlatformIO environments that build it (the build directory's name).
    pub envs: &'static [&'static str],
    /// Shown in front-ends; also identifies the board in save points.
    pub name: &'static str,
    pub chip: Chip,
    pub geometry: PanelGeometry,
    /// Gate driver start pulse, clock and source-driver latch GPIOs (FastEPD ioSPV/ioCKV/ioLE).
    pub spv: u8,
    pub ckv: u8,
    pub le: u8,
    pub power: Power,
    /// Active-low wake button (`interrupt_pin`), if the firmware has one.
    pub button: Option<u8>,
    pub battery: Battery,
    /// The expander/PMIC bus (SDA, SCL), which idles high.
    pub i2c: Option<(u8, u8)>,
}

pub static SPECS: &[ParallelSpec] = &[
    ParallelSpec {
        model: "m5_papers3",
        envs: &["TRMNL_X_PAPERS3"],
        name: "M5Stack PaperS3",
        chip: Chip::Esp32s3,
        geometry: PanelGeometry::ED047TC1,
        spv: 17,
        ckv: 18,
        le: 15,
        power: Power::Gpio { pwr: 46, oe: 45 },
        button: None,
        battery: Battery::Adc { pin: 3 },
        i2c: None,
    },
    ParallelSpec {
        model: "lilygo_t5pro",
        envs: &["TRMNL_X_LILYGO_T5PRO"],
        name: "LilyGo T5 4.7\" S3 Pro",
        chip: Chip::Esp32s3,
        geometry: PanelGeometry::ED047TC1,
        spv: 45,
        ckv: 48,
        le: 42,
        power: Power::Epdiy,
        button: Some(0),
        battery: Battery::Bq27220,
        i2c: Some((39, 40)),
    },
    ParallelSpec {
        model: "sensoria_c5",
        envs: &["TRMNL_X_SENSORIAC5"],
        name: "Sensoria C5",
        chip: Chip::Esp32c5,
        geometry: PanelGeometry::SENSORIA_C5,
        spv: 0xff, // on the expander
        ckv: 5,
        le: 2,
        power: Power::Sensoria,
        button: Some(0),
        battery: Battery::Unwired,
        i2c: Some((7, 6)),
    },
];

/// The board for a PlatformIO environment or `DEVICE_MODEL` name.
pub fn find(name: &str) -> Option<&'static ParallelSpec> {
    SPECS.iter().find(|s| s.model == name || s.envs.contains(&name))
}

pub struct ParallelByodBoard {
    pub spec: &'static ParallelSpec,
    pub panel: ParallelEpd,
    i2c: I2cBus,
    button_down: bool,
    battery_mv: u32,
    out: u64,
    oe: u64,
    last_now: u64,
}

impl ParallelByodBoard {
    pub fn new(spec: &'static ParallelSpec) -> Self {
        let mut b = ParallelByodBoard {
            spec,
            panel: ParallelEpd::new(spec.geometry),
            i2c: Self::new_i2c(spec),
            button_down: false,
            battery_mv: 4100,
            out: 0,
            oe: 0,
            last_now: 0,
        };
        b.set_battery_mv(b.battery_mv);
        b
    }

    /// The I2C chips as they come out of power-on.
    fn new_i2c(spec: &ParallelSpec) -> I2cBus {
        let mut i2c = I2cBus::new();
        match spec.power {
            Power::Epdiy => {
                i2c.add(Box::new(Tca9535::new()));
                i2c.add(Box::new(Tps65185::new()));
            }
            Power::Sensoria => {
                let mut tca = Tca9535::new();
                tca.auto_pwr_good = false; // PWR_GOOD comes from the TPS65185 (pin 6)
                i2c.add(Box::new(tca));
                i2c.add(Box::new(Tps65185::new()));
            }
            Power::Gpio { .. } => {}
        }
        if spec.battery == Battery::Bq27220 {
            i2c.add(Box::new(Bq27220::new()));
        }
        i2c
    }

    /// Driven high by the SoC (undriven pins count as low: pull-downs on the enables).
    fn driven_high(&self, pin: u8) -> bool {
        self.oe >> pin & 1 != 0 && self.out >> pin & 1 != 0
    }

    /// Panel power and OE from the GPIOs or the expander/PMIC.
    fn sync_power(&mut self, now: u64) {
        match self.spec.power {
            Power::Gpio { pwr, oe } => {
                let (on, oe) = (self.driven_high(pwr), self.driven_high(oe));
                self.panel.set_power(now, on);
                self.panel.set_output_enable(now, oe);
            }
            Power::Epdiy => {
                let Some(tca) = self.i2c.device_mut::<Tca9535>() else { return };
                let _ = tca.take_output_changes();
                let (wakeup, pwrup, oe) = (
                    tca.driven_high(pins::TPS_WAKEUP),
                    tca.driven_high(pins::TPS_PWRUP),
                    tca.driven_high(pins::EPD_OE),
                );
                let tps = self.i2c.device_mut::<Tps65185>().expect("tps65185");
                tps.set_pins(now, wakeup, pwrup);
                let rails = tps.rails_on(now);
                self.panel.set_power(now, rails);
                self.panel.set_output_enable(now, oe);
            }
            Power::Sensoria => {
                let Some(tca) = self.i2c.device_mut::<Tca9535>() else { return };
                let _ = tca.take_output_changes();
                let (wakeup, pwrup, oe) = (
                    tca.driven_high(sensoria::WAKEUP),
                    tca.driven_high(sensoria::PWRUP),
                    tca.driven_high(sensoria::OE),
                );
                let tps = self.i2c.device_mut::<Tps65185>().expect("tps65185");
                tps.set_pins(now, wakeup, pwrup);
                let (rails, good) = (tps.rails_on(now), tps.pwr_good(now));
                let tca = self.i2c.device_mut::<Tca9535>().expect("tca9535");
                tca.set_input(sensoria::PWR_GOOD, good);
                self.panel.set_power(now, rails);
                self.panel.set_output_enable(now, oe);
                self.drive_rows(now);
            }
        }
    }

    /// The gate driver's SPV/CKV and the source driver's LE, from the GPIOs (and, on the
    /// Sensoria, SPV from the expander). Released lines idle high (as on the X).
    fn drive_rows(&mut self, now: u64) {
        let (out, oe) = (self.out, self.oe);
        let level = |pin: u8| pin >= 64 || oe >> pin & 1 == 0 || out >> pin & 1 != 0;
        let s = self.spec;
        let spv = match s.power {
            Power::Sensoria => self
                .i2c
                .device_mut::<Tca9535>()
                .is_none_or(|t| !t.is_output(sensoria::SPV) || t.driven_high(sensoria::SPV)),
            _ => level(s.spv),
        };
        self.panel.set_row_pins(now, spv, level(s.ckv), level(s.le));
    }
}

impl Board for ParallelByodBoard {
    fn gpio_out(&mut self, now: u64, out: u64, oe: u64) {
        self.last_now = now;
        self.out = out;
        self.oe = oe;
        self.drive_rows(now);
        self.sync_power(now);
    }

    fn gpio_in(&mut self, _now: u64) -> (u64, u64) {
        let (mut lv, mut mask) = (0u64, 0u64);
        if let Some(b) = self.spec.button {
            mask |= 1 << b;
            if !self.button_down {
                lv |= 1 << b;
            }
        }
        if let Some((sda, scl)) = self.spec.i2c {
            // I2C pull-ups
            mask |= 1 << sda | 1 << scl;
            lv |= 1 << sda | 1 << scl;
        }
        (lv, mask)
    }

    fn spi_transfer(&mut self, _now: u64, _host: u8, _mosi: &[u8], miso_len: usize) -> Vec<u8> {
        vec![0xff; miso_len]
    }

    fn i2c_start(&mut self, now: u64, bus: u8, addr: u8, read: bool) -> bool {
        self.last_now = now;
        bus == 0 && self.i2c.start(now, addr, read)
    }

    fn i2c_write(&mut self, now: u64, _bus: u8, byte: u8) -> bool {
        self.i2c.write(now, byte)
    }

    fn i2c_read(&mut self, now: u64, _bus: u8, ack: bool) -> u8 {
        self.i2c.read(now, ack)
    }

    fn i2c_stop(&mut self, now: u64, _bus: u8) {
        self.i2c.stop(now);
        self.sync_power(now);
    }

    fn lcd_transfer(&mut self, now: u64, data: &[u8]) {
        self.panel.bus_transfer(now, data);
    }

    fn adc_millivolts(&mut self, gpio: u8) -> u32 {
        match self.spec.battery {
            Battery::Adc { pin } if pin == gpio => self.battery_mv / 2,
            _ => 0,
        }
    }

    fn next_event_ns(&self, now: u64) -> Option<u64> {
        self.i2c.next_event_ns(now)
    }

    fn update(&mut self, now: u64) {
        self.last_now = now;
        self.i2c.update(now);
        self.panel.poll(now);
        self.sync_power(now);
    }

    fn diagnostics(&mut self, now: u64) -> String {
        let mut s = format!("panel: {:?}", self.panel.stats());
        if let Some(tca) = self.i2c.device_mut::<Tca9535>() {
            let (lv, oe) = tca.outputs();
            s += &format!("\ntca9535: out={lv:#06x} oe={oe:#06x}");
        }
        if let Some(tps) = self.i2c.device_mut::<Tps65185>() {
            s += &format!("\ntps rails_on={}", tps.rails_on(now));
        }
        s
    }

    fn info(&self) -> BoardInfo {
        BoardInfo {
            name: self.spec.name.into(),
            has_button: self.spec.button.is_some(),
            has_fuel_gauge: self.spec.battery == Battery::Bq27220,
            ..Default::default()
        }
    }

    fn set_button(&mut self, down: bool) {
        self.button_down = down;
    }

    fn set_faults(&mut self, faults: &sim_api::Faults) {
        self.i2c.absent = faults.i2c_absent.clone();
        if let Some(tps) = self.i2c.device_mut::<Tps65185>() {
            tps.rail_fault = faults.panel_busy_stuck;
        }
    }

    fn set_battery_mv(&mut self, mv: u32) {
        self.battery_mv = mv;
        // A rough LiPo curve for the gauge's state of charge: 3.3 V empty .. 4.2 V full.
        let soc = ((mv.clamp(3300, 4200) - 3300) * 100 / 900) as u8;
        if let Some(g) = self.i2c.device_mut::<Bq27220>() {
            g.set_battery(mv as u16, false, soc);
        }
    }

    fn display_status(&self, _now: u64) -> (bool, u64) {
        let s = self.panel.stats();
        (s.update_in_progress, s.updates)
    }

    fn save_state(&self, w: &mut StateWriter, powered: bool) {
        w.u32(self.battery_mv);
        w.u64(self.last_now);
        w.u64(self.out);
        w.u64(self.oe);
        w.section(|w| self.panel.save_state(w, powered));
        if powered {
            w.section(|w| self.i2c.save_state(w));
        }
    }

    fn restore_state(&mut self, r: &mut StateReader, powered: bool) -> anyhow::Result<()> {
        self.battery_mv = r.u32()?;
        self.last_now = r.u64()?;
        self.out = r.u64()?;
        self.oe = r.u64()?;
        r.section(|r| self.panel.restore_state(r, powered))?;
        self.i2c = Self::new_i2c(self.spec);
        if powered {
            r.section(|r| self.i2c.restore_state(r))?;
        }
        self.set_battery_mv(self.battery_mv);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Sensoria C5: FastEPD's SensoriaEinkPower raises OE, GMOD, WAKEUP, PWRUP and VCOM on
    /// the PCA9535's port 0, then spins on PWR_GOOD (pin 6), which follows the TPS65185.
    #[test]
    fn sensoria_power_comes_through_port_0_and_pwr_good_follows_the_pmic() {
        let mut b = ParallelByodBoard::new(find("sensoria_c5").unwrap());
        let wr = |b: &mut ParallelByodBoard, t: u64, bytes: &[u8]| {
            assert!(b.i2c_start(t, 0, 0x20, false));
            for &x in bytes {
                assert!(b.i2c_write(t, 0, x));
            }
            b.i2c_stop(t, 0);
        };
        let pwr_good = |b: &mut ParallelByodBoard, t: u64| {
            assert!(b.i2c_start(t, 0, 0x20, false));
            b.i2c_write(t, 0, 0); // input port 0
            assert!(b.i2c_start(t, 0, 0x20, true));
            let v = b.i2c_read(t, 0, false);
            b.i2c_stop(t, 0);
            v >> 6 & 1 != 0
        };
        wr(&mut b, 0, &[6, 0xc0]); // pins 0-5 outputs
        wr(&mut b, 0, &[2, 0b0011_1111]); // OE, GMOD, SPV, PWRUP, VCOM, WAKEUP
        assert!(!pwr_good(&mut b, 1));
        b.update(5_000_000);
        assert!(b.panel.stats().powered && b.panel.stats().output_enabled);
        assert!(pwr_good(&mut b, 5_000_001));
        assert_eq!(b.adc_millivolts(3), 0, "no battery divider");
        assert_eq!(b.gpio_in(0).0 >> 7 & 1, 1, "SDA pulled up");
    }

    #[test]
    fn models_and_envs_resolve() {
        assert_eq!(find("m5_papers3").unwrap().name, "M5Stack PaperS3");
        assert_eq!(find("TRMNL_X_LILYGO_T5PRO").unwrap().model, "lilygo_t5pro");
        assert!(find("x").is_none() && find("TRMNL_X").is_none());
        for s in SPECS {
            assert!(super::super::spi_epd::find(s.model).is_none(), "{} is also an SPI board", s.model);
        }
    }

    /// FastEPD's `PaperS3EinkPower` + one scan of rows over the bus: pixels only move while
    /// GPIO46 (PWR) and GPIO45 (OE) are both driven high.
    #[test]
    fn papers3_gpio_power_gates_the_panel() {
        let spec = find("m5_papers3").unwrap();
        let mut b = ParallelByodBoard::new(spec);
        let bit = |p: u8| 1u64 << p;
        let outs = bit(spec.spv) | bit(spec.ckv) | bit(spec.le) | bit(45) | bit(46);
        let scan = |b: &mut ParallelByodBoard, t: u64, lvl: u64| {
            // SPV low, then CKV rising: frame start
            b.gpio_out(t, lvl & !bit(spec.spv) & !bit(spec.ckv), outs);
            b.gpio_out(t + 1, (lvl & !bit(spec.spv)) | bit(spec.ckv), outs);
            b.gpio_out(t + 2, lvl | bit(spec.spv) | bit(spec.ckv), outs);
            let row = [0x55u8; 256]; // push every pixel toward black, 16 bytes padding
            for y in 0..540 {
                b.lcd_transfer(t + 3 + y, &row);
            }
        };
        scan(&mut b, 1000, 0);
        assert_eq!(b.panel.darkness(10, 10), 0.0, "unpowered panel must not change");
        scan(&mut b, 10_000, bit(45) | bit(46));
        assert!(b.panel.darkness(10, 10) > 0.5);
        assert!(b.panel.darkness(959, 539) > 0.5);
        assert_eq!(b.panel.stats().frames_driven, 1);
    }

    #[test]
    fn papers3_battery_on_the_adc_divider() {
        let mut b = ParallelByodBoard::new(find("m5_papers3").unwrap());
        b.set_battery_mv(3900);
        assert_eq!(b.adc_millivolts(3), 1950);
        assert_eq!(b.adc_millivolts(1), 0);
        assert!(!b.info().has_button);
    }

    /// EPDiy V7 power path: rails come up only after the expander raises WAKEUP + PWRUP and
    /// the TPS65185's power-good delay passes; the BQ27220 answers Voltage().
    #[test]
    fn lilygo_epdiy_power_and_gauge() {
        let mut b = ParallelByodBoard::new(find("lilygo_t5pro").unwrap());
        let wr = |b: &mut ParallelByodBoard, t: u64, bytes: &[u8]| {
            assert!(b.i2c_start(t, 0, 0x20, false));
            for &x in bytes {
                assert!(b.i2c_write(t, 0, x));
            }
            b.i2c_stop(t, 0);
        };
        wr(&mut b, 0, &[7, 0x00]); // port 1 all outputs
        wr(&mut b, 0, &[3, 1 << 0 | 1 << 3 | 1 << 5]); // OE, PWRUP, WAKEUP (pins 8, 11, 13)
        assert!(!b.panel.stats().powered);
        b.update(5_000_000);
        assert!(b.panel.stats().powered && b.panel.stats().output_enabled);
        b.set_battery_mv(3876);
        assert!(b.i2c_start(1, 0, 0x55, false));
        b.i2c_write(1, 0, 0x08);
        assert!(b.i2c_start(1, 0, 0x55, true));
        let v = b.i2c_read(1, 0, true) as u16 | (b.i2c_read(1, 0, false) as u16) << 8;
        b.i2c_stop(1, 0);
        assert_eq!(v, 3876);
        assert!(b.info().has_button);
        b.set_button(true);
        assert_eq!(b.gpio_in(0).0 & 1, 0);
    }
}
