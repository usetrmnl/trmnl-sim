//! `VirtualNet`: slirp-style router + NAT for a guest station interface.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::ErrorKind;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4, TcpStream, ToSocketAddrs, UdpSocket};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::time::{Duration, Instant};

use smoltcp::iface::{Config, Interface, SocketHandle, SocketSet};
use smoltcp::phy::Medium;
use smoltcp::socket::tcp::{self, State};
use smoltcp::wire::{HardwareAddress, IpAddress, IpCidr, IpListenEndpoint};

use crate::bridge::{Bridge, PumpResult, new_tcp_socket};
use crate::device::QueueDevice;
use crate::packet::*;
use crate::{DnsFault, NetConfig, NetFaults};

const MTU: usize = 1500;
const LEASE_SECS: u32 = 7200;
const UDP_IDLE: Duration = Duration::from_secs(120);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// Guest-side 4-tuple identifying a NATed flow (guest ip is implicit: single guest).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
struct FlowKey {
    guest_port: u16,
    dst: SocketAddrV4,
}

enum TcpFlow {
    /// Host connect() in progress on a worker thread; the latest SYN from the guest is kept
    /// so it can be fed to smoltcp once the host side is up.
    Pending {
        syn: Vec<u8>,
        rx: Receiver<std::io::Result<TcpStream>>,
    },
    Active {
        handle: SocketHandle,
        bridge: Bridge,
    },
}

struct UdpFlow {
    sock: UdpSocket,
    last: Instant,
}

struct DnsResult {
    generation: u64,
    guest: SocketAddrV4,
    response: Vec<u8>,
}

/// The "internet side" a guest station (STA) interface is attached to.
pub struct VirtualNet {
    cfg: NetConfig,
    start: Instant,
    generation: u64,
    guest_mac: Option<[u8; 6]>,
    lease: Option<Ipv4Addr>,
    /// Frames generated directly (ARP/DHCP/DNS/UDP/ICMP/RST), not via smoltcp.
    out: Vec<Vec<u8>>,

    device: QueueDevice,
    iface: Interface,
    sockets: SocketSet<'static>,
    tcp: HashMap<FlowKey, TcpFlow>,
    udp: HashMap<FlowKey, UdpFlow>,

    dns_tx: Sender<DnsResult>,
    dns_rx: Receiver<DnsResult>,
    dns_inflight: HashSet<(u16, u16)>,

    faults: NetFaults,
    /// Frames on their way to / from the guest while latency or a bandwidth limit applies,
    /// with the time they arrive; and when each direction's link is free again.
    downlink: VecDeque<(Instant, Vec<u8>)>,
    uplink: VecDeque<(Instant, Vec<u8>)>,
    down_free: Instant,
    up_free: Instant,
    /// Packet-loss dice (xorshift64, fixed seed so runs are repeatable).
    rng: u64,
}

impl VirtualNet {
    pub fn new(cfg: NetConfig) -> Self {
        let (device, iface) = Self::make_iface(&cfg, Instant::now());
        let (dns_tx, dns_rx) = mpsc::channel();
        Self {
            cfg,
            start: Instant::now(),
            generation: 0,
            guest_mac: None,
            lease: None,
            out: Vec::new(),
            device,
            iface,
            sockets: SocketSet::new(vec![]),
            tcp: HashMap::new(),
            udp: HashMap::new(),
            dns_tx,
            dns_rx,
            dns_inflight: HashSet::new(),
            faults: NetFaults::default(),
            downlink: VecDeque::new(),
            uplink: VecDeque::new(),
            down_free: Instant::now(),
            up_free: Instant::now(),
            rng: 0x2545_f491_4f6c_dd1d,
        }
    }

    /// Replace the injected faults. Frames already queued keep their delivery time.
    pub fn set_faults(&mut self, faults: NetFaults) {
        self.faults = faults;
    }

    pub fn faults(&self) -> &NetFaults {
        &self.faults
    }

    fn make_iface(cfg: &NetConfig, start: Instant) -> (QueueDevice, Interface) {
        let mut device = QueueDevice::new(Medium::Ip, MTU);
        let mut config = Config::new(HardwareAddress::Ip);
        config.random_seed = start.elapsed().as_nanos() as u64 ^ 0x5eed_1234_abcd;
        let mut iface = Interface::new(config, &mut device, smoltcp::time::Instant::ZERO);
        let prefix = u32::from(cfg.netmask).count_ones() as u8;
        iface.update_ip_addrs(|a| {
            let _ = a.push(IpCidr::new(IpAddress::Ipv4(cfg.gateway_ip), prefix));
        });
        // Terminate TCP for every destination address.
        iface.set_any_ip(true);
        (device, iface)
    }

    fn now(&self) -> smoltcp::time::Instant {
        smoltcp::time::Instant::from_micros(self.start.elapsed().as_micros() as i64)
    }

    /// Guest IP once it completed DHCP (for display).
    /// True while anything is in flight on the host side (TCP connections,
    /// pending connects, DNS lookups, recent UDP). Simulators use this to avoid
    /// fast-forwarding guest time while waiting on the real network.
    pub fn busy(&self) -> bool {
        !self.tcp.is_empty()
            || !self.dns_inflight.is_empty()
            || !self.downlink.is_empty()
            || !self.uplink.is_empty()
            || self.udp.values().any(|f| f.last.elapsed() < std::time::Duration::from_secs(2))
    }

    pub fn guest_lease(&self) -> Option<Ipv4Addr> {
        self.lease
    }

    /// The configuration this network was created with.
    pub fn config(&self) -> &NetConfig {
        &self.cfg
    }

    /// Drop all NAT state/connections (guest rebooted or WiFi disconnected).
    pub fn reset(&mut self) {
        self.generation += 1;
        self.guest_mac = None;
        self.lease = None;
        self.out.clear();
        self.tcp.clear(); // drops host streams
        self.udp.clear();
        self.dns_inflight.clear();
        self.downlink.clear();
        self.uplink.clear();
        self.sockets = SocketSet::new(vec![]);
        let (device, iface) = Self::make_iface(&self.cfg, self.start);
        self.device = device;
        self.iface = iface;
    }

    fn in_subnet(&self, ip: Ipv4Addr) -> bool {
        let m = u32::from(self.cfg.netmask);
        u32::from(ip) & m == u32::from(self.cfg.gateway_ip) & m
    }

    fn guest_ip(&self) -> Ipv4Addr {
        self.lease.unwrap_or(self.cfg.guest_ip)
    }

    /// Map a guest-visible destination to the host destination (None = unreachable).
    fn host_target(&self, dst: Ipv4Addr) -> Option<Ipv4Addr> {
        if dst == self.cfg.gateway_ip {
            Some(Ipv4Addr::LOCALHOST)
        } else if self.offline()
            || self.in_subnet(dst)
            || dst.is_broadcast()
            || dst.is_multicast()
            || dst.is_unspecified()
        {
            None
        } else {
            Some(dst)
        }
    }

    fn offline(&self) -> bool {
        self.cfg.offline || self.faults.offline
    }

    /// Roll the packet-loss dice.
    fn lose(&mut self) -> bool {
        if self.faults.loss <= 0.0 {
            return false;
        }
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 7;
        self.rng ^= self.rng << 17;
        ((self.rng >> 11) as f64 / (1u64 << 53) as f64) < self.faults.loss
    }

    /// When a frame of `len` bytes entering a link now is fully across it (bandwidth limit).
    fn link_time(bandwidth: Option<u64>, free: &mut Instant, now: Instant, len: usize) -> Instant {
        let Some(bps) = bandwidth.filter(|&b| b > 0) else { return now };
        let start = (*free).max(now);
        *free = start + Duration::from_nanos(len as u64 * 1_000_000_000 / bps);
        *free
    }

    fn send_ip_to_guest(&mut self, ip_packet: &[u8]) {
        let dst = self.guest_mac.unwrap_or(BROADCAST_MAC);
        let mut f = Vec::with_capacity(ETH_HDR + ip_packet.len());
        eth_header(&mut f, dst, self.cfg.gateway_mac, ETHERTYPE_IPV4);
        f.extend_from_slice(ip_packet);
        self.out.push(f);
    }

    /// An Ethernet frame transmitted by the guest.
    pub fn from_guest(&mut self, frame: &[u8]) {
        if self.lose() {
            return;
        }
        if self.faults.bandwidth.is_some() || !self.uplink.is_empty() {
            let now = Instant::now();
            let at = Self::link_time(self.faults.bandwidth, &mut self.up_free, now, frame.len());
            self.uplink.push_back((at, frame.to_vec()));
            return;
        }
        self.handle_frame(frame);
    }

    fn handle_frame(&mut self, frame: &[u8]) {
        if frame.len() < ETH_HDR {
            return;
        }
        let dst_mac: [u8; 6] = frame[0..6].try_into().unwrap();
        let src_mac: [u8; 6] = frame[6..12].try_into().unwrap();
        if dst_mac != BROADCAST_MAC && dst_mac != self.cfg.gateway_mac && dst_mac[0] & 1 == 0 {
            return; // unicast to someone else
        }
        if src_mac[0] & 1 == 0 && src_mac != [0; 6] {
            self.guest_mac = Some(src_mac);
        }
        let ethertype = u16::from_be_bytes([frame[12], frame[13]]);
        let payload = &frame[ETH_HDR..];
        match ethertype {
            ETHERTYPE_ARP => self.handle_arp(payload),
            ETHERTYPE_IPV4 => self.handle_ipv4(payload),
            _ => {}
        }
    }

    fn handle_arp(&mut self, p: &[u8]) {
        if p.len() < 28 || p[0..2] != [0, 1] || p[2..4] != [8, 0] || p[4] != 6 || p[5] != 4 {
            return;
        }
        let op = u16::from_be_bytes([p[6], p[7]]);
        let sha: [u8; 6] = p[8..14].try_into().unwrap();
        let spa = Ipv4Addr::new(p[14], p[15], p[16], p[17]);
        let tpa = Ipv4Addr::new(p[24], p[25], p[26], p[27]);
        if op != 1 {
            return;
        }
        // Don't answer probes/gratuitous ARP for the guest's own address, nor anything off-subnet.
        if tpa == spa || tpa == self.guest_ip() || !self.in_subnet(tpa) || tpa.is_unspecified() {
            return;
        }
        let bcast = Ipv4Addr::from(u32::from(self.cfg.gateway_ip) | !u32::from(self.cfg.netmask));
        if tpa == bcast {
            return;
        }
        let mut f = Vec::with_capacity(42);
        eth_header(&mut f, sha, self.cfg.gateway_mac, ETHERTYPE_ARP);
        f.extend_from_slice(&[0, 1, 8, 0, 6, 4, 0, 2]);
        f.extend_from_slice(&self.cfg.gateway_mac);
        f.extend_from_slice(&tpa.octets());
        f.extend_from_slice(&sha);
        f.extend_from_slice(&spa.octets());
        self.out.push(f);
    }

    fn handle_ipv4(&mut self, p: &[u8]) {
        let Some(ip) = parse_ipv4(p) else { return };
        match ip.proto {
            1 => self.handle_icmp(&ip),
            6 => self.handle_tcp(&ip),
            17 => self.handle_udp(&ip),
            _ => {}
        }
    }

    fn handle_icmp(&mut self, ip: &Ipv4<'_>) {
        let d = ip.payload;
        if d.len() < 8 || d[0] != 8 || checksum(&[d]) != 0 {
            return;
        }
        if ip.dst != self.cfg.gateway_ip && ip.dst != self.cfg.dns_ip {
            return;
        }
        let mut r = d.to_vec();
        r[0] = 0;
        r[2] = 0;
        r[3] = 0;
        let c = checksum(&[&r]);
        r[2..4].copy_from_slice(&c.to_be_bytes());
        let pkt = ipv4_packet(ip.dst, ip.src, 1, &r);
        self.send_ip_to_guest(&pkt);
    }

    // ------------------------------------------------------------ UDP

    fn handle_udp(&mut self, ip: &Ipv4<'_>) {
        let d = ip.payload;
        if d.len() < 8 {
            return;
        }
        let sport = u16::from_be_bytes([d[0], d[1]]);
        let dport = u16::from_be_bytes([d[2], d[3]]);
        let ulen = usize::from(u16::from_be_bytes([d[4], d[5]]));
        if ulen < 8 || ulen > d.len() {
            return;
        }
        let data = &d[8..ulen];

        if dport == 67 {
            self.handle_dhcp(data);
            return;
        }
        if ip.src.is_unspecified() || self.faults.no_internet {
            return;
        }
        if ip.dst == self.cfg.dns_ip && dport == 53 {
            self.handle_dns(ip.src, sport, data);
            return;
        }
        let Some(target) = self.host_target(ip.dst) else {
            return;
        };
        let key = FlowKey { guest_port: sport, dst: SocketAddrV4::new(ip.dst, dport) };
        let flow = match self.udp.entry(key) {
            std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
            std::collections::hash_map::Entry::Vacant(e) => {
                let sock = match UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))
                    .and_then(|s| s.connect(SocketAddrV4::new(target, dport)).map(|_| s))
                    .and_then(|s| s.set_nonblocking(true).map(|_| s))
                {
                    Ok(s) => s,
                    Err(e) => {
                        log::debug!("vnet: udp socket for {key:?}: {e}");
                        return;
                    }
                };
                e.insert(UdpFlow { sock, last: Instant::now() })
            }
        };
        flow.last = Instant::now();
        if let Err(e) = flow.sock.send(data) {
            log::debug!("vnet: udp send to {key:?}: {e}");
        }
    }

    fn poll_udp(&mut self) {
        let mut buf = [0u8; 2048];
        let mut replies = Vec::new();
        let now = Instant::now();
        self.udp.retain(|key, flow| {
            loop {
                match flow.sock.recv(&mut buf) {
                    Ok(n) => {
                        flow.last = now;
                        replies.push((*key, buf[..n].to_vec()));
                    }
                    Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                    // ICMP port unreachable etc. surface as errors on connected sockets.
                    Err(_) => break,
                }
            }
            now.duration_since(flow.last) < UDP_IDLE
        });
        let guest = self.guest_ip();
        for (key, data) in replies {
            let pkt = udp_ip_packet(*key.dst.ip(), key.dst.port(), guest, key.guest_port, &data);
            self.send_ip_to_guest(&pkt);
        }
    }

    // ------------------------------------------------------------ DHCP

    fn handle_dhcp(&mut self, data: &[u8]) {
        let Some(m) = parse_dhcp(data) else { return };
        if m.op != 1 {
            return;
        }
        let cfg = &self.cfg;
        let params = Some((cfg.netmask, cfg.gateway_ip, cfg.dns_ip, LEASE_SECS));
        let (msg_type, params) = match m.msg_type {
            DHCP_DISCOVER => (DHCP_OFFER, params),
            DHCP_REQUEST => {
                if m.server_id.is_some_and(|s| s != cfg.gateway_ip) {
                    return; // client picked another server
                }
                let wanted = m.requested_ip.or((!m.ciaddr.is_unspecified()).then_some(m.ciaddr));
                if wanted.is_none_or(|w| w == cfg.guest_ip) {
                    self.lease = Some(cfg.guest_ip);
                    (DHCP_ACK, params)
                } else {
                    (DHCP_NAK, None)
                }
            }
            DHCP_INFORM => (DHCP_ACK, params),
            DHCP_RELEASE | DHCP_DECLINE => {
                self.lease = None;
                return;
            }
            _ => return,
        };
        let yiaddr = if m.msg_type == DHCP_INFORM { Ipv4Addr::UNSPECIFIED } else { cfg.guest_ip };
        let reply = build_dhcp_reply(&DhcpReply { req: &m, msg_type, yiaddr, server_ip: cfg.gateway_ip, params });
        // Unicast to a client that already has a configured address (RENEWING/INFORM);
        // otherwise broadcast, which every client (incl. lwIP) accepts.
        let (dst_ip, dst_mac) = if !m.ciaddr.is_unspecified() && msg_type != DHCP_NAK {
            (m.ciaddr, m.chaddr)
        } else {
            (Ipv4Addr::BROADCAST, BROADCAST_MAC)
        };
        let pkt = udp_ip_packet(cfg.gateway_ip, 67, dst_ip, 68, &reply);
        let mut f = Vec::with_capacity(ETH_HDR + pkt.len());
        eth_header(&mut f, dst_mac, cfg.gateway_mac, ETHERTYPE_IPV4);
        f.extend_from_slice(&pkt);
        self.out.push(f);
    }

    // ------------------------------------------------------------ DNS

    fn handle_dns(&mut self, src: Ipv4Addr, sport: u16, data: &[u8]) {
        let Some(q) = parse_dns_query(data) else { return };
        log::debug!("vnet: dns query {} type {} from {src}:{sport}", q.name, q.qtype);
        let guest = SocketAddrV4::new(src, sport);
        let opcode = (q.flags >> 11) & 0xf;
        let immediate = |rcode, ans: &[Ipv4Addr]| build_dns_response(&q, rcode, ans);
        let resp = if let Some(fault) = self.faults.dns {
            match fault {
                DnsFault::ServFail => Some(immediate(RCODE_SERVFAIL, &[])),
                DnsFault::NxDomain => Some(immediate(RCODE_NXDOMAIN, &[])),
                DnsFault::Empty => Some(immediate(RCODE_NOERROR, &[])),
                DnsFault::Timeout => return,
            }
        } else if opcode != 0 {
            Some(immediate(RCODE_NOTIMP, &[]))
        } else if q.qclass != 1 || q.qtype != 1 {
            // Non-A queries (AAAA, ...): empty NOERROR answer.
            Some(immediate(RCODE_NOERROR, &[]))
        } else {
            let name = q.name.trim_end_matches('.').to_ascii_lowercase();
            if let Some((_, ip)) =
                self.cfg.dns_overrides.iter().find(|(h, _)| h.trim_end_matches('.').eq_ignore_ascii_case(&name))
            {
                Some(immediate(RCODE_NOERROR, &[*ip]))
            } else if let Ok(ip) = name.parse::<Ipv4Addr>() {
                Some(immediate(RCODE_NOERROR, &[ip]))
            } else if self.offline() {
                Some(immediate(RCODE_NXDOMAIN, &[]))
            } else {
                None
            }
        };
        if let Some(resp) = resp {
            let pkt = udp_ip_packet(self.cfg.dns_ip, 53, src, sport, &resp);
            self.send_ip_to_guest(&pkt);
            return;
        }
        if !self.dns_inflight.insert((sport, q.id)) {
            return; // retransmission of a query we're already resolving
        }
        let tx = self.dns_tx.clone();
        let generation = self.generation;
        let spawn = std::thread::Builder::new().name("vnet-dns".into()).spawn(move || {
            let name = q.name.trim_end_matches('.').to_string();
            let (rcode, ips) = match (name.as_str(), 0u16).to_socket_addrs() {
                Ok(addrs) => {
                    let mut v: Vec<Ipv4Addr> = Vec::new();
                    for a in addrs {
                        if let IpAddr::V4(ip) = a.ip()
                            && !v.contains(&ip)
                        {
                            v.push(ip);
                        }
                    }
                    // Name exists but only has AAAA records: NOERROR with no answers.
                    (RCODE_NOERROR, v)
                }
                Err(e) => {
                    log::debug!("vnet: resolve {name}: {e}");
                    (RCODE_NXDOMAIN, vec![])
                }
            };
            let response = build_dns_response(&q, rcode, &ips[..ips.len().min(8)]);
            let _ = tx.send(DnsResult { generation, guest, response });
        });
        if spawn.is_err() {
            let resp = immediate_servfail(data);
            if let Some(resp) = resp {
                let pkt = udp_ip_packet(self.cfg.dns_ip, 53, src, sport, &resp);
                self.send_ip_to_guest(&pkt);
            }
        }
    }

    fn poll_dns(&mut self) {
        while let Ok(r) = self.dns_rx.try_recv() {
            if r.generation != self.generation {
                continue;
            }
            let id = u16::from_be_bytes([r.response[0], r.response[1]]);
            self.dns_inflight.remove(&(r.guest.port(), id));
            let pkt = udp_ip_packet(self.cfg.dns_ip, 53, *r.guest.ip(), r.guest.port(), &r.response);
            self.send_ip_to_guest(&pkt);
        }
    }

    // ------------------------------------------------------------ TCP

    fn handle_tcp(&mut self, ip: &Ipv4<'_>) {
        let d = ip.payload;
        if d.len() < 20 || ip.src.is_unspecified() || self.faults.no_internet {
            return;
        }
        let sport = u16::from_be_bytes([d[0], d[1]]);
        let dport = u16::from_be_bytes([d[2], d[3]]);
        let seq = u32::from_be_bytes([d[4], d[5], d[6], d[7]]);
        let flags = d[13];
        let (syn, ack, rst) = (flags & 0x02 != 0, flags & 0x10 != 0, flags & 0x04 != 0);
        let key = FlowKey { guest_port: sport, dst: SocketAddrV4::new(ip.dst, dport) };

        if syn && !ack && !rst {
            match self.tcp.get_mut(&key) {
                Some(TcpFlow::Pending { syn, .. }) => {
                    *syn = ip.packet.to_vec(); // guest retransmitted; keep the latest
                    return;
                }
                Some(TcpFlow::Active { handle, .. }) => {
                    let st = self.sockets.get::<tcp::Socket>(*handle).state();
                    if matches!(st, State::TimeWait | State::Closed) {
                        let h = *handle;
                        self.tcp.remove(&key);
                        self.sockets.remove(h);
                    } else {
                        self.device.rx.push_back(ip.packet.to_vec());
                        return;
                    }
                }
                None => {}
            }
            let Some(target) = self.host_target(ip.dst) else {
                let pkt = tcp_rst_ip_packet(ip.dst, dport, ip.src, sport, 0, seq.wrapping_add(1));
                self.send_ip_to_guest(&pkt);
                return;
            };
            let (tx, rx) = mpsc::channel();
            let addr = SocketAddr::V4(SocketAddrV4::new(target, dport));
            let spawned = std::thread::Builder::new().name("vnet-connect".into()).spawn(move || {
                let _ = tx.send(TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT));
            });
            if spawned.is_err() {
                let pkt = tcp_rst_ip_packet(ip.dst, dport, ip.src, sport, 0, seq.wrapping_add(1));
                self.send_ip_to_guest(&pkt);
                return;
            }
            self.tcp.insert(key, TcpFlow::Pending { syn: ip.packet.to_vec(), rx });
            return;
        }

        if let Some(TcpFlow::Pending { .. }) = self.tcp.get(&key) {
            if rst {
                self.tcp.remove(&key); // guest gave up; host stream dropped when connect finishes
            }
            return;
        }
        // Everything else goes to smoltcp; it RSTs segments for unknown connections.
        self.device.rx.push_back(ip.packet.to_vec());
    }

    fn poll_tcp_pending(&mut self) {
        let keys: Vec<FlowKey> =
            self.tcp.iter().filter(|(_, f)| matches!(f, TcpFlow::Pending { .. })).map(|(k, _)| *k).collect();
        for key in keys {
            let Some(TcpFlow::Pending { rx, syn }) = self.tcp.get(&key) else {
                continue;
            };
            let res = match rx.try_recv() {
                Ok(r) => r,
                Err(TryRecvError::Empty) => continue,
                Err(TryRecvError::Disconnected) => Err(ErrorKind::Other.into()),
            };
            let syn = syn.clone();
            self.tcp.remove(&key);
            let stream = res.and_then(Bridge::new);
            match stream {
                Ok(mut bridge) => {
                    if let Some(cut) = self.faults.tcp_cut.filter(|c| c.port.is_none_or(|p| p == key.dst.port())) {
                        bridge.set_cut(cut);
                    }
                    let mut sock = new_tcp_socket();
                    let ep = IpListenEndpoint { addr: Some(IpAddress::Ipv4(*key.dst.ip())), port: key.dst.port() };
                    if sock.listen(ep).is_err() {
                        continue;
                    }
                    let handle = self.sockets.add(sock);
                    self.tcp.insert(key, TcpFlow::Active { handle, bridge });
                    // Feed the SYN right away so this listener (and no other one for the same
                    // endpoint) picks it up.
                    self.device.rx.push_back(syn);
                    let now = self.now();
                    self.iface.poll(now, &mut self.device, &mut self.sockets);
                }
                Err(e) => {
                    log::debug!("vnet: connect {:?} failed: {e}", key.dst);
                    if let Some(p) = parse_ipv4(&syn) {
                        let d = p.payload;
                        let seq = u32::from_be_bytes([d[4], d[5], d[6], d[7]]);
                        let pkt =
                            tcp_rst_ip_packet(p.dst, key.dst.port(), p.src, key.guest_port, 0, seq.wrapping_add(1));
                        self.send_ip_to_guest(&pkt);
                    }
                }
            }
        }
    }

    fn pump_tcp(&mut self) {
        let mut dead = Vec::new();
        for (key, flow) in self.tcp.iter_mut() {
            if let TcpFlow::Active { handle, bridge } = flow {
                let sock = self.sockets.get_mut::<tcp::Socket>(*handle);
                if let PumpResult::Done = bridge.pump(sock) {
                    dead.push((*key, *handle));
                }
            }
        }
        for (key, handle) in dead {
            self.tcp.remove(&key);
            self.sockets.remove(handle);
        }
    }

    /// Non-blocking: service host sockets and timers, return frames to deliver to the guest.
    pub fn poll(&mut self) -> Vec<Vec<u8>> {
        let now = Instant::now();
        while self.uplink.front().is_some_and(|(at, _)| *at <= now) {
            let (_, f) = self.uplink.pop_front().unwrap();
            self.handle_frame(&f);
        }
        self.poll_tcp_pending();
        let now = self.now();
        self.iface.poll(now, &mut self.device, &mut self.sockets);
        self.pump_tcp();
        self.iface.poll(now, &mut self.device, &mut self.sockets);
        self.poll_udp();
        self.poll_dns();

        let mut out = std::mem::take(&mut self.out);
        let tx = std::mem::take(&mut self.device.tx);
        // No internet: established connections go quiet too.
        if let Some(mac) = self.guest_mac.filter(|_| !self.faults.no_internet) {
            for p in tx {
                let mut f = Vec::with_capacity(ETH_HDR + p.len());
                eth_header(&mut f, mac, self.cfg.gateway_mac, ETHERTYPE_IPV4);
                f.extend_from_slice(&p);
                out.push(f);
            }
        }
        self.shape_downlink(out)
    }

    /// Apply loss, the bandwidth limit and latency to frames for the guest; returns the
    /// frames due now.
    fn shape_downlink(&mut self, frames: Vec<Vec<u8>>) -> Vec<Vec<u8>> {
        let f = &self.faults;
        if f.loss <= 0.0 && f.bandwidth.is_none() && f.latency.is_zero() && self.downlink.is_empty() {
            return frames;
        }
        let now = Instant::now();
        for frame in frames {
            if self.lose() {
                continue;
            }
            let at =
                Self::link_time(self.faults.bandwidth, &mut self.down_free, now, frame.len()) + self.faults.latency;
            self.downlink.push_back((at, frame));
        }
        let mut due = Vec::new();
        while self.downlink.front().is_some_and(|(at, _)| *at <= now) {
            due.push(self.downlink.pop_front().unwrap().1);
        }
        due
    }
}

fn immediate_servfail(query: &[u8]) -> Option<Vec<u8>> {
    let q = parse_dns_query(query)?;
    Some(build_dns_response(&q, RCODE_SERVFAIL, &[]))
}
