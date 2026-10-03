//! SoCs. Each chip module assembles a core, a memory map, peripherals and boot
//! flow, and exposes itself through the chip-agnostic [`Machine`] trait that
//! the runner drives.

pub mod esp32c3;
pub mod esp32c5;
pub mod esp32s3;

use crate::board::Board;
use crate::coverage::Coverage;
use crate::firmware::Symbols;
use crate::savepoint::SocState;

#[derive(Debug, Clone, PartialEq)]
pub enum SliceExit {
    /// Virtual time reached the requested point.
    Reached,
    /// The firmware entered deep sleep (the machine is powered down).
    DeepSleep { timer_ns: Option<u64>, gpio_low_mask: u64 },
    /// The emulator cannot continue.
    Halted(String),
    /// The debugger's breakpoint, step, watchpoint or a fault stopped the CPU (at an
    /// instruction boundary; nothing more ran).
    Debug(sim_api::DebugStop),
    /// A power-loss fault fired during a flash operation (described); the device has no
    /// power until it is reset with `ResetKind::PowerOn`.
    PowerLoss(String),
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
    fn bluetooth(&mut self) -> &mut crate::hle::bluetooth::BluetoothState;
    /// The debugger's access to the cores and memory (`--gdb`).
    fn debug(&mut self) -> &mut dyn crate::debug::Debuggable;
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
    /// Replace the access points in range of the SoC's own radio.
    fn set_wifi_networks(&mut self, networks: &[sim_api::WifiNetwork]);
    /// Whether the host's portal client joins the soft-AP (`Command::SetPortalClient`).
    fn set_portal_client(&mut self, on: bool);
    fn net_status(&self) -> NetStatus;
    /// Something outside the simulation (host network) is being waited on, so
    /// virtual time must not run ahead of wall time even in turbo mode.
    fn realtime_required(&self) -> bool {
        false
    }
    /// Nothing happens until something outside the simulation does it (e.g. a light sleep
    /// that only a GPIO can end): the runner can nap instead of spinning at wall-clock pace.
    fn waiting_for_external(&self) -> bool {
        false
    }
    /// Replace the injected faults. Errors (e.g. an unknown partition) are reported; the
    /// other faults still apply.
    fn set_faults(&mut self, faults: &sim_api::Faults) -> Result<(), String>;
    /// Flash (page programs, erases) so far.
    fn flash_stats(&self) -> (u64, u64);
    /// The partition table in flash.
    fn partitions(&self) -> Vec<sim_api::PartitionInfo>;
    /// Read-only NVS snapshot, including when the guest is paused or asleep.
    fn preferences(&self) -> sim_api::PreferencesSnapshot;
    fn change_preference(&mut self, change: &sim_api::PreferenceChange) -> Result<(), String>;
    /// In light sleep: `Some(wake time)` (`Some(None)` = no timer armed).
    fn light_sleep(&self) -> Option<Option<u64>> {
        None
    }
    /// SoC state for a save point (see `savepoint`). `rtc`: also what only survives deep
    /// sleep (RTC memory and registers, the S3 cache MMU).
    fn save_soc(&mut self, rtc: bool) -> SocState;
    /// Power the SoC down into saved state: nothing of the running firmware is kept, and
    /// the next `reset` boots from it (a deep-sleep wake keeps the restored RTC state).
    fn restore_soc(&mut self, s: &SocState) -> anyhow::Result<()>;
    /// Start recording code coverage (accumulates across resets).
    fn set_coverage(&mut self, cov: Coverage);
    /// Write a new build into flash (see `firmware::install`) and know its ELF; the next boot
    /// runs it.
    fn install_firmware(&mut self, fw: &crate::firmware::Firmware) -> anyhow::Result<()>;
    /// Another app the device may boot (after an OTA update): ELF SHA-256, symbols, name.
    /// It is matched against the booting slot's app descriptor at the next boot.
    fn add_app(&mut self, sha: [u8; 32], symbols: Symbols, name: String);
    fn coverage(&mut self) -> Option<&mut Coverage>;
    /// The `--memcheck` report as JSON (`None` when memcheck is off).
    fn memcheck_json(&mut self) -> Option<String> {
        None
    }
    /// A human-readable memcheck summary, for the end of a run.
    fn memcheck_summary(&mut self) -> Option<String> {
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

/// Merges the two console channels. IDF logs to UART0 (primary console) and USB serial/JTAG
/// (secondary) alike, while Arduino's `Serial` and the ROM use one of them: a line that
/// arrives on both channels is shown once.
#[derive(Default)]
pub struct Console {
    partial: [Vec<u8>; 2],
    /// Lines shown from each channel that the other hasn't repeated (yet).
    unmatched: [std::collections::VecDeque<Vec<u8>>; 2],
}

impl Console {
    pub fn push(&mut self, ch: usize, b: u8, out: &mut Vec<u8>) {
        self.partial[ch].push(b);
        if b != b'\n' && self.partial[ch].len() < 512 {
            return;
        }
        let line = std::mem::take(&mut self.partial[ch]);
        let other = &mut self.unmatched[1 - ch];
        if let Some(i) = other.iter().position(|l| *l == line) {
            other.remove(i);
            return;
        }
        out.extend_from_slice(&line);
        let mine = &mut self.unmatched[ch];
        mine.push_back(line);
        if mine.len() > 16 {
            mine.pop_front();
        }
    }
}
