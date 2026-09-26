//! User-mode virtual network for the emulated ESP32-C3's WiFi interface.
//!
//! The emulated firmware runs its own lwIP stack; the simulator swaps the WiFi driver for a
//! pipe of raw Ethernet II frames. This crate is the host end of that pipe, in two roles:
//!
//! * [`VirtualNet`] — guest is a station (STA). We behave like QEMU's slirp: a router at
//!   `gateway_ip` that answers ARP (for every non-guest address in the subnet), serves DHCP
//!   and DNS, and NATs the guest's traffic onto ordinary host sockets.
//!   * ARP, DHCP, DNS, ICMP echo and UDP are handled by small hand-rolled codecs
//!     (`packet.rs`); UDP flows get one ephemeral host `UdpSocket` each, expired when idle.
//!   * DNS A lookups run `ToSocketAddrs` on a short-lived worker thread; results are picked
//!     up by `poll()`, so nothing blocks. `dns_overrides` and IP literals are answered inline,
//!     non-A queries get an empty NOERROR answer.
//!   * TCP is terminated by a smoltcp [`Interface`](smoltcp::iface::Interface) using
//!     `Medium::Ip` with *AnyIP* enabled (we add/strip Ethernet headers ourselves). A guest
//!     SYN for a new flow is parked while a worker thread does the host `connect()`; on
//!     success a smoltcp socket is put in LISTEN on the original destination and the SYN is
//!     replayed into it, on failure the guest gets a RST. Data is then pumped between the
//!     smoltcp socket and the non-blocking host `TcpStream` (`bridge.rs`), which gives real
//!     windowing/flow-control, retransmission and half-close handling in both directions.
//!   * TCP/UDP to `gateway_ip` is redirected to host `127.0.0.1`; other in-subnet addresses
//!     are unreachable (RST / dropped).
//!
//! * [`ApClient`] — guest runs a SoftAP with its own DHCP server. We are an Ethernet station
//!   implemented entirely with smoltcp (ARP + DHCP client socket + TCP client sockets), and
//!   forward host `TcpListener`s to ports on the guest's AP address so a desktop browser can
//!   reach the captive portal.
//!
//! [`VirtualNet`] can also inject faults ([`NetFaults`]): latency, packet loss, a bandwidth
//! limit, DNS failures, an access point without internet, and TCP connections cut after N
//! bytes (RST or a silent stall).
//!
//! Both types are single-threaded state machines: feed guest frames with `from_guest`, call
//! `poll()` frequently, and deliver the returned frames to the guest.

mod ap;
mod bridge;
mod device;
mod packet;
mod vnet;

use std::net::Ipv4Addr;
use std::time::Duration;

pub use ap::ApClient;
pub use vnet::VirtualNet;

/// Addressing of the virtual network presented to a guest station.
#[derive(Clone, Debug)]
pub struct NetConfig {
    /// Router address; also the DHCP server id. TCP/UDP to it is NATed to host 127.0.0.1.
    pub gateway_ip: Ipv4Addr,
    /// DNS server offered by DHCP (answered locally on UDP 53).
    pub dns_ip: Ipv4Addr,
    /// Address offered to the guest by DHCP.
    pub guest_ip: Ipv4Addr,
    pub netmask: Ipv4Addr,
    /// MAC used for the gateway (and every other virtual host in the subnet).
    pub gateway_mac: [u8; 6],
    /// hostname -> IP answered locally instead of resolving (case-insensitive).
    pub dns_overrides: Vec<(String, Ipv4Addr)>,
    /// Hermetic mode: only the gateway (host localhost) is reachable; other
    /// destinations are refused and unknown names don't resolve.
    pub offline: bool,
    /// (guest port, host port): connections to `gateway_ip`:guest port go to host
    /// 127.0.0.1:host port instead of the same port (e.g. a server that moved).
    pub host_ports: Vec<(u16, u16)>,
}

impl Default for NetConfig {
    fn default() -> Self {
        Self {
            gateway_ip: Ipv4Addr::new(10, 0, 2, 2),
            dns_ip: Ipv4Addr::new(10, 0, 2, 3),
            guest_ip: Ipv4Addr::new(10, 0, 2, 15),
            netmask: Ipv4Addr::new(255, 255, 255, 0),
            gateway_mac: [0x02, 0, 0, 0, 0, 0x02],
            dns_overrides: Vec::new(),
            offline: false,
            host_ports: Vec::new(),
        }
    }
}

/// Faults injected into a [`VirtualNet`] (all off by default). They apply to packets from the
/// moment they are set; `tcp_cut` applies to connections opened afterwards.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct NetFaults {
    /// Added to every frame towards the guest (round trips grow by this much).
    pub latency: Duration,
    /// Probability (0..=1) of dropping a frame, independently in each direction.
    pub loss: f64,
    /// Link rate in bytes per second, each direction; frames queue behind each other.
    pub bandwidth: Option<u64>,
    pub dns: Option<DnsFault>,
    /// ARP and DHCP work, nothing else is routed: DNS goes unanswered and every other
    /// packet (to the internet and to the gateway/host) is dropped silently.
    pub no_internet: bool,
    /// Like [`NetConfig::offline`], switchable at runtime.
    pub offline: bool,
    pub tcp_cut: Option<TcpCut>,
}

/// How the DNS server fails.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DnsFault {
    ServFail,
    NxDomain,
    /// NOERROR with no answers.
    Empty,
    /// No response at all.
    Timeout,
}

/// Cut a TCP connection once `after_bytes` of payload went to the guest on it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TcpCut {
    pub after_bytes: u64,
    /// Stop delivering data but keep the connection open, instead of a RST.
    pub stall: bool,
    /// Only connections to this destination port.
    pub port: Option<u16>,
}
