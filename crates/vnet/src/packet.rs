//! Tiny hand-rolled frame builders/parsers for the protocols VirtualNet answers itself
//! (ARP, DHCP, DNS, UDP NAT, ICMP echo, TCP RST).

use std::net::Ipv4Addr;

pub(crate) const ETH_HDR: usize = 14;
pub(crate) const ETHERTYPE_IPV4: u16 = 0x0800;
pub(crate) const ETHERTYPE_ARP: u16 = 0x0806;
pub(crate) const BROADCAST_MAC: [u8; 6] = [0xff; 6];

pub(crate) fn checksum(chunks: &[&[u8]]) -> u16 {
    let mut sum: u32 = 0;
    let mut odd: Option<u8> = None;
    for c in chunks {
        for &b in c.iter() {
            match odd.take() {
                Some(hi) => sum += u32::from(u16::from_be_bytes([hi, b])),
                None => odd = Some(b),
            }
        }
    }
    if let Some(hi) = odd {
        sum += u32::from(u16::from_be_bytes([hi, 0]));
    }
    while sum > 0xffff {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}

pub(crate) fn eth_header(buf: &mut Vec<u8>, dst: [u8; 6], src: [u8; 6], ethertype: u16) {
    buf.extend_from_slice(&dst);
    buf.extend_from_slice(&src);
    buf.extend_from_slice(&ethertype.to_be_bytes());
}

/// Build an IPv4 packet (no options) with the given L4 payload (already checksummed).
pub(crate) fn ipv4_packet(src: Ipv4Addr, dst: Ipv4Addr, proto: u8, l4: &[u8]) -> Vec<u8> {
    let total = 20 + l4.len();
    let mut h = [0u8; 20];
    h[0] = 0x45;
    h[2..4].copy_from_slice(&(total as u16).to_be_bytes());
    h[6] = 0x40; // DF
    h[8] = 64;
    h[9] = proto;
    h[12..16].copy_from_slice(&src.octets());
    h[16..20].copy_from_slice(&dst.octets());
    let c = checksum(&[&h]);
    h[10..12].copy_from_slice(&c.to_be_bytes());
    let mut v = Vec::with_capacity(total);
    v.extend_from_slice(&h);
    v.extend_from_slice(l4);
    v
}

fn pseudo(src: Ipv4Addr, dst: Ipv4Addr, proto: u8, len: usize) -> [u8; 12] {
    let mut p = [0u8; 12];
    p[0..4].copy_from_slice(&src.octets());
    p[4..8].copy_from_slice(&dst.octets());
    p[9] = proto;
    p[10..12].copy_from_slice(&(len as u16).to_be_bytes());
    p
}

pub(crate) fn udp_ip_packet(src: Ipv4Addr, sport: u16, dst: Ipv4Addr, dport: u16, payload: &[u8]) -> Vec<u8> {
    let len = 8 + payload.len();
    let mut u = Vec::with_capacity(len);
    u.extend_from_slice(&sport.to_be_bytes());
    u.extend_from_slice(&dport.to_be_bytes());
    u.extend_from_slice(&(len as u16).to_be_bytes());
    u.extend_from_slice(&[0, 0]);
    u.extend_from_slice(payload);
    let mut c = checksum(&[&pseudo(src, dst, 17, len), &u]);
    if c == 0 {
        c = 0xffff;
    }
    u[6..8].copy_from_slice(&c.to_be_bytes());
    ipv4_packet(src, dst, 17, &u)
}

pub(crate) fn tcp_rst_ip_packet(src: Ipv4Addr, sport: u16, dst: Ipv4Addr, dport: u16, seq: u32, ack: u32) -> Vec<u8> {
    let mut t = [0u8; 20];
    t[0..2].copy_from_slice(&sport.to_be_bytes());
    t[2..4].copy_from_slice(&dport.to_be_bytes());
    t[4..8].copy_from_slice(&seq.to_be_bytes());
    t[8..12].copy_from_slice(&ack.to_be_bytes());
    t[12] = 5 << 4;
    t[13] = 0x14; // RST|ACK
    let c = checksum(&[&pseudo(src, dst, 6, 20), &t]);
    t[16..18].copy_from_slice(&c.to_be_bytes());
    ipv4_packet(src, dst, 6, &t)
}

/// Parsed view of an IPv4 packet.
pub(crate) struct Ipv4<'a> {
    pub src: Ipv4Addr,
    pub dst: Ipv4Addr,
    pub proto: u8,
    /// Whole IP packet (header + payload), trimmed to total length.
    pub packet: &'a [u8],
    pub payload: &'a [u8],
}

pub(crate) fn parse_ipv4(p: &[u8]) -> Option<Ipv4<'_>> {
    if p.len() < 20 || p[0] >> 4 != 4 {
        return None;
    }
    let ihl = usize::from(p[0] & 0x0f) * 4;
    let total = usize::from(u16::from_be_bytes([p[2], p[3]]));
    if ihl < 20 || total < ihl || total > p.len() {
        return None;
    }
    // Drop fragments (MF set or non-zero offset); lwIP won't fragment at MTU 1500.
    let frag = u16::from_be_bytes([p[6], p[7]]);
    if frag & 0x3fff != 0 {
        return None;
    }
    Some(Ipv4 {
        src: Ipv4Addr::new(p[12], p[13], p[14], p[15]),
        dst: Ipv4Addr::new(p[16], p[17], p[18], p[19]),
        proto: p[9],
        packet: &p[..total],
        payload: &p[ihl..total],
    })
}

// ---------------------------------------------------------------- DHCP

pub(crate) const DHCP_DISCOVER: u8 = 1;
pub(crate) const DHCP_OFFER: u8 = 2;
pub(crate) const DHCP_REQUEST: u8 = 3;
pub(crate) const DHCP_DECLINE: u8 = 4;
pub(crate) const DHCP_ACK: u8 = 5;
pub(crate) const DHCP_NAK: u8 = 6;
pub(crate) const DHCP_RELEASE: u8 = 7;
pub(crate) const DHCP_INFORM: u8 = 8;

pub(crate) struct DhcpMsg {
    pub op: u8,
    pub xid: u32,
    pub flags: u16,
    pub ciaddr: Ipv4Addr,
    pub giaddr: Ipv4Addr,
    pub chaddr: [u8; 6],
    pub msg_type: u8,
    pub requested_ip: Option<Ipv4Addr>,
    pub server_id: Option<Ipv4Addr>,
}

const DHCP_MAGIC: [u8; 4] = [99, 130, 83, 99];

fn opt_ip(d: &[u8]) -> Option<Ipv4Addr> {
    (d.len() == 4).then(|| Ipv4Addr::new(d[0], d[1], d[2], d[3]))
}

pub(crate) fn parse_dhcp(p: &[u8]) -> Option<DhcpMsg> {
    if p.len() < 240 || p[1] != 1 || p[2] != 6 || p[236..240] != DHCP_MAGIC {
        return None;
    }
    let ip = |o: usize| Ipv4Addr::new(p[o], p[o + 1], p[o + 2], p[o + 3]);
    let mut m = DhcpMsg {
        op: p[0],
        xid: u32::from_be_bytes([p[4], p[5], p[6], p[7]]),
        flags: u16::from_be_bytes([p[10], p[11]]),
        ciaddr: ip(12),
        giaddr: ip(24),
        chaddr: p[28..34].try_into().unwrap(),
        msg_type: 0,
        requested_ip: None,
        server_id: None,
    };
    let mut i = 240;
    while i < p.len() {
        let kind = p[i];
        if kind == 0 {
            i += 1;
            continue;
        }
        if kind == 255 || i + 1 >= p.len() {
            break;
        }
        let len = usize::from(p[i + 1]);
        let Some(d) = p.get(i + 2..i + 2 + len) else {
            break;
        };
        match kind {
            53 if len == 1 => m.msg_type = d[0],
            50 => m.requested_ip = opt_ip(d),
            54 => m.server_id = opt_ip(d),
            _ => {}
        }
        i += 2 + len;
    }
    (m.msg_type != 0).then_some(m)
}

pub(crate) struct DhcpReply<'a> {
    pub req: &'a DhcpMsg,
    pub msg_type: u8,
    pub yiaddr: Ipv4Addr,
    pub server_ip: Ipv4Addr,
    /// (subnet mask, router, dns, lease secs); omitted for NAK.
    pub params: Option<(Ipv4Addr, Ipv4Addr, Ipv4Addr, u32)>,
}

pub(crate) fn build_dhcp_reply(r: &DhcpReply<'_>) -> Vec<u8> {
    let mut p = vec![0u8; 240];
    p[0] = 2; // BOOTREPLY
    p[1] = 1;
    p[2] = 6;
    p[4..8].copy_from_slice(&r.req.xid.to_be_bytes());
    p[10..12].copy_from_slice(&r.req.flags.to_be_bytes());
    if r.msg_type != DHCP_NAK {
        p[12..16].copy_from_slice(&r.req.ciaddr.octets());
        p[16..20].copy_from_slice(&r.yiaddr.octets());
        p[20..24].copy_from_slice(&r.server_ip.octets());
    }
    p[24..28].copy_from_slice(&r.req.giaddr.octets());
    p[28..34].copy_from_slice(&r.req.chaddr);
    p[236..240].copy_from_slice(&DHCP_MAGIC);
    p.extend_from_slice(&[53, 1, r.msg_type]);
    p.extend_from_slice(&[54, 4]);
    p.extend_from_slice(&r.server_ip.octets());
    if let Some((mask, router, dns, lease)) = r.params {
        p.extend_from_slice(&[51, 4]);
        p.extend_from_slice(&lease.to_be_bytes());
        p.extend_from_slice(&[58, 4]);
        p.extend_from_slice(&(lease / 2).to_be_bytes());
        p.extend_from_slice(&[59, 4]);
        p.extend_from_slice(&(lease / 8 * 7).to_be_bytes());
        p.extend_from_slice(&[1, 4]);
        p.extend_from_slice(&mask.octets());
        p.extend_from_slice(&[3, 4]);
        p.extend_from_slice(&router.octets());
        p.extend_from_slice(&[6, 4]);
        p.extend_from_slice(&dns.octets());
        let bcast = Ipv4Addr::from(u32::from(r.yiaddr) | !u32::from(mask));
        p.extend_from_slice(&[28, 4]);
        p.extend_from_slice(&bcast.octets());
    }
    p.push(255);
    // Pad to the classic 300-byte BOOTP minimum.
    if p.len() < 300 {
        p.resize(300, 0);
    }
    p
}

// ---------------------------------------------------------------- DNS

pub(crate) struct DnsQuery {
    pub id: u16,
    pub flags: u16,
    pub name: String,
    pub qtype: u16,
    pub qclass: u16,
    /// Raw question section bytes (name + type + class), echoed in the response.
    pub question: Vec<u8>,
}

pub(crate) fn parse_dns_query(p: &[u8]) -> Option<DnsQuery> {
    if p.len() < 12 {
        return None;
    }
    let flags = u16::from_be_bytes([p[2], p[3]]);
    let qd = u16::from_be_bytes([p[4], p[5]]);
    if flags & 0x8000 != 0 || qd == 0 {
        return None;
    }
    let mut i = 12;
    let mut labels: Vec<String> = Vec::new();
    loop {
        let l = usize::from(*p.get(i)?);
        if l == 0 {
            i += 1;
            break;
        }
        if l & 0xc0 != 0 {
            return None; // no compression in queries
        }
        let lab = p.get(i + 1..i + 1 + l)?;
        labels.push(String::from_utf8_lossy(lab).into_owned());
        i += 1 + l;
    }
    let tail = p.get(i..i + 4)?;
    Some(DnsQuery {
        id: u16::from_be_bytes([p[0], p[1]]),
        flags,
        name: labels.join("."),
        qtype: u16::from_be_bytes([tail[0], tail[1]]),
        qclass: u16::from_be_bytes([tail[2], tail[3]]),
        question: p[12..i + 4].to_vec(),
    })
}

pub(crate) const RCODE_NOERROR: u16 = 0;
pub(crate) const RCODE_SERVFAIL: u16 = 2;
pub(crate) const RCODE_NXDOMAIN: u16 = 3;
pub(crate) const RCODE_NOTIMP: u16 = 4;

pub(crate) fn build_dns_response(q: &DnsQuery, rcode: u16, answers: &[Ipv4Addr]) -> Vec<u8> {
    let mut p = Vec::with_capacity(64 + answers.len() * 16);
    p.extend_from_slice(&q.id.to_be_bytes());
    // QR=1, opcode copied, AA=0, RD copied, RA=1
    let flags = 0x8000 | (q.flags & 0x7900) | 0x0080 | (rcode & 0xf);
    p.extend_from_slice(&flags.to_be_bytes());
    p.extend_from_slice(&1u16.to_be_bytes());
    p.extend_from_slice(&(answers.len() as u16).to_be_bytes());
    p.extend_from_slice(&[0, 0, 0, 0]);
    p.extend_from_slice(&q.question);
    for a in answers {
        p.extend_from_slice(&[0xc0, 0x0c]); // pointer to the question name
        p.extend_from_slice(&1u16.to_be_bytes()); // A
        p.extend_from_slice(&1u16.to_be_bytes()); // IN
        p.extend_from_slice(&60u32.to_be_bytes()); // TTL
        p.extend_from_slice(&4u16.to_be_bytes());
        p.extend_from_slice(&a.octets());
    }
    p
}
