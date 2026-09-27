//! What a board needs from an SPI e-paper controller (UC81xx, SSD16xx, ...): the serial
//! interface pins, the BUSY line and the image on the glass.

use sim_api::SharedFrame;

use crate::savepoint::{StateReader, StateWriter};

pub trait SpiEpd: Send {
    /// The serial interface's pin levels (CS, D/C, SCK, MOSI, RST) changed; for bit-banged
    /// transfers and reads (the controller answers on MOSI, see [`mosi_out`](Self::mosi_out)).
    fn set_pins(&mut self, now: u64, cs: bool, dc: bool, sck: bool, mosi: bool, rst: bool);
    /// Bytes a SPI host clocked out while CS was low (D/C as last set by `set_pins`).
    fn spi_bytes(&mut self, now: u64, data: &[u8]);
    /// The level the controller drives on MOSI while answering a read, if any.
    fn mosi_out(&self) -> Option<bool> {
        None
    }
    /// The controller is busy (refreshing, powering up, ...).
    fn busy(&self, now: u64) -> bool;
    /// The level on the BUSY pin while busy: UC81xx pull BUSY_N low, SSD16xx drive BUSY high.
    fn busy_level(&self) -> bool {
        false
    }
    /// The board switched the panel's supply (boards with a panel power switch).
    fn set_power(&mut self, _now: u64, _on: bool) {}
    /// Next time the controller wants `update` (refresh animation, BUSY release).
    fn next_event(&self, now: u64) -> Option<u64>;
    /// Advance the refresh animation to `now`.
    fn update(&mut self, now: u64);
    /// The image on the glass.
    fn frame(&self) -> SharedFrame;
    fn refresh_count(&self) -> u64;
    /// A color panel, whose refresh flashes through its inks (see `set_flashing`).
    fn is_color(&self) -> bool {
        false
    }
    fn set_flashing(&mut self, _on: bool) {}
    /// Fault: the controller never finishes (BUSY stuck).
    fn set_busy_stuck(&mut self, stuck: bool);
    /// See [`Board::save_state`](crate::board::Board::save_state).
    fn save_state(&self, w: &mut StateWriter, powered: bool);
    fn restore_state(&mut self, r: &mut StateReader, powered: bool) -> anyhow::Result<()>;
}
