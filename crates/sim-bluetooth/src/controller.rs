use std::collections::VecDeque;

const HANDLE: u16 = 1;
const ACL_SIZE: usize = 251;
const MAX_QUEUED: usize = 256;

/// A single-link BLE 4.0 controller. NimBLE/GATT execute in the guest; this
/// replaces the radio, not the host or the application's security protocol.
pub struct Controller {
    mac: [u8; 6],
    generation: u64,
    event_mask: u64,
    le_event_mask: u64,
    advertising: bool,
    connected: bool,
    advertisement: Vec<u8>,
    scan_response: Vec<u8>,
    host: VecDeque<Vec<u8>>,
    att: VecDeque<Vec<u8>>,
    reassembly: Vec<u8>,
}

impl Controller {
    pub fn new(mac: [u8; 6]) -> Self {
        Self {
            mac,
            generation: 0,
            event_mask: 0x1fff_ffff_ffff,
            le_event_mask: 0x1f,
            advertising: false,
            connected: false,
            advertisement: Vec::new(),
            scan_response: Vec::new(),
            host: VecDeque::new(),
            att: VecDeque::new(),
            reassembly: Vec::new(),
        }
    }

    pub fn reset(&mut self) {
        let next = self.generation.wrapping_add(1);
        *self = Self::new(self.mac);
        self.generation = next;
    }
    pub fn set_address(&mut self, mac: [u8; 6]) {
        self.mac = mac;
        self.reset();
    }
    pub fn generation(&self) -> u64 {
        self.generation
    }
    pub fn advertising(&self) -> bool {
        self.advertising
    }
    pub fn connected(&self) -> bool {
        self.connected
    }
    pub fn advertisement(&self) -> &[u8] {
        &self.advertisement
    }
    pub fn scan_response(&self) -> &[u8] {
        &self.scan_response
    }
    pub fn pop_host_packet(&mut self) -> Option<Vec<u8>> {
        self.host.pop_front()
    }
    pub fn pop_att(&mut self) -> Option<Vec<u8>> {
        self.att.pop_front()
    }

    fn event(&mut self, kind: u8, data: &[u8]) {
        // Command flow-control events are unmaskable. All other events honor
        // the host's classic mask; LE meta events also honor the subevent mask.
        if ![14, 15, 19].contains(&kind) {
            if self.event_mask & (1u64 << (kind - 1)) == 0 {
                return;
            }
            if kind == 0x3e && (data.is_empty() || self.le_event_mask & (1u64 << (data[0] - 1)) == 0) {
                return;
            }
        }
        let mut packet = vec![4, kind, data.len() as u8];
        packet.extend_from_slice(data);
        self.host.push_back(packet);
    }

    fn complete(&mut self, opcode: u16, result: &[u8]) {
        let mut data = vec![1, opcode as u8, (opcode >> 8) as u8];
        data.extend_from_slice(result);
        self.event(14, &data);
    }

    pub fn receive(&mut self, packet: &[u8], random: [u8; 8]) -> Result<(), &'static str> {
        if self.host.len() >= MAX_QUEUED - 2 {
            return Err("controller event queue full");
        }
        match packet.first() {
            Some(1) if packet.len() >= 4 && packet.len() == 4 + packet[3] as usize => {
                self.command(u16::from_le_bytes([packet[1], packet[2]]), &packet[4..], random);
                Ok(())
            }
            Some(2) if packet.len() >= 5 && packet.len() == 5 + u16::from_le_bytes([packet[3], packet[4]]) as usize => {
                self.receive_acl(u16::from_le_bytes([packet[1], packet[2]]), &packet[5..])
            }
            _ => Err("malformed or unsupported H4 packet"),
        }
    }

    fn command(&mut self, op: u16, p: &[u8], random: [u8; 8]) {
        // Lengths and result layouts follow ESP-IDF NimBLE's hci_common.h.
        let expected = match op {
            0x0c03 | 0x1001 | 0x1003 | 0x1009 | 0x2002 | 0x2003 | 0x2007 | 0x2018 | 0x201c => 0,
            0x0c01 | 0x0c63 | 0x2001 => 8,
            0x2006 => 15,
            0x2008 | 0x2009 => 32,
            0x200a => 1,
            0x2016 => 2,
            0x0406 => 3,
            0x2013 => 14,
            _ => {
                self.complete(op, &[1]);
                return;
            }
        };
        if p.len() != expected {
            self.complete(op, &[0x12]);
            return;
        }
        let mut result = vec![0];
        match op {
            0x0c03 => self.reset(),
            0x0c01 => self.event_mask = u64::from_le_bytes(p.try_into().unwrap()),
            0x2001 => self.le_event_mask = u64::from_le_bytes(p.try_into().unwrap()),
            0x2006 => {
                let min = u16::from_le_bytes([p[0], p[1]]);
                let max = u16::from_le_bytes([p[2], p[3]]);
                if self.advertising {
                    result[0] = 0x0c;
                } else if min < 0x20 || max > 0x4000 || min > max || p[13] == 0 || p[13] > 7 {
                    result[0] = 0x12;
                } else if p[4] != 0 || p[5] != 0 || p[14] != 0 {
                    // Only undirected connectable advertising, public own address,
                    // and an unrestricted central are implemented.
                    result[0] = 0x11;
                }
            }
            // Report BLE 4.0 and no optional link features we don't emulate.
            0x1001 => result.extend_from_slice(&[6, 0, 0, 6, 0xff, 0xff, 0, 0]),
            0x1003 => result.extend_from_slice(&[0, 0, 0, 0, 0x60, 0, 0, 0]),
            0x1009 => result.extend(self.mac.iter().rev()),
            0x2002 => result.extend_from_slice(&[ACL_SIZE as u8, 0, 8]),
            0x2003 => result.extend_from_slice(&[0; 8]),
            0x2007 => result.push(0),
            0x2018 => result.extend_from_slice(&random),
            0x201c => result.extend_from_slice(&[4, 1, 0, 0, 0, 0, 0, 0]),
            0x2008 | 0x2009 => {
                if p[0] > 31 {
                    result[0] = 0x12;
                } else {
                    let target = if op == 0x2008 { &mut self.advertisement } else { &mut self.scan_response };
                    *target = p[1..1 + p[0] as usize].to_vec();
                }
            }
            0x200a => {
                if p[0] > 1 {
                    result[0] = 0x12;
                } else if self.connected && p[0] == 1 {
                    result[0] = 0x0c;
                } else {
                    self.advertising = p[0] == 1;
                }
            }
            0x0406 | 0x2016 | 0x2013 => {
                let valid = self.connected && u16::from_le_bytes([p[0], p[1]]) == HANDLE;
                self.event(15, &[if valid { 0 } else { 2 }, 1, op as u8, (op >> 8) as u8]);
                if valid {
                    match op {
                        0x0406 => {
                            let _ = self.disconnect(p[2]);
                        }
                        0x2016 => self.event(0x3e, &[4, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
                        _ => self.event(0x3e, &[3, 0, 1, 0, p[2], p[3], p[6], p[7], p[8], p[9]]),
                    }
                }
                return;
            }
            _ => {}
        }
        self.complete(op, &result);
    }

    pub fn connect(&mut self) -> Result<(), &'static str> {
        if !self.advertising || self.connected {
            return Err("device is not connectable");
        }
        if self.host.len() >= MAX_QUEUED - 1 {
            return Err("controller event queue full");
        }
        self.generation = self.generation.wrapping_add(1);
        self.connected = true;
        self.advertising = false;
        // LE Connection Complete: peripheral role, deterministic mock public address,
        // 30 ms interval, zero latency, 5 second supervision timeout.
        self.event(0x3e, &[1, 0, 1, 0, 1, 0, 0x99, 0, 0, 0, 0, 2, 24, 0, 0, 0, 0xf4, 1, 0]);
        Ok(())
    }

    pub fn disconnect(&mut self, reason: u8) -> Result<(), &'static str> {
        if !self.connected {
            return Err("no connected central");
        }
        self.connected = false;
        self.reassembly.clear();
        self.att.clear();
        // Old ACL packets must never arrive after this link's disconnect.
        self.host.retain(|p| p.first() != Some(&2));
        self.event(5, &[0, 1, 0, reason]);
        Ok(())
    }

    pub fn send_att(&mut self, data: &[u8]) -> Result<(), &'static str> {
        if !self.connected {
            return Err("no connected central");
        }
        if data.is_empty() || data.len() > 517 {
            return Err("ATT packet must contain 1..517 bytes");
        }
        let mut l2cap = vec![data.len() as u8, (data.len() >> 8) as u8, 4, 0];
        l2cap.extend_from_slice(data);
        if self.host.len() + l2cap.len().div_ceil(ACL_SIZE) >= MAX_QUEUED {
            return Err("controller event queue full");
        }
        for (index, chunk) in l2cap.chunks(ACL_SIZE).enumerate() {
            let mut packet = vec![2, 1, if index == 0 { 0x20 } else { 0x10 }, chunk.len() as u8, 0];
            packet.extend_from_slice(chunk);
            self.host.push_back(packet);
        }
        Ok(())
    }

    fn receive_acl(&mut self, handle_flags: u16, data: &[u8]) -> Result<(), &'static str> {
        if !self.connected || handle_flags & 0x0fff != HANDLE {
            return Err("unknown ACL connection");
        }
        if data.is_empty() || data.len() > ACL_SIZE {
            return Err("invalid ACL size");
        }
        let boundary = (handle_flags >> 12) & 3;
        match boundary {
            0 | 2 if data.len() >= 4 && self.reassembly.is_empty() => {}
            1 if !self.reassembly.is_empty() => {}
            _ => return Err("invalid ACL fragment boundary"),
        }
        let expected = if self.reassembly.is_empty() {
            4 + u16::from_le_bytes([data[0], data[1]]) as usize
        } else {
            4 + u16::from_le_bytes([self.reassembly[0], self.reassembly[1]]) as usize
        };
        if expected > 521 || self.reassembly.len() + data.len() > expected || self.att.len() >= MAX_QUEUED {
            self.reassembly.clear();
            return Err("invalid or excessive L2CAP payload");
        }
        self.reassembly.extend_from_slice(data);
        self.event(0x13, &[1, 1, 0, 1, 0]); // Number Of Completed Packets: restore host credit.
        if self.reassembly.len() == expected {
            let packet = std::mem::take(&mut self.reassembly);
            if packet[2..4] == [4, 0] {
                self.att.push_back(packet[4..].to_vec());
            }
        }
        Ok(())
    }
}
