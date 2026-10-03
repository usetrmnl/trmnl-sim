//! A GDB remote serial protocol stub (`--gdb ADDR`): `target remote ADDR` in the toolchain's
//! GDB (`riscv32-esp-elf-gdb`, `xtensa-esp32s3-elf-gdb`) debugs the firmware running in the
//! simulator. One debugger at a time; it stops the whole machine while it has control.
//!
//! Threads are the SoC's cores (thread 1 = core 0). Everything chip-specific (register
//! numbering, the target description) comes from the emulator through `sim_api`'s debug
//! requests, so this crate is only the protocol.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender, select};
use sim_api::{Command, DebugReply, DebugRequest, DebugStop, DebugTarget, RegValue, SimHandle, StopReason, WatchKind};

/// Listen for GDB on `addr` (e.g. `127.0.0.1:3333`; port 0 picks one) on a thread of its own.
pub fn serve(addr: &str, sim: SimHandle) -> std::io::Result<SocketAddr> {
    let listener = TcpListener::bind(addr)?;
    let local = listener.local_addr()?;
    std::thread::Builder::new().name("gdb".into()).spawn(move || {
        for conn in listener.incoming() {
            let Ok(stream) = conn else { continue };
            let peer = stream.peer_addr().map_or("?".into(), |a| a.to_string());
            sim.console.lock().push_sim(&format!("gdb: attached from {peer}"));
            let end = match Session::start(stream, &sim) {
                Ok(mut s) => s.run(),
                Err(e) => Err(e),
            };
            // Whatever ended the session, the target runs on without the debugger.
            let _ = request(&sim.commands, DebugRequest::Detach);
            let why = end.err().map(|e| format!(" ({e})")).unwrap_or_default();
            sim.console.lock().push_sim(&format!("gdb: detached{why}"));
        }
    })?;
    Ok(local)
}

/// How long the emulator thread may take to answer (it answers between 20 ms slices).
const REPLY_TIMEOUT: Duration = Duration::from_secs(10);

fn request(commands: &Sender<Command>, req: DebugRequest) -> Result<DebugReply, String> {
    let (tx, rx) = crossbeam_channel::bounded(1);
    commands.send(Command::Debug { request: req, reply: tx }).map_err(|_| "the simulator stopped".to_string())?;
    rx.recv_timeout(REPLY_TIMEOUT).map_err(|_| "the simulator did not answer".to_string())
}

/// What the client sent.
#[derive(Debug, PartialEq)]
enum Incoming {
    Packet(Vec<u8>),
    /// A packet whose checksum didn't match (to be NAKed in ack mode).
    Corrupt,
    /// Ctrl-C.
    Interrupt,
}

/// Splits the byte stream into packets (`$data#cs`), acks and interrupts.
#[derive(Default)]
struct Framer {
    buf: Vec<u8>,
    in_packet: bool,
    /// Checksum digits still to come after `#`.
    trailer: Option<Vec<u8>>,
}

impl Framer {
    fn push(&mut self, b: u8, out: &mut Vec<Incoming>) {
        if let Some(t) = &mut self.trailer {
            t.push(b);
            if t.len() == 2 {
                let sum = self.buf.iter().fold(0u8, |a, &c| a.wrapping_add(c));
                let ok = std::str::from_utf8(t).ok().and_then(|h| u8::from_str_radix(h, 16).ok()) == Some(sum);
                let data = unescape(&std::mem::take(&mut self.buf));
                out.push(if ok { Incoming::Packet(data) } else { Incoming::Corrupt });
                self.trailer = None;
                self.in_packet = false;
            }
            return;
        }
        match b {
            b'$' => {
                self.buf.clear();
                self.in_packet = true;
            }
            b'#' if self.in_packet => self.trailer = Some(Vec::new()),
            _ if self.in_packet => self.buf.push(b),
            0x03 => out.push(Incoming::Interrupt),
            _ => {} // acks ('+', '-') and noise
        }
    }
}

fn unescape(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut it = data.iter();
    while let Some(&b) = it.next() {
        out.push(if b == b'}' { it.next().map_or(b, |&n| n ^ 0x20) } else { b });
    }
    out
}

fn hex(data: &[u8]) -> String {
    data.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok()).collect()
}

fn num(s: &str) -> Option<u64> {
    u64::from_str_radix(s, 16).ok()
}

/// A thread id from `Hg`/`vCont`: `None` for "any" (0) or "all" (-1).
fn thread_id(s: &str) -> Option<usize> {
    match s {
        "0" | "-1" => None,
        _ => num(s).map(|t| t as usize),
    }
}

fn reg_hex(v: &RegValue) -> String {
    match v {
        RegValue::Value(b) => hex(b),
        RegValue::Unavailable(n) => "xx".repeat(*n),
    }
}

/// The stop reply packet for a stop, and console text to show before it.
fn stop_reply(s: &DebugStop) -> (String, Option<String>) {
    let thread = format!("thread:{:x};", s.core + 1);
    match &s.reason {
        StopReason::Interrupted => (format!("T02{thread}"), None),
        StopReason::Breakpoint | StopReason::Step => (format!("T05{thread}"), None),
        StopReason::BreakInstruction => (format!("T05{thread}"), Some("breakpoint instruction\n".into())),
        StopReason::Watchpoint { kind, addr } => {
            let k = match kind {
                WatchKind::Write => "watch",
                WatchKind::Read => "rwatch",
                WatchKind::Access => "awatch",
            };
            (format!("T05{thread}{k}:{addr:x};"), None)
        }
        StopReason::Fault { signal, description } => {
            (format!("T{signal:02x}{thread}"), Some(format!("{description}\n")))
        }
        StopReason::Halted(msg) => (format!("T06{thread}"), Some(format!("simulator halted: {msg}\n"))),
    }
}

const MONITOR_HELP: &str = "\
monitor reset        press the reset button (RTC memory kept)
monitor power-cycle  remove and restore power
monitor wake         end a deep sleep now
";

enum Flow {
    Reply(String),
    /// The target runs; the reply is the next stop.
    Running,
    /// Reply, then end the session.
    Close(Option<String>),
}

struct Session<'a> {
    stream: TcpStream,
    sim: &'a SimHandle,
    incoming: Receiver<Incoming>,
    events: Receiver<DebugStop>,
    target: DebugTarget,
    acks: bool,
    /// `QStartNoAckMode`: no acks once its reply is out.
    stop_acks: bool,
    last_stop: DebugStop,
    /// Thread for register access (`Hg`) and for steps without one (`Hc`), 1-based.
    g_thread: usize,
    c_thread: usize,
}

impl<'a> Session<'a> {
    fn start(stream: TcpStream, sim: &'a SimHandle) -> Result<Self, String> {
        stream.set_nodelay(true).ok();
        let reader = stream.try_clone().map_err(|e| e.to_string())?;
        let (in_tx, incoming) = crossbeam_channel::unbounded();
        std::thread::Builder::new()
            .name("gdb-reader".into())
            .spawn(move || read_loop(reader, in_tx))
            .map_err(|e| e.to_string())?;
        let (ev_tx, events) = crossbeam_channel::unbounded();
        let target = match request(&sim.commands, DebugRequest::Attach { events: ev_tx })? {
            DebugReply::Target(t) => t,
            DebugReply::Error(e) => return Err(e),
            r => return Err(format!("unexpected reply {r:?}")),
        };
        // Attaching stops the target; that is the first stop GDB asks about.
        let last_stop = events.recv_timeout(REPLY_TIMEOUT).map_err(|_| "the target did not stop".to_string())?;
        Ok(Session {
            stream,
            sim,
            incoming,
            events,
            target,
            acks: true,
            stop_acks: false,
            last_stop,
            g_thread: 1,
            c_thread: 1,
        })
    }

    fn run(&mut self) -> Result<(), String> {
        loop {
            let Ok(msg) = self.incoming.recv() else { return Ok(()) };
            let packet = match msg {
                Incoming::Packet(p) => p,
                Incoming::Corrupt => {
                    if self.acks {
                        self.write(b"-")?;
                    }
                    continue;
                }
                Incoming::Interrupt => continue, // already stopped
            };
            if self.acks {
                self.write(b"+")?;
            }
            let text = String::from_utf8_lossy(&packet).into_owned();
            log::debug!("gdb <- {text}");
            match self.handle(&text)? {
                Flow::Reply(r) => {
                    self.send(&r)?;
                    self.acks &= !std::mem::take(&mut self.stop_acks);
                }
                Flow::Close(r) => {
                    if let Some(r) = r {
                        self.send(&r)?;
                    }
                    return Ok(());
                }
                Flow::Running => {
                    if !self.wait_for_stop()? {
                        return Ok(());
                    }
                }
            }
        }
    }

    /// Until the target stops (or GDB goes away: false); reports the stop.
    fn wait_for_stop(&mut self) -> Result<bool, String> {
        loop {
            select! {
                recv(self.events) -> ev => {
                    let Ok(stop) = ev else {
                        // The simulator is gone: the program "exited".
                        self.send("W00")?;
                        return Ok(false);
                    };
                    let (reply, text) = stop_reply(&stop);
                    if let Some(t) = text {
                        self.send(&format!("O{}", hex(t.as_bytes())))?;
                    }
                    self.g_thread = stop.core + 1;
                    self.c_thread = stop.core + 1;
                    self.last_stop = stop;
                    self.send(&reply)?;
                    return Ok(true);
                }
                recv(self.incoming) -> msg => match msg {
                    Ok(Incoming::Interrupt) => {
                        self.ask(DebugRequest::Interrupt)?;
                    }
                    Ok(_) => {} // nothing else is valid while running
                    Err(_) => return Ok(false),
                },
            }
        }
    }

    fn handle(&mut self, p: &str) -> Result<Flow, String> {
        let reply = |s: &str| Ok(Flow::Reply(s.to_string()));
        let (head, rest) = p.split_at(p.len().min(1));
        match head {
            "?" => reply(&stop_reply(&self.last_stop).0),
            "g" => match self.ask(DebugRequest::ReadRegisters { core: self.g_thread - 1 })? {
                DebugReply::Registers(regs) => reply(&regs.iter().map(reg_hex).collect::<String>()),
                _ => reply("E01"),
            },
            "p" => {
                let Some(n) = num(rest) else { return reply("E01") };
                match self.ask(DebugRequest::ReadRegister { core: self.g_thread - 1, n: n as usize })? {
                    DebugReply::Register(Some(v)) => reply(&reg_hex(&v)),
                    _ => reply("E01"),
                }
            }
            "P" => {
                let parsed = rest.split_once('=').and_then(|(n, v)| Some((num(n)? as usize, unhex(v)?)));
                let Some((n, value)) = parsed else { return reply("E01") };
                self.ok(DebugRequest::WriteRegister { core: self.g_thread - 1, n, value })
            }
            "m" => {
                let parsed = rest.split_once(',').and_then(|(a, l)| Some((num(a)? as u32, num(l)? as usize)));
                let Some((addr, len)) = parsed else { return reply("E01") };
                match self.ask(DebugRequest::ReadMemory { addr, len: len.min(PACKET_SIZE / 2) })? {
                    DebugReply::Memory(b) if !b.is_empty() => reply(&hex(&b)),
                    _ => reply("E14"),
                }
            }
            "M" => {
                let parsed = rest.split_once(':').and_then(|(al, d)| {
                    let (a, _) = al.split_once(',')?;
                    Some((num(a)? as u32, unhex(d)?))
                });
                let Some((addr, data)) = parsed else { return reply("E01") };
                self.ok(DebugRequest::WriteMemory { addr, data })
            }
            "c" | "C" => self.resume(None),
            "s" | "S" => self.resume(Some(self.c_thread - 1)),
            "Z" | "z" => self.point(head == "Z", rest),
            "H" => {
                let (which, t) = rest.split_at(rest.len().min(1));
                if let Some(t) = thread_id(t).filter(|t| (1..=self.target.cores).contains(t)) {
                    match which {
                        "g" => self.g_thread = t,
                        _ => self.c_thread = t,
                    }
                }
                reply("OK")
            }
            "T" => {
                let cores = self.cores()? as u64;
                let alive = num(rest).is_some_and(|t| (1..=cores).contains(&t));
                reply(if alive { "OK" } else { "E01" })
            }
            "D" => Ok(Flow::Close(Some("OK".into()))),
            "k" => Ok(Flow::Close(None)),
            _ => self.handle_named(p),
        }
    }

    fn handle_named(&mut self, p: &str) -> Result<Flow, String> {
        let reply = |s: &str| Ok(Flow::Reply(s.to_string()));
        if p.starts_with("qSupported") {
            let xml = if self.target.target_xml.is_some() { "qXfer:features:read+;" } else { "" };
            return reply(&format!("PacketSize={PACKET_SIZE:x};{xml}QStartNoAckMode+;vContSupported+"));
        }
        if p == "QStartNoAckMode" {
            self.stop_acks = true;
            return reply("OK");
        }
        if p == "vCont?" {
            return reply("vCont;c;C;s;S");
        }
        if let Some(actions) = p.strip_prefix("vCont;") {
            // Steps name their thread; a bare step steps the `Hc` thread. The rest continue.
            let step = actions.split(';').find_map(|a| {
                let (act, t) = a.split_once(':').unwrap_or((a, ""));
                act.starts_with(['s', 'S']).then(|| thread_id(t).unwrap_or(self.c_thread))
            });
            return self.resume(step.map(|t| t.max(1) - 1));
        }
        if p == "qfThreadInfo" {
            let ids: Vec<String> = (1..=self.cores()?).map(|t| format!("{t:x}")).collect();
            return reply(&format!("m{}", ids.join(",")));
        }
        if p == "qsThreadInfo" {
            return reply("l");
        }
        if p == "qC" {
            return reply(&format!("QC{:x}", self.last_stop.core + 1));
        }
        if p == "qAttached" {
            return reply("1");
        }
        if let Some(t) = p.strip_prefix("qThreadExtraInfo,") {
            let core = num(t).unwrap_or(1).max(1) - 1;
            let name = match (self.target.arch, core) {
                ("xtensa", 0) => "core 0 (PRO_CPU)".to_string(),
                ("xtensa", 1) => "core 1 (APP_CPU)".to_string(),
                _ => format!("core {core}"),
            };
            return reply(&hex(name.as_bytes()));
        }
        if let Some(args) = p.strip_prefix("qXfer:features:read:target.xml:") {
            let Some(xml) = self.target.target_xml else { return reply("E00") };
            let range = args.split_once(',').and_then(|(o, l)| Some((num(o)? as usize, num(l)? as usize)));
            let Some((off, len)) = range else { return reply("E00") };
            let part = xml.as_bytes().get(off..).unwrap_or(&[]);
            let chunk = &part[..part.len().min(len)];
            let more = if chunk.len() < part.len() { "m" } else { "l" };
            return reply(&format!("{more}{}", String::from_utf8_lossy(chunk)));
        }
        if let Some(cmd) = p.strip_prefix("qRcmd,") {
            let cmd = unhex(cmd).map(|b| String::from_utf8_lossy(&b).trim().to_string()).unwrap_or_default();
            return self.monitor(&cmd);
        }
        if p.starts_with("qSymbol") {
            return reply("OK");
        }
        if p == "vKill" || p.starts_with("vKill;") {
            return Ok(Flow::Close(Some("OK".into())));
        }
        // Unsupported: the empty reply (GDB falls back, e.g. from `X` to `M`).
        reply("")
    }

    fn monitor(&mut self, cmd: &str) -> Result<Flow, String> {
        let (command, text) = match cmd {
            "reset" => (Some(Command::Reset), "reset\n"),
            "power-cycle" => (Some(Command::PowerCycle), "power cycled\n"),
            "wake" => (Some(Command::WakeFromSleep), "woken\n"),
            _ => (None, MONITOR_HELP),
        };
        if let Some(c) = command {
            self.sim.send(c);
            // Let it land before GDB reads registers again.
            self.ask(DebugRequest::Target)?;
        }
        self.send(&format!("O{}", hex(text.as_bytes())))?;
        Ok(Flow::Reply("OK".into()))
    }

    fn point(&mut self, set: bool, args: &str) -> Result<Flow, String> {
        let mut it = args.split([',', ';']);
        let (Some(kind), Some(addr), Some(len)) = (it.next(), it.next().and_then(num), it.next().and_then(num)) else {
            return Ok(Flow::Reply("E01".into()));
        };
        let addr = addr as u32;
        let req = match kind {
            "0" | "1" => DebugRequest::Breakpoint { addr, set },
            "2" | "3" | "4" => {
                let kind =
                    [WatchKind::Write, WatchKind::Read, WatchKind::Access][kind.as_bytes()[0] as usize - b'2' as usize];
                DebugRequest::Watchpoint { kind, addr, len: len as u32, set }
            }
            _ => return Ok(Flow::Reply(String::new())),
        };
        self.ok(req)
    }

    fn resume(&mut self, step: Option<usize>) -> Result<Flow, String> {
        self.ask(DebugRequest::Resume { step })?;
        Ok(Flow::Running)
    }

    fn cores(&mut self) -> Result<usize, String> {
        if let DebugReply::Target(t) = self.ask(DebugRequest::Target)? {
            self.target = t;
        }
        Ok(self.target.cores)
    }

    fn ask(&self, req: DebugRequest) -> Result<DebugReply, String> {
        request(&self.sim.commands, req)
    }

    fn ok(&self, req: DebugRequest) -> Result<Flow, String> {
        Ok(Flow::Reply(match self.ask(req)? {
            DebugReply::Ok => "OK".into(),
            _ => "E01".into(),
        }))
    }

    fn send(&mut self, data: &str) -> Result<(), String> {
        log::debug!("gdb -> {data}");
        let sum = data.bytes().fold(0u8, |a, c| a.wrapping_add(c));
        self.write(format!("${data}#{sum:02x}").as_bytes())
    }

    fn write(&mut self, b: &[u8]) -> Result<(), String> {
        self.stream.write_all(b).map_err(|e| e.to_string())
    }
}

/// The largest packet GDB may send us (and half of it, the largest memory read).
const PACKET_SIZE: usize = 0x4000;

fn read_loop(mut stream: TcpStream, out: Sender<Incoming>) {
    let mut framer = Framer::default();
    let mut buf = [0u8; 4096];
    let mut items = Vec::new();
    while let Ok(n) = stream.read(&mut buf) {
        if n == 0 {
            break;
        }
        for &b in &buf[..n] {
            framer.push(b, &mut items);
        }
        for i in items.drain(..) {
            if out.send(i).is_err() {
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests;
