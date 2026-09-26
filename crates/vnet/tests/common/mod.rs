//! A fake guest: a second smoltcp stack wired to a VirtualNet/ApClient in memory.
#![allow(dead_code)]

use std::collections::VecDeque;
use std::net::Ipv4Addr;
use std::time::{Duration, Instant};

use smoltcp::iface::{Config, Interface, SocketSet};
use smoltcp::phy::{self, Device, DeviceCapabilities, Medium};
use smoltcp::socket::tcp;
use smoltcp::wire::{EthernetAddress, HardwareAddress, IpCidr};

pub struct Dev {
    pub rx: VecDeque<Vec<u8>>,
    pub tx: Vec<Vec<u8>>,
}
pub struct Rx(Vec<u8>);
pub struct Tx<'a>(&'a mut Vec<Vec<u8>>);
impl phy::RxToken for Rx {
    fn consume<R, F: FnOnce(&[u8]) -> R>(self, f: F) -> R {
        f(&self.0)
    }
}
impl phy::TxToken for Tx<'_> {
    fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, len: usize, f: F) -> R {
        let mut b = vec![0; len];
        let r = f(&mut b);
        self.0.push(b);
        r
    }
}
impl Device for Dev {
    type RxToken<'a> = Rx;
    type TxToken<'a> = Tx<'a>;
    fn receive(&mut self, _: smoltcp::time::Instant) -> Option<(Rx, Tx<'_>)> {
        let p = self.rx.pop_front()?;
        Some((Rx(p), Tx(&mut self.tx)))
    }
    fn transmit(&mut self, _: smoltcp::time::Instant) -> Option<Tx<'_>> {
        Some(Tx(&mut self.tx))
    }
    fn capabilities(&self) -> DeviceCapabilities {
        let mut c = DeviceCapabilities::default();
        c.medium = Medium::Ethernet;
        c.max_transmission_unit = 1514;
        c
    }
}

#[allow(clippy::wrong_self_convention)]
pub trait Net {
    fn from_guest(&mut self, f: &[u8]);
    fn poll(&mut self) -> Vec<Vec<u8>>;
}
impl Net for vnet::VirtualNet {
    fn from_guest(&mut self, f: &[u8]) {
        vnet::VirtualNet::from_guest(self, f)
    }
    fn poll(&mut self) -> Vec<Vec<u8>> {
        vnet::VirtualNet::poll(self)
    }
}
impl Net for vnet::ApClient {
    fn from_guest(&mut self, f: &[u8]) {
        vnet::ApClient::from_guest(self, f)
    }
    fn poll(&mut self) -> Vec<Vec<u8>> {
        vnet::ApClient::poll(self)
    }
}

pub const GUEST_MAC: [u8; 6] = [0x02, 0x11, 0x22, 0x33, 0x44, 0x55];

pub struct Guest {
    pub dev: Dev,
    pub iface: Interface,
    pub sockets: SocketSet<'static>,
    start: Instant,
    /// Frames seen from the net side (for inspection).
    pub frames_in: usize,
}

impl Guest {
    pub fn new(mac: [u8; 6]) -> Self {
        let mut dev = Dev { rx: VecDeque::new(), tx: Vec::new() };
        let mut cfg = Config::new(HardwareAddress::Ethernet(EthernetAddress(mac)));
        cfg.random_seed = 0x1234_5678;
        let iface = Interface::new(cfg, &mut dev, smoltcp::time::Instant::ZERO);
        Self { dev, iface, sockets: SocketSet::new(vec![]), start: Instant::now(), frames_in: 0 }
    }

    pub fn set_ip(&mut self, ip: Ipv4Addr, prefix: u8, gw: Option<Ipv4Addr>) {
        self.iface.update_ip_addrs(|a| {
            a.clear();
            a.push(IpCidr::new(ip.into(), prefix)).unwrap();
        });
        if let Some(gw) = gw {
            self.iface.routes_mut().add_default_ipv4_route(gw).unwrap();
        }
    }

    pub fn now(&self) -> smoltcp::time::Instant {
        smoltcp::time::Instant::from_micros(self.start.elapsed().as_micros() as i64)
    }

    /// One exchange round between guest and net.
    pub fn step(&mut self, net: &mut dyn Net) {
        let now = self.now();
        self.iface.poll(now, &mut self.dev, &mut self.sockets);
        for f in self.dev.tx.drain(..) {
            net.from_guest(&f);
        }
        let back = net.poll();
        self.frames_in += back.len();
        self.dev.rx.extend(back);
        let now = self.now();
        self.iface.poll(now, &mut self.dev, &mut self.sockets);
    }

    /// Step until `cond` holds or timeout; returns whether cond held.
    pub fn run_until(
        &mut self,
        net: &mut dyn Net,
        timeout: Duration,
        mut cond: impl FnMut(&mut Guest) -> bool,
    ) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            self.step(net);
            if cond(self) {
                return true;
            }
            std::thread::sleep(Duration::from_micros(200));
        }
        false
    }
}

pub fn tcp_socket(rx: usize, tx: usize) -> tcp::Socket<'static> {
    tcp::Socket::new(tcp::SocketBuffer::new(vec![0; rx]), tcp::SocketBuffer::new(vec![0; tx]))
}

pub fn pattern(len: usize, seed: u32) -> Vec<u8> {
    let mut x = seed.wrapping_mul(2654435761).wrapping_add(1);
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            x as u8
        })
        .collect()
}
