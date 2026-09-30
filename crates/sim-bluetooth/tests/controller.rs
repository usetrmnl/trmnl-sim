use sim_bluetooth::Controller;

fn command(c: &mut Controller, opcode: u16, data: &[u8]) -> Vec<u8> {
    let mut packet = vec![1, opcode as u8, (opcode >> 8) as u8, data.len() as u8];
    packet.extend_from_slice(data);
    c.receive(&packet, [7; 8]).unwrap();
    c.pop_host_packet().unwrap()
}

#[test]
fn host_initialization_reports_real_limits_and_address_order() {
    let mut c = Controller::new([1, 2, 3, 4, 5, 6]);
    assert_eq!(command(&mut c, 0x0c03, &[]), [4, 14, 4, 1, 3, 12, 0]);
    assert_eq!(command(&mut c, 0x1009, &[]), [4, 14, 10, 1, 9, 16, 0, 6, 5, 4, 3, 2, 1]);
    assert_eq!(command(&mut c, 0x2002, &[]), [4, 14, 7, 1, 2, 32, 0, 251, 0, 8]);
    assert_eq!(command(&mut c, 0xffff, &[]), [4, 14, 4, 1, 255, 255, 1]);
    assert_eq!(command(&mut c, 0x200a, &[2]).last(), Some(&0x12));
}

#[test]
fn malformed_packets_cannot_mutate_advertising_state() {
    let mut c = Controller::new([0; 6]);
    for packet in [&[][..], &[1], &[1, 10, 32, 2, 1], &[1, 10, 32, 0, 1], &[9, 0]] {
        assert!(c.receive(packet, [0; 8]).is_err());
        assert!(!c.advertising());
    }
    assert_eq!(command(&mut c, 0x2008, &[32; 32]).last(), Some(&0x12));
    assert!(c.advertisement().is_empty());
}

#[test]
fn advertising_and_connection_have_explicit_lifecycle() {
    let mut c = Controller::new([0; 6]);
    assert!(c.connect().is_err());
    let mut adv = [0; 32];
    adv[..4].copy_from_slice(&[3, 2, 1, 6]);
    command(&mut c, 0x2008, &adv);
    command(&mut c, 0x200a, &[1]);
    assert_eq!(c.advertisement(), [2, 1, 6]);
    command(&mut c, 0x0c01, &[255; 8]);
    c.connect().unwrap();
    assert!(!c.advertising());
    assert!(c.connected());
    assert!(c.connect().is_err());
    let event = c.pop_host_packet().unwrap();
    assert_eq!(&event[..8], [4, 0x3e, 19, 1, 0, 1, 0, 1]);
    c.disconnect(0x13).unwrap();
    assert_eq!(c.pop_host_packet().unwrap(), [4, 5, 4, 0, 1, 0, 0x13]);
    assert!(!c.connected());
    assert!(c.disconnect(0x13).is_err());
    c.reset();
    assert!(c.advertisement().is_empty());
    assert!(c.pop_host_packet().is_none());
}

#[test]
fn att_uses_acl_framing_and_reassembles_guest_fragments() {
    let mut c = Controller::new([0; 6]);
    command(&mut c, 0x200a, &[1]);
    c.connect().unwrap();
    c.pop_host_packet();
    c.send_att(&[0x0a, 3, 0]).unwrap();
    assert_eq!(c.pop_host_packet().unwrap(), [2, 1, 0x20, 7, 0, 3, 0, 4, 0, 0x0a, 3, 0]);
    c.receive(&[2, 1, 0, 6, 0, 4, 0, 4, 0, 0x0b, 42], [0; 8]).unwrap();
    assert!(c.pop_att().is_none());
    c.receive(&[2, 1, 0x10, 2, 0, 43, 44], [0; 8]).unwrap();
    assert_eq!(c.pop_att().unwrap(), [0x0b, 42, 43, 44]);
    c.disconnect(0x13).unwrap();
    assert!(c.send_att(&[0x0a, 3, 0]).is_err());
}

#[test]
fn reset_and_reconnect_invalidate_connection_tokens() {
    let mut c = Controller::new([0; 6]);
    command(&mut c, 0x200a, &[1]);
    c.connect().unwrap();
    let first = c.generation();
    c.disconnect(0x13).unwrap();
    command(&mut c, 0x200a, &[1]);
    c.connect().unwrap();
    assert_ne!(first, c.generation());
    let second = c.generation();
    c.reset();
    assert_ne!(second, c.generation());
    assert!(c.pop_host_packet().is_none());
}

#[test]
fn unsupported_scanning_and_whitelist_commands_do_not_claim_success() {
    let mut c = Controller::new([0; 6]);
    for (op, parameters) in [
        (0x200b, vec![0; 7]),
        (0x200c, vec![0; 2]),
        (0x200f, vec![]),
        (0x2010, vec![]),
        (0x2011, vec![0; 7]),
        (0x2012, vec![0; 7]),
    ] {
        assert_eq!(command(&mut c, op, &parameters).last(), Some(&1));
    }
    // Supported states: connectable advertising and peripheral connection only.
    assert_eq!(&command(&mut c, 0x201c, &[])[7..], &[4, 1, 0, 0, 0, 0, 0, 0]);
}

#[test]
fn saturated_acl_queue_can_always_disconnect() {
    let mut c = Controller::new([0; 6]);
    command(&mut c, 0x200a, &[1]);
    c.connect().unwrap();
    while c.send_att(&[0x52, 3, 0, 1]).is_ok() {}
    c.disconnect(0x13).unwrap();
    assert!(!c.connected());
    while let Some(packet) = c.pop_host_packet() {
        assert_ne!(packet[0], 2);
    }
}

#[test]
fn unsupported_advertisement_modes_and_random_address_are_rejected() {
    let mut c = Controller::new([0; 6]);
    let mut parameters = [0; 15];
    parameters[..4].copy_from_slice(&[0xa0, 0, 0xa0, 0]);
    parameters[13] = 7;
    assert_eq!(command(&mut c, 0x2006, &parameters).last(), Some(&0));
    for (index, value) in [(4, 3), (5, 1), (14, 1)] {
        let mut unsupported = parameters;
        unsupported[index] = value;
        assert_eq!(command(&mut c, 0x2006, &unsupported).last(), Some(&0x11));
    }
    assert_eq!(command(&mut c, 0x2005, &[0; 6]).last(), Some(&1));
}

#[test]
fn event_masks_suppress_unrequested_connection_events() {
    let mut c = Controller::new([0; 6]);
    command(&mut c, 0x0c01, &[0; 8]);
    command(&mut c, 0x2001, &[0; 8]);
    command(&mut c, 0x200a, &[1]);
    c.connect().unwrap();
    assert!(c.pop_host_packet().is_none());
    c.disconnect(0x13).unwrap();
    assert!(c.pop_host_packet().is_none());
}

#[test]
fn replacing_address_resets_link_and_invalidates_tokens() {
    let mut c = Controller::new([0; 6]);
    command(&mut c, 0x200a, &[1]);
    c.connect().unwrap();
    let token = c.generation();
    c.set_address([1, 2, 3, 4, 5, 6]);
    assert!(!c.connected());
    assert_ne!(token, c.generation());
    assert_eq!(&command(&mut c, 0x1009, &[])[7..], &[6, 5, 4, 3, 2, 1]);
}
