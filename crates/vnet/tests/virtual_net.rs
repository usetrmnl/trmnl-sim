mod common;

use std::io::{Read, Write};
use std::net::{Ipv4Addr, Shutdown, TcpListener, UdpSocket};
use std::thread;
use std::time::Duration;

use common::*;
use smoltcp::iface::SocketHandle;
use smoltcp::socket::{dhcpv4, tcp, udp};
use smoltcp::wire::{IpAddress, IpEndpoint};
use vnet::{NetConfig, VirtualNet};

const T: Duration = Duration::from_secs(20);
const GW: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 2);

fn configured_guest(net: &mut VirtualNet) -> Guest {
    let mut g = Guest::new(GUEST_MAC);
    g.set_ip(Ipv4Addr::new(10, 0, 2, 15), 24, Some(GW));
    // Let the net learn our MAC.
    let _ = net;
    g
}

#[test]
fn dhcp_lease() {
    let mut net = VirtualNet::new(NetConfig::default());
    let mut g = Guest::new(GUEST_MAC);
    let h = g.sockets.add(dhcpv4::Socket::new());
    let mut got = None;
    let ok = g.run_until(&mut net, T, |g| {
        if let Some(dhcpv4::Event::Configured(c)) = g.sockets.get_mut::<dhcpv4::Socket>(h).poll() {
            got = Some((c.address, c.router, c.dns_servers.to_vec()));
            return true;
        }
        false
    });
    assert!(ok, "no DHCP lease");
    let (addr, router, dns) = got.unwrap();
    assert_eq!(addr.address(), Ipv4Addr::new(10, 0, 2, 15));
    assert_eq!(addr.prefix_len(), 24);
    assert_eq!(router, Some(GW));
    assert_eq!(dns, vec![Ipv4Addr::new(10, 0, 2, 3)]);
    assert_eq!(net.guest_lease(), Some(Ipv4Addr::new(10, 0, 2, 15)));
    net.reset();
    assert_eq!(net.guest_lease(), None);
}

fn connect(g: &mut Guest, rx: usize, port: u16, local: u16) -> SocketHandle {
    let mut s = tcp_socket(rx, 4096);
    s.connect(g.iface.context(), (IpAddress::Ipv4(GW), port), local).unwrap();
    g.sockets.add(s)
}

/// Guest sends `req`, receives until FIN, then closes. Returns received bytes.
fn guest_exchange(g: &mut Guest, net: &mut VirtualNet, h: SocketHandle, req: &[u8]) -> Vec<u8> {
    let mut sent = false;
    let mut recvd = Vec::new();
    let ok = g.run_until(net, T, |g| {
        let s = g.sockets.get_mut::<tcp::Socket>(h);
        if !sent && s.can_send() {
            assert_eq!(s.send_slice(req).unwrap(), req.len());
            sent = true;
        }
        while s.can_recv() {
            s.recv(|b| {
                recvd.extend_from_slice(b);
                (b.len(), ())
            })
            .unwrap();
        }
        if sent && !s.may_recv() && s.state() == tcp::State::CloseWait {
            s.close();
        }
        matches!(s.state(), tcp::State::Closed | tcp::State::TimeWait)
    });
    assert!(ok, "exchange timed out; got {} bytes", recvd.len());
    recvd
}

fn spawn_server(n_conns: usize, resp_len: usize) -> (u16, thread::JoinHandle<()>) {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    let j = thread::spawn(move || {
        let mut workers = vec![];
        for i in 0..n_conns {
            let (mut s, _) = l.accept().unwrap();
            workers.push(thread::spawn(move || {
                let mut req = Vec::new();
                let mut b = [0u8; 256];
                while !req.ends_with(b"\r\n\r\n") {
                    let n = s.read(&mut b).unwrap();
                    assert!(n > 0);
                    req.extend_from_slice(&b[..n]);
                }
                let seed: u32 = std::str::from_utf8(&req[4..req.len() - 4]).unwrap().parse().unwrap();
                let _ = i;
                s.write_all(&pattern(resp_len, seed)).unwrap();
                s.shutdown(Shutdown::Write).unwrap();
                // Guest should close its side too.
                let n = s.read(&mut b).unwrap();
                assert_eq!(n, 0, "expected EOF from guest");
            }));
        }
        for w in workers {
            w.join().unwrap();
        }
    });
    (port, j)
}

#[test]
fn tcp_large_download_via_gateway() {
    let mut net = VirtualNet::new(NetConfig::default());
    let mut g = configured_guest(&mut net);
    let len = 200 * 1024;
    let (port, server) = spawn_server(3, len);
    // Several sequential connections, with a small (lwIP-like) receive window.
    for i in 0..3u32 {
        let h = connect(&mut g, 5744, port, 50000 + i as u16);
        let got = guest_exchange(&mut g, &mut net, h, format!("GET {i}\r\n\r\n").as_bytes());
        assert_eq!(got.len(), len);
        assert!(got == pattern(len, i), "payload mismatch");
        g.sockets.remove(h);
    }
    server.join().unwrap();
    // Everything torn down on the net side eventually.
    g.run_until(&mut net, Duration::from_millis(200), |_| false);
}

#[test]
fn tcp_simultaneous_connections() {
    let mut net = VirtualNet::new(NetConfig::default());
    let mut g = configured_guest(&mut net);
    let len = 60 * 1024;
    let n = 4;
    let (port, server) = spawn_server(n, len);
    let hs: Vec<_> = (0..n).map(|i| connect(&mut g, 8192, port, 51000 + i as u16)).collect();
    let mut sent = vec![false; n];
    let mut recvd = vec![Vec::new(); n];
    let ok = g.run_until(&mut net, T, |g| {
        let mut done = 0;
        for (i, &h) in hs.iter().enumerate() {
            let s = g.sockets.get_mut::<tcp::Socket>(h);
            if !sent[i] && s.can_send() {
                s.send_slice(format!("GET {}\r\n\r\n", 100 + i).as_bytes()).unwrap();
                sent[i] = true;
            }
            while s.can_recv() {
                s.recv(|b| {
                    recvd[i].extend_from_slice(b);
                    (b.len(), ())
                })
                .unwrap();
            }
            if s.state() == tcp::State::CloseWait {
                s.close();
            }
            if matches!(s.state(), tcp::State::Closed | tcp::State::TimeWait) {
                done += 1;
            }
        }
        done == n
    });
    assert!(ok);
    for (i, r) in recvd.iter().enumerate() {
        assert!(*r == pattern(len, 100 + i as u32), "conn {i} mismatch");
    }
    server.join().unwrap();
}

#[test]
fn tcp_upload_with_guest_half_close() {
    let mut net = VirtualNet::new(NetConfig::default());
    let mut g = configured_guest(&mut net);
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    let up = pattern(100 * 1024, 7);
    let up2 = up.clone();
    let server = thread::spawn(move || {
        let (mut s, _) = l.accept().unwrap();
        let mut all = Vec::new();
        s.read_to_end(&mut all).unwrap(); // until guest FIN
        assert!(all == up2, "upload mismatch ({} bytes)", all.len());
        s.write_all(b"thanks").unwrap();
    });
    let h = connect(&mut g, 4096, port, 52000);
    let mut off = 0;
    let mut closed = false;
    let mut recvd = Vec::new();
    let ok = g.run_until(&mut net, T, |g| {
        let s = g.sockets.get_mut::<tcp::Socket>(h);
        if s.may_send() && off < up.len() {
            off += s.send_slice(&up[off..]).unwrap();
        }
        if off == up.len() && !closed {
            s.close();
            closed = true;
        }
        while s.can_recv() {
            s.recv(|b| {
                recvd.extend_from_slice(b);
                (b.len(), ())
            })
            .unwrap();
        }
        matches!(s.state(), tcp::State::Closed | tcp::State::TimeWait)
    });
    assert!(ok);
    assert_eq!(recvd, b"thanks");
    server.join().unwrap();
}

#[test]
fn tcp_closed_port_resets() {
    let mut net = VirtualNet::new(NetConfig::default());
    let mut g = configured_guest(&mut net);
    let port = {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let h = connect(&mut g, 4096, port, 53000);
    let ok = g.run_until(&mut net, T, |g| g.sockets.get::<tcp::Socket>(h).state() == tcp::State::Closed);
    assert!(ok, "expected RST");

    // Unreachable in-subnet address also resets immediately.
    let mut s = tcp_socket(1024, 1024);
    s.connect(g.iface.context(), (IpAddress::v4(10, 0, 2, 77), 80), 53001).unwrap();
    let h = g.sockets.add(s);
    let ok = g.run_until(&mut net, T, |g| g.sockets.get::<tcp::Socket>(h).state() == tcp::State::Closed);
    assert!(ok);
}

#[test]
fn tcp_guest_reset_closes_host() {
    let mut net = VirtualNet::new(NetConfig::default());
    let mut g = configured_guest(&mut net);
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    let server = thread::spawn(move || {
        let (mut s, _) = l.accept().unwrap();
        s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let mut b = [0u8; 16];
        // Guest aborts; host should see EOF or reset, not a timeout.
        match s.read(&mut b) {
            Ok(0) => {}
            Ok(n) => panic!("unexpected {n} bytes"),
            Err(e) => assert_ne!(e.kind(), std::io::ErrorKind::WouldBlock, "timed out"),
        }
    });
    let h = connect(&mut g, 4096, port, 54000);
    assert!(g.run_until(&mut net, T, |g| { g.sockets.get::<tcp::Socket>(h).state() == tcp::State::Established }));
    g.sockets.get_mut::<tcp::Socket>(h).abort();
    g.run_until(&mut net, Duration::from_millis(300), |_| false);
    server.join().unwrap();
}

fn dns_query(id: u16, name: &str, qtype: u16) -> Vec<u8> {
    let mut q = vec![];
    q.extend_from_slice(&id.to_be_bytes());
    q.extend_from_slice(&[0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0]);
    for l in name.split('.') {
        q.push(l.len() as u8);
        q.extend_from_slice(l.as_bytes());
    }
    q.push(0);
    q.extend_from_slice(&qtype.to_be_bytes());
    q.extend_from_slice(&1u16.to_be_bytes());
    q
}

/// Returns (rcode, A records).
fn parse_answer(p: &[u8], id: u16) -> (u8, Vec<Ipv4Addr>) {
    assert_eq!(u16::from_be_bytes([p[0], p[1]]), id);
    assert!(p[2] & 0x80 != 0);
    let rcode = p[3] & 0xf;
    let an = u16::from_be_bytes([p[6], p[7]]);
    let mut i = 12;
    while p[i] != 0 {
        i += 1 + p[i] as usize;
    }
    i += 5;
    let mut ips = vec![];
    for _ in 0..an {
        assert_eq!(&p[i..i + 2], &[0xc0, 0x0c]);
        let ty = u16::from_be_bytes([p[i + 2], p[i + 3]]);
        let rdlen = u16::from_be_bytes([p[i + 10], p[i + 11]]) as usize;
        if ty == 1 {
            ips.push(Ipv4Addr::new(p[i + 12], p[i + 13], p[i + 14], p[i + 15]));
        }
        i += 12 + rdlen;
    }
    (rcode, ips)
}

fn udp_roundtrip(g: &mut Guest, net: &mut VirtualNet, h: SocketHandle, dst: IpEndpoint, payload: &[u8]) -> Vec<u8> {
    let mut sent = false;
    let mut resp = None;
    let ok = g.run_until(net, T, |g| {
        let s = g.sockets.get_mut::<udp::Socket>(h);
        if !sent {
            s.send_slice(payload, dst).unwrap();
            sent = true;
        }
        if let Ok((d, meta)) = s.recv() {
            assert_eq!(meta.endpoint, dst);
            resp = Some(d.to_vec());
            return true;
        }
        false
    });
    assert!(ok, "no UDP response");
    resp.unwrap()
}

fn udp_sock(g: &mut Guest, port: u16) -> SocketHandle {
    let mut s = udp::Socket::new(
        udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY; 8], vec![0; 8192]),
        udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY; 8], vec![0; 8192]),
    );
    s.bind(port).unwrap();
    g.sockets.add(s)
}

#[test]
fn dns_resolution() {
    let cfg =
        NetConfig { dns_overrides: vec![("trmnl.example".into(), Ipv4Addr::new(10, 0, 2, 2))], ..Default::default() };
    let mut net = VirtualNet::new(cfg);
    let mut g = configured_guest(&mut net);
    let h = udp_sock(&mut g, 40000);
    let dns = IpEndpoint::new(IpAddress::v4(10, 0, 2, 3), 53);

    let r = udp_roundtrip(&mut g, &mut net, h, dns, &dns_query(1, "localhost", 1));
    let (rc, ips) = parse_answer(&r, 1);
    assert_eq!(rc, 0);
    assert!(ips.contains(&Ipv4Addr::LOCALHOST), "{ips:?}");

    let r = udp_roundtrip(&mut g, &mut net, h, dns, &dns_query(2, "TRMNL.example", 1));
    assert_eq!(parse_answer(&r, 2), (0, vec![GW]));

    // AAAA: quick empty NOERROR.
    let r = udp_roundtrip(&mut g, &mut net, h, dns, &dns_query(3, "localhost", 28));
    assert_eq!(parse_answer(&r, 3), (0, vec![]));
}

#[test]
fn udp_echo_nat() {
    let mut net = VirtualNet::new(NetConfig::default());
    let mut g = configured_guest(&mut net);
    let echo = UdpSocket::bind("127.0.0.1:0").unwrap();
    let port = echo.local_addr().unwrap().port();
    thread::spawn(move || {
        let mut b = [0u8; 2048];
        loop {
            let (n, from) = echo.recv_from(&mut b).unwrap();
            let mut r = b"echo:".to_vec();
            r.extend_from_slice(&b[..n]);
            echo.send_to(&r, from).unwrap();
        }
    });
    let h = udp_sock(&mut g, 40001);
    let dst = IpEndpoint::new(IpAddress::Ipv4(GW), port);
    for i in 0..3 {
        let msg = format!("hello {i}");
        let r = udp_roundtrip(&mut g, &mut net, h, dst, msg.as_bytes());
        assert_eq!(r, format!("echo:hello {i}").into_bytes());
    }
}

#[test]
fn icmp_ping_gateway() {
    let mut net = VirtualNet::new(NetConfig::default());
    let mut g = configured_guest(&mut net);
    use smoltcp::socket::icmp;
    use smoltcp::wire::{Icmpv4Packet, Icmpv4Repr};
    let mut s = icmp::Socket::new(
        icmp::PacketBuffer::new(vec![icmp::PacketMetadata::EMPTY; 4], vec![0; 1024]),
        icmp::PacketBuffer::new(vec![icmp::PacketMetadata::EMPTY; 4], vec![0; 1024]),
    );
    s.bind(icmp::Endpoint::Ident(0x22)).unwrap();
    let h = g.sockets.add(s);
    let mut sent = false;
    let ok = g.run_until(&mut net, T, |g| {
        let caps = smoltcp::phy::ChecksumCapabilities::default();
        let s = g.sockets.get_mut::<icmp::Socket>(h);
        if !sent {
            let repr = Icmpv4Repr::EchoRequest { ident: 0x22, seq_no: 1, data: b"ping" };
            let buf = s.send(repr.buffer_len(), IpAddress::Ipv4(GW)).unwrap();
            repr.emit(&mut Icmpv4Packet::new_unchecked(buf), &caps);
            sent = true;
        }
        if let Ok((d, _)) = s.recv() {
            let p = Icmpv4Packet::new_checked(d).unwrap();
            let r = Icmpv4Repr::parse(&p, &caps).unwrap();
            return matches!(r, Icmpv4Repr::EchoReply { ident: 0x22, seq_no: 1, data } if data == b"ping");
        }
        false
    });
    assert!(ok);
}

#[test]
fn host_ports_remap_gateway_connections() {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    let server = thread::spawn(move || {
        let (mut s, _) = l.accept().unwrap();
        s.write_all(b"moved").unwrap();
    });
    // The guest dials port 1 (nothing listens there); host_ports sends it to the server.
    let mut net = VirtualNet::new(NetConfig { host_ports: vec![(1, port)], ..Default::default() });
    let mut g = configured_guest(&mut net);
    let h = connect(&mut g, 4096, 1, 52100);
    let mut recvd = Vec::new();
    let ok = g.run_until(&mut net, T, |g| {
        let s = g.sockets.get_mut::<tcp::Socket>(h);
        while s.can_recv() {
            s.recv(|b| {
                recvd.extend_from_slice(b);
                (b.len(), ())
            })
            .unwrap();
        }
        recvd == b"moved"
    });
    assert!(ok, "{recvd:?}");
    server.join().unwrap();
}
