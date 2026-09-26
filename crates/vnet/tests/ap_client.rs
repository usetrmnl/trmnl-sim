mod common;

use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::thread;
use std::time::Duration;

use common::*;
use smoltcp::iface::SocketHandle;
use smoltcp::socket::{tcp, udp};
use smoltcp::wire::{DhcpMessageType, DhcpPacket, DhcpRepr, IpAddress, IpEndpoint};
use vnet::ApClient;

const AP_IP: Ipv4Addr = Ipv4Addr::new(192, 168, 4, 1);
const CLIENT_MAC: [u8; 6] = [0x02, 0xaa, 0xbb, 0xcc, 0xdd, 0x01];

/// Minimal DHCP server on the fake AP: offers/acks 192.168.4.2 to anyone.
fn serve_dhcp(g: &mut Guest, h: SocketHandle) {
    let s = g.sockets.get_mut::<udp::Socket>(h);
    let mut reply = None;
    if let Ok((d, _)) = s.recv() {
        let pkt = DhcpPacket::new_checked(d).unwrap();
        let req = DhcpRepr::parse(&pkt).unwrap();
        let mt = match req.message_type {
            DhcpMessageType::Discover => DhcpMessageType::Offer,
            DhcpMessageType::Request => DhcpMessageType::Ack,
            _ => return,
        };
        let r = DhcpRepr {
            message_type: mt,
            transaction_id: req.transaction_id,
            secs: 0,
            client_hardware_address: req.client_hardware_address,
            client_ip: Ipv4Addr::UNSPECIFIED,
            your_ip: Ipv4Addr::new(192, 168, 4, 2),
            server_ip: AP_IP,
            router: Some(AP_IP),
            subnet_mask: Some(Ipv4Addr::new(255, 255, 255, 0)),
            relay_agent_ip: Ipv4Addr::UNSPECIFIED,
            broadcast: false,
            requested_ip: None,
            client_identifier: None,
            server_identifier: Some(AP_IP),
            parameter_request_list: None,
            dns_servers: None,
            max_size: None,
            lease_duration: Some(7200),
            renew_duration: None,
            rebind_duration: None,
            additional_options: &[],
        };
        let mut buf = vec![0u8; r.buffer_len()];
        r.emit(&mut DhcpPacket::new_unchecked(&mut buf[..])).unwrap();
        reply = Some(buf);
    }
    if let Some(buf) = reply {
        s.send_slice(&buf, IpEndpoint::new(IpAddress::v4(255, 255, 255, 255), 68)).unwrap();
    }
}

fn ap_guest(with_dhcp: bool) -> (Guest, Option<SocketHandle>, SocketHandle) {
    let mut g = Guest::new(GUEST_MAC);
    g.set_ip(AP_IP, 24, None);
    let dhcp = with_dhcp.then(|| {
        let mut s = udp::Socket::new(
            udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY; 4], vec![0; 4096]),
            udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY; 4], vec![0; 4096]),
        );
        s.bind(67).unwrap();
        g.sockets.add(s)
    });
    let mut srv = tcp_socket(4096, 8192);
    srv.listen(80).unwrap();
    let srv = g.sockets.add(srv);
    (g, dhcp, srv)
}

struct Http {
    req: Vec<u8>,
    off: usize,
    body: Vec<u8>,
}

/// Tiny HTTP server on the fake AP: after a full request, send `body` and close.
fn serve_http(g: &mut Guest, h: &mut SocketHandle, st: &mut Http, served: &mut usize) {
    let s = g.sockets.get_mut::<tcp::Socket>(*h);
    while s.can_recv() {
        s.recv(|b| {
            st.req.extend_from_slice(b);
            (b.len(), ())
        })
        .unwrap();
    }
    if st.req.ends_with(b"\r\n\r\n") && st.off < st.body.len() && s.can_send() {
        st.off += s.send_slice(&st.body[st.off..]).unwrap();
        if st.off == st.body.len() {
            s.close();
        }
    }
    if matches!(s.state(), tcp::State::Closed | tcp::State::TimeWait) && st.off == st.body.len() {
        // Re-arm a fresh listener for the next request.
        *served += 1;
        g.sockets.remove(*h);
        let mut srv = tcp_socket(4096, 8192);
        srv.listen(80).unwrap();
        *h = g.sockets.add(srv);
        st.req.clear();
        st.off = 0;
    }
}

fn run_ap_test(ap: &mut ApClient, mut g: Guest, dhcp: Option<SocketHandle>, mut srv: SocketHandle) {
    let addr: SocketAddr = ap.listen_addrs()[0];
    let body = pattern(30_000, 9);
    let mut st = Http { req: vec![], off: 0, body: body.clone() };
    // Two sequential browser requests; with DHCP the first is made before the lease exists.
    for round in 0..2 {
        let client = thread::spawn(move || {
            let mut c = TcpStream::connect(addr).unwrap();
            c.write_all(b"GET / HTTP/1.0\r\n\r\n").unwrap();
            let mut v = Vec::new();
            c.read_to_end(&mut v).unwrap();
            v
        });
        let mut served = 0;
        let ok = g.run_until(ap, Duration::from_secs(20), |g| {
            if let Some(d) = dhcp {
                serve_dhcp(g, d);
            }
            serve_http(g, &mut srv, &mut st, &mut served);
            served > 0 && client.is_finished()
        });
        assert!(ok, "round {round} timed out");
        let got = client.join().unwrap();
        assert!(got == body, "round {round}: got {} bytes", got.len());
    }
    assert_eq!(ap.guest_ap_ip(), Some(AP_IP));
}

#[test]
fn ap_client_dhcp_and_forward() {
    let mut ap = ApClient::new(CLIENT_MAC, vec![("127.0.0.1:0".parse().unwrap(), 80)]).unwrap();
    assert_eq!(ap.client_ip(), None);
    let (g, dhcp, srv) = ap_guest(true);
    run_ap_test(&mut ap, g, dhcp, srv);
    assert_eq!(ap.client_ip(), Some(Ipv4Addr::new(192, 168, 4, 2)));
    ap.reset();
    assert_eq!(ap.client_ip(), None);
    assert_eq!(ap.guest_ap_ip(), None);
}

#[test]
fn ap_client_static_forward() {
    let mut ap = ApClient::with_static_ip(
        CLIENT_MAC,
        vec![("127.0.0.1:0".parse().unwrap(), 80)],
        Ipv4Addr::new(192, 168, 4, 9),
        24,
        AP_IP,
    )
    .unwrap();
    let (g, dhcp, srv) = ap_guest(false);
    run_ap_test(&mut ap, g, dhcp, srv);
    assert_eq!(ap.client_ip(), Some(Ipv4Addr::new(192, 168, 4, 9)));
}

#[test]
fn ap_client_retries_discover() {
    let mut ap = ApClient::new(CLIENT_MAC, vec![]).unwrap();
    let (mut g, dhcp, _srv) = ap_guest(true);
    let dhcp = dhcp.unwrap();
    // DHCP server "not up yet": swallow the first DISCOVERs.
    g.run_until(&mut ap, Duration::from_millis(2500), |g| {
        let _ = g.sockets.get_mut::<udp::Socket>(dhcp).recv();
        false
    });
    assert_eq!(ap.client_ip(), None);
    let start = std::time::Instant::now();
    while ap.client_ip().is_none() && start.elapsed() < Duration::from_secs(5) {
        g.step(&mut ap);
        serve_dhcp(&mut g, dhcp);
        thread::sleep(Duration::from_micros(200));
    }
    assert!(start.elapsed() < Duration::from_secs(3), "retry too slow");
    assert_eq!(ap.client_ip(), Some(Ipv4Addr::new(192, 168, 4, 2)));
}
