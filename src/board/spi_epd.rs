//! Boards with an SPI e-paper panel, one button and (usually) a LiPo: the TRMNL OG and
//! the BYOD boards the firmware drives with bb_epaper. Each is a row of the firmware's
//! `device_list[]` (display.cpp), selected by its `DEVICE_MODEL`; see [`SPECS`].
//!
//! Optional environment sensors (`--sensor`) sit on I2C0 next to any fuel gauge or PMIC.

use super::Board;
use crate::devices::dual_epd::DualSpiEpd;
use crate::devices::epd::SpiEpd;
use crate::devices::i2c::I2cBus;
use crate::devices::i2c::axp2101::Axp2101;
use crate::devices::i2c::bq27220::Bq27220;
use crate::devices::i2c::env_sensors::{Aht20, Climate, Scd41};
use crate::devices::i2c::m5_py32::M5Py32;
use crate::devices::i2c::m5ioe1::M5Ioe1;
use crate::devices::ssd16xx::{Glass, Ssd16xx};
use crate::devices::uc8179::{ColorPanel, Uc8179, X3_RESPONSE};
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
    /// UC8179, 7.5" 800x480 black/white/red, two 1-bit planes (Waveshare, EP75R_800x480).
    Uc8179Bwr,
    /// UC81xx, 5.83" 648x480 black and white (DEPG0583BN, EP583_648x480).
    Uc81xx583,
    /// UC81xx, 3.68" 792x528 black and white (Xteink X3, EP368_792x528).
    Uc81xx368,
    /// bb_epaper's EPD_M5_PAPER_COLOR: a 4" 400x600 Spectra 6 panel (EP40_SPECTRA_400x600,
    /// GDEP040E01) powered through GPIO0 of the board's PY32 (I2C 0x6e).
    M5PaperColor,
    /// bb_epaper's EPD_SEEED_E1004: a 13.3" 1200x1600 Spectra 6 panel (EP133_SPECTRA_1200x1600)
    /// with two controllers, the left half's on the board's CS and the right half's on CS2
    /// (GPIO2), powered while GPIO12 is high.
    SeeedE1004,
    /// bb_epaper's EPD_M5_PAPER_MONO: a 3.97" 800x480 black-and-white SSD1677 panel
    /// (EP426_800x480, 4-gray EP426_800x480_4GRAY) whose LDO and RST hang off GPIO3 and GPIO5
    /// of the board's M5IOE1 expander (I2C 0x4f).
    M5PaperMono,
    /// SSD1677, 4.26" 800x480 black and white (EP426_800x480, 4-gray capable).
    Ssd1677Ep426,
    /// SSD1677, 3.97" 800x480 black and white (EP397_800x480, 4-gray capable).
    Ssd1677Ep397,
    /// The 3.97" panel mounted rotated 180 degrees (see [`Glass::Ep397Flipped`]).
    Ssd1677Ep397Flipped,
    /// SSD1683, 4.2" 400x300 black and white (EP42B_400x300, 4-gray capable).
    Ssd1683Ep42b,
}

/// What switches the panel's supply. Unpowered, the controller ignores its inputs and
/// holds BUSY low; power coming on resets it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EpdPower {
    Always,
    /// Powered while this GPIO is driven high.
    Gpio(u8),
    /// Powered while GPIO0 of the M5Stack PY32 at I2C 0x6e drives high.
    M5Py32Gpio0,
    /// Powered while GPIO3 of the M5IOE1 expander at I2C 0x4f drives high.
    M5Ioe1Gpio3,
}

impl Panel {
    pub fn controller(self, rev: u32) -> Box<dyn SpiEpd> {
        match self {
            Panel::Uc8179 => Box::new(Uc8179::new(rev)),
            Panel::Uc8179Bwry => Box::new(Uc8179::new_bwry(rev)),
            Panel::Uc8179Spectra6 => Box::new(Uc8179::new_color(rev, ColorPanel::Spectra6)),
            Panel::Uc8179Bwr => Box::new(Uc8179::new_color(rev, ColorPanel::Bwr)),
            Panel::Uc81xx583 => Box::new(Uc8179::with_size(rev, 648, 480)),
            Panel::Uc81xx368 => Box::new(Uc8179::with_size(rev, 792, 528).with_response(X3_RESPONSE)),
            Panel::M5PaperColor => Box::new(Uc8179::new_color_sized(rev, ColorPanel::Spectra6, 400, 600)),
            Panel::M5PaperMono => Box::new(Ssd16xx::new(Glass::Ep426)),
            Panel::Ssd1677Ep426 => Box::new(Ssd16xx::new(Glass::Ep426)),
            Panel::Ssd1677Ep397 => Box::new(Ssd16xx::new(Glass::Ep397)),
            Panel::Ssd1677Ep397Flipped => Box::new(Ssd16xx::new(Glass::Ep397Flipped)),
            Panel::Ssd1683Ep42b => Box::new(Ssd16xx::new(Glass::Ep42b)),
            Panel::SeeedE1004 => Box::new(DualSpiEpd::new(
                Box::new(Uc8179::new_color_sized(rev, ColorPanel::Spectra6, 600, 1600)),
                Box::new(Uc8179::new_color_sized(rev, ColorPanel::Spectra6, 600, 1600)),
            )),
        }
    }

    /// The second controller's chip select, for panels with two.
    pub fn cs2(self) -> Option<u8> {
        match self {
            Panel::SeeedE1004 => Some(2),
            _ => None,
        }
    }

    /// What powers the panel (bb_epaper's built-in boards switch it on in `begin()`).
    pub fn power(self) -> EpdPower {
        match self {
            Panel::M5PaperColor => EpdPower::M5Py32Gpio0,
            Panel::SeeedE1004 => EpdPower::Gpio(12),
            Panel::M5PaperMono => EpdPower::M5Ioe1Gpio3,
            // board supply switches: Seeed Sticky GPIO47, CrowPanel GPIO7
            Panel::Ssd1677Ep397Flipped => EpdPower::Gpio(47),
            Panel::Ssd1683Ep42b => EpdPower::Gpio(7),
            _ => EpdPower::Always,
        }
    }

    /// What the built-in mock server serves this panel.
    pub fn mock_panel(self) -> mock_trmnl::Panel {
        match self {
            Panel::Uc8179 | Panel::Ssd1677Ep426 | Panel::Ssd1677Ep397 | Panel::Ssd1677Ep397Flipped => {
                mock_trmnl::Panel::Og
            }
            Panel::Ssd1683Ep42b => mock_trmnl::Panel::new(mock_trmnl::Inks::Mono, 400, 300),
            Panel::Uc8179Bwry => mock_trmnl::Panel::Bwry,
            Panel::Uc8179Spectra6 => mock_trmnl::Panel::Spectra6,
            // The firmware shows images in black and white only on this panel.
            Panel::Uc8179Bwr => mock_trmnl::Panel::Og,
            Panel::Uc81xx583 => mock_trmnl::Panel::new(mock_trmnl::Inks::Mono, 648, 480),
            Panel::Uc81xx368 => mock_trmnl::Panel::new(mock_trmnl::Inks::Mono, 792, 528),
            Panel::M5PaperColor => mock_trmnl::Panel::new(mock_trmnl::Inks::Spectra6, 400, 600),
            Panel::SeeedE1004 => mock_trmnl::Panel::new(mock_trmnl::Inks::Spectra6, 1200, 1600),
            Panel::M5PaperMono => mock_trmnl::Panel::Og,
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
    },
    BoardSpec {
        model: "og_4clr",
        envs: &["trmnl_4clr"],
        name: "TRMNL BWRY",
        chip: Chip::Esp32c3,
        pins: OG_PINS,
        battery: Battery::Adc { pin: 3, enable: None },
        panel: Panel::Uc8179Bwry,
    },
    BoardSpec {
        model: "seeed_esp32c3",
        envs: &["seeed_xiao_esp32c3"],
        name: "XIAO ESP32-C3 + 7.5\" panel",
        chip: Chip::Esp32c3,
        pins: Pins { sck: 8, mosi: 10, cs: 3, rst: 2, dc: 5, busy: 4, button: 9 },
        battery: Battery::Unwired,
        panel: Panel::Uc8179,
    },
    BoardSpec {
        model: "seeed_esp32s3",
        envs: &["seeed_xiao_esp32s3"],
        name: "XIAO ESP32-S3 + 7.5\" panel",
        chip: Chip::Esp32s3,
        pins: Pins { sck: 7, mosi: 9, cs: 2, rst: 1, dc: 4, busy: 3, button: 0 },
        battery: Battery::Unwired,
        panel: Panel::Uc8179,
    },
    BoardSpec {
        model: "xiao_epaper_display",
        envs: &["TRMNL_7inch5_OG_DIY_Kit"],
        name: "TRMNL 7.5\" DIY Kit",
        chip: Chip::Esp32s3,
        pins: XIAO_EPAPER_PINS,
        battery: Battery::Adc { pin: 1, enable: Some(6) },
        panel: Panel::Uc8179,
    },
    BoardSpec {
        model: "xiao_epaper_3clr",
        envs: &["TRMNL_7inch5_OG_DIY_Kit_3CLR"],
        name: "TRMNL 7.5\" BWR DIY Kit",
        chip: Chip::Esp32s3,
        pins: XIAO_EPAPER_PINS,
        battery: Battery::Adc { pin: 1, enable: Some(6) },
        panel: Panel::Uc8179Bwr,
    },
    BoardSpec {
        model: "xiao_epaper_6clr",
        envs: &["TRMNL_7inch5_OG_DIY_Kit_6CLR"],
        name: "TRMNL 7.3\" Spectra 6 DIY Kit",
        chip: Chip::Esp32s3,
        pins: XIAO_EPAPER_PINS,
        battery: Battery::Adc { pin: 1, enable: Some(6) },
        panel: Panel::Uc8179Spectra6,
    },
    BoardSpec {
        model: "reterminal_e1001",
        envs: &["seeed_reTerminal_E1001"],
        name: "reTerminal E1001",
        chip: Chip::Esp32s3,
        pins: RETERMINAL_PINS,
        battery: Battery::Adc { pin: 1, enable: Some(21) },
        panel: Panel::Uc8179,
    },
    BoardSpec {
        model: "reterminal_e1002",
        envs: &["seeed_reTerminal_E1002"],
        name: "reTerminal E1002",
        chip: Chip::Esp32s3,
        pins: RETERMINAL_PINS,
        battery: Battery::Adc { pin: 1, enable: Some(21) },
        panel: Panel::Uc8179Spectra6,
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
    },
    BoardSpec {
        model: "xiao_epaper_mini",
        envs: &["TRMNL_4inch26_DIY_Kit"],
        name: "TRMNL 4.26\" DIY Kit",
        chip: Chip::Esp32s3,
        pins: Pins { button: 2, ..XIAO_EPAPER_PINS },
        battery: Battery::Adc { pin: 1, enable: Some(6) },
        panel: Panel::Ssd1677Ep426,
    },
    BoardSpec {
        model: "waveshare_397",
        envs: &["WAVESHARE_397"],
        name: "Waveshare ESP32-S3 3.97\"",
        chip: Chip::Esp32s3,
        pins: Pins { sck: 11, mosi: 12, cs: 10, rst: 46, dc: 9, busy: 3, button: 0 },
        battery: Battery::Axp2101,
        panel: Panel::Ssd1677Ep397,
    },
    BoardSpec {
        model: "seeed_sticky",
        envs: &["seeed_sticky"],
        name: "Seeed Sticky",
        chip: Chip::Esp32s3,
        pins: Pins { sck: 13, mosi: 14, cs: 15, rst: 17, dc: 16, busy: 18, button: 4 },
        battery: Battery::Bq27220,
        panel: Panel::Ssd1677Ep397Flipped,
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
    },
    BoardSpec {
        model: "trmnl_steam",
        envs: &["trmnl_steam"],
        name: "TRMNL Steam",
        chip: Chip::Esp32c3,
        pins: OG_PINS,
        battery: Battery::Adc { pin: 3, enable: None },
        panel: Panel::Uc81xx583,
    },
    BoardSpec {
        model: "xteink_x3",
        envs: &["xteink_x3"],
        name: "Xteink X3",
        chip: Chip::Esp32c3,
        pins: Pins { sck: 8, mosi: 10, cs: 21, rst: 5, dc: 4, busy: 6, button: 3 },
        battery: Battery::Bq27220,
        panel: Panel::Uc81xx368,
    },
    BoardSpec {
        model: "m5_paper_mono",
        envs: &["m5_paper_mono"],
        name: "M5Paper Mono",
        chip: Chip::Esp32s3,
        // bb_epaper's begin(EPD_M5_PAPER_MONO); the panel's RST is on the I/O expander
        pins: Pins { sck: 15, mosi: 14, cs: 16, rst: 0xff, dc: 17, busy: 18, button: 2 },
        battery: Battery::None,
        panel: Panel::M5PaperMono,
    },
    BoardSpec {
        model: "m5_paper_color",
        envs: &["m5_paper_color"],
        name: "M5Paper Color",
        chip: Chip::Esp32s3,
        // bb_epaper's begin(EPD_M5_PAPER_COLOR); device_list has no SPI pins for it
        pins: Pins { sck: 15, mosi: 13, cs: 44, rst: 12, dc: 43, busy: 11, button: 1 },
        battery: Battery::None,
        panel: Panel::M5PaperColor,
    },
    BoardSpec {
        model: "reterminal_e1004",
        envs: &["seeed_reTerminal_E1004"],
        name: "reTerminal E1004",
        chip: Chip::Esp32s3,
        // bb_epaper's begin(EPD_SEEED_E1004); CS2 (GPIO2) is the panel's
        pins: Pins { sck: 7, mosi: 9, cs: 10, rst: 38, dc: 11, busy: 13, button: 4 },
        battery: Battery::Adc { pin: 1, enable: Some(21) },
        panel: Panel::SeeedE1004,
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
    /// The panel's supply is on (see [`EpdPower`]).
    epd_powered: bool,
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
        match spec.panel.power() {
            EpdPower::M5Py32Gpio0 => _ = i2c.add(Box::new(M5Py32::new())),
            EpdPower::M5Ioe1Gpio3 => _ = i2c.add(Box::new(M5Ioe1::new())),
            _ => {}
        }
        let mut b = SpiEpdBoard {
            spec,
            panel: spec.panel.controller(panel_rev),
            i2c,
            button_down: false,
            battery_mv: 4100,
            out: 0,
            oe: 0,
            epd_powered: false,
        };
        b.epd_powered = b.epd_supply();
        if !b.epd_powered {
            b.panel.set_power(0, false); // until the firmware switches it on
        }
        b
    }

    fn level(&self, pin: u8) -> bool {
        self.out >> pin & 1 != 0
    }

    /// Is the panel's supply switched on?
    fn epd_supply(&self) -> bool {
        match self.spec.panel.power() {
            EpdPower::Always => true,
            EpdPower::Gpio(pin) => self.oe >> pin & 1 != 0 && self.level(pin),
            EpdPower::M5Py32Gpio0 => self.i2c.device::<M5Py32>().is_some_and(|p| p.gpio_high(0)),
            EpdPower::M5Ioe1Gpio3 => self.i2c.device::<M5Ioe1>().is_some_and(|e| e.gpio(3) == Some(true)),
        }
    }

    /// Pass the serial interface's pin levels to the panel (if it is powered).
    fn drive_panel(&mut self, now: u64) {
        self.update_epd_power(now);
        if !self.epd_powered {
            return;
        }
        let (out, oe) = (self.out, self.oe);
        let p = &self.spec.pins;
        // Undriven lines idle high (pull-ups on CS/RST on the real board).
        let lvl = |pin: u8| if oe >> pin & 1 != 0 { out >> pin & 1 != 0 } else { true };
        let rst = match self.spec.panel {
            // RST is on the M5IOE1 expander's GPIO5
            Panel::M5PaperMono => self.i2c.device::<M5Ioe1>().is_none_or(|e| e.gpio(5) != Some(false)),
            _ => lvl(p.rst),
        };
        let mosi_driven = oe >> p.mosi & 1 != 0;
        let (cs, dc, sck) = (lvl(p.cs), lvl(p.dc), oe >> p.sck & 1 != 0 && self.level(p.sck));
        let mosi = mosi_driven && self.level(p.mosi);
        if let Some(cs2) = self.spec.panel.cs2() {
            self.panel.set_cs2(lvl(cs2));
        }
        self.panel.set_pins(now, cs, dc, sck, mosi, rst);
    }

    /// Follow the panel's supply; power coming on resets the controller (a RST pulse).
    fn update_epd_power(&mut self, now: u64) {
        let on = self.epd_supply();
        if on != self.epd_powered {
            self.panel.set_power(now, on);
        }
        if on && !self.epd_powered {
            self.panel.set_pins(now, true, true, false, false, false);
        }
        self.epd_powered = on;
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
        if self.spec.panel.power() != EpdPower::Always {
            // an I/O chip may have switched the panel's supply or reset line
            self.drive_panel(now);
        }
    }

    fn gpio_out(&mut self, now: u64, out: u64, oe: u64) {
        self.out = out;
        self.oe = oe;
        self.drive_panel(now);
    }

    fn gpio_in(&mut self, now: u64) -> (u64, u64) {
        let p = &self.spec.pins;
        let mut lv = 0u64;
        let mut mask = 1u64 << p.busy | 1u64 << p.button;
        if self.epd_powered && self.panel.busy(now) == self.panel.busy_level() {
            lv |= 1 << p.busy;
        }
        if !self.button_down {
            lv |= 1 << p.button;
        }
        if let Some(b) = self.panel.mosi_out()
            && self.epd_powered
            && self.oe >> p.mosi & 1 == 0
        {
            mask |= 1 << p.mosi;
            lv |= (b as u64) << p.mosi;
        }
        (lv, mask)
    }

    fn spi_transfer(&mut self, now: u64, host: u8, mosi: &[u8], miso_len: usize) -> Vec<u8> {
        if host == 2 && self.epd_powered {
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
        if powered {
            w.section(|w| self.i2c.save_state(w));
        }
    }

    fn restore_state(&mut self, r: &mut StateReader, powered: bool) -> anyhow::Result<()> {
        self.battery_mv = r.u32()?;
        self.out = r.u64()?;
        self.oe = r.u64()?;
        // Gauge readings follow the battery (sensors keep their own, unsaved, state).
        self.set_battery_mv(self.battery_mv);
        r.section(|r| self.panel.restore_state(r, powered))?;
        if powered {
            r.section(|r| self.i2c.restore_state(r))?;
        }
        self.epd_powered = self.epd_supply();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_model_and_env_is_listed_once() {
        // A model may share its name with the environment that builds it (xteink_x3).
        let mut names: Vec<&str> = SPECS
            .iter()
            .flat_map(|s| std::iter::once(s.model).chain(s.envs.iter().copied().filter(|e| *e != s.model)))
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

    /// Send the UC81xx power-on command (BUSY for a while after) over GPSPI2.
    fn power_on_cmd(b: &mut SpiEpdBoard, now: u64) {
        let p = b.spec.pins;
        let oe = 1u64 << p.cs | 1 << p.dc | 1 << p.rst | 1 << p.sck | 1 << p.mosi | b.oe;
        let keep = b.out & !(1u64 << p.cs | 1 << p.dc);
        b.gpio_out(now, keep | 1 << p.rst, oe); // CS and D/C low: command
        b.spi_transfer(now, 2, &[0x04], 0);
        b.gpio_out(now, keep | 1 << p.rst | 1 << p.cs | 1 << p.dc, oe);
    }

    fn busy_pin_low(b: &mut SpiEpdBoard, now: u64) -> bool {
        b.gpio_in(now).0 >> b.spec.pins.busy & 1 == 0
    }

    #[test]
    fn m5_paper_color_panel_is_powered_by_the_py32() {
        use crate::devices::i2c::m5_py32::{ADDR, REG_GPIO_DIR, REG_GPIO_OUT};
        let mut b = SpiEpdBoard::new(find("m5_paper_color").unwrap(), 0, &[]);
        power_on_cmd(&mut b, 0);
        assert!(!b.display_status(1_000_000).0, "unpowered: the command is lost");
        assert!(busy_pin_low(&mut b, 1_000_000), "unpowered: BUSY reads low");
        for (reg, v) in [(REG_GPIO_DIR, 1), (REG_GPIO_OUT, 1)] {
            assert!(b.i2c_start(0, 0, ADDR, false));
            b.i2c_write(0, 0, reg);
            b.i2c_write(0, 0, v);
            b.i2c_stop(0, 0);
        }
        assert!(!busy_pin_low(&mut b, 1_000_000), "powered and idle: BUSY_N high");
        power_on_cmd(&mut b, 2_000_000);
        assert!(b.display_status(3_000_000).0);
        assert!(busy_pin_low(&mut b, 3_000_000));
    }

    #[test]
    fn m5_paper_mono_panel_power_and_reset_are_on_the_expander() {
        use crate::devices::i2c::m5ioe1::{ADDR, REG_MODE, REG_OUT};
        let mut b = SpiEpdBoard::new(find("m5_paper_mono").unwrap(), 0, &[]);
        let write = |b: &mut SpiEpdBoard, reg: u8, v: u16| {
            assert!(b.i2c_start(0, 0, ADDR, false));
            for byte in [reg, v as u8, (v >> 8) as u8] {
                b.i2c_write(0, 0, byte);
            }
            b.i2c_stop(0, 0);
        };
        // SSD1677 SW reset: BUSY (high) for a while, unless unpowered or held in reset
        let sw_reset = |b: &mut SpiEpdBoard, now: u64| {
            let p = b.spec.pins;
            let oe = 1u64 << p.cs | 1 << p.dc | 1 << p.sck | 1 << p.mosi;
            b.gpio_out(now, 0, oe);
            b.spi_transfer(now, 2, &[0x12], 0);
            b.gpio_out(now, 1 << p.cs | 1 << p.dc, oe);
            b.display_status(now + 1_000_000).0
        };
        assert!(!sw_reset(&mut b, 0), "unpowered");
        write(&mut b, REG_MODE, 1 << 2 | 1 << 4); // GPIO3 (LDO) and GPIO5 (RST) outputs, low
        assert!(!sw_reset(&mut b, 0), "LDO off");
        write(&mut b, REG_OUT, 1 << 2); // LDO on, RST low
        assert!(!sw_reset(&mut b, 0), "held in reset");
        write(&mut b, REG_OUT, 1 << 2 | 1 << 4);
        assert!(sw_reset(&mut b, 0));
    }

    #[test]
    fn e1004_panel_is_powered_by_gpio12() {
        let mut b = SpiEpdBoard::new(find("reterminal_e1004").unwrap(), 0, &[]);
        let cs2 = 1u64 << 2;
        b.gpio_out(0, cs2, cs2);
        power_on_cmd(&mut b, 0);
        assert!(!b.display_status(1_000_000).0);
        b.gpio_out(1_000_000, cs2 | 1 << 12, cs2 | 1 << 12);
        power_on_cmd(&mut b, 2_000_000);
        assert!(b.display_status(3_000_000).0);
    }
}
