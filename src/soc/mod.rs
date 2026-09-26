//! SoCs. Each chip module assembles a core, a memory map, peripherals and boot
//! flow, and exposes itself through the chip-agnostic [`Machine`] trait that
//! the runner drives.

pub mod esp32c3;
pub mod esp32s3;

use crate::board::Board;

#[derive(Debug, Clone, PartialEq)]
pub enum SliceExit {
    /// Virtual time reached the requested point.
    Reached,
    /// The firmware entered deep sleep (the machine is powered down).
    DeepSleep { timer_ns: Option<u64>, gpio_low_mask: u64 },
    /// The emulator cannot continue.
    Halted(String),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ResetKind {
    /// Power applied: all RAM (including RTC) lost.
    PowerOn,
    /// EN/RST pin: chip reset, RTC memory kept.
    ResetPin,
    /// Waking from deep sleep.
    DeepSleepWake { by_timer: bool, by_gpio: bool },
}

pub trait Machine: Send {
    /// Run until virtual time reaches `until_ns` (or something noteworthy happens).
    fn run_slice(&mut self, until_ns: u64) -> SliceExit;
    fn reset(&mut self, kind: ResetKind);
    fn now_ns(&self) -> u64;
    /// Advance virtual time without executing (e.g. while in deep sleep).
    fn advance_time(&mut self, ns: u64);
    fn instructions(&self) -> u64;
    fn board(&mut self) -> &mut dyn Board;
    /// Serial output and simulator messages produced since the last call, in order.
    fn take_output(&mut self) -> Vec<Output>;
    /// Persist flash contents.
    fn flush(&mut self);
    /// Human-readable CPU state with symbolized addresses.
    fn debug_dump(&self) -> String;
    /// Where the CPU spent its time recently (symbol, samples), for diagnosing hangs.
    fn profile(&mut self) -> Vec<(String, u64)>;
    fn boot_count(&self) -> u32;
    /// Whether the simulated WiFi network is in range.
    fn set_wifi_available(&mut self, on: bool);
    fn net_status(&self) -> NetStatus;
    /// Something outside the simulation (host network) is being waited on, so
    /// virtual time must not run ahead of wall time even in turbo mode.
    fn realtime_required(&self) -> bool {
        false
    }
    /// In light sleep: `Some(wake time)` (`Some(None)` = no timer armed).
    fn light_sleep(&self) -> Option<Option<u64>> {
        None
    }
}

#[derive(Debug, Clone)]
pub enum Output {
    Serial(Vec<u8>),
    Sim(String),
}

#[derive(Debug, Clone, Default)]
pub struct NetStatus {
    pub connected: bool,
    pub ip: Option<String>,
    /// Host URL forwarding to the device's captive portal while it runs a soft-AP.
    pub portal_url: Option<String>,
}
