//! WiFi driver replacement (ESP-IDF 4.4 API).
//!
//! The binary WiFi/PHY libraries are bypassed: every `esp_wifi_*` blob entry
//! point is hooked. The IDF glue (esp_netif, default event handlers) and lwIP
//! run for real. Connectivity is provided by `vnet`: frames lwIP transmits via
//! `esp_wifi_internal_tx` go to a user-mode router/NAT, and frames coming back
//! are injected through the rx callback lwIP registered, from a guest task the
//! HLE creates and drives ("sim_wifi"), just like the real driver's task would.

use std::collections::VecDeque;
use std::net::{Ipv4Addr, SocketAddr};

use vnet::{ApClient, NetConfig, NetFaults, VirtualNet};

use super::{Flow, HleCtx, Hooks, MAGIC_BASE};
use crate::firmware::Symbols;

const ESP_OK: u32 = 0;
const ESP_ERR_WIFI_NOT_CONNECT: u32 = 0x3000 + 15;
const ESP_ERR_INVALID_ARG: u32 = 0x102;

/// Entry point of the HLE-driven guest task.
pub const TASK_LOOP: u32 = MAGIC_BASE + 0x100;

// wifi_event_t
const EV_SCAN_DONE: u32 = 1;
const EV_STA_START: u32 = 2;
const EV_STA_STOP: u32 = 3;
const EV_STA_CONNECTED: u32 = 4;
const EV_STA_DISCONNECTED: u32 = 5;
const EV_AP_START: u32 = 12;
const EV_AP_STOP: u32 = 13;
const EV_AP_STACONNECTED: u32 = 14;
const EV_AP_STADISCONNECTED: u32 = 15;

// wifi_err_reason_t
const REASON_ASSOC_LEAVE: u8 = 8;
const REASON_HANDSHAKE_TIMEOUT: u8 = 15;
const REASON_BEACON_TIMEOUT: u8 = 200;
const REASON_NO_AP_FOUND: u8 = 201;

/// Struct layouts of the ESP-IDF WiFi API, which change between IDF releases.
#[derive(Clone, Copy, Debug)]
pub struct WifiAbi {
    pub config_size: usize,
    pub ap_record_size: usize,
    pub connected_event_size: usize,
    /// wifi_sta_list_t: entries of 12 bytes, then `num`.
    pub sta_list_entries: usize,
}

impl WifiAbi {
    pub const IDF_4_4: WifiAbi =
        WifiAbi { config_size: 140, ap_record_size: 80, connected_event_size: 44, sta_list_entries: 10 };
    pub const IDF_5_5: WifiAbi =
        WifiAbi { config_size: 184, ap_record_size: 92, connected_event_size: 48, sta_list_entries: 15 };
    /// IDF 5.5 on the dual-band ESP32-C5: wifi_country_t gains `wifi_5g_channel_mask`
    /// (wifi_ap_record_t is 96 bytes) and ESP_WIFI_MAX_CONN_NUM is 10.
    pub const IDF_5_5_DUAL_BAND: WifiAbi =
        WifiAbi { config_size: 184, ap_record_size: 96, connected_event_size: 48, sta_list_entries: 10 };

    /// Pick the layout for an app's ESP-IDF version string (e.g. "v4.4.7", "5.5.2").
    pub fn for_idf(version: &str) -> WifiAbi {
        let major = version.trim_start_matches('v').split('.').next().and_then(|m| m.parse::<u32>().ok());
        if major.is_some_and(|m| m >= 5) { Self::IDF_5_5 } else { Self::IDF_4_4 }
    }
}
const MS: u64 = 1_000_000;

/// An access point visible to the simulated device.
#[derive(Clone, Debug)]
pub struct SimAp {
    pub ssid: String,
    /// None: any password is accepted.
    pub password: Option<String>,
    pub rssi: i8,
    pub channel: u8,
    /// wifi_auth_mode_t (0 open, 3 WPA2-PSK)
    pub authmode: u32,
    /// Whether associating actually gets you online.
    pub internet: bool,
}

struct PendingEvent {
    at_ns: u64,
    id: u32,
    data: Vec<u8>,
}

pub struct WifiState {
    pub available: bool,
    pub networks: Vec<SimAp>,
    /// The radio also does 5 GHz (ESP32-C5); otherwise APs on channels 36+ are invisible.
    pub dual_band: bool,
    pub base_mac: [u8; 6],
    pub portal_forward: SocketAddr,
    pub net_config: NetConfig,
    /// Injected network faults (survive chip resets, like the rest of the "air").
    net_faults: NetFaults,

    task_created: bool,
    mode: u32,
    started: bool,
    pub abi: WifiAbi,
    sta_config: Vec<u8>,
    ap_config: Vec<u8>,
    /// Index into `networks` of the AP we're associated with.
    connected: Option<usize>,
    connecting: bool,
    rxcb: [u32; 2],
    events: Vec<PendingEvent>,
    rx: VecDeque<(usize, Vec<u8>)>,
    net: VirtualNet,
    ap_client: Option<ApClient>,
    ap_client_joined: bool,
    /// The host "browser client" stays off the soft-AP (`set_portal_client(false)`), so an
    /// unattended portal can run ahead of wall-clock time in turbo mode.
    portal_client_away: bool,
    scan_results: Vec<SimAp>,
    pub stats: WifiStats,
}

#[derive(Default, Clone, Debug)]
pub struct WifiStats {
    pub tx_frames: u64,
    pub rx_frames: u64,
}

impl WifiState {
    pub fn new(base_mac: [u8; 6]) -> Self {
        WifiState {
            available: true,
            networks: vec![
                SimAp { ssid: "TRMNL-Sim".into(), password: None, rssi: -54, channel: 6, authmode: 3, internet: true },
                SimAp {
                    ssid: "Neighbors WiFi".into(),
                    password: Some("hunter2hunter2".into()),
                    rssi: -81,
                    channel: 11,
                    authmode: 3,
                    internet: true,
                },
            ],
            dual_band: false,
            base_mac,
            portal_forward: "127.0.0.1:8080".parse().unwrap(),
            net_config: NetConfig::default(),
            net_faults: NetFaults::default(),
            task_created: false,
            mode: 0,
            started: false,
            abi: WifiAbi::IDF_4_4,
            sta_config: vec![0; WifiAbi::IDF_4_4.config_size],
            ap_config: vec![0; WifiAbi::IDF_4_4.config_size],
            connected: None,
            connecting: false,
            rxcb: [0; 2],
            events: Vec::new(),
            rx: VecDeque::new(),
            net: VirtualNet::new(NetConfig::default()),
            ap_client: None,
            ap_client_joined: false,
            portal_client_away: false,
            scan_results: Vec::new(),
            stats: WifiStats::default(),
        }
    }

    /// The chip reset: the driver state is gone (the "air" is not).
    pub fn reset(&mut self) {
        let keep = (self.available, self.networks.clone(), self.base_mac, self.portal_forward, self.net_config.clone());
        let (abi, faults, away, dual) = (self.abi, self.net_faults.clone(), self.portal_client_away, self.dual_band);
        *self = WifiState::new(keep.2);
        self.dual_band = dual;
        self.set_abi(abi);
        self.portal_client_away = away;
        self.available = keep.0;
        self.networks = keep.1;
        self.portal_forward = keep.3;
        self.net_faults = faults;
        self.set_net_config(keep.4);
    }

    /// A dual-band radio (ESP32-C5): 5 GHz access points become visible, and the default
    /// environment gains **TRMNL-Sim-5G** (channel 36, any password).
    pub fn set_dual_band(&mut self) {
        self.dual_band = true;
        if !self.networks.iter().any(|a| a.channel >= 36) {
            self.networks.push(SimAp {
                ssid: "TRMNL-Sim-5G".into(),
                password: None,
                rssi: -48,
                channel: 36,
                authmode: 3,
                internet: true,
            });
        }
    }

    /// Whether the radio can see `ap` (5 GHz needs a dual-band chip).
    fn visible(&self, ap: &SimAp) -> bool {
        self.dual_band || ap.channel < 36
    }

    pub fn set_abi(&mut self, abi: WifiAbi) {
        self.abi = abi;
        self.sta_config = vec![0; abi.config_size];
        self.ap_config = vec![0; abi.config_size];
    }

    pub fn set_net_config(&mut self, cfg: NetConfig) {
        self.net = VirtualNet::new(cfg.clone());
        self.net.set_faults(self.net_faults.clone());
        self.net_config = cfg;
    }

    pub fn set_net_faults(&mut self, faults: NetFaults) {
        self.net.set_faults(faults.clone());
        self.net_faults = faults;
    }

    /// Guest time must track wall time: host-side network activity is in flight,
    /// or the setup portal is up (the device is waiting for a person/browser).
    pub fn net_busy(&self) -> bool {
        (self.connected.is_some() && self.net.busy()) || (self.ap_client.is_some() && !self.portal_client_away)
    }

    /// The host's portal client joins the soft-AP whenever it runs (true, the default) or
    /// stays away (false; leaving now if it had joined).
    pub fn set_portal_client(&mut self, on: bool, now: u64) {
        self.portal_client_away = !on;
        if !on && self.ap_client_joined {
            self.ap_client_joined = false;
            let mut d = vec![0u8; 12]; // mac, aid (IDF 5: is_mesh_child, reason)
            d[..6].copy_from_slice(&[0x02, 0, 0, 0, 0, 0x99]);
            d[6] = 1;
            self.post(now + 100 * MS, EV_AP_STADISCONNECTED, d);
        }
    }

    pub fn is_connected(&self) -> bool {
        self.connected.is_some()
    }

    pub fn ip(&self) -> Option<Ipv4Addr> {
        self.connected.and(self.net.guest_lease())
    }

    /// Host URL of the captive portal, once the host "client" has joined the AP and has a lease.
    pub fn portal_url(&self) -> Option<String> {
        self.ap_client.as_ref().filter(|c| !self.portal_client_away && c.client_ip().is_some()).map(|c| {
            let addr = c.listen_addrs().first().copied().unwrap_or(self.portal_forward);
            format!("http://{addr}/")
        })
    }

    fn sta_mac(&self) -> [u8; 6] {
        self.base_mac
    }

    fn ap_mac(&self) -> [u8; 6] {
        let mut m = self.base_mac;
        m[5] = m[5].wrapping_add(1);
        m
    }

    fn post(&mut self, at_ns: u64, id: u32, data: Vec<u8>) {
        self.events.push(PendingEvent { at_ns, id, data });
    }

    fn sta_ssid(&self) -> String {
        let s = &self.sta_config[..32];
        let n = s.iter().position(|&b| b == 0).unwrap_or(32);
        String::from_utf8_lossy(&s[..n]).into_owned()
    }

    fn sta_password(&self) -> String {
        let s = &self.sta_config[32..96];
        let n = s.iter().position(|&b| b == 0).unwrap_or(64);
        String::from_utf8_lossy(&s[..n]).into_owned()
    }

    fn bssid(i: usize) -> [u8; 6] {
        [0x02, 0x5e, 0x51, 0x00, 0x00, i as u8 + 1]
    }

    fn disconnected_event(&self, ap: Option<&SimAp>, reason: u8) -> Vec<u8> {
        let mut d = vec![0u8; 41];
        let ssid = ap.map(|a| a.ssid.clone()).unwrap_or_else(|| self.sta_ssid());
        let n = ssid.len().min(32);
        d[..n].copy_from_slice(&ssid.as_bytes()[..n]);
        d[32] = n as u8;
        d[39] = reason;
        d[40] = ap.map(|a| a.rssi as u8).unwrap_or(0);
        d
    }

    fn connected_event(&self, ap: &SimAp, i: usize) -> Vec<u8> {
        let mut d = vec![0u8; self.abi.connected_event_size];
        let n = ap.ssid.len().min(32);
        d[..n].copy_from_slice(&ap.ssid.as_bytes()[..n]);
        d[32] = n as u8;
        d[33..39].copy_from_slice(&Self::bssid(i));
        d[39] = ap.channel;
        d[40..44].copy_from_slice(&ap.authmode.to_le_bytes());
        d
    }

    fn ap_record(&self, ap: &SimAp, i: usize) -> Vec<u8> {
        let mut r = vec![0u8; self.abi.ap_record_size];
        r[..6].copy_from_slice(&Self::bssid(i));
        let n = ap.ssid.len().min(32);
        r[6..6 + n].copy_from_slice(&ap.ssid.as_bytes()[..n]);
        r[39] = ap.channel;
        r[44] = ap.rssi as u8;
        r[48..52].copy_from_slice(&ap.authmode.to_le_bytes());
        r[52..56].copy_from_slice(&4u32.to_le_bytes()); // CCMP
        r[56..60].copy_from_slice(&4u32.to_le_bytes());
        // 11b/g/n on 2.4 GHz; 11a/n/ac/ax on 5 GHz
        let phy: u32 = if ap.channel >= 36 { 1 << 2 | 1 << 4 | 1 << 5 | 1 << 6 } else { 0b111 };
        r[64..68].copy_from_slice(&phy.to_le_bytes());
        r[68..70].copy_from_slice(b"01");
        r[71] = 1;
        r[72] = 13;
        r[73] = 20;
        r
    }

    fn start_connect(&mut self, now: u64) {
        let ssid = self.sta_ssid();
        let pw = self.sta_password();
        let found = if self.available {
            self.networks.iter().position(|a| a.ssid == ssid && self.visible(a))
        } else {
            None
        };
        self.connecting = true;
        match found {
            Some(i) => {
                let ap = self.networks[i].clone();
                if ap.password.as_ref().is_some_and(|p| *p != pw) {
                    let d = self.disconnected_event(Some(&ap), REASON_HANDSHAKE_TIMEOUT);
                    self.post(now + 1500 * MS, EV_STA_DISCONNECTED, d);
                } else {
                    self.post(now + 400 * MS, EV_STA_CONNECTED, self.connected_event(&ap, i));
                }
            }
            None => {
                let d = self.disconnected_event(None, REASON_NO_AP_FOUND);
                self.post(now + 2500 * MS, EV_STA_DISCONNECTED, d);
            }
        }
    }

    /// Called when the front-end toggles the access point.
    pub fn set_available(&mut self, on: bool, now: u64) {
        self.available = on;
        if !on && let Some(i) = self.connected.take() {
            let ap = self.networks[i].clone();
            let d = self.disconnected_event(Some(&ap), REASON_BEACON_TIMEOUT);
            self.post(now + 100 * MS, EV_STA_DISCONNECTED, d);
            self.net.reset();
        }
    }

    /// Replace the access points in range. An association with an AP that is gone (or
    /// whose password changed) drops, as when it goes out of range.
    pub fn set_networks(&mut self, networks: &[sim_api::WifiNetwork], now: u64) {
        let current = self.connected.map(|i| self.networks[i].clone());
        self.networks = networks
            .iter()
            .map(|n| SimAp {
                ssid: n.ssid.clone(),
                password: n.password.clone(),
                rssi: n.rssi,
                channel: n.channel,
                authmode: if n.open { 0 } else { 3 },
                internet: n.internet,
            })
            .collect();
        if let Some(ap) = current {
            let same = |a: &SimAp| a.ssid == ap.ssid && a.password == ap.password && a.authmode == ap.authmode;
            self.connected = self.networks.iter().position(same);
            if self.connected.is_none() {
                let d = self.disconnected_event(Some(&ap), REASON_BEACON_TIMEOUT);
                self.post(now + 100 * MS, EV_STA_DISCONNECTED, d);
                self.net.reset();
            }
        }
    }

    /// Frames lwIP sent on an interface.
    fn tx(&mut self, ifx: usize, frame: &[u8]) {
        self.stats.tx_frames += 1;
        log::trace!(target: "wifi", "tx if{ifx} {} bytes", frame.len());
        match ifx {
            0 if self.connected.is_some_and(|i| self.networks[i].internet) => {
                self.net.from_guest(frame);
                self.pump();
            }
            1 => {
                if let Some(c) = &mut self.ap_client {
                    c.from_guest(frame);
                }
                self.pump();
            }
            _ => {}
        }
    }

    /// Move frames from the network simulators to the rx queue.
    fn pump(&mut self) {
        if self.connected.is_some() {
            for f in self.net.poll() {
                self.rx.push_back((0, f));
            }
        }
        if let Some(c) = &mut self.ap_client {
            for f in c.poll() {
                self.rx.push_back((1, f));
            }
        }
    }

    fn take_due_event(&mut self, now: u64) -> Option<PendingEvent> {
        // Earliest due event; ties keep posting order.
        let i = self.events.iter().enumerate().filter(|(_, e)| e.at_ns <= now).min_by_key(|(i, e)| (e.at_ns, *i))?.0;
        let e = self.events.remove(i);
        // Driver state follows the events as they are delivered.
        match e.id {
            EV_STA_CONNECTED => {
                let ssid_len = e.data[32] as usize;
                let ssid = String::from_utf8_lossy(&e.data[..ssid_len]).into_owned();
                self.connected = self.networks.iter().position(|a| a.ssid == ssid);
                self.connecting = false;
                self.net.reset();
            }
            EV_STA_DISCONNECTED => {
                self.connected = None;
                self.connecting = false;
            }
            EV_AP_START => {
                match ApClient::new([0x02, 0, 0, 0, 0, 0x99], vec![(self.portal_forward, 80)])
                    .or_else(|_| ApClient::new([0x02, 0, 0, 0, 0, 0x99], vec![("127.0.0.1:0".parse().unwrap(), 80)]))
                {
                    Ok(c) => self.ap_client = Some(c),
                    Err(err) => log::error!("captive portal forward: {err}"),
                }
                self.ap_client_joined = false;
            }
            EV_AP_STOP => {
                self.ap_client = None;
                self.ap_client_joined = false;
                self.events.retain(|e| e.id != EV_AP_STACONNECTED);
            }
            _ => {}
        }
        Some(e)
    }
}

// ---- installation ------------------------------------------------------------------------------

/// IDF-source glue that must run for real (it wires esp_netif to the driver).
const GLUE: &[&str] = &[
    "esp_wifi_set_default_wifi_sta_handlers",
    "esp_wifi_set_default_wifi_ap_handlers",
    "esp_wifi_create_if_driver",
    "esp_wifi_destroy_if_driver",
    "esp_wifi_register_if_rxcb",
    "esp_wifi_get_if_mac",
    "esp_wifi_is_if_ready_when_started",
    "esp_wifi_power_domain_on",
    "esp_wifi_power_domain_off",
];

pub fn install(hooks: &mut Hooks, syms: &Symbols) {
    // Everything else with the esp_wifi_ prefix becomes a no-op returning ESP_OK...
    for name in syms.names_with_prefix("esp_wifi_") {
        if !GLUE.contains(&name.as_str()) {
            let leaked: &'static str = Box::leak(name.into_boxed_str());
            hooks.install(syms, leaked, stub_ok);
        }
    }
    // ...unless it has real behaviour here.
    let real: &[(&'static str, super::HookFn)] = &[
        ("esp_wifi_init", wifi_init),
        ("esp_wifi_set_mode", set_mode),
        ("esp_wifi_get_mode", get_mode),
        ("esp_wifi_start", start),
        ("esp_wifi_stop", stop),
        ("esp_wifi_set_config", set_config),
        ("esp_wifi_get_config", get_config),
        ("esp_wifi_connect", connect),
        ("esp_wifi_disconnect", disconnect),
        ("esp_wifi_scan_start", scan_start),
        ("esp_wifi_scan_get_ap_num", scan_get_ap_num),
        ("esp_wifi_scan_get_ap_records", scan_get_ap_records),
        ("esp_wifi_sta_get_ap_info", sta_get_ap_info),
        ("esp_wifi_ap_get_sta_list", ap_get_sta_list),
        ("esp_wifi_get_mac", get_mac),
        ("esp_wifi_get_channel", get_channel),
        ("esp_wifi_internal_reg_rxcb", reg_rxcb),
        ("esp_wifi_internal_tx", internal_tx),
        // Zero-copy variant (Arduino's prebuilt IDF 4.4 for the S3): the extra netstack
        // buffer only needs a reference when the driver queues the frame; we copy it now.
        ("esp_wifi_internal_tx_by_ref", internal_tx),
        ("esp_wifi_internal_free_rx_buffer", free_rx_buffer),
    ];
    for (name, f) in real {
        hooks.install(syms, name, *f);
    }
    hooks.trampoline(TASK_LOOP, "sim_wifi task", task_loop);
}

fn stub_ok(_c: &mut HleCtx) -> Flow {
    Flow::Return(Some(ESP_OK))
}

fn sym(c: &HleCtx, name: &str) -> u32 {
    c.syms.addr(name).unwrap_or_else(|| panic!("firmware lacks symbol {name}"))
}

/// Reserve `n` bytes on the guest stack, returning their address. Leaves 16
/// bytes of headroom: on Xtensa the words just below SP belong to the caller
/// (its spilled a0-a3).
fn stack_alloc(c: &mut HleCtx, n: u32) -> u32 {
    c.cpu.alloc_scratch(n)
}

// ---- driver API ----------------------------------------------------------------------------------

fn wifi_init(c: &mut HleCtx) -> Flow {
    if c.state.wifi.task_created {
        return Flow::Return(Some(ESP_OK));
    }
    c.state.wifi.task_created = true;
    // xTaskCreatePinnedToCore(TASK_LOOP, "sim_wifi", 4096, NULL, 23, NULL, tskNO_AFFINITY)
    let saved_sp = c.cpu.sp();
    let name = stack_alloc(c, 16);
    c.mem.write_bytes(name, b"sim_wifi\0");
    Flow::Call {
        func: sym(c, "xTaskCreatePinnedToCore"),
        args: vec![TASK_LOOP, name, 4096, 0, 23, 0, 0x7fff_ffff],
        then: Box::new(move |c, _| {
            c.cpu.set_sp(saved_sp);
            Flow::Return(Some(ESP_OK))
        }),
    }
}

fn set_mode(c: &mut HleCtx) -> Flow {
    let mode = c.cpu.arg(0);
    if mode > 3 {
        return Flow::Return(Some(ESP_ERR_INVALID_ARG));
    }
    let now = c.env.now_ns();
    c.env.console(&format!("wifi: esp_wifi_set_mode({mode})"));
    let w = &mut c.state.wifi;
    let old = w.mode;
    w.mode = mode;
    if w.started {
        // Interfaces coming and going while started generate start/stop events,
        // like the real driver restarting with the new mode.
        let (sta_was, sta_now, ap_was, ap_now) = (old & 1 != 0, mode & 1 != 0, old & 2 != 0, mode & 2 != 0);
        if sta_was && !sta_now {
            if let Some(i) = w.connected {
                let ap = w.networks[i].clone();
                let d = w.disconnected_event(Some(&ap), REASON_ASSOC_LEAVE);
                w.post(now, EV_STA_DISCONNECTED, d);
            }
            w.events.retain(|e| e.id != EV_STA_CONNECTED);
            w.post(now, EV_STA_STOP, vec![]);
        }
        if ap_was && !ap_now {
            w.post(now, EV_AP_STOP, vec![]);
        }
        if !sta_was && sta_now {
            w.post(now + MS, EV_STA_START, vec![]);
        }
        if !ap_was && ap_now {
            w.post(now + MS, EV_AP_START, vec![]);
        }
        if mode == 0 {
            w.started = false;
        }
    }
    Flow::Return(Some(ESP_OK))
}

fn get_mode(c: &mut HleCtx) -> Flow {
    let p = c.cpu.arg(0);
    let mode = c.state.wifi.mode;
    if p != 0 {
        c.mem.write_u32(p, mode);
    }
    Flow::Return(Some(ESP_OK))
}

fn start(c: &mut HleCtx) -> Flow {
    let now = c.env.now_ns();
    c.env.console("wifi: esp_wifi_start()");
    let w = &mut c.state.wifi;
    if !w.started {
        w.started = true;
        if w.mode & 1 != 0 {
            w.post(now + 5 * MS, EV_STA_START, vec![]);
        }
        if w.mode & 2 != 0 {
            w.post(now + 5 * MS, EV_AP_START, vec![]);
        }
    }
    Flow::Return(Some(ESP_OK))
}

/// Like the real driver, `esp_wifi_stop` posts its DISCONNECTED/STOP events before
/// returning (callers tear down their netifs right after).
fn stop(c: &mut HleCtx) -> Flow {
    c.env.console("wifi: esp_wifi_stop()");
    let w = &mut c.state.wifi;
    let mut events = Vec::new();
    if w.started {
        w.started = false;
        if let Some(i) = w.connected.take() {
            let ap = w.networks[i].clone();
            events.push((EV_STA_DISCONNECTED, w.disconnected_event(Some(&ap), REASON_ASSOC_LEAVE)));
        }
        if w.mode & 1 != 0 {
            events.push((EV_STA_STOP, vec![]));
        }
        if w.mode & 2 != 0 {
            events.push((EV_AP_STOP, vec![]));
            w.ap_client = None;
            w.ap_client_joined = false;
        }
        w.connecting = false;
        w.events.retain(|e| e.id != EV_STA_CONNECTED && e.id != EV_AP_STACONNECTED);
    }
    let posted = !events.is_empty();
    post_then(c, events.into(), move |c| {
        let delay = c.syms.addr("vTaskDelay").unwrap_or(0);
        if !posted || delay == 0 {
            return Flow::Return(Some(ESP_OK));
        }
        // The real call blocks while the driver task stops, and the (higher priority) event
        // task handles STA_STOP meanwhile; callers free their netifs right after returning.
        Flow::Call { func: delay, args: vec![STOP_SETTLE_TICKS], then: Box::new(|_, _| Flow::Return(Some(ESP_OK))) }
    })
}

/// FreeRTOS ticks `esp_wifi_stop` blocks for after posting its events (1 kHz tick).
const STOP_SETTLE_TICKS: u32 = 20;

/// Post events from inside a hook (each via a guest call to `esp_event_post`), then
/// continue with `done`.
fn post_then(
    c: &mut HleCtx,
    mut events: std::collections::VecDeque<(u32, Vec<u8>)>,
    done: impl FnOnce(&mut HleCtx) -> Flow + Send + 'static,
) -> Flow {
    let Some((id, data)) = events.pop_front() else {
        return done(c);
    };
    c.env.console(&format!("wifi: event WIFI_EVENT {id} (sync)"));
    let base = c.mem.read_u32(sym(c, "WIFI_EVENT")).unwrap_or(0);
    let saved_sp = c.cpu.sp();
    let len = data.len() as u32;
    let ptr = if len > 0 {
        let p = stack_alloc(c, len.max(16));
        c.mem.write_bytes(p, &data);
        p
    } else {
        0
    };
    Flow::Call {
        func: sym(c, "esp_event_post"),
        args: vec![base, id, ptr, len, 0xffff_ffff],
        then: Box::new(move |c, _| {
            c.cpu.set_sp(saved_sp);
            post_then(c, events, done)
        }),
    }
}

fn set_config(c: &mut HleCtx) -> Flow {
    let (ifx, p) = (c.cpu.arg(0), c.cpu.arg(1));
    let Some(data) = c.mem.read_bytes(p, c.state.wifi.abi.config_size) else {
        return Flow::Return(Some(ESP_ERR_INVALID_ARG));
    };
    let w = &mut c.state.wifi;
    match ifx {
        0 => w.sta_config.copy_from_slice(&data),
        1 => w.ap_config.copy_from_slice(&data),
        _ => return Flow::Return(Some(ESP_ERR_INVALID_ARG)),
    }
    Flow::Return(Some(ESP_OK))
}

fn get_config(c: &mut HleCtx) -> Flow {
    let (ifx, p) = (c.cpu.arg(0), c.cpu.arg(1));
    let data = match ifx {
        0 => c.state.wifi.sta_config.clone(),
        1 => c.state.wifi.ap_config.clone(),
        _ => return Flow::Return(Some(ESP_ERR_INVALID_ARG)),
    };
    c.mem.write_bytes(p, &data);
    Flow::Return(Some(ESP_OK))
}

fn connect(c: &mut HleCtx) -> Flow {
    let now = c.env.now_ns();
    let w = &mut c.state.wifi;
    if w.connected.is_none() && !w.connecting {
        let ssid = w.sta_ssid();
        c.env.console(&format!("wifi: connecting to \"{ssid}\""));
        w.start_connect(now);
    }
    Flow::Return(Some(ESP_OK))
}

fn disconnect(c: &mut HleCtx) -> Flow {
    let now = c.env.now_ns();
    let w = &mut c.state.wifi;
    w.events.retain(|e| e.id != EV_STA_CONNECTED);
    w.connecting = false;
    if let Some(i) = w.connected {
        let ap = w.networks[i].clone();
        let d = w.disconnected_event(Some(&ap), REASON_ASSOC_LEAVE);
        w.post(now + MS, EV_STA_DISCONNECTED, d);
    }
    Flow::Return(Some(ESP_OK))
}

fn scan_start(c: &mut HleCtx) -> Flow {
    let now = c.env.now_ns();
    let w = &mut c.state.wifi;
    w.scan_results = if w.available { w.networks.iter().filter(|a| w.visible(a)).cloned().collect() } else { vec![] };
    let mut d = vec![0u8; 8];
    d[4] = w.scan_results.len() as u8;
    w.post(now + 1200 * MS, EV_SCAN_DONE, d);
    Flow::Return(Some(ESP_OK))
}

fn scan_get_ap_num(c: &mut HleCtx) -> Flow {
    let p = c.cpu.arg(0);
    let n = c.state.wifi.scan_results.len() as u16;
    c.mem.write_bytes(p, &n.to_le_bytes());
    Flow::Return(Some(ESP_OK))
}

fn scan_get_ap_records(c: &mut HleCtx) -> Flow {
    let (pn, precs) = (c.cpu.arg(0), c.cpu.arg(1));
    let max = c.mem.read_bytes(pn, 2).map(|b| u16::from_le_bytes([b[0], b[1]])).unwrap_or(0) as usize;
    let results = std::mem::take(&mut c.state.wifi.scan_results);
    let n = results.len().min(max);
    for (i, ap) in results.iter().take(n).enumerate() {
        let idx = c.state.wifi.networks.iter().position(|a| a.ssid == ap.ssid).unwrap_or(i);
        let rec = c.state.wifi.ap_record(ap, idx);
        c.mem.write_bytes(precs + (i * rec.len()) as u32, &rec);
    }
    c.mem.write_bytes(pn, &(n as u16).to_le_bytes());
    Flow::Return(Some(ESP_OK))
}

fn sta_get_ap_info(c: &mut HleCtx) -> Flow {
    let p = c.cpu.arg(0);
    let w = &c.state.wifi;
    match w.connected {
        Some(i) => {
            let rec = w.ap_record(&w.networks[i], i);
            c.mem.write_bytes(p, &rec);
            Flow::Return(Some(ESP_OK))
        }
        None => Flow::Return(Some(ESP_ERR_WIFI_NOT_CONNECT)),
    }
}

fn ap_get_sta_list(c: &mut HleCtx) -> Flow {
    let p = c.cpu.arg(0);
    let n = c.state.wifi.abi.sta_list_entries;
    let mut list = vec![0u8; n * 12 + 4];
    if c.state.wifi.ap_client_joined {
        list[..6].copy_from_slice(&[0x02, 0, 0, 0, 0, 0x99]);
        list[6] = (-40i8) as u8;
        list[8] = 0b111;
        list[n * 12] = 1;
    }
    c.mem.write_bytes(p, &list);
    Flow::Return(Some(ESP_OK))
}

fn get_mac(c: &mut HleCtx) -> Flow {
    let (ifx, p) = (c.cpu.arg(0), c.cpu.arg(1));
    let mac = if ifx == 1 { c.state.wifi.ap_mac() } else { c.state.wifi.sta_mac() };
    c.mem.write_bytes(p, &mac);
    Flow::Return(Some(ESP_OK))
}

fn get_channel(c: &mut HleCtx) -> Flow {
    let (pch, psec) = (c.cpu.arg(0), c.cpu.arg(1));
    let w = &c.state.wifi;
    let ch = w.connected.map(|i| w.networks[i].channel).unwrap_or(1);
    c.mem.write_bytes(pch, &[ch]);
    if psec != 0 {
        c.mem.write_u32(psec, 0);
    }
    Flow::Return(Some(ESP_OK))
}

fn reg_rxcb(c: &mut HleCtx) -> Flow {
    let (ifx, f) = (c.cpu.arg(0) as usize, c.cpu.arg(1));
    if ifx < 2 {
        c.state.wifi.rxcb[ifx] = f;
    }
    Flow::Return(Some(ESP_OK))
}

fn internal_tx(c: &mut HleCtx) -> Flow {
    let (ifx, buf, len) = (c.cpu.arg(0) as usize, c.cpu.arg(1), c.cpu.arg(2) & 0xffff);
    if let Some(frame) = c.mem.read_bytes(buf, len as usize) {
        c.state.wifi.tx(ifx, &frame);
    }
    Flow::Return(Some(ESP_OK))
}

/// Our rx buffers come from the guest heap: release them with free().
fn free_rx_buffer(c: &mut HleCtx) -> Flow {
    let eb = c.cpu.arg(0);
    if eb == 0 {
        return Flow::Return(None);
    }
    Flow::Call { func: sym(c, "free"), args: vec![eb], then: Box::new(|_, _| Flow::Return(None)) }
}

// ---- the driver task -------------------------------------------------------------------------------

fn back_to_loop(c: &mut HleCtx, _ret: u32) -> Flow {
    c.cpu.set_pc(TASK_LOOP);
    Flow::Redirected
}

fn task_loop(c: &mut HleCtx) -> Flow {
    let now = c.env.now_ns();
    c.state.wifi.pump();

    // The host "browser client" joins the soft-AP shortly after it starts.
    let w = &mut c.state.wifi;
    if w.ap_client.is_some() && !w.ap_client_joined && !w.portal_client_away && w.mode & 2 != 0 {
        w.ap_client_joined = true;
        let mut d = vec![0u8; 8];
        d[..6].copy_from_slice(&[0x02, 0, 0, 0, 0, 0x99]);
        d[6] = 1;
        w.post(now + 1000 * MS, EV_AP_STACONNECTED, d);
    }

    if let Some(ev) = c.state.wifi.take_due_event(now) {
        return post_event(c, ev);
    }
    if let Some((ifx, frame)) = c.state.wifi.rx.pop_front() {
        let cb = c.state.wifi.rxcb[ifx];
        if cb != 0 {
            return deliver_frame(c, cb, frame);
        }
    }
    Flow::Call { func: sym(c, "vTaskDelay"), args: vec![1], then: Box::new(back_to_loop) }
}

fn post_event(c: &mut HleCtx, ev: PendingEvent) -> Flow {
    let base_ptr = sym(c, "WIFI_EVENT");
    let base = c.mem.read_u32(base_ptr).unwrap_or(0);
    let saved_sp = c.cpu.sp();
    let len = ev.data.len() as u32;
    let data = if len > 0 {
        let p = stack_alloc(c, len.max(16) + 16);
        c.mem.write_bytes(p, &ev.data);
        p
    } else {
        0
    };
    let name = match ev.id {
        EV_SCAN_DONE => "SCAN_DONE",
        EV_STA_START => "STA_START",
        EV_STA_STOP => "STA_STOP",
        EV_STA_CONNECTED => "STA_CONNECTED",
        EV_STA_DISCONNECTED => "STA_DISCONNECTED",
        EV_AP_START => "AP_START",
        EV_AP_STOP => "AP_STOP",
        EV_AP_STACONNECTED => "AP_STACONNECTED",
        EV_AP_STADISCONNECTED => "AP_STADISCONNECTED",
        _ => "?",
    };
    c.env.console(&format!("wifi: event WIFI_EVENT_{name}"));
    Flow::Call {
        func: sym(c, "esp_event_post"),
        args: vec![base, ev.id, data, len, 0xffff_ffff],
        then: Box::new(move |c, _| {
            c.cpu.set_sp(saved_sp);
            c.cpu.set_pc(TASK_LOOP);
            Flow::Redirected
        }),
    }
}

fn deliver_frame(c: &mut HleCtx, cb: u32, frame: Vec<u8>) -> Flow {
    let len = frame.len() as u32;
    Flow::Call {
        func: sym(c, "malloc"),
        args: vec![len],
        then: Box::new(move |c, buf| {
            if buf == 0 {
                log::warn!("wifi: guest out of memory, dropping rx frame");
                c.cpu.set_pc(TASK_LOOP);
                return Flow::Redirected;
            }
            c.mem.write_bytes(buf, &frame);
            c.state.wifi.stats.rx_frames += 1;
            Flow::Call { func: cb, args: vec![buf, len, buf], then: Box::new(back_to_loop) }
        }),
    }
}
