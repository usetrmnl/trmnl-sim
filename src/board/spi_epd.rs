//! Boards with an SPI e-paper panel, one button and (usually) a LiPo: the TRMNL OG and
//! the BYOD boards the firmware drives with bb_epaper. Each is a row of the firmware's
//! `device_list[]` (display.cpp), selected by its `DEVICE_MODEL`; see [`SPECS`].
//!
//! Optional environment sensors (`--sensor`) sit on I2C0 next to any fuel gauge or PMIC.

use super::Board;
use crate::devices::epd::SpiEpd;
use crate::devices::i2c::I2cBus;
use crate::devices::i2c::axp2101::Axp2101;
use crate::devices::i2c::bq27220::Bq27220;
use crate::devices::i2c::env_sensors::{Aht20, Climate, Scd41};
use crate::devices::ssd16xx::{Glass, Ssd16xx};
use crate::devices::uc8179::{ColorPanel, Uc8179};
use crate::savepoint::{StateReader, StateWriter};

/// The SoC a board is built around.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Chip {
    Esp32c3,
    Esp32s3,
}

/// The panel's serial interface and the button, as `device_list[]` (or, for boards whose
/// wiring is built into bb_epaper, its `begin()`) has them.
#[derive(Clone, Copy, Debug)]
pub struct Pins {
    pub sck: u8,
    pub mosi: u8,
    pub cs: u8,
    pub rst: u8,
    pub dc: u8,
    pub busy: u8,
    /// Active-low wake button (`interrupt_pin`).
    pub button: u8,
}

/// How the firmware measures the battery (`batt_type`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Battery {
    /// The LiPo through a 1:2 divider on an ADC pin; with `enable`, the divider is only
    /// connected while the firmware drives that pin high.
    Adc { pin: u8, enable: Option<u8> },
    /// `BATT_ADC` with no ADC pin (`batt_pin` 0xff): reads as 0 V.
    Unwired,
    /// `BATT_NONE`: the firmware reports a fixed 4.2 V.
    None,
    /// A TI BQ27220 fuel gauge on I2C (0x55).
    Bq27220,
    /// An X-Powers AXP2101 PMIC on I2C (0x34).
    Axp2101,
}

/// The e-paper panel (and its controller).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Panel {
    /// UC8179, 7.5" 800x480 black and white (bb_epaper EP75_800x480).
    Uc8179,
    /// UC8179, 7.5" 800x480 black/white/yellow/red (EP75YR_800x480).
    Uc8179Bwry,
    /// UC8179, 7.3" 800x480 Spectra 6 (EP73_SPECTRA_800x480).
    Uc8179Spectra6,
    /// SSD1677, 4.26" 800x480 black and white (EP426_800x480, 4-gray capable).
    Ssd1677Ep426,
    /// SSD1677, 3.97" 800x480 black and white (EP397_800x480, 4-gray capable).
    Ssd1677Ep397,
    /// The 3.97" panel mounted rotated 180 degrees (see [`Glass::Ep397Flipped`]).
    Ssd1677Ep397Flipped,
    /// SSD1683, 4.2" 400x300 black and white (EP42B_400x300, 4-gray capable).
    Ssd1683Ep42b,
}

impl Panel {
    pub fn controller(self, rev: u32) -> Box<dyn SpiEpd> {
        match self {
            Panel::Uc8179 => Box::new(Uc8179::new(rev)),
            Panel::Uc8179Bwry => Box::new(Uc8179::new_bwry(rev)),
            Panel::Uc8179Spectra6 => Box::new(Uc8179::new_color(rev, ColorPanel::Spectra6)),
            Panel::Ssd1677Ep426 => Box::new(Ssd16xx::new(Glass::Ep426)),
            Panel::Ssd1677Ep397 => Box::new(Ssd16xx::new(Glass::Ep397)),
            Panel::Ssd1677Ep397Flipped => Box::new(Ssd16xx::new(Glass::Ep397Flipped)),
            Panel::Ssd1683Ep42b => Box::new(Ssd16xx::new(Glass::Ep42b)),
        }
    }

    /// What the built-in mock server serves this panel.
    pub fn mock_panel(self) -> mock_trmnl::Panel {
        match self {
            Panel::Uc8179 | Panel::Ssd1677Ep426 | Panel::Ssd1677Ep397 | Panel::Ssd1677Ep397Flipped => {
                mock_trmnl::Panel::Og
            }
            // TODO(mock): 400x300
            Panel::Ssd1683Ep42b => mock_trmnl::Panel::Og,
            Panel::Uc8179Bwry => mock_trmnl::Panel::Bwry,
            Panel::Uc8179Spectra6 => mock_trmnl::Panel::Spectra6,
        }
    }
}

/// One supported board.
#[derive(Debug)]
pub struct BoardSpec {
    /// The firmware's `DEVICE_MODEL` (its `device_list[]` row).
    pub model: &'static str,
    /// The PlatformIO environments that build it (the build directory's name).
    pub envs: &'static [&'static str],
    /// Shown in front-ends; also identifies the board in save points.
    pub name: &'static str,
    pub chip: Chip,
    pub pins: Pins,
    pub battery: Battery,
    pub panel: Panel,
    /// A GPIO that switches the panel's supply (on while driven high).
    pub panel_power: Option<u8>,
}

const OG_PINS: Pins = Pins { sck: 7, mosi: 8, cs: 6, rst: 10, dc: 5, busy: 4, button: 2 };
const RETERMINAL_PINS: Pins = Pins { sck: 7, mosi: 9, cs: 10, rst: 12, dc: 11, busy: 13, button: 3 };
const XIAO_EPAPER_PINS: Pins = Pins { sck: 7, mosi: 9, cs: 44, rst: 38, dc: 10, busy: 4, button: 5 };

/// Every supported SPI e-paper board.
pub static SPECS: &[BoardSpec] = &[
    BoardSpec {
        model: "og",
        envs: &["trmnl", "trmnl_test", "local"],
        name: "TRMNL OG",
        chip: Chip::Esp32c3,
        pins: OG_PINS,
        battery: Battery::Adc { pin: 3, enable: None },
        panel: Panel::Uc8179,
        panel_power: None,
    },
    BoardSpec {
        model: "og_4clr",
        envs: &["trmnl_4clr"],
        name: "TRMNL BWRY",
        chip: Chip::Esp32c3,
        pins: OG_PINS,
        battery: Battery::Adc { pin: 3, enable: None },
        panel: Panel::Uc8179Bwry,
        panel_power: None,
    },
    BoardSpec {
        model: "seeed_esp32c3",
        envs: &["seeed_xiao_esp32c3"],
        name: "XIAO ESP32-C3 + 7.5\" panel",
        chip: Chip::Esp32c3,
        pins: Pins { sck: 8, mosi: 10, cs: 3, rst: 2, dc: 5, busy: 4, button: 9 },
        battery: Battery::Unwired,
        panel: Panel::Uc8179,
        panel_power: None,
    },
    BoardSpec {
        model: "seeed_esp32s3",
        envs: &["seeed_xiao_esp32s3"],
        name: "XIAO ESP32-S3 + 7.5\" panel",
        chip: Chip::Esp32s3,
        pins: Pins { sck: 7, mosi: 9, cs: 2, rst: 1, dc: 4, busy: 3, button: 0 },
        battery: Battery::Unwired,
        panel: Panel::Uc8179,
        panel_power: None,
    },
    BoardSpec {
        model: "xiao_epaper_display",
        envs: &["TRMNL_7inch5_OG_DIY_Kit"],
        name: "TRMNL 7.5\" DIY Kit",
        chip: Chip::Esp32s3,
        pins: XIAO_EPAPER_PINS,
        battery: Battery::Adc { pin: 1, enable: Some(6) },
        panel: Panel::Uc8179,
        panel_power: None,
    },
    BoardSpec {
        model: "xiao_epaper_6clr",
        envs: &["TRMNL_7inch5_OG_DIY_Kit_6CLR"],
        name: "TRMNL 7.3\" Spectra 6 DIY Kit",
        chip: Chip::Esp32s3,
        pins: XIAO_EPAPER_PINS,
        battery: Battery::Adc { pin: 1, enable: Some(6) },
        panel: Panel::Uc8179Spectra6,
        panel_power: None,
    },
    BoardSpec {
        model: "reterminal_e1001",
        envs: &["seeed_reTerminal_E1001"],
        name: "reTerminal E1001",
        chip: Chip::Esp32s3,
        pins: RETERMINAL_PINS,
        battery: Battery::Adc { pin: 1, enable: Some(21) },
        panel: Panel::Uc8179,
        panel_power: None,
    },
    BoardSpec {
        model: "reterminal_e1002",
        envs: &["seeed_reTerminal_E1002"],
        name: "reTerminal E1002",
        chip: Chip::Esp32s3,
        pins: RETERMINAL_PINS,
        battery: Battery::Adc { pin: 1, enable: Some(21) },
        panel: Panel::Uc8179Spectra6,
        panel_power: None,
    },
    BoardSpec {
        model: "xteink_x4",
        envs: &["xteink_x4", "xteink_x4_pwr_btn"],
        name: "Xteink X4",
        chip: Chip::Esp32c3,
        pins: Pins { sck: 8, mosi: 10, cs: 21, rst: 5, dc: 4, busy: 6, button: 3 },
        // The LiPo's divider is on GPIO0 (config.h PIN_BATTERY), but device_list[] has
        // batt_pin 0xff: the firmware never reads it and reports 0 V.
        battery: Battery::Adc { pin: 0, enable: None },
        panel: Panel::Ssd1677Ep426,
        panel_power: None,
    },
    BoardSpec {
        model: "xiao_epaper_mini",
        envs: &["TRMNL_4inch26_DIY_Kit"],
        name: "TRMNL 4.26\" DIY Kit",
        chip: Chip::Esp32s3,
        pins: Pins { button: 2, ..XIAO_EPAPER_PINS },
        battery: Battery::Adc { pin: 1, enable: Some(6) },
        panel: Panel::Ssd1677Ep426,
        panel_power: None,
    },
    BoardSpec {
        model: "waveshare_397",
        envs: &["WAVESHARE_397"],
        name: "Waveshare ESP32-S3 3.97\"",
        chip: Chip::Esp32s3,
        pins: Pins { sck: 11, mosi: 12, cs: 10, rst: 46, dc: 9, busy: 3, button: 0 },
        battery: Battery::Axp2101,
        panel: Panel::Ssd1677Ep397,
        panel_power: None,
    },
    BoardSpec {
        model: "seeed_sticky",
        envs: &["seeed_sticky"],
        name: "Seeed Sticky",
        chip: Chip::Esp32s3,
        pins: Pins { sck: 13, mosi: 14, cs: 15, rst: 17, dc: 16, busy: 18, button: 4 },
        battery: Battery::Bq27220,
        panel: Panel::Ssd1677Ep397Flipped,
        panel_power: Some(47),
    },
    BoardSpec {
        model: "crowpanel42",
        envs: &["CrowPanel42"],
        name: "CrowPanel 4.2\"",
        chip: Chip::Esp32s3,
        // Wired as bb_epaper's begin(EPD_CROWPANEL42) has it (device_list[] pins are 0).
        pins: Pins { sck: 12, mosi: 11, cs: 45, rst: 47, dc: 46, busy: 48, button: 2 },
        battery: Battery::None,
        panel: Panel::Ssd1683Ep42b,
        panel_power: Some(7),
    },
];

/// The board for a PlatformIO environment or `DEVICE_MODEL` name.
pub fn find(name: &str) -> Option<&'static BoardSpec> {
    SPECS.iter().find(|s| s.model == name || s.envs.contains(&name))
}

/// An environment sensor on the I2C header.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Sensor {
    /// Sensirion SCD41 CO2 sensor (0x62).
    Scd41,
    /// ASAIR AHT20 temperature/humidity sensor (0x38).
    Aht20,
}

pub struct SpiEpdBoard {
    pub spec: &'static BoardSpec,
    pub panel: Box<dyn SpiEpd>,
    i2c: I2cBus,
    button_down: bool,
    battery_mv: u32,
    out: u64,
    oe: u64,
}

impl SpiEpdBoard {
    pub fn new(spec: &'static BoardSpec, panel_rev: u32, sensors: &[Sensor]) -> Self {
        let mut i2c = I2cBus::new();
        for s in sensors {
            match s {
                Sensor::Scd41 => i2c.add(Box::new(Scd41::new(Climate::default()))),
                Sensor::Aht20 => i2c.add(Box::new(Aht20::new(Climate::default()))),
            };
        }
        match spec.battery {
            Battery::Bq27220 => _ = i2c.add(Box::new(Bq27220::new())),
            Battery::Axp2101 => _ = i2c.add(Box::new(Axp2101::new())),
            _ => {}
        }
        let mut panel = spec.panel.controller(panel_rev);
        if spec.panel_power.is_some() {
            panel.set_power(0, false); // until the firmware switches it on
        }
        SpiEpdBoard { spec, panel, i2c, button_down: false, battery_mv: 4100, out: 0, oe: 0 }
    }

    fn level(&self, pin: u8) -> bool {
        self.out >> pin & 1 != 0
    }
}

impl Board for SpiEpdBoard {
    fn i2c_start(&mut self, now: u64, bus: u8, addr: u8, read: bool) -> bool {
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
    }

    fn gpio_out(&mut self, now: u64, out: u64, oe: u64) {
        self.out = out;
        self.oe = oe;
        let p = &self.spec.pins;
        // Undriven lines idle high (pull-ups on CS/RST on the real board).
        let lvl = |pin: u8| if oe >> pin & 1 != 0 { out >> pin & 1 != 0 } else { true };
        let mosi_driven = oe >> p.mosi & 1 != 0;
        let (cs, dc, sck, rst) = (lvl(p.cs), lvl(p.dc), oe >> p.sck & 1 != 0 && self.level(p.sck), lvl(p.rst));
        let mosi = mosi_driven && self.level(p.mosi);
        if let Some(pin) = self.spec.panel_power {
            self.panel.set_power(now, oe >> pin & 1 != 0 && out >> pin & 1 != 0);
        }
        self.panel.set_pins(now, cs, dc, sck, mosi, rst);
    }

    fn gpio_in(&mut self, now: u64) -> (u64, u64) {
        let p = &self.spec.pins;
        let mut lv = 0u64;
        let mut mask = 1u64 << p.busy | 1u64 << p.button;
        if self.panel.busy(now) == self.panel.busy_level() {
            lv |= 1 << p.busy;
        }
        if !self.button_down {
            lv |= 1 << p.button;
        }
        if let Some(b) = self.panel.mosi_out()
            && self.oe >> p.mosi & 1 == 0
        {
            mask |= 1 << p.mosi;
            lv |= (b as u64) << p.mosi;
        }
        (lv, mask)
    }

    fn spi_transfer(&mut self, now: u64, host: u8, mosi: &[u8], miso_len: usize) -> Vec<u8> {
        if host == 2 {
            self.panel.spi_bytes(now, mosi);
        }
        vec![0xff; miso_len]
    }

    fn adc_millivolts(&mut self, gpio: u8) -> u32 {
        match self.spec.battery {
            Battery::Adc { pin, enable } if pin == gpio => {
                let connected = enable.is_none_or(|en| self.oe >> en & 1 != 0 && self.level(en));
                if connected { self.battery_mv / 2 } else { 0 }
            }
            _ => 0,
        }
    }

    fn next_event_ns(&self, now: u64) -> Option<u64> {
        self.panel.next_event(now)
    }

    fn update(&mut self, now: u64) {
        self.panel.update(now);
    }

    fn info(&self) -> sim_api::BoardInfo {
        sim_api::BoardInfo {
            name: self.spec.name.into(),
            has_button: true,
            has_refresh_flashing: self.panel.is_color(),
            ..Default::default()
        }
    }

    fn set_refresh_flashing(&mut self, on: bool) {
        self.panel.set_flashing(on);
    }

    fn set_faults(&mut self, faults: &sim_api::Faults) {
        self.panel.set_busy_stuck(faults.panel_busy_stuck);
    }

    fn set_button(&mut self, down: bool) {
        self.button_down = down;
    }

    fn set_battery_mv(&mut self, mv: u32) {
        self.battery_mv = mv;
        // A rough LiPo curve for the gauges' state of charge: 3.3 V empty .. 4.2 V full.
        let soc = ((mv.clamp(3300, 4200) - 3300) * 100 / 900) as u8;
        if let Some(g) = self.i2c.device_mut::<Bq27220>() {
            g.set_battery(mv as u16, false, soc);
        }
        if let Some(g) = self.i2c.device_mut::<Axp2101>() {
            g.set_battery(mv as u16, false, soc);
        }
    }

    fn display_status(&self, now: u64) -> (bool, u64) {
        (self.panel.busy(now), self.panel.refresh_count())
    }

    fn save_state(&self, w: &mut StateWriter, powered: bool) {
        w.u32(self.battery_mv);
        w.u64(self.out);
        w.u64(self.oe);
        w.section(|w| self.panel.save_state(w, powered));
    }

    fn restore_state(&mut self, r: &mut StateReader, powered: bool) -> anyhow::Result<()> {
        self.battery_mv = r.u32()?;
        self.out = r.u64()?;
        self.oe = r.u64()?;
        // Gauge readings follow the battery (sensors keep their own, unsaved, state).
        self.set_battery_mv(self.battery_mv);
        r.section(|r| self.panel.restore_state(r, powered))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_model_and_env_is_listed_once() {
        let mut names: Vec<&str> = SPECS
            .iter()
            .flat_map(|s| std::iter::once(s.model).chain(s.envs.iter().copied().filter(|&e| e != s.model)))
            .collect();
        let n = names.len();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), n, "a model or environment appears twice");
        let mut boards: Vec<&str> = SPECS.iter().map(|s| s.name).collect();
        boards.sort();
        boards.dedup();
        assert_eq!(boards.len(), SPECS.len(), "two boards share a name");
    }
}
