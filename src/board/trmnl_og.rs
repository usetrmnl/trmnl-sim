//! TRMNL OG: ESP32-C3, UC8179 7.5" panel on GPSPI2, one button, LiPo via divider.
//! The TRMNL BWRY (`trmnl_4clr`) is the same board with a 4-color panel.

use super::Board;
use crate::devices::uc8179::Uc8179;
use crate::savepoint::{StateReader, StateWriter};

pub struct Pins {
    pub sck: u8,
    pub mosi: u8,
    pub cs: u8,
    pub rst: u8,
    pub dc: u8,
    pub busy: u8,
    pub button: u8,
    pub battery_adc: u8,
}

/// Matches the "og" row of `device_list[]` in the firmware's display.cpp.
pub const PINS: Pins = Pins { sck: 7, mosi: 8, cs: 6, rst: 10, dc: 5, busy: 4, button: 2, battery_adc: 3 };

pub struct TrmnlOg {
    pub pins: Pins,
    pub panel: Uc8179,
    button_down: bool,
    battery_mv: u32,
    out: u64,
    oe: u64,
}

impl TrmnlOg {
    pub fn new(panel: Uc8179) -> Self {
        TrmnlOg { pins: PINS, panel, button_down: false, battery_mv: 4100, out: 0, oe: 0 }
    }

    fn level(&self, pin: u8) -> bool {
        self.out >> pin & 1 != 0
    }
}

impl Board for TrmnlOg {
    fn gpio_out(&mut self, now: u64, out: u64, oe: u64) {
        self.out = out;
        self.oe = oe;
        let p = &self.pins;
        // Undriven lines idle high (pull-ups on CS/RST on the real board).
        let lvl = |pin: u8| if oe >> pin & 1 != 0 { out >> pin & 1 != 0 } else { true };
        let mosi_driven = oe >> p.mosi & 1 != 0;
        let (cs, dc, sck, rst) = (lvl(p.cs), lvl(p.dc), oe >> p.sck & 1 != 0 && self.level(p.sck), lvl(p.rst));
        let mosi = mosi_driven && self.level(p.mosi);
        self.panel.set_pins(now, cs, dc, sck, mosi, rst);
    }

    fn gpio_in(&mut self, now: u64) -> (u64, u64) {
        let p = &self.pins;
        let mut lv = 0u64;
        let mut mask = 1u64 << p.busy | 1u64 << p.button;
        if self.panel.busy_n(now) {
            lv |= 1 << p.busy;
        }
        if !self.button_down {
            lv |= 1 << p.button;
        }
        if let Some(b) = self.panel.mosi_out
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
        if gpio == self.pins.battery_adc { self.battery_mv / 2 } else { 0 }
    }

    fn next_event_ns(&self, now: u64) -> Option<u64> {
        self.panel.next_event(now)
    }

    fn update(&mut self, now: u64) {
        self.panel.update(now);
    }

    fn info(&self) -> sim_api::BoardInfo {
        let name = if self.panel.is_bwry() { "TRMNL BWRY" } else { "TRMNL OG" };
        sim_api::BoardInfo {
            name: name.into(),
            has_button: true,
            has_refresh_flashing: self.panel.is_bwry(),
            ..Default::default()
        }
    }

    fn set_refresh_flashing(&mut self, on: bool) {
        self.panel.flashing = on;
    }

    fn set_button(&mut self, down: bool) {
        self.button_down = down;
    }

    fn set_battery_mv(&mut self, mv: u32) {
        self.battery_mv = mv;
    }

    fn display_status(&self, now: u64) -> (bool, u64) {
        (!self.panel.busy_n(now), self.panel.refresh_count)
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
        r.section(|r| self.panel.restore_state(r, powered))
    }
}
