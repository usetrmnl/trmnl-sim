//! Boards: what is wired to the SoC's pins. A board is chip-agnostic; the SoC
//! calls into it with pin levels and bus transfers, in nanoseconds of virtual time.

pub mod trmnl_og;
pub mod trmnl_x;

use crate::savepoint::{StateReader, StateWriter};

/// The outside world as seen from a SoC.
pub trait Board: Send {
    /// GPIO output levels / output-enable masks changed (bit n = GPIO n).
    fn gpio_out(&mut self, now_ns: u64, out: u64, oe: u64);
    /// Levels external devices drive onto pins, as (levels, driven-mask). Undriven
    /// pins read their pull resistor state (decided by the SoC).
    fn gpio_in(&mut self, now_ns: u64) -> (u64, u64);
    /// A SPI host (e.g. GPSPI2 = host 2) clocked `mosi` out; returns MISO bytes.
    fn spi_transfer(&mut self, now_ns: u64, host: u8, mosi: &[u8], miso_len: usize) -> Vec<u8>;
    /// I2C bus events from a SoC I2C controller (`bus` = controller number).
    /// START or repeated START with the 7-bit address; returns ACK.
    fn i2c_start(&mut self, _now_ns: u64, _bus: u8, _addr: u8, _read: bool) -> bool {
        false
    }
    /// Master wrote a data byte; returns ACK.
    fn i2c_write(&mut self, _now_ns: u64, _bus: u8, _byte: u8) -> bool {
        false
    }
    /// Master reads a data byte (`ack`: master will ACK it, i.e. wants more).
    fn i2c_read(&mut self, _now_ns: u64, _bus: u8, _ack: bool) -> u8 {
        0xff
    }
    fn i2c_stop(&mut self, _now_ns: u64, _bus: u8) {}
    /// Bytes a SoC UART transmitted.
    fn uart_tx(&mut self, _now_ns: u64, _port: u8, _data: &[u8]) {}
    /// Bytes arriving at a SoC UART's RX pin by `now_ns`.
    fn uart_rx(&mut self, _now_ns: u64, _port: u8) -> Vec<u8> {
        Vec::new()
    }
    /// One completed transfer on a parallel LCD/e-paper bus (e.g. ESP32-S3 LCD_CAM i80).
    fn lcd_transfer(&mut self, _now_ns: u64, _data: &[u8]) {}
    /// Host network activity in flight on a board device (e.g. a modem): pace to wall time.
    fn realtime_required(&self) -> bool {
        false
    }
    /// Voltage on an ADC-capable GPIO, in millivolts.
    fn adc_millivolts(&mut self, gpio: u8) -> u32;
    /// Earliest future time at which the board's outputs will change on their own
    /// (e.g. the display BUSY line releasing). Used when fast-forwarding idle time.
    fn next_event_ns(&self, now_ns: u64) -> Option<u64>;
    /// Advance autonomous device behaviour (refresh animation etc.) to `now_ns`.
    fn update(&mut self, now_ns: u64);
    /// Free-form device state for diagnostics (`--profile` dumps).
    fn diagnostics(&mut self, _now_ns: u64) -> String {
        String::new()
    }
    /// What this board has (drives which controls front-ends show).
    fn info(&self) -> sim_api::BoardInfo;
    /// Front-end inputs.
    fn set_button(&mut self, down: bool);
    /// Finger down (true) or lifted (false) on a touch bar zone.
    fn set_touch(&mut self, _zone: sim_api::TouchZone, _down: bool) {}
    fn set_docked(&mut self, _docked: bool) {}
    /// Show (true) or hide the display's refresh flashes.
    fn set_refresh_flashing(&mut self, _on: bool) {}
    /// Apply the peripheral faults this board has (I2C devices absent, panel stuck, modem
    /// unresponsive, the modem's network faults).
    fn set_faults(&mut self, _faults: &sim_api::Faults) {}
    /// The battery is currently charging.
    fn charging(&self) -> bool {
        false
    }
    fn set_battery_mv(&mut self, mv: u32);
    /// Whether the display is currently busy / how many refreshes it did.
    fn display_status(&self, now_ns: u64) -> (bool, u64);
    /// Device state for a save point (see `savepoint`). `powered`: the board stays powered
    /// (deep sleep), so also volatile state (controller RAM, chip configuration); otherwise
    /// only what survives a battery pull (the e-paper image, flash memories).
    fn save_state(&self, _w: &mut StateWriter, _powered: bool) {}
    /// Load what `save_state` wrote with the same `powered`. Without `powered`, devices
    /// come back as after power-on.
    fn restore_state(&mut self, _r: &mut StateReader, _powered: bool) -> anyhow::Result<()> {
        Ok(())
    }
}
