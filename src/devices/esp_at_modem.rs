//! ESP32-C5 "modem" running Espressif ESP-AT firmware, modelled at its UART.
//!
//! The TRMNL X's ESP32-S3 drives a secondary ESP32-C5 over UART for 5 GHz WiFi: the C5 does
//! WiFi, DNS, TCP, TLS and HTTP itself and the S3 only sends AT commands (see
//! `lib/trmnl_x/src/modem.cpp` in trmnl-firmware). This device is chip agnostic: the host
//! SoC's UART model feeds it transmitted bytes and collects received ones, and the board glue
//! drives its EN pin and SPI_BOOT strap (via the IO expander on the real board).
//!
//! * **AT mode** (normal boot): ESP-AT with its default echo (ATE1), `\r\nready\r\n` after boot,
//!   the WiFi/SNTP/HTTP client commands the firmware uses, and URCs (`WIFI CONNECTED`,
//!   `WIFI GOT IP`, `WIFI DISCONNECT`, `+TIME_UPDATED`). Networks are a configured list;
//!   HTTP(S) requests are made for real on a worker thread (see `http.rs`).
//! * **ROM download mode** (EN rising with SPI_BOOT low): the serial bootloader subset that
//!   esp-serial-flasher uses to write a new ESP-AT image (see `rom.rs`).
//!
//! Everything is timed in the caller's virtual nanoseconds; only HTTP depends on wall time,
//! and [`EspAtModem::busy`] tells the simulator when that is the case.

#![allow(dead_code)] // Public API used by board glue / tests; not every item is wired up yet.

mod http;
mod params;
mod rom;
#[cfg(test)]
mod tests;

use std::net::Ipv4Addr;
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::{SystemTime, UNIX_EPOCH};

use params::{Param, asctime, fmt_mac, parse_params};

use crate::savepoint::{StateReader, StateWriter};

const MS: u64 = 1_000_000;

/// An access point the modem can see.
#[derive(Clone, Debug, PartialEq)]
pub struct ModemAp {
    pub ssid: String,
    /// Required passphrase; `None` accepts any.
    pub password: Option<String>,
    pub rssi: i8,
    /// 1..=14 is 2.4 GHz, >= 36 is 5 GHz.
    pub channel: u8,
    /// ESP-AT encryption code: 0 open, 1 WEP, 2 WPA_PSK, 3 WPA2_PSK, 4 WPA_WPA2_PSK,
    /// 5 WPA2_ENTERPRISE, 6 WPA3_PSK, 7 WPA2_WPA3_PSK.
    pub ecn: u8,
    pub bssid: [u8; 6],
}

#[derive(Clone, Debug)]
pub struct ModemConfig {
    /// Station MAC reported by AT+CIPSTAMAC? (and the ROM's eFuse MAC).
    pub mac: [u8; 6],
    /// What AT+CWLAP returns and AT+CWJAP can join (while WiFi is available).
    pub networks: Vec<ModemAp>,
    /// Only HTTP(S) to 10.0.2.2 (= host 127.0.0.1) is allowed; unknown names don't resolve.
    pub offline: bool,
    /// hostname -> IP answered instead of resolving (case-insensitive), applied before the
    /// 10.0.2.2 -> 127.0.0.1 mapping.
    pub dns_overrides: Vec<(String, Ipv4Addr)>,
    /// (guest port, host port) remaps for 10.0.2.2, as in `vnet::NetConfig::host_ports`.
    pub host_ports: Vec<(u16, u16)>,
    /// Capacity code of the modem's SPI flash in the ROM loader's READ_ID reply (0x16 = 4 MB).
    pub flash_size_id: u8,
}

impl Default for ModemConfig {
    fn default() -> Self {
        Self {
            mac: [0x02, 0xc5, 0x00, 0x00, 0x00, 0x01],
            networks: Vec::new(),
            offline: false,
            dns_overrides: Vec::new(),
            host_ports: Vec::new(),
            flash_size_id: 0x16,
        }
    }
}

/// Virtual-time latencies. The defaults approximate real ESP-AT on an ESP32-C5.
#[derive(Clone, Debug)]
pub struct ModemTiming {
    /// EN rising edge -> ROM boot banner.
    pub banner_ns: u64,
    /// EN rising edge -> `ready` (AT firmware up; earlier input waits in the 128-byte RX FIFO).
    pub boot_ns: u64,
    /// EN rising edge (download strap) -> ROM loader accepts commands.
    pub rom_boot_ns: u64,
    /// Ordinary AT command turnaround.
    pub cmd_ns: u64,
    /// AT+CWLAP scan (all channels, both bands).
    pub scan_ns: u64,
    /// AT+CWJAP -> `WIFI CONNECTED` (association).
    pub assoc_ns: u64,
    /// AT+CWJAP -> `WIFI GOT IP` + `OK` (association + DHCP).
    pub join_ns: u64,
    /// AT+CWJAP with a wrong password -> `+CWJAP:2` / `FAIL` (4-way handshake retries).
    pub join_bad_password_ns: u64,
    /// AT+CWJAP for an SSID not in range -> `+CWJAP:3` / `FAIL` (scan finds nothing).
    pub join_not_found_ns: u64,
    /// AP back in range -> automatic reconnect `WIFI CONNECTED`/`WIFI GOT IP`.
    pub reconnect_ns: u64,
    /// SNTP enabled and connected -> `+TIME_UPDATED`.
    pub sntp_ns: u64,
    /// Minimum AT+HTTPCLIENT latency before the first output (the rest is real network time).
    pub http_min_ns: u64,
    /// ROM loader per-command turnaround.
    pub rom_cmd_ns: u64,
    /// ROM flash erase, per 64 KiB block and per leftover 4 KiB sector.
    pub rom_erase_block_ns: u64,
    pub rom_erase_sector_ns: u64,
    /// ROM flash programming time per KiB of FLASH_DATA.
    pub rom_write_ns_per_kb: u64,
}

impl Default for ModemTiming {
    fn default() -> Self {
        Self {
            banner_ns: 2 * MS,
            boot_ns: 300 * MS,
            rom_boot_ns: 40 * MS,
            cmd_ns: 2 * MS,
            scan_ns: 2000 * MS,
            assoc_ns: 1000 * MS,
            join_ns: 1500 * MS,
            join_bad_password_ns: 5000 * MS,
            join_not_found_ns: 3000 * MS,
            reconnect_ns: 2500 * MS,
            sntp_ns: 1000 * MS,
            http_min_ns: 5 * MS,
            rom_cmd_ns: MS / 5,
            rom_erase_block_ns: 150 * MS,
            rom_erase_sector_ns: 40 * MS,
            rom_write_ns_per_kb: 2500 * 1000,
        }
    }
}

/// Counters and last-seen values, for tests and diagnostics. Survive power cycles.
#[derive(Clone, Debug, Default)]
pub struct ModemStats {
    /// Normal (AT firmware) boots, including AT+RST and FLASH_END reboots.
    pub boots: u32,
    /// Boots into ROM download mode.
    pub rom_boots: u32,
    pub at_commands: u64,
    pub last_command: Option<String>,
    /// Lines ESP-AT rejected as unknown/malformed (`ERROR`).
    pub unknown_commands: u64,
    pub last_unknown_command: Option<String>,
    /// Commands rejected with `busy p...` while a scan/join/request was running.
    pub busy_rejections: u64,
    pub scans: u32,
    pub join_attempts: u32,
    pub joins: u32,
    pub sntp_syncs: u32,
    pub http_requests: u64,
    pub http_errors: u64,
    /// Body bytes relayed to the host.
    pub http_bytes: u64,
    pub last_url: Option<String>,
    /// Final HTTP status of the last request (None if it never got one).
    pub last_http_status: Option<u16>,
    /// Custom (AT+HTTPCHEAD) headers sent with the last request.
    pub last_request_headers: Vec<(String, String)>,
    /// Hostname set with AT+CWHOSTNAME (as of the last join attempt).
    pub hostname: Option<String>,
    pub rom_packets: u64,
    pub rom_checksum_errors: u64,
    pub rom_unknown_commands: u64,
    pub flash_begins: u32,
    pub flash_data_packets: u64,
    pub flash_bytes_written: u64,
    pub flash_ends: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Off,
    At,
    Rom,
}

enum Ev {
    Emit(Vec<u8>),
    BootReady,
    RomReady,
    /// End of a timed busy operation (scan).
    OpDone,
    JoinAssoc {
        conn_gen: u64,
        ap: ModemAp,
    },
    JoinGotIp {
        conn_gen: u64,
        ap: ModemAp,
        password: String,
    },
    JoinFail {
        conn_gen: u64,
        code: u8,
    },
    Reconnect {
        conn_gen: u64,
    },
    TimeSync {
        conn_gen: u64,
    },
    /// AT+RST or FLASH_END(run): restart into the AT firmware.
    Restart,
}

enum Op {
    Idle,
    /// A scan or join is in progress; new commands get `busy p...`.
    Busy,
    Http {
        rx: Receiver<http::HttpMsg>,
        not_before: u64,
    },
}

#[derive(Clone, Copy)]
enum DataKind {
    Header,
    Url,
}

struct DataMode {
    kind: DataKind,
    remaining: usize,
    buf: Vec<u8>,
}

/// UART RX FIFO depth of the ESP32-C5; bytes arriving before ESP-AT is up wait here.
const RX_FIFO: usize = 128;
const MAX_LINE: usize = 2048;

pub struct EspAtModem {
    cfg: ModemConfig,
    timing: ModemTiming,
    now: u64,
    mode: Mode,
    /// AT firmware / ROM loader is accepting input.
    ready: bool,
    boot_fifo: Vec<u8>,
    /// Pending events, sorted by (due, insertion order).
    timeline: Vec<(u64, u64, Ev)>,
    seq: u64,
    /// Bytes due for the host, not yet collected by poll().
    rx: Vec<u8>,

    // ESP-AT state (lost on power off).
    echo: bool,
    line: Vec<u8>,
    data_mode: Option<DataMode>,
    cmd_free_at: u64,
    op: Op,
    connected: Option<ModemAp>,
    /// (ssid, password) of the last successful join; ESP-AT keeps reconnecting to it.
    reconnect: Option<(String, String)>,
    /// Bumped on every connection-state change so stale join/SNTP events are dropped.
    conn_gen: u64,
    hostname: String,
    headers: Vec<Vec<u8>>,
    url_cfg: Option<String>,
    /// Some(timezone offset in seconds) while SNTP is enabled.
    sntp: Option<i64>,
    sntp_servers: Vec<String>,
    time_synced: bool,
    uart_baud: u32,
    cwmode: i64,

    // ROM loader state.
    rom: rom::RomLoader,
    rom_free_at: u64,

    // Survives power cycles.
    wifi_available: bool,
    /// Faults: AT input is ignored; network faults for the HTTP client.
    unresponsive: bool,
    /// Fault: AT commands starting with one of these answer ERROR.
    at_errors: Vec<String>,
    net_faults: vnet::NetFaults,
    flash: Option<Vec<u8>>,
    stats: ModemStats,
}

impl EspAtModem {
    pub fn new(cfg: ModemConfig) -> Self {
        Self::with_timing(cfg, ModemTiming::default())
    }

    pub fn with_timing(cfg: ModemConfig, timing: ModemTiming) -> Self {
        EspAtModem {
            cfg,
            timing,
            now: 0,
            mode: Mode::Off,
            ready: false,
            boot_fifo: Vec::new(),
            timeline: Vec::new(),
            seq: 0,
            rx: Vec::new(),
            echo: true,
            line: Vec::new(),
            data_mode: None,
            cmd_free_at: 0,
            op: Op::Idle,
            connected: None,
            reconnect: None,
            conn_gen: 0,
            hostname: String::new(),
            headers: Vec::new(),
            url_cfg: None,
            sntp: None,
            sntp_servers: Vec::new(),
            time_synced: false,
            uart_baud: 115_200,
            cwmode: 1,
            rom: rom::RomLoader::default(),
            rom_free_at: 0,
            wifi_available: true,
            unresponsive: false,
            at_errors: Vec::new(),
            net_faults: vnet::NetFaults::default(),
            flash: None,
            stats: ModemStats::default(),
        }
    }

    // ------------------------------------------------------------------------------------
    // Public API
    // ------------------------------------------------------------------------------------

    /// EN pin level and boot strap. `download_strap` = SPI_BOOT held low at the EN rising edge
    /// (ROM download mode); it is only sampled on that edge.
    pub fn set_power(&mut self, now_ns: u64, enabled: bool, download_strap: bool) {
        self.advance(now_ns);
        match (enabled, self.mode) {
            (true, Mode::Off) => {
                self.reset_state();
                if download_strap {
                    self.stats.rom_boots += 1;
                } else {
                    self.stats.boots += 1;
                }
                self.boot(self.now, download_strap);
            }
            (false, Mode::At | Mode::Rom) => {
                self.reset_state();
                self.mode = Mode::Off;
            }
            _ => {}
        }
    }

    /// Bytes the host transmitted on its UART TX.
    pub fn host_tx(&mut self, now_ns: u64, data: &[u8]) {
        self.advance(now_ns);
        match self.mode {
            Mode::Off => {}
            Mode::At if self.unresponsive => {
                log::debug!(target: "modem", "fault: ignoring {} bytes", data.len());
            }
            Mode::At if !self.ready => {
                let room = RX_FIFO.saturating_sub(self.boot_fifo.len());
                self.boot_fifo.extend_from_slice(&data[..data.len().min(room)]);
            }
            Mode::At => self.at_input(self.now, data),
            Mode::Rom if !self.ready => {}
            Mode::Rom => self.rom_input(data),
        }
        self.advance(self.now);
    }

    /// Bytes for the host's UART RX that are due by `now_ns`. Never blocks.
    pub fn poll(&mut self, now_ns: u64) -> Vec<u8> {
        self.advance(now_ns);
        std::mem::take(&mut self.rx)
    }

    /// When poll() will next have something to say, for idle fast-forwarding. None while
    /// waiting on nothing, or only on the network (see [`busy`](Self::busy)).
    pub fn next_event_ns(&self) -> Option<u64> {
        if !self.rx.is_empty() {
            return Some(self.now);
        }
        self.timeline.first().map(|e| e.0)
    }

    /// True while waiting on host network I/O; guest time should then track wall time.
    pub fn busy(&self) -> bool {
        matches!(self.op, Op::Http { .. })
    }

    /// Networks in or out of range. Losing them drops the connection (`WIFI DISCONNECT`);
    /// when they come back ESP-AT reconnects on its own to the last joined AP.
    pub fn set_wifi_available(&mut self, on: bool) {
        if on == self.wifi_available {
            return;
        }
        self.wifi_available = on;
        if self.mode != Mode::At || !self.ready {
            return;
        }
        if !on {
            if self.connected.take().is_some() {
                self.conn_gen += 1;
                self.emit(self.now, b"WIFI DISCONNECT\r\n");
            }
        } else if self.connected.is_none() && !matches!(self.op, Op::Busy) && self.reconnect.is_some() {
            let conn_gen = self.conn_gen;
            self.schedule(self.now + self.timing.reconnect_ns, Ev::Reconnect { conn_gen });
        }
    }

    /// Fault: stop answering AT commands (everything the host sends is ignored; replies
    /// already on their way still arrive). The ROM loader is unaffected.
    /// Fault: answer ERROR to AT commands that start with one of `prefixes`.
    pub fn set_at_errors(&mut self, prefixes: &[String]) {
        self.at_errors = prefixes.to_vec();
    }

    pub fn set_unresponsive(&mut self, on: bool) {
        self.unresponsive = on;
    }

    /// Network faults for the HTTP requests started from now on.
    pub fn set_net_faults(&mut self, faults: vnet::NetFaults) {
        self.net_faults = faults;
    }

    pub fn connected_ssid(&self) -> Option<String> {
        self.connected.as_ref().map(|ap| ap.ssid.clone())
    }

    pub fn stats(&self) -> ModemStats {
        self.stats.clone()
    }

    // Extras beyond the minimal API, handy for board glue and tests.

    pub fn powered(&self) -> bool {
        self.mode != Mode::Off
    }

    /// Powered and running the ROM serial loader.
    pub fn in_download_mode(&self) -> bool {
        self.mode == Mode::Rom
    }

    /// Baud rate the modem's UART is currently set to (AT+UART_CUR; 115200 after boot).
    pub fn uart_baud(&self) -> u32 {
        self.uart_baud
    }

    /// Contents of the modem's flash as written through the ROM loader (None if never).
    pub fn flash_image(&self) -> Option<&[u8]> {
        self.flash.as_deref()
    }

    pub fn wifi_available(&self) -> bool {
        self.wifi_available
    }

    /// Save point state: what survives power cycles (the flashed image, the MAC, whether
    /// networks are in range). ESP-AT's running state is not saved.
    pub fn save_state(&self, w: &mut StateWriter) {
        w.bytes(&self.cfg.mac);
        w.bool(self.wifi_available);
        w.opt_bytes(self.flash.as_deref());
    }

    /// Load `save_state` output into a powered-off modem at virtual time `now_ns`.
    pub fn restore_state(&mut self, r: &mut StateReader, now_ns: u64) -> anyhow::Result<()> {
        self.reset_state();
        self.mode = Mode::Off;
        self.rx.clear();
        self.now = now_ns;
        self.cfg.mac = r.array()?;
        self.wifi_available = r.bool()?;
        self.flash = r.opt_bytes()?.map(<[u8]>::to_vec);
        Ok(())
    }

    // ------------------------------------------------------------------------------------
    // Timeline
    // ------------------------------------------------------------------------------------

    fn schedule(&mut self, due: u64, ev: Ev) {
        let pos = self.timeline.partition_point(|e| e.0 <= due);
        self.timeline.insert(pos, (due, self.seq, ev));
        self.seq += 1;
    }

    fn emit(&mut self, due: u64, bytes: &[u8]) {
        if bytes.len() < 200 {
            log::debug!(target: "modem", "-> {:?}", String::from_utf8_lossy(bytes));
        }
        self.schedule(due, Ev::Emit(bytes.to_vec()));
    }

    fn advance(&mut self, now: u64) {
        self.now = self.now.max(now);
        loop {
            self.pump_http();
            if self.timeline.first().is_none_or(|e| e.0 > self.now) {
                break;
            }
            let (due, _, ev) = self.timeline.remove(0);
            self.handle_event(due, ev);
        }
    }

    fn handle_event(&mut self, t: u64, ev: Ev) {
        match ev {
            Ev::Emit(b) => self.rx.extend_from_slice(&b),
            Ev::BootReady => {
                self.ready = true;
                self.rx.extend_from_slice(b"\r\nready\r\n");
                let fifo = std::mem::take(&mut self.boot_fifo);
                self.at_input(t, &fifo);
            }
            Ev::RomReady => self.ready = true,
            Ev::OpDone => self.op = Op::Idle,
            Ev::JoinAssoc { conn_gen, ap } if conn_gen == self.conn_gen => {
                if self.ap_in_range(&ap) {
                    self.rx.extend_from_slice(b"WIFI CONNECTED\r\n");
                } else {
                    self.join_failed(t, 3);
                }
            }
            Ev::JoinGotIp { conn_gen, ap, password } if conn_gen == self.conn_gen => {
                if self.ap_in_range(&ap) {
                    self.rx.extend_from_slice(b"WIFI GOT IP\r\n\r\nOK\r\n");
                    self.reconnect = Some((ap.ssid.clone(), password));
                    self.connected = Some(ap);
                    self.op = Op::Idle;
                    self.stats.joins += 1;
                    self.on_got_ip(t);
                } else {
                    self.rx.extend_from_slice(b"WIFI DISCONNECT\r\n");
                    self.join_failed(t, 4);
                }
            }
            Ev::JoinFail { conn_gen, code } if conn_gen == self.conn_gen => self.join_failed(t, code),
            Ev::Reconnect { conn_gen } if conn_gen == self.conn_gen => {
                if self.connected.is_some() || !matches!(self.op, Op::Idle | Op::Http { .. }) {
                    return;
                }
                let Some((ssid, pwd)) = self.reconnect.clone() else { return };
                if let Ok(ap) = self.find_ap(&ssid, &pwd) {
                    self.rx.extend_from_slice(b"WIFI CONNECTED\r\nWIFI GOT IP\r\n");
                    self.connected = Some(ap);
                    self.stats.joins += 1;
                    self.on_got_ip(t);
                }
            }
            Ev::TimeSync { conn_gen } if conn_gen == self.conn_gen => {
                if self.connected.is_some() && self.sntp.is_some() && !self.time_synced {
                    self.time_synced = true;
                    self.stats.sntp_syncs += 1;
                    self.rx.extend_from_slice(b"+TIME_UPDATED\r\n");
                }
            }
            Ev::Restart => {
                self.reset_state();
                self.stats.boots += 1;
                self.boot(t, false);
            }
            // Stale connection events.
            Ev::JoinAssoc { .. }
            | Ev::JoinGotIp { .. }
            | Ev::JoinFail { .. }
            | Ev::Reconnect { .. }
            | Ev::TimeSync { .. } => {}
        }
    }

    fn join_failed(&mut self, _t: u64, code: u8) {
        self.conn_gen += 1;
        self.op = Op::Idle;
        self.rx.extend_from_slice(format!("+CWJAP:{code}\r\n\r\nFAIL\r\n").as_bytes());
    }

    fn on_got_ip(&mut self, t: u64) {
        if self.sntp.is_some() && !self.time_synced {
            let conn_gen = self.conn_gen;
            self.schedule(t + self.timing.sntp_ns, Ev::TimeSync { conn_gen });
        }
    }

    fn reset_state(&mut self) {
        self.ready = false;
        self.boot_fifo.clear();
        self.timeline.clear();
        self.echo = true;
        self.line.clear();
        self.data_mode = None;
        self.cmd_free_at = 0;
        self.op = Op::Idle; // drops any HTTP worker's channel, cancelling it
        self.connected = None;
        self.reconnect = None;
        self.conn_gen += 1;
        self.hostname.clear();
        self.headers.clear();
        self.url_cfg = None;
        self.sntp = None;
        self.sntp_servers.clear();
        self.time_synced = false;
        self.uart_baud = 115_200;
        self.cwmode = 1;
        self.rom = rom::RomLoader::default();
        self.rom_free_at = 0;
    }

    fn boot(&mut self, t: u64, download: bool) {
        self.mode = if download { Mode::Rom } else { Mode::At };
        self.ready = false;
        let boot_mode = if download {
            "boot:0x14 (DOWNLOAD(USB/UART0))\r\nwaiting for download\r\n"
        } else {
            "boot:0x18 (SPI_FAST_FLASH_BOOT)\r\n"
        };
        let banner = format!("ESP-ROM:esp32c5-eco2-20250121\r\nBuild:Jan 21 2025\r\nrst:0x1 (POWERON),{boot_mode}");
        self.emit(t + self.timing.banner_ns, banner.as_bytes());
        if download {
            self.schedule(t + self.timing.rom_boot_ns, Ev::RomReady);
        } else {
            self.schedule(t + self.timing.boot_ns, Ev::BootReady);
        }
    }

    // ------------------------------------------------------------------------------------
    // ROM download mode
    // ------------------------------------------------------------------------------------

    fn rom_input(&mut self, data: &[u8]) {
        for pkt in self.rom.slip.feed(data) {
            let ctx = rom::RomCtx {
                flash: &mut self.flash,
                flash_size_id: self.cfg.flash_size_id,
                mac: self.cfg.mac,
                stats: &mut self.stats,
                timing: &self.timing,
            };
            let Some(reply) = self.rom.handle(&pkt, ctx) else { continue };
            let t = self.now.max(self.rom_free_at) + reply.cost_ns;
            self.rom_free_at = t;
            self.schedule(t, Ev::Emit(reply.bytes));
            if reply.reboot {
                self.schedule(t + MS, Ev::Restart);
            }
        }
    }

    // ------------------------------------------------------------------------------------
    // AT mode
    // ------------------------------------------------------------------------------------

    fn at_input(&mut self, t: u64, data: &[u8]) {
        let mut echo = Vec::new();
        for &b in data {
            if let Some(dm) = &mut self.data_mode {
                // Raw data after a `>` prompt: exactly `remaining` bytes, not echoed.
                dm.buf.push(b);
                dm.remaining -= 1;
                if dm.remaining == 0 {
                    let dm = self.data_mode.take().unwrap();
                    self.finish_data(t, dm);
                }
                continue;
            }
            if self.echo {
                echo.push(b);
            }
            if self.line.len() < MAX_LINE {
                self.line.push(b);
            }
            if b == b'\n' {
                if !echo.is_empty() {
                    self.emit(t, &std::mem::take(&mut echo));
                }
                let line = std::mem::take(&mut self.line);
                self.handle_line(t, &line);
            }
        }
        if !echo.is_empty() {
            self.emit(t, &echo);
        }
    }

    fn handle_line(&mut self, t: u64, raw: &[u8]) {
        let text = String::from_utf8_lossy(raw);
        let text = text.trim_end_matches(['\r', '\n']);
        // ESP-AT scans for the "AT" prefix; anything else (e.g. the S3's boot banner) is noise.
        let Some(pos) = text.find("AT") else { return };
        let cmd = text[pos..].to_string();
        if !matches!(self.op, Op::Idle) {
            self.stats.busy_rejections += 1;
            self.emit(t + self.timing.cmd_ns, b"busy p...\r\n");
            return;
        }
        self.stats.at_commands += 1;
        self.stats.last_command = Some(cmd.clone());
        log::debug!(target: "modem", "t={t} AT {cmd}");
        let t0 = t.max(self.cmd_free_at) + self.timing.cmd_ns;
        self.cmd_free_at = t0;
        if self.at_errors.iter().any(|p| cmd.starts_with(p.as_str())) {
            self.error(t0);
            return;
        }
        if !self.dispatch(t0, &cmd) {
            self.stats.unknown_commands += 1;
            self.stats.last_unknown_command = Some(cmd);
            self.error(t0);
        }
    }

    fn ok(&mut self, t: u64) {
        self.emit(t, b"\r\nOK\r\n");
    }

    fn error(&mut self, t: u64) {
        self.emit(t, b"\r\nERROR\r\n");
    }

    fn info_ok(&mut self, t: u64, info: &str) {
        self.emit(t, format!("{info}\r\n\r\nOK\r\n").as_bytes());
    }

    /// Returns false for unknown/malformed commands (caller answers ERROR).
    fn dispatch(&mut self, t: u64, cmd: &str) -> bool {
        match cmd {
            "AT" => {
                self.ok(t);
                return true;
            }
            "ATE0" | "ATE1" => {
                self.echo = cmd == "ATE1";
                self.ok(t);
                return true;
            }
            _ => {}
        }
        let Some(rest) = cmd.strip_prefix("AT+") else { return false };
        let (name, form) = match rest.find(['=', '?']) {
            None => (rest, Form::Exec),
            Some(i) => {
                let (n, tail) = rest.split_at(i);
                if tail == "?" {
                    (n, Form::Query)
                } else if tail == "=?" {
                    (n, Form::Test)
                } else if let Some(p) = tail.strip_prefix('=') {
                    match parse_params(p) {
                        Some(v) => (n, Form::Set(v)),
                        None => return false,
                    }
                } else {
                    return false;
                }
            }
        };
        match (name, form) {
            ("RST", Form::Exec) => {
                self.ok(t);
                self.schedule(t + MS, Ev::Restart);
            }
            ("GMR", Form::Exec) => self.info_ok(
                t,
                "AT version:4.1.1.0(sim)\r\nSDK version:v5.4.1\r\ncompile time:Jan 21 2025 00:00:00\r\nBin version:v4.1.1.0(ESP32C5-4MB)",
            ),
            ("CWMODE", Form::Set(p)) => match p.first().and_then(Param::int) {
                Some(m @ 0..=3) => {
                    self.cwmode = m;
                    self.ok(t);
                }
                _ => return false,
            },
            ("CWMODE", Form::Query) => self.info_ok(t, &format!("+CWMODE:{}", self.cwmode)),
            ("CWAUTOCONN", Form::Set(p)) => match p.first().and_then(Param::int) {
                Some(0 | 1) => self.ok(t),
                _ => return false,
            },
            ("UART_CUR" | "UART_DEF", Form::Set(p)) => match p.first().and_then(Param::int) {
                Some(baud @ 80..=5_000_000) => {
                    self.ok(t);
                    // The new rate applies after the OK has gone out.
                    self.uart_baud = baud as u32;
                }
                _ => return false,
            },
            ("CIPSTAMAC", Form::Query) => self.info_ok(t, &format!("+CIPSTAMAC:\"{}\"", fmt_mac(&self.cfg.mac))),
            ("CWHOSTNAME", Form::Set(p)) => match p.first().and_then(Param::str) {
                Some(h) if !h.is_empty() && h.len() <= 32 => {
                    self.hostname = h.to_string();
                    self.ok(t);
                }
                _ => return false,
            },
            ("CWHOSTNAME", Form::Query) => {
                let h = if self.hostname.is_empty() { "espressif" } else { &self.hostname };
                self.info_ok(t, &format!("+CWHOSTNAME:{h}"));
            }
            ("CWLAP", Form::Exec) => self.cwlap(t, None),
            ("CWLAP", Form::Set(p)) => match p.first().and_then(Param::str) {
                Some(s) => self.cwlap(t, Some(s.to_string())),
                None => return false,
            },
            ("CWJAP", Form::Set(p)) => {
                let (Some(ssid), pwd) = (p.first().and_then(Param::str), p.get(1).and_then(Param::str)) else {
                    return false;
                };
                let (ssid, pwd) = (ssid.to_string(), pwd.unwrap_or("").to_string());
                self.cwjap(t, ssid, pwd);
            }
            ("CWJAP", Form::Query) => match &self.connected {
                Some(ap) => {
                    let s = format!(
                        "+CWJAP:\"{}\",\"{}\",{},{},0,1,3,0,1",
                        ap.ssid,
                        fmt_mac(&ap.bssid),
                        ap.channel,
                        ap.rssi
                    );
                    self.info_ok(t, &s);
                }
                None => self.info_ok(t, "No AP"),
            },
            ("CWQAP", Form::Exec) => {
                self.ok(t);
                self.reconnect = None;
                self.conn_gen += 1;
                if self.connected.take().is_some() {
                    self.emit(t, b"WIFI DISCONNECT\r\n");
                }
            }
            ("CIPSNTPCFG", Form::Set(p)) => {
                let enable = p.first().and_then(Param::int);
                match enable {
                    Some(0) => {
                        self.sntp = None;
                        self.time_synced = false;
                        self.ok(t);
                    }
                    Some(1) => {
                        let tz = match p.get(1) {
                            None | Some(Param::Empty) => 0,
                            Some(Param::Int(v)) => *v,
                            Some(Param::Str(_)) => return false,
                        };
                        let tz_secs = if (-12..=14).contains(&tz) {
                            tz * 3600
                        } else {
                            (tz / 100) * 3600 + (tz % 100) * 60
                        };
                        self.sntp = Some(tz_secs);
                        self.sntp_servers = p.iter().skip(2).filter_map(Param::str).map(String::from).collect();
                        self.time_synced = false;
                        self.ok(t);
                        if self.connected.is_some() {
                            let conn_gen = self.conn_gen;
                            self.schedule(t + self.timing.sntp_ns, Ev::TimeSync { conn_gen });
                        }
                    }
                    _ => return false,
                }
            }
            ("CIPSNTPCFG", Form::Query) => {
                let mut s = match self.sntp {
                    Some(tz) => format!("+CIPSNTPCFG:1,{}", tz / 3600),
                    None => "+CIPSNTPCFG:0".to_string(),
                };
                for srv in &self.sntp_servers {
                    s.push_str(&format!(",\"{srv}\""));
                }
                self.info_ok(t, &s);
            }
            ("CIPSNTPTIME", Form::Query) => {
                let epoch = if self.time_synced {
                    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
                } else {
                    (t / 1_000_000_000) as i64 // RTC counts from 1970 until synced
                };
                let s = format!("+CIPSNTPTIME:{}", asctime(epoch + self.sntp.unwrap_or(0)));
                self.info_ok(t, &s);
            }
            ("HTTPCHEAD", Form::Set(p)) => match p.first().and_then(Param::int) {
                Some(0) => {
                    self.headers.clear();
                    self.ok(t);
                }
                Some(n @ 1..=8192) => {
                    self.emit(t, b"\r\nOK\r\n\r\n>");
                    self.data_mode = Some(DataMode { kind: DataKind::Header, remaining: n as usize, buf: Vec::new() });
                }
                _ => return false,
            },
            ("HTTPCHEAD", Form::Query) => {
                let mut s = String::new();
                for (i, h) in self.headers.iter().enumerate() {
                    s.push_str(&format!("+HTTPCHEAD:{i},\"{}\"\r\n", String::from_utf8_lossy(h)));
                }
                self.emit(t, format!("{s}\r\nOK\r\n").as_bytes());
            }
            ("HTTPURLCFG", Form::Set(p)) => match p.first().and_then(Param::int) {
                Some(0) => {
                    self.url_cfg = None;
                    self.emit(t, b"\r\nSET OK\r\n");
                }
                Some(n @ 8..=8192) => {
                    self.emit(t, b"\r\nOK\r\n\r\n>");
                    self.data_mode = Some(DataMode { kind: DataKind::Url, remaining: n as usize, buf: Vec::new() });
                }
                _ => return false,
            },
            ("HTTPURLCFG", Form::Query) => {
                let s = match &self.url_cfg {
                    Some(u) => format!("+HTTPURLCFG:{},{}", u.len(), u),
                    None => "+HTTPURLCFG:0,".to_string(),
                };
                self.info_ok(t, &s);
            }
            ("HTTPCLIENT", Form::Set(p)) => return self.httpclient(t, &p),
            _ => return false,
        }
        true
    }

    fn finish_data(&mut self, t: u64, dm: DataMode) {
        let t = t.max(self.cmd_free_at) + self.timing.cmd_ns;
        self.cmd_free_at = t;
        match dm.kind {
            DataKind::Header => {
                self.headers.push(dm.buf);
                self.ok(t);
            }
            DataKind::Url => {
                self.url_cfg = Some(String::from_utf8_lossy(&dm.buf).into_owned());
                self.emit(t, b"\r\nSET OK\r\n");
            }
        }
    }

    fn visible_networks(&self) -> impl Iterator<Item = &ModemAp> {
        self.cfg.networks.iter().filter(|_| self.wifi_available)
    }

    fn ap_in_range(&self, ap: &ModemAp) -> bool {
        self.visible_networks().any(|a| a == ap)
    }

    /// Strongest visible AP with this SSID; Err(ESP-AT CWJAP error code) otherwise.
    fn find_ap(&self, ssid: &str, pwd: &str) -> Result<ModemAp, u8> {
        let ap = self.visible_networks().filter(|a| a.ssid == ssid).max_by_key(|a| a.rssi).ok_or(3u8)?;
        match &ap.password {
            Some(p) if p != pwd => Err(2),
            _ => Ok(ap.clone()),
        }
    }

    fn cwlap(&mut self, t: u64, filter: Option<String>) {
        self.stats.scans += 1;
        self.op = Op::Busy;
        let mut out = String::new();
        let nets: Vec<ModemAp> =
            self.visible_networks().filter(|a| filter.as_ref().is_none_or(|f| *f == a.ssid)).cloned().collect();
        for a in &nets {
            // <ecn>,<ssid>,<rssi>,<mac>,<channel>,<freq_offset>,<freqcal_val>,<pairwise_cipher>,
            // <group_cipher>,<bgn>,<wps>  (ESP-AT's default AT+CWLAPOPT mask)
            let cipher = if a.ecn == 0 { 0 } else { 4 };
            out.push_str(&format!(
                "+CWLAP:({},\"{}\",{},\"{}\",{},-1,-1,{},{},7,0)\r\n",
                a.ecn,
                a.ssid,
                a.rssi,
                fmt_mac(&a.bssid),
                a.channel,
                cipher,
                cipher,
            ));
        }
        out.push_str("\r\nOK\r\n");
        let done = t + self.timing.scan_ns;
        self.emit(done, out.as_bytes());
        self.schedule(done, Ev::OpDone);
    }

    fn cwjap(&mut self, t: u64, ssid: String, pwd: String) {
        self.stats.join_attempts += 1;
        self.stats.hostname = (!self.hostname.is_empty()).then(|| self.hostname.clone());
        self.conn_gen += 1;
        self.reconnect = None;
        if self.connected.take().is_some() {
            self.emit(t, b"WIFI DISCONNECT\r\n");
        }
        self.op = Op::Busy;
        let conn_gen = self.conn_gen;
        match self.find_ap(&ssid, &pwd) {
            Ok(ap) => {
                self.schedule(t + self.timing.assoc_ns, Ev::JoinAssoc { conn_gen, ap: ap.clone() });
                self.schedule(t + self.timing.join_ns, Ev::JoinGotIp { conn_gen, ap, password: pwd });
            }
            Err(code) => {
                let d = if code == 2 { self.timing.join_bad_password_ns } else { self.timing.join_not_found_ns };
                self.schedule(t + d, Ev::JoinFail { conn_gen, code });
            }
        }
    }

    /// AT+HTTPCLIENT=<opt>,<content-type>,<"url">,[<"host">],[<"path">],<transport>[,<"data">][,<"header">...]
    fn httpclient(&mut self, t: u64, p: &[Param]) -> bool {
        let opt = p.first().and_then(Param::int);
        let url = match p.get(2) {
            Some(Param::Str(u)) if !u.is_empty() => u.clone(),
            Some(Param::Str(_) | Param::Empty) | None => match &self.url_cfg {
                Some(u) => u.clone(),
                None => return false,
            },
            Some(Param::Int(_)) => return false,
        };
        let head_only = match opt {
            Some(1) => true,
            Some(2) => false,
            _ => return false, // POST/PUT/DELETE: not used by the firmware
        };
        let mut headers: Vec<(String, String)> = self
            .headers
            .iter()
            .filter_map(|h| {
                let h = String::from_utf8_lossy(h);
                let (k, v) = h.split_once(':')?;
                Some((k.trim().to_string(), v.trim().to_string()))
            })
            .collect();
        // Extra per-request headers after the data parameter.
        for h in p.iter().skip(7).filter_map(Param::str) {
            if let Some((k, v)) = h.split_once(':') {
                headers.push((k.trim().to_string(), v.trim().to_string()));
            }
        }
        self.stats.http_requests += 1;
        self.stats.last_url = Some(url.clone());
        self.stats.last_http_status = None;
        self.stats.last_request_headers = headers.clone();
        if self.connected.is_none() {
            self.stats.http_errors += 1;
            self.error(t);
            return true;
        }
        log::debug!(target: "modem", "http start {url} connected={}", self.connected.is_some());
        let rx = http::spawn(http::HttpJob {
            url,
            headers,
            head_only,
            offline: self.cfg.offline,
            dns_overrides: self.cfg.dns_overrides.clone(),
            host_ports: self.cfg.host_ports.clone(),
            faults: self.net_faults.clone(),
        });
        self.op = Op::Http { rx, not_before: t.max(self.now) + self.timing.http_min_ns };
        true
    }

    /// Relay whatever the HTTP worker has produced, stamped at the current time.
    fn pump_http(&mut self) {
        let Op::Http { rx, not_before } = &self.op else { return };
        let at = self.now.max(*not_before);
        let mut out: Vec<Vec<u8>> = Vec::new();
        let mut done = None;
        loop {
            match rx.try_recv() {
                Ok(http::HttpMsg::Status(s)) => self.stats.last_http_status = Some(s),
                Ok(http::HttpMsg::Data(d)) => {
                    self.stats.http_bytes += d.len() as u64;
                    let mut f = format!("+HTTPCLIENT:{},", d.len()).into_bytes();
                    f.extend_from_slice(&d);
                    f.extend_from_slice(b"\r\n");
                    out.push(f);
                }
                Ok(http::HttpMsg::Done(r)) => {
                    done = Some(r.is_ok());
                    break;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    done = Some(false);
                    break;
                }
            }
        }
        for f in out {
            self.schedule(at, Ev::Emit(f));
        }
        if let Some(ok) = done {
            log::debug!(target: "modem", "http done ok={ok} at={at} bytes={}", self.stats.http_bytes);
            self.op = Op::Idle;
            if ok {
                self.emit(at, b"\r\nOK\r\n");
            } else {
                self.stats.http_errors += 1;
                self.emit(at, b"\r\nERROR\r\n");
            }
            self.cmd_free_at = self.cmd_free_at.max(at);
        }
    }
}

enum Form {
    Exec,
    Query,
    Test,
    Set(Vec<Param>),
}
