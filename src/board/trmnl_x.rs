//! TRMNL X: ESP32-S3; 10.3" 1872×1404 parallel e-paper (LCD_CAM i80 bus + GPIO
//! row control, TPS65185 PMIC); IQS323 capacitive touch bar; TCA9535 I/O
//! expander (display power sequencing, charger status = the magnetic dock,
//! modem straps); BQ27427 fuel gauge; ESP32-C5 ESP-AT modem on UART0 for 5 GHz.

use sim_api::{BoardInfo, TouchZone};

use super::Board;
use crate::devices::esp_at_modem::{EspAtModem, ModemAp, ModemConfig};
use crate::devices::i2c::bq27427::Bq27427;
use crate::devices::i2c::iqs323::Iqs323;
use crate::devices::i2c::tca9535::{Tca9535, pins};
use crate::devices::i2c::tps65185::Tps65185;
use crate::devices::i2c::{BitBangI2cSlave, I2cBus};
use crate::devices::parallel_epd::{PanelGeometry, ParallelEpd};
use crate::savepoint::{StateReader, StateWriter};

// S3 GPIOs
const GPIO_IQS_RDY: u8 = 3;
const GPIO_TCA_INT: u8 = 38;
const GPIO_SDA: u8 = 39;
const GPIO_SCL: u8 = 40;
const GPIO_LE: u8 = 42;
const GPIO_CKV: u8 = 45;
const GPIO_SPV: u8 = 48;

// Modem straps on the expander
const P_MODEM_SPI_BOOT: u8 = 4;
const P_MODEM_EN: u8 = 6;

pub struct TrmnlX {
    pub panel: ParallelEpd,
    pub i2c: I2cBus,
    bitbang: BitBangI2cSlave,
    pub modem: EspAtModem,
    docked: bool,
    battery_mv: u32,
    modem_power: (bool, bool),
    last_now: u64,
}

impl TrmnlX {
    pub fn new(modem_mac: [u8; 6], net: &vnet::NetConfig) -> Self {
        let modem = EspAtModem::new(ModemConfig {
            mac: modem_mac,
            networks: vec![
                ModemAp {
                    ssid: "TRMNL-Sim-5G".into(),
                    password: None,
                    rssi: -48,
                    channel: 36,
                    ecn: 3,
                    bssid: [0x02, 0x5e, 0x51, 0x00, 0x05, 0x01],
                },
                ModemAp {
                    ssid: "TRMNL-Sim".into(),
                    password: None,
                    rssi: -54,
                    channel: 6,
                    ecn: 3,
                    bssid: [0x02, 0x5e, 0x51, 0x00, 0x00, 0x01],
                },
            ],
            offline: net.offline,
            dns_overrides: net.dns_overrides.clone(),
            host_ports: net.host_ports.clone(),
            flash_size_id: 0x16,
        });
        let mut b = TrmnlX {
            panel: ParallelEpd::new(PanelGeometry::TRMNL_X),
            i2c: Self::new_i2c(),
            bitbang: BitBangI2cSlave::new(),
            modem,
            docked: false,
            battery_mv: 4000,
            modem_power: (false, false),
            last_now: 0,
        };
        b.apply_power_inputs();
        b
    }

    /// The I2C chips as they come out of power-on.
    fn new_i2c() -> I2cBus {
        let mut i2c = I2cBus::new();
        let mut tca = Tca9535::new();
        tca.set_battery_cells(1);
        i2c.add(Box::new(tca));
        i2c.add(Box::new(Tps65185::new()));
        i2c.add(Box::new(Iqs323::new()));
        i2c.add(Box::new(Bq27427::new(1)));
        i2c
    }

    fn tca(&mut self) -> &mut Tca9535 {
        self.i2c.device_mut::<Tca9535>().expect("tca9535")
    }

    fn iqs(&mut self) -> &mut Iqs323 {
        self.i2c.device_mut::<Iqs323>().expect("iqs323")
    }

    fn charging_now(&self) -> bool {
        self.docked && self.battery_mv < 4150
    }

    /// Dock/charger/battery state onto the expander and the gauge.
    fn apply_power_inputs(&mut self) {
        let (docked, charging, mv) = (self.docked, self.charging_now(), self.battery_mv);
        self.tca().set_charger(docked, charging);
        let soc = (mv.saturating_sub(3000) as f32 / 12.0).clamp(0.0, 100.0) as u8;
        if let Some(bq) = self.i2c.device_mut::<Bq27427>() {
            bq.set_battery(mv as u16, charging, soc);
        }
    }

    /// React to expander outputs: PMIC pins, panel power, modem EN/straps.
    fn sync_outputs(&mut self, now: u64) {
        let tca = self.tca();
        let _ = tca.take_output_changes();
        let (wakeup, pwrup, oe) =
            (tca.driven_high(pins::TPS_WAKEUP), tca.driven_high(pins::TPS_PWRUP), tca.driven_high(pins::EPD_OE));
        let (levels, out_en) = tca.outputs();
        let en = tca.driven_high(P_MODEM_EN);
        // Download mode: SPI_BOOT is an output driven low while EN rises.
        let strap_low = out_en >> P_MODEM_SPI_BOOT & 1 != 0 && levels >> P_MODEM_SPI_BOOT & 1 == 0;
        let tps = self.i2c.device_mut::<Tps65185>().expect("tps65185");
        tps.set_pins(now, wakeup, pwrup);
        let rails = tps.rails_on(now);
        self.panel.set_power(now, rails);
        self.panel.set_output_enable(now, oe);
        if (en, strap_low) != self.modem_power {
            self.modem_power = (en, strap_low);
            self.modem.set_power(now, en, strap_low);
        }
    }
}

impl Board for TrmnlX {
    fn gpio_out(&mut self, now: u64, out: u64, oe: u64) {
        self.last_now = now;
        let level = |pin: u8| oe >> pin & 1 == 0 || out >> pin & 1 != 0;
        self.panel.set_row_pins(now, level(GPIO_SPV), level(GPIO_CKV), level(GPIO_LE));
        // Wake stub: bit-banged I2C on the (open-drain) SDA/SCL pins. When the
        // hardware I2C controller owns the pins the GPIO outputs stay released.
        let sda_low = oe >> GPIO_SDA & 1 != 0 && out >> GPIO_SDA & 1 == 0;
        let scl = !(oe >> GPIO_SCL & 1 != 0 && out >> GPIO_SCL & 1 == 0);
        self.bitbang.pins(&mut self.i2c, now, sda_low, scl);
        // The SoC pulling RDY low is an MCLR reset of the IQS323.
        let rdy_low = oe >> GPIO_IQS_RDY & 1 != 0 && out >> GPIO_IQS_RDY & 1 == 0;
        self.iqs().soc_drive_rdy(now, rdy_low);
        self.sync_outputs(now);
    }

    fn gpio_in(&mut self, now: u64) -> (u64, u64) {
        self.last_now = now;
        let mut lv = 1u64 << GPIO_SCL;
        let mask = 1u64 << GPIO_IQS_RDY | 1u64 << GPIO_TCA_INT | 1u64 << GPIO_SDA | 1u64 << GPIO_SCL;
        if !self.iqs().rdy_low(now) {
            lv |= 1 << GPIO_IQS_RDY;
        }
        if !self.tca().int_low(now) {
            lv |= 1 << GPIO_TCA_INT;
        }
        if !self.bitbang.sda_low() {
            lv |= 1 << GPIO_SDA;
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
        self.sync_outputs(now);
    }

    fn uart_tx(&mut self, now: u64, port: u8, data: &[u8]) {
        if port == 0 {
            self.modem.host_tx(now, data);
        }
    }

    fn uart_rx(&mut self, now: u64, port: u8) -> Vec<u8> {
        if port == 0 { self.modem.poll(now) } else { Vec::new() }
    }

    fn lcd_transfer(&mut self, now: u64, data: &[u8]) {
        self.panel.bus_transfer(now, data);
    }

    fn realtime_required(&self) -> bool {
        self.modem.busy()
    }

    fn adc_millivolts(&mut self, _gpio: u8) -> u32 {
        0
    }

    fn next_event_ns(&self, now: u64) -> Option<u64> {
        [self.modem.next_event_ns(), self.i2c.next_event_ns(now)].into_iter().flatten().min()
    }

    fn update(&mut self, now: u64) {
        self.last_now = now;
        self.i2c.update(now);
        self.panel.poll(now);
        self.sync_outputs(now);
    }

    fn info(&self) -> BoardInfo {
        BoardInfo {
            name: "TRMNL X".into(),
            has_button: false,
            has_touchbar: true,
            has_dock: true,
            has_5ghz: true,
            has_fuel_gauge: true,
            ..Default::default()
        }
    }

    fn set_button(&mut self, _down: bool) {}

    fn set_touch(&mut self, zone: TouchZone, down: bool) {
        let ch = match zone {
            TouchZone::Left => 0,
            TouchZone::Center => 1,
            TouchZone::Right => 2,
        };
        let now = self.last_now;
        self.iqs().touch(now, ch, down);
    }

    fn gesture(&mut self, g: sim_api::SliderGesture) -> bool {
        use crate::devices::i2c::iqs323::Gesture;
        let g = match g {
            sim_api::SliderGesture::SwipeNext => Gesture::SwipePos,
            sim_api::SliderGesture::SwipeBack => Gesture::SwipeNeg,
            sim_api::SliderGesture::FlickNext => Gesture::FlickPos,
            sim_api::SliderGesture::FlickBack => Gesture::FlickNeg,
        };
        let now = self.last_now;
        self.iqs().gesture(now, g)
    }

    fn set_docked(&mut self, docked: bool) {
        self.docked = docked;
        self.apply_power_inputs();
    }

    fn charging(&self) -> bool {
        self.charging_now()
    }

    fn set_faults(&mut self, faults: &sim_api::Faults) {
        self.i2c.absent = faults.i2c_absent.clone();
        if let Some(tps) = self.i2c.device_mut::<Tps65185>() {
            tps.rail_fault = faults.panel_busy_stuck;
        }
        self.modem.set_unresponsive(faults.modem_unresponsive);
        let now = self.last_now;
        let iqs = self.iqs();
        iqs.lockup = faults.touch_bar == Some(sim_api::TouchBarFault::Lockup);
        match faults.touch_bar {
            Some(sim_api::TouchBarFault::Reset) => iqs.inject_reset(now),
            Some(sim_api::TouchBarFault::AtiError) => iqs.inject_ati_error(now),
            _ => {}
        }
        self.modem.set_net_faults(crate::faults::net_faults(&faults.net));
    }

    fn set_battery_mv(&mut self, mv: u32) {
        self.battery_mv = mv;
        self.apply_power_inputs();
    }

    fn diagnostics(&mut self, now: u64) -> String {
        let stats = self.panel.stats();
        let tca = self.tca();
        let (lv, oe) = tca.outputs();
        let int = tca.int_low(now);
        let rails = self.i2c.device_mut::<Tps65185>().map(|t| t.rails_on(now));
        let rdy = self.iqs().rdy_low(now);
        format!(
            "panel: {stats:?}\ntca9535: out={lv:#06x} oe={oe:#06x} int_low={int}\ntps rails_on={rails:?}\niqs rdy_low={rdy}\nmodem: {:?}",
            self.modem.stats()
        )
    }

    fn display_status(&self, _now: u64) -> (bool, u64) {
        let s = self.panel.stats();
        (s.update_in_progress, s.updates)
    }
    fn save_state(&self, w: &mut StateWriter, powered: bool) {
        w.bool(self.docked);
        w.u32(self.battery_mv);
        w.u64(self.last_now);
        w.section(|w| self.panel.save_state(w, powered));
        w.section(|w| self.modem.save_state(w));
        if powered {
            w.section(|w| self.i2c.save_state(w));
        }
    }

    /// The modem comes back powered off either way; if the expander has its EN high it
    /// boots afresh (ESP-AT's session isn't part of a save point).
    fn restore_state(&mut self, r: &mut StateReader, powered: bool) -> anyhow::Result<()> {
        self.docked = r.bool()?;
        self.battery_mv = r.u32()?;
        self.last_now = r.u64()?;
        r.section(|r| self.panel.restore_state(r, powered))?;
        let now = self.last_now;
        r.section(|r| self.modem.restore_state(r, now))?;
        self.modem_power = (false, false);
        self.bitbang = BitBangI2cSlave::new();
        self.i2c = Self::new_i2c();
        if powered {
            r.section(|r| self.i2c.restore_state(r))?;
        }
        self.apply_power_inputs();
        Ok(())
    }
}
