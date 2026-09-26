//! `ApClient`: the host acting as a WiFi station on the guest's SoftAP.

use std::io::ErrorKind;
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::time::{Duration, Instant};

use smoltcp::iface::{Config, Interface, SocketHandle, SocketSet};
use smoltcp::phy::Medium;
use smoltcp::socket::{dhcpv4, tcp};
use smoltcp::wire::{EthernetAddress, HardwareAddress, IpAddress, IpCidr, Ipv4Cidr};

use crate::bridge::{Bridge, PumpResult, new_tcp_socket};
use crate::device::QueueDevice;

/// Host connections accepted before DHCP completed are dropped after this long.
const PENDING_TIMEOUT: Duration = Duration::from_secs(30);

struct Forward {
    listener: TcpListener,
    guest_port: u16,
}

struct Conn {
    handle: SocketHandle,
    bridge: Bridge,
}

/// The host as a WiFi client station of the guest's SoftAP; forwards host TCP listeners
/// to ports on the guest AP address.
pub struct ApClient {
    start: Instant,
    mac: [u8; 6],
    device: QueueDevice,
    iface: Interface,
    sockets: SocketSet<'static>,
    dhcp: Option<SocketHandle>,
    /// Static configuration (client cidr, guest AP ip) used instead of DHCP.
    static_cfg: Option<(Ipv4Cidr, Ipv4Addr)>,
    client: Option<Ipv4Cidr>,
    ap_ip: Option<Ipv4Addr>,
    forwards: Vec<Forward>,
    pending: Vec<(TcpStream, u16, Instant)>,
    conns: Vec<Conn>,
    next_port: u16,
}

impl ApClient {
    /// Create a client that obtains its address via DHCP from the guest AP.
    pub fn new(client_mac: [u8; 6], forwards: Vec<(SocketAddr, u16)>) -> std::io::Result<Self> {
        Self::build(client_mac, forwards, None)
    }

    /// Like [`ApClient::new`] but skip DHCP: use `client_ip/prefix` and assume the AP is at
    /// `guest_ap_ip`. Mainly for tests.
    pub fn with_static_ip(
        client_mac: [u8; 6],
        forwards: Vec<(SocketAddr, u16)>,
        client_ip: Ipv4Addr,
        prefix_len: u8,
        guest_ap_ip: Ipv4Addr,
    ) -> std::io::Result<Self> {
        Self::build(client_mac, forwards, Some((Ipv4Cidr::new(client_ip, prefix_len), guest_ap_ip)))
    }

    fn build(
        mac: [u8; 6],
        forwards: Vec<(SocketAddr, u16)>,
        static_cfg: Option<(Ipv4Cidr, Ipv4Addr)>,
    ) -> std::io::Result<Self> {
        let mut fw = Vec::new();
        for (addr, guest_port) in forwards {
            let listener = TcpListener::bind(addr)?;
            listener.set_nonblocking(true)?;
            fw.push(Forward { listener, guest_port });
        }
        let start = Instant::now();
        let (device, iface) = Self::make_iface(mac);
        let seed = start.elapsed().as_nanos() as u16;
        let mut me = Self {
            start,
            mac,
            device,
            iface,
            sockets: SocketSet::new(vec![]),
            dhcp: None,
            static_cfg,
            client: None,
            ap_ip: None,
            forwards: fw,
            pending: Vec::new(),
            conns: Vec::new(),
            next_port: 49152 + (seed % 8192),
        };
        me.init_addressing();
        Ok(me)
    }

    fn make_iface(mac: [u8; 6]) -> (QueueDevice, Interface) {
        let mut device = QueueDevice::new(Medium::Ethernet, 1514);
        let mut config = Config::new(HardwareAddress::Ethernet(EthernetAddress(mac)));
        config.random_seed = u64::from_le_bytes([mac[0], mac[1], mac[2], mac[3], mac[4], mac[5], 0x42, 0x17])
            ^ Instant::now().elapsed().as_nanos() as u64;
        let iface = Interface::new(config, &mut device, smoltcp::time::Instant::ZERO);
        (device, iface)
    }

    fn init_addressing(&mut self) {
        if let Some((cidr, ap)) = self.static_cfg {
            self.set_address(cidr, ap);
        } else {
            let mut dhcp = dhcpv4::Socket::new();
            let mut retry = dhcp.get_retry_config();
            retry.discover_timeout = smoltcp::time::Duration::from_secs(2);
            retry.initial_request_timeout = smoltcp::time::Duration::from_secs(2);
            dhcp.set_retry_config(retry);
            self.dhcp = Some(self.sockets.add(dhcp));
        }
    }

    fn set_address(&mut self, cidr: Ipv4Cidr, ap: Ipv4Addr) {
        self.iface.update_ip_addrs(|a| {
            a.clear();
            let _ = a.push(IpCidr::Ipv4(cidr));
        });
        self.client = Some(cidr);
        self.ap_ip = Some(ap);
    }

    fn clear_address(&mut self) {
        self.iface.update_ip_addrs(|a| a.clear());
        self.client = None;
        self.ap_ip = None;
        self.abort_conns();
    }

    fn abort_conns(&mut self) {
        for c in self.conns.drain(..) {
            self.sockets.remove(c.handle);
        }
    }

    fn now(&self) -> smoltcp::time::Instant {
        smoltcp::time::Instant::from_micros(self.start.elapsed().as_micros() as i64)
    }

    /// Our address on the guest's AP network, once known.
    /// True while browser connections are being forwarded (or waiting to be).
    pub fn busy(&self) -> bool {
        !self.pending.is_empty() || !self.conns.is_empty()
    }

    pub fn client_ip(&self) -> Option<Ipv4Addr> {
        self.client.map(|c| c.address())
    }

    /// The guest AP's address (DHCP router option, else server identifier).
    pub fn guest_ap_ip(&self) -> Option<Ipv4Addr> {
        self.ap_ip
    }

    /// Actual local addresses of the host listeners (useful when binding to port 0).
    pub fn listen_addrs(&self) -> Vec<SocketAddr> {
        self.forwards.iter().filter_map(|f| f.listener.local_addr().ok()).collect()
    }

    /// Drop the lease and all forwarded connections (guest rebooted / AP went away).
    pub fn reset(&mut self) {
        self.abort_conns();
        self.pending.clear();
        self.sockets = SocketSet::new(vec![]);
        self.dhcp = None;
        self.client = None;
        self.ap_ip = None;
        let (device, iface) = Self::make_iface(self.mac);
        self.device = device;
        self.iface = iface;
        self.init_addressing();
    }

    /// An Ethernet frame transmitted by the guest (AP).
    pub fn from_guest(&mut self, frame: &[u8]) {
        self.device.rx.push_back(frame.to_vec());
    }

    fn poll_dhcp(&mut self) {
        let Some(h) = self.dhcp else { return };
        let ev = self.sockets.get_mut::<dhcpv4::Socket>(h).poll();
        match ev {
            Some(dhcpv4::Event::Configured(cfg)) => {
                let ap = cfg.router.unwrap_or(cfg.server.address);
                let addr = cfg.address;
                log::info!("vnet ap-client: got {addr} from AP {ap}");
                if self.client != Some(addr) || self.ap_ip != Some(ap) {
                    self.abort_conns();
                }
                self.set_address(addr, ap);
            }
            Some(dhcpv4::Event::Deconfigured) => {
                log::info!("vnet ap-client: lease lost");
                self.clear_address();
            }
            None => {}
        }
    }

    fn accept(&mut self) {
        for f in &self.forwards {
            loop {
                match f.listener.accept() {
                    Ok((s, _)) => self.pending.push((s, f.guest_port, Instant::now())),
                    Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                    Err(e) => {
                        log::debug!("vnet ap-client: accept: {e}");
                        break;
                    }
                }
            }
        }
        let now = Instant::now();
        let Some(ap) = self.ap_ip else {
            self.pending.retain(|(_, _, t)| now.duration_since(*t) < PENDING_TIMEOUT);
            return;
        };
        for (stream, port, _) in std::mem::take(&mut self.pending) {
            let Ok(bridge) = Bridge::new(stream) else { continue };
            let mut sock = new_tcp_socket();
            let local = self.next_port;
            self.next_port = if self.next_port >= 65000 { 49152 } else { self.next_port + 1 };
            if let Err(e) = sock.connect(self.iface.context(), (IpAddress::Ipv4(ap), port), local) {
                log::debug!("vnet ap-client: connect: {e}");
                continue;
            }
            let handle = self.sockets.add(sock);
            self.conns.push(Conn { handle, bridge });
        }
    }

    fn pump(&mut self) {
        let sockets = &mut self.sockets;
        self.conns.retain_mut(|c| {
            let sock = sockets.get_mut::<tcp::Socket>(c.handle);
            match c.bridge.pump(sock) {
                PumpResult::Alive => true,
                PumpResult::Done => {
                    sockets.remove(c.handle);
                    false
                }
            }
        });
    }

    /// Non-blocking: service host sockets and timers, return frames to deliver to the guest.
    pub fn poll(&mut self) -> Vec<Vec<u8>> {
        let now = self.now();
        self.iface.poll(now, &mut self.device, &mut self.sockets);
        self.poll_dhcp();
        self.accept();
        self.pump();
        self.iface.poll(now, &mut self.device, &mut self.sockets);
        std::mem::take(&mut self.device.tx)
    }
}
