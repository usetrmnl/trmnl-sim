//! A Realtek RTL8153 USB Ethernet adapter (VID 0x0BDA, PID 0x8153), as the TRMNL X's CDC-ECM
//! driver (components_usb_host/cdc_ecm_host) uses it: configuration 1 is Realtek's vendor
//! mode, configuration 2 is a standard CDC-ECM function (USB CDC ECM 1.2): a communications
//! interface with the Ethernet functional descriptor and a notification endpoint (0x83), and
//! a data interface whose alternate setting 1 has the bulk endpoints (0x81 IN, 0x02 OUT). The
//! S3's port is full speed, so bulk endpoints have 64-byte packets.
//!
//! Frames go to and come from a [`vnet::VirtualNet`] of their own: the same router, DNS, NTP
//! and host forwarding as the WiFi network, with the device at 10.0.2.16 (WiFi: 10.0.2.15),
//! like a LAN with a wired and a wireless side. Without link, the adapter reports the cable
//! out (NETWORK_CONNECTION 0) and passes nothing; with DHCP off, the router doesn't answer the
//! device's DHCP requests (it gets link, never an address).
//!
//! Only what CDC-ECM needs is modelled: the vendor configuration's endpoints NAK, and the
//! vendor register requests (0x40/0xC0) STALL.

use std::collections::VecDeque;
use std::net::Ipv4Addr;

use vnet::{NetConfig, NetFaults, VirtualNet};

use super::usb::{Reply, Setup, UsbDevice};

pub const VID: u16 = 0x0BDA;
pub const PID: u16 = 0x8153;
/// The device's address on the virtual LAN (WiFi gets 10.0.2.15 from its own router).
pub const GUEST_IP: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 16);
/// Full-speed bulk and control packet size.
const MPS: usize = 64;
const NOTIFY_MPS: u8 = 16;
/// wMaxSegmentSize: an Ethernet II frame without its FCS.
const MAX_SEGMENT: usize = 1514;
const EP_BULK_IN: u8 = 1;
const EP_BULK_OUT: u8 = 2;
const EP_NOTIFY: u8 = 3;
const STR_MANUFACTURER: u8 = 1;
const STR_PRODUCT: u8 = 2;
const STR_MAC: u8 = 3;
const STR_SERIAL: u8 = 6;
/// Frames waiting for the host to poll for them; more are dropped, as a full FIFO does.
const RX_QUEUE: usize = 256;
/// Reported link speed (CONNECTION_SPEED_CHANGE), both directions.
const LINK_BPS: u32 = 1_000_000_000;

/// The cable and the network behind it, as the control API sets them.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Link {
    pub up: bool,
    /// The router answers DHCP.
    pub dhcp: bool,
}

pub struct UsbEthernet {
    link: Link,
    /// The adapter's own MAC (iMACAddress); the firmware gives its netif the ESP32's ETH MAC.
    mac: [u8; 6],
    address: u8,
    configuration: u8,
    /// Alternate setting of the data interface (1: the bulk endpoints are live).
    data_alt: u8,
    notifications: VecDeque<Vec<u8>>,
    /// Frames from the network to the device.
    rx: VecDeque<Vec<u8>>,
    /// A frame the host is still sending (packets so far, no short packet yet).
    tx: Vec<u8>,
    net: VirtualNet,
    messages: Vec<String>,
    warned_no_zlp: bool,
    pub tx_frames: u64,
    pub rx_frames: u64,
}

impl UsbEthernet {
    /// `net`: the WiFi network's configuration, reused for the wired side (with its own
    /// device address).
    pub fn new(link: Link, net: &NetConfig, faults: NetFaults, base_mac: [u8; 6]) -> Self {
        let cfg = NetConfig { guest_ip: GUEST_IP, ..net.clone() };
        let mut vn = VirtualNet::new(cfg);
        vn.set_faults(faults);
        UsbEthernet {
            link,
            mac: [0x00, 0xE0, 0x4C, base_mac[3], base_mac[4], base_mac[5]],
            address: 0,
            configuration: 0,
            data_alt: 0,
            notifications: VecDeque::new(),
            rx: VecDeque::new(),
            tx: Vec::new(),
            net: vn,
            messages: Vec::new(),
            warned_no_zlp: false,
            tx_frames: 0,
            rx_frames: 0,
        }
    }

    pub fn link(&self) -> Link {
        self.link
    }

    /// Plug the cable in or out, or change the DHCP server's behaviour. A cable change is
    /// notified to the host (NETWORK_CONNECTION) and drops the network's state.
    pub fn set_link(&mut self, link: Link) {
        let changed = link.up != self.link.up;
        self.link = link;
        if changed {
            self.net.reset();
            self.rx.clear();
            self.tx.clear();
            if self.active() {
                self.notify_link();
            }
        }
    }

    pub fn set_net_faults(&mut self, faults: NetFaults) {
        self.net.set_faults(faults);
    }

    /// The host selected the CDC-ECM configuration and the data interface's bulk endpoints.
    pub fn active(&self) -> bool {
        self.configuration == 2 && self.data_alt == 1
    }

    /// The address the router's DHCP server gave the device.
    pub fn lease(&self) -> Option<Ipv4Addr> {
        self.net.guest_lease()
    }

    /// NETWORK_CONNECTION, then (with link) CONNECTION_SPEED_CHANGE, as CDC-ECM devices send
    /// when their data interface is enabled and when the cable changes (ECM 1.2 §6.3).
    fn notify_link(&mut self) {
        self.notifications.push_back(vec![0xA1, 0x00, self.link.up as u8, 0, 0, 0, 0, 0]);
        if self.link.up {
            let mut n = vec![0xA1, 0x2A, 0, 0, 0, 0, 8, 0];
            n.extend_from_slice(&LINK_BPS.to_le_bytes());
            n.extend_from_slice(&LINK_BPS.to_le_bytes());
            self.notifications.push_back(n);
        }
    }

    /// A whole frame from the host.
    fn frame_out(&mut self, frame: &[u8]) {
        if !self.link.up || frame.is_empty() {
            return;
        }
        if frame.len() > MAX_SEGMENT {
            self.messages.push(format!(
                "USB Ethernet: dropped a {}-byte frame from the device (over wMaxSegmentSize {MAX_SEGMENT})",
                frame.len()
            ));
            return;
        }
        if !self.link.dhcp && is_dhcp_request(frame) {
            return;
        }
        self.tx_frames += 1;
        self.net.from_guest(frame);
        self.pump();
    }

    /// Frames the network sent the device.
    fn pump(&mut self) {
        if !self.link.up || !self.active() {
            return;
        }
        for f in self.net.poll() {
            if self.rx.len() < RX_QUEUE {
                self.rx.push_back(f);
            }
        }
    }

    fn string(&self, index: u8) -> Option<String> {
        Some(match index {
            STR_MANUFACTURER => "Realtek".into(),
            STR_PRODUCT => "USB 10/100/1000 LAN".into(),
            STR_MAC => self.mac.iter().map(|b| format!("{b:02X}")).collect(),
            STR_SERIAL => "000001".into(),
            _ => return None,
        })
    }

    fn descriptor(&self, setup: &Setup) -> Option<Vec<u8>> {
        let [index, kind] = setup.value.to_le_bytes();
        let d = match kind {
            1 => device_descriptor(),
            2 => match index {
                0 => vendor_configuration(),
                1 => ecm_configuration(),
                _ => return None,
            },
            3 if index == 0 => vec![4, 3, 0x09, 0x04],
            3 => {
                let s = self.string(index)?;
                let mut d = vec![0, 3];
                for c in s.encode_utf16() {
                    d.extend_from_slice(&c.to_le_bytes());
                }
                d[0] = d.len() as u8;
                d
            }
            _ => return None,
        };
        Some(d)
    }
}

/// UDP from the DHCP client port to the server port.
fn is_dhcp_request(frame: &[u8]) -> bool {
    frame.len() >= 14 + 20 + 8 && frame[12..14] == [0x08, 0x00] && frame[14 + 9] == 17 && {
        let ihl = (frame[14] & 0x0f) as usize * 4;
        let udp = 14 + ihl;
        frame.len() >= udp + 4 && frame[udp + 2..udp + 4] == [0, 67]
    }
}

#[rustfmt::skip]
fn device_descriptor() -> Vec<u8> {
    let [v0, v1] = VID.to_le_bytes();
    let [p0, p1] = PID.to_le_bytes();
    vec![
        18, 1, 0x10, 0x02,                          // bcdUSB 2.10
        0, 0, 0, MPS as u8,                         // class per interface, EP0 64 bytes
        v0, v1, p0, p1, 0x00, 0x31,                 // bcdDevice 31.00
        STR_MANUFACTURER, STR_PRODUCT, STR_SERIAL, 2, // two configurations
    ]
}

fn endpoint(addr: u8, attributes: u8, mps: u8, interval: u8) -> [u8; 7] {
    [7, 5, addr, attributes, mps, 0, interval]
}

fn configuration(value: u8, interfaces: u8, body: &[u8]) -> Vec<u8> {
    let total = (9 + body.len()) as u16;
    let [t0, t1] = total.to_le_bytes();
    let mut d = vec![9, 2, t0, t1, interfaces, value, 0, 0xA0, 0x32];
    d.extend_from_slice(body);
    d
}

/// Configuration 1: Realtek's vendor-specific interface (what r8152 drives on Linux).
fn vendor_configuration() -> Vec<u8> {
    let mut b = vec![9, 4, 0, 0, 3, 0xFF, 0xFF, 0x00, 0];
    b.extend(endpoint(0x80 | EP_BULK_IN, 2, MPS as u8, 0));
    b.extend(endpoint(EP_BULK_OUT, 2, MPS as u8, 0));
    b.extend(endpoint(0x80 | EP_NOTIFY, 3, 2, 8));
    configuration(1, 1, &b)
}

/// Configuration 2: CDC-ECM.
fn ecm_configuration() -> Vec<u8> {
    let [s0, s1] = (MAX_SEGMENT as u16).to_le_bytes();
    let mut b = vec![9, 4, 0, 0, 1, 0x02, 0x06, 0x00, 0]; // communications, ECM
    b.extend([5, 0x24, 0x00, 0x10, 0x01]); // header, bcdCDC 1.10
    b.extend([5, 0x24, 0x06, 0, 1]); // union: control 0, data 1
    b.extend([13, 0x24, 0x0F, STR_MAC, 0, 0, 0, 0, s0, s1, 0, 0, 0]); // Ethernet networking
    b.extend(endpoint(0x80 | EP_NOTIFY, 3, NOTIFY_MPS, 8));
    b.extend([9, 4, 1, 0, 0, 0x0A, 0, 0, 0]); // data interface, no endpoints
    b.extend([9, 4, 1, 1, 2, 0x0A, 0, 0, 0]); // ... and with its bulk endpoints
    b.extend(endpoint(0x80 | EP_BULK_IN, 2, MPS as u8, 0));
    b.extend(endpoint(EP_BULK_OUT, 2, MPS as u8, 0));
    configuration(2, 2, &b)
}

impl UsbDevice for UsbEthernet {
    fn bus_reset(&mut self) {
        self.address = 0;
        self.configuration = 0;
        self.data_alt = 0;
        self.notifications.clear();
        self.rx.clear();
        self.tx.clear();
        self.net.reset();
    }

    fn address(&self) -> u8 {
        self.address
    }

    fn control(&mut self, setup: &Setup, data: &[u8]) -> Option<Vec<u8>> {
        let len = setup.length as usize;
        let answer = match (setup.request_type, setup.request) {
            (0x80, 0x00) | (0x81, 0x00) | (0x82, 0x00) => vec![0, 0], // GET_STATUS
            (0x00..=0x02, 0x01 | 0x03) => vec![],                     // CLEAR/SET_FEATURE
            (0x00, 0x05) => {
                self.address = (setup.value & 0x7f) as u8;
                vec![]
            }
            (0x80, 0x06) | (0x81, 0x06) => self.descriptor(setup)?,
            (0x80, 0x08) => vec![self.configuration],
            (0x00, 0x09) => {
                let value = setup.value as u8;
                if value > 2 {
                    return None;
                }
                if value != self.configuration {
                    self.configuration = value;
                    self.data_alt = 0;
                    self.notifications.clear();
                    self.rx.clear();
                    self.tx.clear();
                }
                vec![]
            }
            (0x81, 0x0A) if self.configuration == 2 && setup.index <= 1 => {
                vec![if setup.index == 1 { self.data_alt } else { 0 }]
            }
            (0x01, 0x0B) if self.configuration == 2 => match (setup.index, setup.value) {
                (0, 0) => vec![],
                (1, alt @ (0 | 1)) => {
                    let was = self.data_alt;
                    self.data_alt = alt as u8;
                    if alt == 1 && was != 1 {
                        self.messages.push(format!(
                            "USB Ethernet: CDC-ECM data interface enabled ({})",
                            if self.link.up { "link up" } else { "no link" }
                        ));
                        self.notify_link();
                    } else if alt == 0 {
                        self.rx.clear();
                        self.tx.clear();
                    }
                    vec![]
                }
                _ => return None,
            },
            // CDC-ECM class requests to the communications interface: packet, multicast and
            // power filters (accepted, the adapter passes every frame).
            (0x21, 0x40..=0x43) if self.configuration == 2 && data.len() == len => vec![],
            _ => return None,
        };
        Some(answer.into_iter().take(len).collect())
    }

    fn data_in(&mut self, ep: u8, max: usize) -> Reply {
        if self.configuration != 2 {
            return Reply::Nak;
        }
        match ep {
            EP_NOTIFY => match self.notifications.front() {
                Some(n) if n.len() <= max => Reply::Data(self.notifications.pop_front().unwrap()),
                Some(_) => Reply::Stall,
                None => Reply::Nak,
            },
            EP_BULK_IN if self.data_alt == 1 => {
                self.pump();
                match self.rx.pop_front() {
                    Some(mut f) => {
                        f.truncate(max);
                        self.rx_frames += 1;
                        Reply::Data(f)
                    }
                    None => Reply::Nak,
                }
            }
            _ => Reply::Nak,
        }
    }

    fn data_out(&mut self, ep: u8, data: &[u8], ends: bool) -> Reply {
        if self.configuration != 2 || ep != EP_BULK_OUT {
            return Reply::Data(Vec::new());
        }
        if self.data_alt != 1 {
            // The data interface is off: a real adapter's FIFO takes and drops it.
            return Reply::Data(Vec::new());
        }
        self.tx.extend_from_slice(data);
        if ends {
            let frame = std::mem::take(&mut self.tx);
            self.frame_out(&frame);
        } else if !self.warned_no_zlp && !self.tx.is_empty() {
            // CDC-ECM 1.2 §3.3.1: a frame that is a multiple of the packet size must end with
            // a zero-length packet, or the adapter takes the next transfer as more of it.
            self.warned_no_zlp = true;
            self.messages.push(format!(
                "USB Ethernet: a {}-byte frame ended without a zero-length packet; the adapter joins it to the next one",
                self.tx.len()
            ));
        }
        Reply::Data(Vec::new())
    }

    fn poll(&mut self, _now_ns: u64) {
        self.pump();
    }

    fn take_messages(&mut self) -> Vec<String> {
        std::mem::take(&mut self.messages)
    }

    fn net_busy(&self) -> bool {
        self.link.up && self.active() && self.net.busy()
    }

    fn ethernet(&mut self) -> Option<&mut UsbEthernet> {
        Some(self)
    }

    fn ethernet_ref(&self) -> Option<&UsbEthernet> {
        Some(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn adapter(link: Link) -> UsbEthernet {
        UsbEthernet::new(link, &NetConfig { offline: true, ..Default::default() }, NetFaults::default(), [0; 6])
    }

    fn req(request_type: u8, request: u8, value: u16, index: u16, length: u16) -> Setup {
        Setup { request_type, request, value, index, length }
    }

    /// Enumerate and select CDC-ECM as the firmware does (enum.c, then cdc_ecm_host_open).
    fn configure(d: &mut UsbEthernet) {
        d.control(&req(0x00, 0x05, 1, 0, 0), &[]).unwrap();
        d.control(&req(0x00, 0x09, 1, 0, 0), &[]).unwrap();
        d.control(&req(0x00, 0x09, 2, 0, 0), &[]).unwrap();
        d.control(&req(0x01, 0x0B, 1, 1, 0), &[]).unwrap();
    }

    #[test]
    fn descriptors_are_what_the_cdc_ecm_driver_parses() {
        let mut d = adapter(Link { up: true, dhcp: true });
        let dev = d.control(&req(0x80, 0x06, 0x0100, 0, 18), &[]).unwrap();
        assert_eq!((dev.len(), dev[17]), (18, 2));
        assert_eq!(u16::from_le_bytes([dev[8], dev[9]]), 0x0BDA);
        // A short read gets the first bytes (enum.c reads 8, then the whole descriptor).
        assert_eq!(d.control(&req(0x80, 0x06, 0x0100, 0, 8), &[]).unwrap(), dev[..8]);
        let vendor = d.control(&req(0x80, 0x06, 0x0200, 0, 255), &[]).unwrap();
        assert_eq!(vendor[14], 0xFF, "configuration 1's interface is vendor-specific");
        let ecm = d.control(&req(0x80, 0x06, 0x0201, 0, 255), &[]).unwrap();
        assert_eq!(u16::from_le_bytes([ecm[2], ecm[3]]) as usize, ecm.len());
        // The Ethernet functional descriptor names the MAC string; the OUT endpoint is 0x02
        // (cdc_ecm_host_data_tx_blocking insists on it).
        let eth = ecm.windows(3).position(|w| w == [13, 0x24, 0x0F]).unwrap();
        assert_eq!(ecm[eth + 3], STR_MAC);
        assert!(ecm.windows(4).any(|w| w == [7, 5, 0x02, 2]));
        let mac = d.control(&req(0x80, 0x06, 0x0300 | STR_MAC as u16, 0x0409, 32), &[]).unwrap();
        assert_eq!(mac.len(), 2 + 24);
        assert_eq!(d.control(&req(0x80, 0x06, 0x0309, 0x0409, 32), &[]), None);
    }

    #[test]
    fn enabling_the_data_interface_reports_the_link() {
        let mut d = adapter(Link { up: true, dhcp: true });
        configure(&mut d);
        assert!(d.active());
        match d.data_in(EP_NOTIFY, 16) {
            Reply::Data(n) => assert_eq!(n, [0xA1, 0x00, 1, 0, 0, 0, 0, 0]),
            r => panic!("{r:?}"),
        }
        match d.data_in(EP_NOTIFY, 16) {
            Reply::Data(n) => assert_eq!((n.len(), n[1]), (16, 0x2A)),
            r => panic!("{r:?}"),
        }
        assert_eq!(d.data_in(EP_NOTIFY, 16), Reply::Nak);
        d.set_link(Link { up: false, dhcp: true });
        assert_eq!(d.data_in(EP_NOTIFY, 16), Reply::Data(vec![0xA1, 0x00, 0, 0, 0, 0, 0, 0]));
    }

    #[test]
    fn a_dhcp_discover_gets_an_offer_unless_dhcp_is_off() {
        for dhcp in [true, false] {
            let mut d = adapter(Link { up: true, dhcp });
            configure(&mut d);
            let discover = dhcp_discover();
            // Sent in 64-byte packets: only the transfer's short packet ends the frame.
            let (whole, rest) = discover.split_at(discover.len() / MPS * MPS);
            assert!(!rest.is_empty());
            d.data_out(EP_BULK_OUT, whole, false);
            d.data_out(EP_BULK_OUT, rest, true);
            let got = (0..20).find_map(|_| match d.data_in(EP_BULK_IN, 1536) {
                Reply::Data(f) => Some(f),
                _ => {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                    None
                }
            });
            assert_eq!(got.is_some(), dhcp, "dhcp {dhcp}");
        }
    }

    fn dhcp_discover() -> Vec<u8> {
        let mac = [0x02, 0x11, 0x22, 0x33, 0x44, 0x55];
        let mut dhcp = vec![0u8; 240];
        dhcp[0] = 1;
        dhcp[1] = 1;
        dhcp[2] = 6;
        dhcp[4..8].copy_from_slice(&[1, 2, 3, 4]);
        dhcp[28..34].copy_from_slice(&mac);
        dhcp[236..240].copy_from_slice(&[99, 130, 83, 99]);
        dhcp.extend([53, 1, 1, 255]);
        let udp_len = 8 + dhcp.len();
        let mut udp = vec![0, 68, 0, 67];
        udp.extend((udp_len as u16).to_be_bytes());
        udp.extend([0, 0]);
        udp.extend(dhcp);
        let total = 20 + udp.len();
        let mut ip =
            vec![0x45, 0, (total >> 8) as u8, total as u8, 0, 0, 0, 0, 64, 17, 0, 0, 0, 0, 0, 0, 255, 255, 255, 255];
        ip.extend(udp);
        let mut f = vec![0xff; 6];
        f.extend(mac);
        f.extend([0x08, 0x00]);
        f.extend(ip);
        f
    }
}
