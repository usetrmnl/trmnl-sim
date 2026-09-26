//! Fault injection: DNS failures, no-internet APs, latency, loss, bandwidth, cut connections.

mod common;

use std::io::{Read, Write};
use std::net::{Ipv4Addr, TcpListener};
use std::thread;
use std::time::{Duration, Instant};

use common::*;
use smoltcp::iface::SocketHandle;
use smoltcp::socket::{dhcpv4, tcp, udp};
use smoltcp::wire::{IpAddress, IpEndpoint};
use vnet::{DnsFault, NetConfig, NetFaults, TcpCut, VirtualNet};

const T: Duration = Duration::from_secs(20);
const GW: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 2);
const DNS: IpEndpoint = IpEndpoint { addr: IpAddress::Ipv4(Ipv4Addr::new(10, 0, 2, 3)), port: 53 };

fn net_with(faults: NetFaults) -> VirtualNet {
    let cfg = NetConfig { dns_overrides: vec![("mock.test".into(), GW)], ..Default::default() };
    let mut net = VirtualNet::new(cfg);
    net.set_faults(faults);
    net
}

fn guest() -> Guest {
    let mut g = Guest::new(GUEST_MAC);
    g.set_ip(Ipv4Addr::new(10, 0, 2, 15), 24, Some(GW));
    g
}

fn udp_sock(g: &mut Guest, port: u16) -> SocketHandle {
    let mut s = udp::Socket::new(
        udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY; 8], vec![0; 8192]),
        udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY; 8], vec![0; 8192]),
    );
    s.bind(port).unwrap();
    g.sockets.add(s)
}

fn dns_query(id: u16, name: &str) -> Vec<u8> {
    let mut q = id.to_be_bytes().to_vec();
    q.extend_from_slice(&[0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0]);
    for l in name.split('.') {
        q.push(l.len() as u8);
        q.extend_from_slice(l.as_bytes());
    }
    q.extend_from_slice(&[0, 0, 1, 0, 1]);
    q
}

/// Send a DNS query; returns (rcode, answer count) or None if nothing came back in `wait`.
fn ask(g: &mut Guest, net: &mut VirtualNet, id: u16, name: &str, wait: Duration) -> Option<(u8, u16)> {
    let h = udp_sock(g, 40000 + id);
    g.sockets.get_mut::<udp::Socket>(h).send_slice(&dns_query(id, name), DNS).unwrap();
    let mut resp = None;
    g.run_until(net, wait, |g| {
        if let Ok((d, _)) = g.sockets.get_mut::<udp::Socket>(h).recv() {
            assert_eq!(u16::from_be_bytes([d[0], d[1]]), id);
            resp = Some((d[3] & 0xf, u16::from_be_bytes([d[6], d[7]])));
        }
        resp.is_some()
    });
    g.sockets.remove(h);
    resp
}

/// A host server that sends `len` bytes to each connection, then closes.
fn serve(len: usize) -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    thread::spawn(move || {
        for s in l.incoming() {
            let Ok(mut s) = s else { return };
            thread::spawn(move || {
                let _ = s.write_all(&pattern(len, 7));
                let _ = s.shutdown(std::net::Shutdown::Write);
                let _ = s.read(&mut [0u8; 16]);
            });
        }
    });
    port
}

struct Download {
    data: Vec<u8>,
    state: tcp::State,
    elapsed: Duration,
}

/// Connect to the gateway on `port` and receive until the connection closes, `stop_after`
/// passes, or `T`.
fn download(g: &mut Guest, net: &mut VirtualNet, port: u16, stop_after: Option<Duration>) -> Download {
    let mut s = tcp_socket(5744, 1024);
    s.connect(g.iface.context(), (IpAddress::Ipv4(GW), port), 50000).unwrap();
    let h = g.sockets.add(s);
    let start = Instant::now();
    let mut data = Vec::new();
    g.run_until(net, stop_after.unwrap_or(T), |g| {
        let s = g.sockets.get_mut::<tcp::Socket>(h);
        while s.can_recv() {
            s.recv(|b| {
                data.extend_from_slice(b);
                (b.len(), ())
            })
            .unwrap();
        }
        if s.state() == tcp::State::CloseWait {
            s.close();
        }
        matches!(s.state(), tcp::State::Closed | tcp::State::TimeWait)
    });
    let state = g.sockets.get::<tcp::Socket>(h).state();
    g.sockets.remove(h);
    Download { data, state, elapsed: start.elapsed() }
}

#[test]
fn dns_faults_answer_as_configured() {
    for (fault, rcode, answers) in [(DnsFault::ServFail, 2, 0), (DnsFault::NxDomain, 3, 0), (DnsFault::Empty, 0, 0)] {
        let mut net = net_with(NetFaults { dns: Some(fault), ..Default::default() });
        let mut g = guest();
        assert_eq!(ask(&mut g, &mut net, 1, "mock.test", T), Some((rcode, answers)), "{fault:?}");
    }
    let mut net = net_with(NetFaults { dns: Some(DnsFault::Timeout), ..Default::default() });
    let mut g = guest();
    assert_eq!(ask(&mut g, &mut net, 2, "mock.test", Duration::from_millis(500)), None);
    // Cleared: answers again.
    net.set_faults(NetFaults::default());
    assert_eq!(ask(&mut g, &mut net, 3, "mock.test", T), Some((0, 1)));
}

#[test]
fn no_internet_leases_an_address_but_routes_nothing() {
    let mut net = net_with(NetFaults { no_internet: true, ..Default::default() });
    let mut g = Guest::new(GUEST_MAC);
    let h = g.sockets.add(dhcpv4::Socket::new());
    let ok = g.run_until(&mut net, T, |g| {
        matches!(g.sockets.get_mut::<dhcpv4::Socket>(h).poll(), Some(dhcpv4::Event::Configured(_)))
    });
    assert!(ok, "DHCP must still work");
    g.sockets.remove(h);
    g.set_ip(Ipv4Addr::new(10, 0, 2, 15), 24, Some(GW));

    assert_eq!(ask(&mut g, &mut net, 1, "mock.test", Duration::from_millis(500)), None, "DNS must go unanswered");
    // A connection to the host neither completes nor gets refused.
    let port = serve(100);
    let d = download(&mut g, &mut net, port, Some(Duration::from_millis(800)));
    assert_eq!(d.state, tcp::State::SynSent);
    assert!(d.data.is_empty());
}

#[test]
fn latency_delays_replies() {
    let mut net = net_with(NetFaults { latency: Duration::from_millis(300), ..Default::default() });
    let mut g = guest();
    let t = Instant::now();
    assert_eq!(ask(&mut g, &mut net, 1, "mock.test", T), Some((0, 1)));
    let rtt = t.elapsed();
    assert!(rtt >= Duration::from_millis(300) && rtt < Duration::from_secs(2), "{rtt:?}");
}

#[test]
fn bandwidth_limit_slows_downloads() {
    let len = 60 * 1024;
    let mut net = net_with(NetFaults { bandwidth: Some(50_000), ..Default::default() });
    let mut g = guest();
    let d = download(&mut g, &mut net, serve(len), None);
    assert_eq!(d.data, pattern(len, 7));
    // 60 KB (+ headers) at 50 kB/s.
    assert!(d.elapsed >= Duration::from_millis(1200), "{:?}", d.elapsed);
}

#[test]
fn lossy_link_still_delivers_intact_data() {
    let len = 20 * 1024;
    let mut net = net_with(NetFaults { loss: 0.1, ..Default::default() });
    let mut g = guest();
    let d = download(&mut g, &mut net, serve(len), None);
    assert_eq!(d.data.len(), len);
    assert!(d.data == pattern(len, 7));

    net.set_faults(NetFaults { loss: 1.0, ..Default::default() });
    assert_eq!(ask(&mut g, &mut net, 1, "mock.test", Duration::from_millis(500)), None);
}

#[test]
fn tcp_cut_resets_after_n_bytes() {
    let port = serve(50_000);
    let cut = TcpCut { after_bytes: 10_000, stall: false, port: None };
    let mut net = net_with(NetFaults { tcp_cut: Some(cut), ..Default::default() });
    let mut g = guest();
    let d = download(&mut g, &mut net, port, None);
    assert_eq!(d.data.len(), 10_000);
    assert!(d.data == pattern(50_000, 7)[..10_000]);
    assert_eq!(d.state, tcp::State::Closed, "expected a reset");
}

#[test]
fn tcp_cut_stalls_after_n_bytes() {
    let port = serve(50_000);
    let cut = TcpCut { after_bytes: 12_345, stall: true, port: None };
    let mut net = net_with(NetFaults { tcp_cut: Some(cut), ..Default::default() });
    let mut g = guest();
    let d = download(&mut g, &mut net, port, Some(Duration::from_secs(1)));
    assert_eq!(d.data.len(), 12_345);
    assert_eq!(d.state, tcp::State::Established, "the connection must stay open");
    assert!(net.busy());
}

#[test]
fn tcp_cut_only_hits_its_port() {
    let port = serve(30_000);
    let cut = TcpCut { after_bytes: 100, stall: false, port: Some(port.wrapping_add(1)) };
    let mut net = net_with(NetFaults { tcp_cut: Some(cut), ..Default::default() });
    let mut g = guest();
    let d = download(&mut g, &mut net, port, None);
    assert_eq!(d.data.len(), 30_000);
}
