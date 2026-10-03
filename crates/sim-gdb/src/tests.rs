use std::io::{BufRead, BufReader};
use std::sync::Arc;

use sim_api::{Frame, SimPorts};

use super::*;

fn fake_target() -> DebugTarget {
    DebugTarget { arch: "riscv:rv32", cores: 1, target_xml: Some("<target/>") }
}

/// A stand-in for the runner: one core with 33 registers (pc = 0x42000000 + 2 per step) and
/// 16 bytes of memory at 0x3fc80000.
fn fake_emulator(ports: SimPorts) {
    let mut events: Option<Sender<DebugStop>> = None;
    let mut pc = 0x4200_0000u32;
    let mut mem = [0u8; 16];
    let mut breakpoint = None;
    while let Ok(cmd) = ports.commands.recv() {
        let Command::Debug { request, reply } = cmd else { continue };
        let stop = |events: &Option<Sender<DebugStop>>, reason| {
            events.as_ref().unwrap().send(DebugStop { core: 0, reason }).unwrap();
        };
        let r = match request {
            DebugRequest::Attach { events: e } => {
                events = Some(e);
                stop(&events, StopReason::Interrupted);
                DebugReply::Target(fake_target())
            }
            DebugRequest::Detach => DebugReply::Ok,
            DebugRequest::Interrupt => {
                stop(&events, StopReason::Interrupted);
                DebugReply::Ok
            }
            DebugRequest::Target => DebugReply::Target(fake_target()),
            DebugRequest::Resume { step: Some(_) } => {
                pc += 2;
                stop(&events, StopReason::Step);
                DebugReply::Ok
            }
            DebugRequest::Resume { step: None } => {
                if let Some(b) = breakpoint {
                    pc = b;
                    stop(&events, StopReason::Breakpoint);
                }
                DebugReply::Ok
            }
            DebugRequest::ReadRegisters { .. } => {
                let mut regs = vec![RegValue::Value(vec![0; 4]); 32];
                regs.push(RegValue::Value(pc.to_le_bytes().to_vec()));
                DebugReply::Registers(regs)
            }
            DebugRequest::ReadRegister { n, .. } => DebugReply::Register(match n {
                32 => Some(RegValue::Value(pc.to_le_bytes().to_vec())),
                833 => Some(RegValue::Unavailable(4)),
                _ => None,
            }),
            DebugRequest::WriteRegister { n: 32, value, .. } => {
                pc = u32::from_le_bytes(value.try_into().unwrap());
                DebugReply::Ok
            }
            DebugRequest::WriteRegister { .. } => DebugReply::Error("no".into()),
            DebugRequest::ReadMemory { addr, len } => {
                let off = addr.wrapping_sub(0x3fc8_0000) as usize;
                DebugReply::Memory(mem.get(off..).map_or(vec![], |m| m[..len.min(m.len())].to_vec()))
            }
            DebugRequest::WriteMemory { addr, data } => {
                let off = addr.wrapping_sub(0x3fc8_0000) as usize;
                mem[off..off + data.len()].copy_from_slice(&data);
                DebugReply::Ok
            }
            DebugRequest::Breakpoint { addr, set } => {
                breakpoint = set.then_some(addr);
                DebugReply::Ok
            }
            DebugRequest::Watchpoint { .. } => DebugReply::Ok,
        };
        reply.send(r).unwrap();
    }
}

struct Client {
    stream: TcpStream,
    reader: BufReader<TcpStream>,
}

impl Client {
    fn connect() -> Client {
        let frame = Arc::new(parking_lot::Mutex::new(Frame::new(1, 1)));
        let (handle, ports) = sim_api::channel(frame);
        std::thread::spawn(move || fake_emulator(ports));
        let addr = serve("127.0.0.1:0", handle).unwrap();
        let stream = TcpStream::connect(addr).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let reader = BufReader::new(stream.try_clone().unwrap());
        Client { stream, reader }
    }

    /// Send a packet; return the reply's payload (after its ack, when acks are on).
    fn ask(&mut self, p: &str) -> String {
        let sum = p.bytes().fold(0u8, |a, c| a.wrapping_add(c));
        self.stream.write_all(format!("${p}#{sum:02x}").as_bytes()).unwrap();
        self.reply()
    }

    fn reply(&mut self) -> String {
        let mut junk = Vec::new();
        self.reader.read_until(b'$', &mut junk).unwrap();
        let mut body = Vec::new();
        self.reader.read_until(b'#', &mut body).unwrap();
        body.pop();
        let mut sum = [0u8; 2];
        self.reader.read_exact(&mut sum).unwrap();
        let body = String::from_utf8(body).unwrap();
        let expect = body.bytes().fold(0u8, |a, c| a.wrapping_add(c));
        assert_eq!(std::str::from_utf8(&sum).unwrap(), format!("{expect:02x}"), "checksum of {body}");
        body
    }
}

#[test]
fn session_reads_and_writes_registers_and_memory() {
    let mut c = Client::connect();
    assert!(c.ask("qSupported:multiprocess+;swbreak+").contains("qXfer:features:read+"));
    assert_eq!(c.ask("QStartNoAckMode"), "OK");
    assert_eq!(c.ask("?"), "T02thread:1;");
    let g = c.ask("g");
    assert_eq!((g.len(), &g[256..]), (33 * 8, "00000042"));
    assert_eq!(c.ask("p20"), "00000042");
    assert_eq!(c.ask("p341"), "xxxxxxxx");
    assert_eq!(c.ask("p99"), "E01");
    assert_eq!(c.ask("P20=10000042"), "OK");
    assert_eq!(c.ask("p20"), "10000042");
    assert_eq!(c.ask("M3fc80004,2:abcd"), "OK");
    assert_eq!(c.ask("m3fc80003,4"), "00abcd00");
    assert_eq!(c.ask("m3fc8000e,8"), "0000", "a read stops at the end of memory");
    assert_eq!(c.ask("m0,4"), "E14");
    assert_eq!(c.ask("qXfer:features:read:target.xml:0,4"), "m<tar");
    assert_eq!(c.ask("qXfer:features:read:target.xml:4,100"), "lget/>");
    assert_eq!(c.ask("qfThreadInfo"), "m1");
    assert_eq!(c.ask("qsThreadInfo"), "l");
    assert_eq!(c.ask("X0,0:"), "", "binary writes are left to M");
}

#[test]
fn session_steps_continues_to_breakpoints_and_interrupts() {
    let mut c = Client::connect();
    c.ask("QStartNoAckMode");
    assert_eq!(c.ask("vCont?"), "vCont;c;C;s;S");
    assert_eq!(c.ask("vCont;s:1;c"), "T05thread:1;");
    assert_eq!(c.ask("p20"), "02000042");
    assert_eq!(c.ask("Z0,42000100,2"), "OK");
    assert_eq!(c.ask("c"), "T05thread:1;");
    assert_eq!(c.ask("p20"), "00010042");
    assert_eq!(c.ask("z0,42000100,2"), "OK");
    // Without a breakpoint the target runs until interrupted.
    c.stream.write_all(b"$c#63").unwrap();
    std::thread::sleep(Duration::from_millis(50));
    c.stream.write_all(&[3]).unwrap();
    assert_eq!(c.reply(), "T02thread:1;");
    assert_eq!(c.ask("D"), "OK");
}

#[test]
fn acks_until_no_ack_mode() {
    let mut c = Client::connect();
    c.stream.write_all(b"$?#3f").unwrap();
    let mut ack = [0u8; 1];
    c.reader.read_exact(&mut ack).unwrap();
    assert_eq!(&ack, b"+");
    assert_eq!(c.reply(), "T02thread:1;");
    // A corrupt packet is NAKed.
    c.stream.write_all(b"$?#00").unwrap();
    c.reader.read_exact(&mut ack).unwrap();
    assert_eq!(&ack, b"-");
}

#[test]
fn framer_unescapes_and_reports_interrupts() {
    let mut f = Framer::default();
    let mut out = Vec::new();
    for &b in b"+\x03$X0,1:}]#00" {
        f.push(b, &mut out);
    }
    assert_eq!(out[0], Incoming::Interrupt);
    assert_eq!(out[1], Incoming::Corrupt);
    let sum = b"X0,1:}]".iter().fold(0u8, |a, &c| a.wrapping_add(c));
    out.clear();
    for &b in format!("$X0,1:}}]#{sum:02x}").as_bytes() {
        f.push(b, &mut out);
    }
    assert_eq!(out, vec![Incoming::Packet(b"X0,1:}".to_vec())]);
}

#[test]
fn stop_replies_name_the_watchpoint_and_signal() {
    let watch = DebugStop { core: 1, reason: StopReason::Watchpoint { kind: WatchKind::Read, addr: 0x3fc8_0000 } };
    assert_eq!(stop_reply(&watch).0, "T05thread:2;rwatch:3fc80000;");
    let fault = DebugStop { core: 0, reason: StopReason::Fault { signal: 11, description: "load".into() } };
    assert_eq!(stop_reply(&fault), ("T0bthread:1;".into(), Some("load\n".into())));
}
