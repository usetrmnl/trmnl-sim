//! Boards: what is wired to the SoC's pins. A board is chip-agnostic; the SoC
//! calls into it with pin levels and bus transfers, in nanoseconds of virtual time.

pub mod trmnl_og;

/// The outside world as seen from a SoC.
pub trait Board: Send {
    /// GPIO output levels / output-enable masks changed (bit n = GPIO n).
    fn gpio_out(&mut self, now_ns: u64, out: u64, oe: u64);
    /// Levels external devices drive onto pins, as (levels, driven-mask). Undriven
    /// pins read their pull resistor state (decided by the SoC).
    fn gpio_in(&mut self, now_ns: u64) -> (u64, u64);
    /// A SPI host (e.g. GPSPI2 = host 2) clocked `mosi` out; returns MISO bytes.
    fn spi_transfer(&mut self, now_ns: u64, host: u8, mosi: &[u8], miso_len: usize) -> Vec<u8>;
    /// I2C: does a device ACK this 7-bit address?
    fn i2c_probe(&mut self, _addr: u8) -> bool {
        false
    }
    /// I2C write transaction (address already ACKed).
    fn i2c_write(&mut self, _addr: u8, _data: &[u8]) {}
    /// I2C read transaction; None if nothing answers.
    fn i2c_read(&mut self, _addr: u8, _n: usize) -> Option<Vec<u8>> {
        None
    }
    /// Voltage on an ADC-capable GPIO, in millivolts.
    fn adc_millivolts(&mut self, gpio: u8) -> u32;
    /// Earliest future time at which the board's outputs will change on their own
    /// (e.g. the display BUSY line releasing). Used when fast-forwarding idle time.
    fn next_event_ns(&self, now_ns: u64) -> Option<u64>;
    /// Advance autonomous device behaviour (refresh animation etc.) to `now_ns`.
    fn update(&mut self, now_ns: u64);
    /// Front-end inputs.
    fn set_button(&mut self, down: bool);
    fn set_battery_mv(&mut self, mv: u32);
    /// Whether the display is currently busy / how many refreshes it did.
    fn display_status(&self, now_ns: u64) -> (bool, u64);
}
