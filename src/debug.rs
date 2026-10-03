//! The debugger's view of a machine (`--gdb`): breakpoints, single steps, watchpoints and
//! stops on CPU faults, shared by every SoC. The GDB protocol itself is in `sim-gdb`.
//!
//! Stops cost nothing while no debugger uses them: a breakpoint sets its bit in the HLE
//! hooks' pre-instruction filter (every bit while single-stepping), so the run loops only
//! call [`DebugState::check`] where the filter says a hook *or a stop* may be, and watched
//! accesses are noticed on the bus's memcheck path.

use std::collections::BTreeSet;

use sim_api::{DebugStop, DebugTarget, RegValue, StopReason, WatchKind};

use crate::hle::Hooks;

/// What a SoC offers the debugger. Registers are numbered as the core's GDB numbers them.
pub trait Debuggable {
    fn debug_target(&self) -> DebugTarget;
    /// A debugger attached (stops on faults and breakpoint instructions) or detached (all
    /// breakpoints and watchpoints cleared).
    fn debug_attach(&mut self, on: bool);
    /// The registers of GDB's `g` packet.
    fn debug_registers(&self, core: usize) -> Vec<RegValue>;
    fn debug_register(&self, core: usize, n: usize) -> Option<RegValue>;
    fn debug_set_register(&mut self, core: usize, n: usize, value: &[u8]) -> bool;
    /// Bytes from `addr` up to the first unreadable one (memory only: peripheral registers
    /// aren't read, since reading some has side effects).
    fn debug_read_memory(&self, addr: u32, len: usize) -> Vec<u8>;
    fn debug_write_memory(&mut self, addr: u32, data: &[u8]) -> bool;
    fn debug_breakpoint(&mut self, addr: u32, set: bool);
    fn debug_watchpoint(&mut self, kind: WatchKind, addr: u32, len: u32, set: bool) -> bool;
    /// Run on (`step`: one instruction on that core, then stop).
    fn debug_resume(&mut self, step: Option<usize>);
}

pub const MAX_CORES: usize = 2;

#[derive(Default)]
pub struct DebugState {
    pub attached: bool,
    breakpoints: BTreeSet<u32>,
    /// The core single-stepping.
    step: Option<usize>,
    /// Per core: the pc it resumes at, where it must not stop again before running.
    skip: [Option<u32>; MAX_CORES],
    /// Per core: a fault reported at this pc; when the instruction faults again after the
    /// resume, the exception is taken.
    fault_reported: [Option<u32>; MAX_CORES],
    /// Per core: a breakpoint instruction it stopped at (pc, length), stepped over on resume.
    break_insn: [Option<(u32, u32)>; MAX_CORES],
    /// A stop found where the run loop couldn't return it right away.
    pub pending: Option<DebugStop>,
}

impl DebugState {
    /// At an instruction boundary where the hooks' filter matched: whether to stop there.
    #[inline]
    pub fn check(&mut self, core: usize, pc: u32) -> Option<DebugStop> {
        if !self.attached {
            return None;
        }
        if let Some(s) = self.skip[core].take()
            && s == pc
        {
            return None;
        }
        if self.step == Some(core) {
            self.step = None;
            return Some(DebugStop { core, reason: StopReason::Step });
        }
        self.breakpoints.contains(&pc).then_some(DebugStop { core, reason: StopReason::Breakpoint })
    }

    /// While single-stepping, interrupts wait (as OpenOCD's `maskisr`), so a step stays in
    /// the code being debugged.
    #[inline]
    pub fn holds_interrupts(&self) -> bool {
        self.step.is_some()
    }

    /// The core is about to take a trap: whether to stop first. `fatal`: the signal GDB
    /// should show for an exception the firmware won't recover from; `insn_len` for a
    /// breakpoint instruction (which the resume steps over).
    pub fn trap(
        &mut self,
        core: usize,
        pc: u32,
        fatal: Option<u8>,
        insn_len: Option<u32>,
        what: String,
    ) -> Option<DebugStop> {
        if !self.attached {
            return None;
        }
        if let Some(len) = insn_len {
            self.break_insn[core] = Some((pc, len));
            return Some(DebugStop { core, reason: StopReason::BreakInstruction });
        }
        let signal = fatal?;
        if self.fault_reported[core].take() == Some(pc) {
            return None;
        }
        self.fault_reported[core] = Some(pc);
        Some(DebugStop { core, reason: StopReason::Fault { signal, description: what } })
    }

    pub fn attach(&mut self, on: bool, hooks: &mut Hooks) {
        *self = DebugState { attached: on, ..Default::default() };
        self.apply(hooks);
    }

    pub fn set_breakpoint(&mut self, addr: u32, set: bool, hooks: &mut Hooks) {
        if set {
            self.breakpoints.insert(addr);
        } else {
            self.breakpoints.remove(&addr);
        }
        self.apply(hooks);
    }

    /// Run on from `pcs` (each core's pc); returns the pc each core continues at, having
    /// stepped over a breakpoint instruction it stopped at.
    pub fn resume(&mut self, step: Option<usize>, pcs: &[u32], hooks: &mut Hooks) -> Vec<u32> {
        self.step = step;
        let mut out = Vec::with_capacity(pcs.len());
        for (core, &pc) in pcs.iter().enumerate() {
            let pc = match self.break_insn[core].take() {
                Some((at, len)) if at == pc => pc + len,
                _ => pc,
            };
            self.skip[core] = (step.is_some() || self.breakpoints.contains(&pc)).then_some(pc);
            out.push(pc);
        }
        self.apply(hooks);
        out
    }

    /// Bring the hooks' filter in line (also after the HLE rebinds hooks for a new boot).
    pub fn apply(&self, hooks: &mut Hooks) {
        hooks.set_debug_filter(self.breakpoints.iter().copied(), self.step.is_some());
    }
}

/// The signal GDB shows for a RISC-V exception cause, when the firmware won't recover.
pub fn riscv_fault_signal(cause: u32) -> Option<u8> {
    match cause {
        2 => Some(4),          // illegal instruction: SIGILL
        0 | 4 | 6 => Some(10), // misaligned: SIGBUS
        1 | 5 | 7 => Some(11), // access fault: SIGSEGV
        _ => None,
    }
}

/// The signal and name of an Xtensa general exception the firmware won't recover from
/// (window, alloca, syscall and coprocessor exceptions are normal operation).
pub fn xtensa_fault(cause: u32) -> Option<(u8, &'static str)> {
    Some(match cause {
        0 => (4, "IllegalInstruction"),
        2 => (11, "InstructionFetchError"),
        3 => (11, "LoadStoreError"),
        6 => (8, "IntegerDivideByZero"),
        9 => (10, "LoadStoreAlignment"),
        20 => (11, "InstrFetchProhibited"),
        28 => (11, "LoadProhibited"),
        29 => (11, "StoreProhibited"),
        _ => return None,
    })
}

/// Memory the debugger reads: everything from `addr` that `peek` can give, a byte at a
/// time past the first unreadable region boundary.
pub fn read_memory(peek: impl Fn(u32, usize) -> Option<Vec<u8>>, addr: u32, len: usize) -> Vec<u8> {
    if let Some(b) = peek(addr, len) {
        return b;
    }
    let mut out = Vec::new();
    while out.len() < len
        && let Some(b) = peek(addr.wrapping_add(out.len() as u32), 1)
    {
        out.push(b[0]);
    }
    out
}

/// [`Debuggable`] for a single-core RISC-V SoC with `cpu`, `bus`, `hooks` and `debug` fields.
macro_rules! riscv_debuggable {
    ($soc:ty) => {
        impl crate::debug::Debuggable for $soc {
            fn debug_target(&self) -> sim_api::DebugTarget {
                sim_api::DebugTarget {
                    arch: "riscv:rv32",
                    cores: 1,
                    target_xml: Some(crate::arch::gdb::RISCV_TARGET_XML),
                }
            }

            fn debug_attach(&mut self, on: bool) {
                self.debug.attach(on, &mut self.hooks);
                self.bus.watch.clear();
            }

            fn debug_registers(&self, _core: usize) -> Vec<sim_api::RegValue> {
                crate::arch::gdb::riscv_registers(&self.cpu)
            }

            fn debug_register(&self, _core: usize, n: usize) -> Option<sim_api::RegValue> {
                crate::arch::gdb::riscv_register(&self.cpu, n, self.bus.clock.cycles)
            }

            fn debug_set_register(&mut self, _core: usize, n: usize, value: &[u8]) -> bool {
                let cycles = self.bus.clock.cycles;
                crate::arch::gdb::riscv_set_register(&mut self.cpu, n, value, cycles)
            }

            fn debug_read_memory(&self, addr: u32, len: usize) -> Vec<u8> {
                crate::debug::read_memory(|a, n| self.bus.peek_bytes(a, n), addr, len)
            }

            fn debug_write_memory(&mut self, addr: u32, data: &[u8]) -> bool {
                self.bus.load_bytes(addr, data)
            }

            fn debug_breakpoint(&mut self, addr: u32, set: bool) {
                self.debug.set_breakpoint(addr, set, &mut self.hooks);
            }

            fn debug_watchpoint(&mut self, kind: sim_api::WatchKind, addr: u32, len: u32, set: bool) -> bool {
                self.bus.watch.set(kind, addr, len, set);
                true
            }

            fn debug_resume(&mut self, step: Option<usize>) {
                self.cpu.pc = self.debug.resume(step, &[self.cpu.pc], &mut self.hooks)[0];
            }
        }
    };
}
pub(crate) use riscv_debuggable;

/// Length of the RISC-V instruction whose first halfword is `lo` (compressed or not).
pub fn riscv_insn_len(lo: Option<Vec<u8>>) -> u32 {
    if lo.is_some_and(|b| b[0] & 3 == 3) { 4 } else { 2 }
}

/// Watched memory, checked on every CPU load and store while any is set.
#[derive(Default)]
pub struct Watchpoints {
    list: Vec<(WatchKind, u32, u32)>,
    /// The first watched access since the run loop last looked: (core, kind, address).
    pub hit: Option<(usize, WatchKind, u32)>,
}

impl Watchpoints {
    #[inline(always)]
    pub fn active(&self) -> bool {
        !self.list.is_empty()
    }

    pub fn set(&mut self, kind: WatchKind, addr: u32, len: u32, set: bool) {
        self.list.retain(|w| *w != (kind, addr, len));
        if set {
            self.list.push((kind, addr, len));
        }
    }

    pub fn clear(&mut self) {
        self.list.clear();
        self.hit = None;
    }

    /// Record a watched access; true if it is one (the run loop then looks at `hit`).
    pub fn access(&mut self, core: usize, addr: u32, len: u32, write: bool) -> bool {
        if self.hit.is_some() {
            return false;
        }
        let end = addr as u64 + len as u64;
        for &(kind, a, l) in &self.list {
            let wanted = match kind {
                WatchKind::Write => write,
                WatchKind::Read => !write,
                WatchKind::Access => true,
            };
            if wanted && (addr as u64) < a as u64 + l as u64 && end > a as u64 {
                self.hit = Some((core, kind, a));
                return true;
            }
        }
        false
    }

    pub fn stop(&mut self) -> Option<DebugStop> {
        self.hit.take().map(|(core, kind, addr)| DebugStop { core, reason: StopReason::Watchpoint { kind, addr } })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn breakpoint_stops_once_and_resumes_past_itself() {
        let mut hooks = Hooks::default();
        let mut d = DebugState::default();
        d.attach(true, &mut hooks);
        d.set_breakpoint(0x4200_0010, true, &mut hooks);
        assert!(hooks.maybe(0x4200_0010));
        assert_eq!(d.check(0, 0x4200_0010).unwrap().reason, StopReason::Breakpoint);
        // Resuming at the breakpoint runs its instruction before it can stop there again.
        d.resume(None, &[0x4200_0010], &mut hooks);
        assert!(d.check(0, 0x4200_0010).is_none());
        assert!(d.check(0, 0x4200_0010).is_some());
    }

    #[test]
    fn step_stops_at_the_next_instruction_and_filters_every_pc() {
        let mut hooks = Hooks::default();
        let mut d = DebugState::default();
        d.attach(true, &mut hooks);
        d.resume(Some(0), &[0x4200_0000], &mut hooks);
        assert!(hooks.maybe(0x4200_1234) && d.holds_interrupts());
        assert!(d.check(0, 0x4200_0000).is_none());
        assert_eq!(d.check(0, 0x4200_0002).unwrap().reason, StopReason::Step);
        d.resume(None, &[0x4200_0002], &mut hooks);
        assert!(!hooks.maybe(0x4200_1234));
    }

    #[test]
    fn fault_is_reported_once_then_taken() {
        let mut hooks = Hooks::default();
        let mut d = DebugState::default();
        assert!(d.trap(0, 0x100, Some(11), None, "x".into()).is_none(), "not attached");
        d.attach(true, &mut hooks);
        assert!(d.trap(0, 0x100, Some(11), None, "x".into()).is_some());
        assert!(d.trap(0, 0x100, Some(11), None, "x".into()).is_none());
        assert!(d.trap(0, 0x100, None, None, "ecall".into()).is_none());
    }

    #[test]
    fn breakpoint_instruction_is_stepped_over() {
        let mut hooks = Hooks::default();
        let mut d = DebugState::default();
        d.attach(true, &mut hooks);
        assert!(d.trap(0, 0x200, None, Some(2), "ebreak".into()).is_some());
        assert_eq!(d.resume(None, &[0x200], &mut hooks), vec![0x202]);
    }

    #[test]
    fn watchpoints_match_overlapping_accesses_of_their_kind() {
        let mut w = Watchpoints::default();
        w.set(WatchKind::Write, 0x3fc8_0000, 4, true);
        assert!(!w.access(0, 0x3fc8_0000, 4, false));
        assert!(!w.access(0, 0x3fc8_0004, 1, true));
        assert!(w.access(1, 0x3fc8_0003, 1, true));
        assert_eq!(
            w.stop(),
            Some(DebugStop { core: 1, reason: StopReason::Watchpoint { kind: WatchKind::Write, addr: 0x3fc8_0000 } })
        );
        w.set(WatchKind::Write, 0x3fc8_0000, 4, false);
        assert!(!w.active());
    }
}
