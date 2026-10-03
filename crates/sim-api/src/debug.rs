//! The debugger contract: what a debugger front-end (the GDB stub) asks of the emulator
//! thread, and how it learns that the target stopped.
//!
//! The debugger stops the whole machine (all cores, virtual time) at once. Registers are
//! numbered as the architecture's GDB numbers them and given as target-endian bytes.

use crossbeam_channel::Sender;

/// One register's contents, or its size when the simulator can't give its value.
#[derive(Debug, Clone, PartialEq)]
pub enum RegValue {
    Value(Vec<u8>),
    Unavailable(usize),
}

/// What kind of CPU the debugger is talking to.
#[derive(Debug, Clone, PartialEq)]
pub struct DebugTarget {
    /// GDB architecture name, e.g. `riscv:rv32` or `xtensa`.
    pub arch: &'static str,
    /// Cores that run code now (GDB threads 1..=cores).
    pub cores: usize,
    /// A GDB target description (`target.xml`), for architectures whose GDB reads one.
    pub target_xml: Option<&'static str>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WatchKind {
    Write,
    Read,
    Access,
}

/// Why the target stopped.
#[derive(Debug, Clone, PartialEq)]
pub enum StopReason {
    /// The debugger asked (attach, Ctrl-C).
    Interrupted,
    Breakpoint,
    /// A single step finished.
    Step,
    /// An access hit a watchpoint; stopped after the accessing instruction.
    Watchpoint {
        kind: WatchKind,
        addr: u32,
    },
    /// The firmware executed a breakpoint instruction (`ebreak`, `BREAK`).
    BreakInstruction,
    /// The CPU is about to take a fatal exception (stopped before it is taken, at the
    /// faulting instruction). `signal` is the GDB signal number (4 SIGILL, 8 SIGFPE,
    /// 10 SIGBUS, 11 SIGSEGV).
    Fault {
        signal: u8,
        description: String,
    },
    /// The emulator halted (it cannot continue).
    Halted(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct DebugStop {
    /// The core that stopped (0-based).
    pub core: usize,
    pub reason: StopReason,
}

/// A debugger request. The runner answers each with one [`DebugReply`]; stops are sent on
/// the events channel given at [`DebugRequest::Attach`].
#[derive(Debug, Clone)]
pub enum DebugRequest {
    /// Stop the target and report stops on `events` until detached. Replies `Target`.
    Attach {
        events: Sender<DebugStop>,
    },
    /// Clear breakpoints and watchpoints and let the target run on.
    Detach,
    /// Stop the target (a stop event follows).
    Interrupt,
    /// Replies `Target` (the running cores change when the S3 starts its second core).
    Target,
    /// Run on. `step`: execute one instruction on that core, then stop.
    Resume {
        step: Option<usize>,
    },
    /// The registers of GDB's `g` packet. Replies `Registers`.
    ReadRegisters {
        core: usize,
    },
    /// One register. Replies `Register` (`None`: no such register).
    ReadRegister {
        core: usize,
        n: usize,
    },
    WriteRegister {
        core: usize,
        n: usize,
        value: Vec<u8>,
    },
    /// Replies `Memory`: the bytes from `addr` up to the first one that can't be read.
    ReadMemory {
        addr: u32,
        len: usize,
    },
    WriteMemory {
        addr: u32,
        data: Vec<u8>,
    },
    Breakpoint {
        addr: u32,
        set: bool,
    },
    Watchpoint {
        kind: WatchKind,
        addr: u32,
        len: u32,
        set: bool,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum DebugReply {
    Ok,
    Error(String),
    Target(DebugTarget),
    Registers(Vec<RegValue>),
    Register(Option<RegValue>),
    Memory(Vec<u8>),
}

pub type DebugReplySender = Sender<DebugReply>;
