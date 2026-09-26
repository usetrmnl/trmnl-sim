//! Xtensa LX7 interpreter configured as the ESP32-S3 core.
//!
//! Configuration (from ESP-IDF `components/xtensa/esp32s3/.../core-isa.h`): little
//! endian, 64 physical address registers with the windowed ABI, density, zero-overhead
//! loops, NSA, MIN/MAX, SEXT, CLAMPS, MUL16/MUL32/MULH, DIV32, MAC16, booleans,
//! THREADPTR, single-precision FPU (coprocessor 0) with the DIV/SQRT/RECIP/RSQRT helper
//! instructions, S32C1I, XEA2 exceptions with relocatable vectors (VECBASE), 32
//! interrupts on 6 levels + NMI (level 7), 3 CCOMPARE timers, debug level 6, no caches,
//! no MMU (region protection only), and the "cop_ai" PIE/SIMD extension as
//! coprocessor 3 (only the parts ESP-IDF itself uses are implemented, see below).
//!
//! # Driving the core
//!
//! [`Xtensa::step`] executes one instruction (or takes one interrupt) and returns a
//! [`Step`]. Everything architectural - window overflow/underflow, level-1 exceptions,
//! double exceptions, interrupts at every level, coprocessor-disabled exceptions - is
//! dispatched to the guest's own vector code at `VECBASE + offset` exactly like the
//! hardware does, so a caller can simply keep stepping. [`Step::Exception`] is returned
//! (after dispatch) for general exceptions so the SoC can log or halt on them.
//! [`Step::Trap`] is returned (without dispatch, PC unchanged) for things the caller
//! must decide about: `BREAK`/`BREAK.N` (debug exceptions), instructions the emulator
//! does not implement (reports the raw bytes), and HLE bookkeeping errors.
//!
//! # Interrupts
//!
//! Interrupts are internal to the core, as on the real chip. The SoC (interrupt
//! matrix) drives the 32 external inputs with [`Xtensa::set_irq_lines`]: for
//! level-triggered lines the INTERRUPT bit follows the input; for edge-triggered lines
//! (and the NMI, line 14) a rising edge latches the bit (cleared by `INTCLEAR`, or when
//! the NMI is taken). Software interrupts (7, 29) are set via `WSR INTSET` and the
//! CCOMPARE0/1/2 timers raise lines 6/15/16 when CCOUNT reaches them. Levels and types
//! come from the core-isa.h tables below. The check for a deliverable interrupt costs
//! one boolean test per step.
//!
//! CCOUNT is per core and advances by one per executed instruction. `WAITI` returns
//! [`Step::Wfi`] when nothing is deliverable; the SoC can then fast-forward with
//! [`Xtensa::advance_ccount`] (which fires CCOMPARE matches that are crossed) and should
//! use [`Xtensa::cycles_to_timer`] to know how far it may skip.
//!
//! # HLE (GuestCpu)
//!
//! Hooks fire at the first instruction of a function, i.e. *before* its `ENTRY`, when
//! the register window is still the caller's and `PS.CALLINC` holds the caller's
//! window increment n. The callee's arguments are then the caller's `a(4n+2)..`, its
//! return address is the caller's `a(4n)` (low 30 bits; the top two bits are the
//! increment), and its return value goes to the caller's `a(4n+2)`. See
//! [`GuestCpu`] impl notes and [`Xtensa::begin_call`] for how calls into guest code
//! are synthesised without disturbing the window.
//!
//! HLE caveats: `arg(n)` supports the six register arguments only; hooks on CALL0-ABI
//! code work for arg/ret/return but `begin_call` needs a windowed context
//! (PS.CALLINC != 0). The 16 bytes directly below a windowed frame's SP hold its
//! caller's spilled a0-a3, so HLE code that carves guest stack space by lowering SP
//! must skip those 16 bytes (allocate `n + 16`).
//!
//! # Not modelled
//!
//! FSR exception flags (not accumulated), directed rounding for the divide/sqrt
//! helpers, IBREAK/DBREAK/ICOUNT (registers stored, no debug events), ATOMCTL (S32C1I
//! always behaves as an internal-memory RCW), RER/WER (external registers read 0),
//! region protection (WxTLB attributes are stored; faults come from the bus instead:
//! fetch/load/store faults raise InstFetchProhibited/LoadProhibited/StoreProhibited),
//! privilege rings, cache ops (no-ops; the S3 core has no caches), and the cop_ai
//! (PIE/SIMD) instructions beyond LD.QR/ST.QR and the CP3 user registers - none are
//! reachable in the TRMNL firmware, its bootloader or the S3 ROM (see the coverage
//! tests); any other one returns [`Trap::Unimplemented`] with its raw bytes.

#![allow(dead_code)] // until an SoC instantiates the core

pub mod decode;
pub mod fpu;
#[cfg(test)]
mod scan;
#[cfg(test)]
mod tests;

use super::{GuestCpu, MemBus};
use decode::{Insn, Op, decode, insn_len};

// ------------------------------------------------------------------------------------------------
// ESP32-S3 configuration
// ------------------------------------------------------------------------------------------------

pub mod config {
    /// Reset vector (XCHAL_RESET_VECTOR_VADDR).
    pub const RESET_PC: u32 = 0x4000_0400;
    /// VECBASE reset value.
    pub const VECBASE_RESET: u32 = 0x4000_0000;

    pub const WINDOW_OF4: u32 = 0x000;
    pub const WINDOW_UF4: u32 = 0x040;
    pub const WINDOW_OF8: u32 = 0x080;
    pub const WINDOW_UF8: u32 = 0x0c0;
    pub const WINDOW_OF12: u32 = 0x100;
    pub const WINDOW_UF12: u32 = 0x140;
    /// Interrupt level vectors, indexed by level (2..=7; 6 = debug, 7 = NMI).
    pub const LEVEL_VEC: [u32; 8] = [0, 0, 0x180, 0x1c0, 0x200, 0x240, 0x280, 0x2c0];
    pub const KERNEL_VEC: u32 = 0x300;
    pub const USER_VEC: u32 = 0x340;
    pub const DOUBLE_VEC: u32 = 0x3c0;

    pub const EXCM_LEVEL: u32 = 3;
    pub const DEBUG_LEVEL: u32 = 6;
    pub const NMI_LEVEL: u32 = 7;

    /// XCHAL_INTLEVELn_MASK, indexed by level.
    pub const LEVEL_MASK: [u32; 8] =
        [0, 0x0006_37FF, 0x0038_0000, 0x28C0_8800, 0x5300_0000, 0x8401_0000, 0x0000_0000, 0x0000_4000];
    pub const INT_SOFTWARE: u32 = 0x2000_0080;
    pub const INT_EXTERN_EDGE: u32 = 0x5040_0400;
    pub const INT_EXTERN_LEVEL: u32 = 0x8FBE_333F;
    pub const INT_TIMER: u32 = 0x0001_8040;
    pub const INT_NMI: u32 = 0x0000_4000;
    pub const INT_PROFILING: u32 = 0x0000_0800;
    /// Interrupt numbers raised by CCOMPARE0..2.
    pub const TIMER_INT: [u32; 3] = [6, 15, 16];

    pub const CONFIGID0: u32 = 0xC2F0_FFFE;
    pub const CONFIGID1: u32 = 0x2309_0F1F;
}
use config::*;

/// EXCCAUSE values.
pub mod cause {
    pub const ILLEGAL: u32 = 0;
    pub const SYSCALL: u32 = 1;
    pub const INSTR_ERROR: u32 = 2;
    pub const LOAD_STORE_ERROR: u32 = 3;
    pub const LEVEL1_INTERRUPT: u32 = 4;
    pub const ALLOCA: u32 = 5;
    pub const DIVIDE_BY_ZERO: u32 = 6;
    pub const PRIVILEGED: u32 = 8;
    pub const UNALIGNED: u32 = 9;
    pub const INSTR_PROHIBITED: u32 = 20;
    pub const LOAD_PROHIBITED: u32 = 28;
    pub const STORE_PROHIBITED: u32 = 29;
    pub const CP0_DISABLED: u32 = 32;
    pub const CP3_DISABLED: u32 = 35;
}

/// PS register fields.
pub mod ps {
    pub const INTLEVEL: u32 = 0xf;
    pub const EXCM: u32 = 1 << 4;
    pub const UM: u32 = 1 << 5;
    pub const RING: u32 = 3 << 6;
    pub const OWB_SHIFT: u32 = 8;
    pub const OWB: u32 = 0xf << 8;
    pub const CALLINC_SHIFT: u32 = 16;
    pub const CALLINC: u32 = 3 << 16;
    pub const WOE: u32 = 1 << 18;
    pub const WRITABLE: u32 = INTLEVEL | EXCM | UM | RING | OWB | CALLINC | WOE;
}

/// Pseudo-PCs used to synthesise HLE calls (see [`Xtensa::begin_call`]). They sit in
/// the HLE "magic" range (>= 0x7F00_0000) below the addresses the HLE layer hands out,
/// and must not be mapped for instruction fetch.
pub const SYNTH_CALL_PC: u32 = 0x7F0F_FF00;
pub const SYNTH_RET_PC: u32 = 0x7F0F_FF10;
const SYNTH_COOKIE: u32 = 0x5EC0_CA11;
const DCACHE_SIZE: usize = 1 << 10;

// ------------------------------------------------------------------------------------------------
// Step results
// ------------------------------------------------------------------------------------------------

/// A general exception that was raised and already dispatched to the guest vector.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Exception {
    pub cause: u32,
    /// PC of the faulting instruction.
    pub pc: u32,
    /// EXCVADDR (for memory exceptions; otherwise 0).
    pub vaddr: u32,
    /// Taken while PS.EXCM was set: went to the double-exception vector.
    pub double: bool,
}

/// Conditions reported to the caller without being dispatched (PC left at the instruction).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trap {
    /// `BREAK s, t` (or `BREAK.N s`, with `narrow`). Real hardware raises a debug
    /// exception; call [`Xtensa::take_debug_exception`] to emulate that.
    Break { pc: u32, s: u8, t: u8, narrow: bool },
    /// An instruction the emulator cannot decode/execute. `raw` holds `len` bytes,
    /// little-endian. Call [`Xtensa::raise_exception`] with [`cause::ILLEGAL`] to give
    /// it to the guest.
    Unimplemented { pc: u32, raw: u32, len: u8 },
    /// Instruction fetch at an HLE pseudo-PC without a pending synthesised call, or a
    /// corrupted synthesised-call frame.
    HleCall { pc: u32 },
}

pub enum Step {
    Ok,
    /// `WAITI` executed and no interrupt is deliverable: the caller may fast-forward.
    Wfi,
    /// A general exception was taken (already dispatched; for diagnostics).
    Exception(Exception),
    Trap(Trap),
}

/// Internal: how an instruction ended when it did not simply continue.
enum Ex {
    /// Raise a general exception with (cause, vaddr).
    Gen(u32, u32),
    Trap(Trap),
    Wfi,
    /// State (including PC) already updated, e.g. a window exception was dispatched.
    Done,
}

// ------------------------------------------------------------------------------------------------
// State
// ------------------------------------------------------------------------------------------------

/// ESP32-S3 "cop_ai" (PIE) architectural state, coprocessor 3.
#[derive(Clone, Default, Debug)]
pub struct Cp3 {
    pub q: [[u32; 4]; 8],
    pub accx: [u32; 2],
    pub qacc_h: [u32; 5],
    pub qacc_l: [u32; 5],
    pub sar_byte: u32,
    pub fft_bit_width: u32,
    pub ua_state: [u32; 4],
}

#[derive(Clone, Debug)]
struct PendingCall {
    func: u32,
    args: [u32; 14],
    nargs: usize,
    return_to: u32,
    rot: u32,
    callinc: u32,
}

#[derive(Clone)]
pub struct Xtensa {
    /// Physical address registers; the current window is `ar[wb*4 ..][..16]` (mod 64).
    pub ar: [u32; 64],
    pub pc: u32,
    wb: u32,
    ws: u32,
    ps: u32,
    pub sar: u32,
    pub lbeg: u32,
    pub lend: u32,
    pub lcount: u32,
    /// Boolean registers b0..b15.
    pub br: u32,
    pub scompare1: u32,
    pub acclo: u32,
    pub acchi: u32,
    pub m: [u32; 4],
    pub vecbase: u32,
    /// EPC1..EPC7 at indices 1..7.
    pub epc: [u32; 8],
    /// EPS2..EPS7 at indices 2..7.
    pub eps: [u32; 8],
    /// EXCSAVE1..EXCSAVE7 at indices 1..7.
    pub excsave: [u32; 8],
    pub depc: u32,
    pub exccause: u32,
    pub excvaddr: u32,
    pub debugcause: u32,
    pub cpenable: u32,
    intenable: u32,
    /// Latched interrupt bits (software, edge, timer, NMI).
    latched: u32,
    /// Current external line inputs.
    lines: u32,
    ccount: u32,
    ccompare: [u32; 3],
    timer_left: u64,
    pub prid: u32,
    pub icount: u32,
    pub icountlevel: u32,
    pub ibreakenable: u32,
    pub ibreaka: [u32; 2],
    pub dbreaka: [u32; 2],
    pub dbreakc: [u32; 2],
    pub ddr: u32,
    pub misc: [u32; 4],
    pub atomctl: u32,
    pub memctl: u32,
    pub threadptr: u32,
    pub fcr: u32,
    pub fsr: u32,
    /// FPU registers f0..f15 (raw bits).
    pub f: [u32; 16],
    pub cp3: Cp3,
    /// Region-protection attributes (8 x 512 MiB), as written by WITLB/WDTLB.
    pub itlb_attr: [u32; 8],
    pub dtlb_attr: [u32; 8],
    // ---- derived ----
    /// Address-register quads above the current window usable without an overflow
    /// (0..=3; 3 when window checks are off).
    win_free: u8,
    /// Highest level among pending & enabled interrupts (0 = none).
    pend_level: u32,
    /// `pend_level` exceeds the current interrupt mask level.
    irq_armed: bool,
    pending_call: Option<PendingCall>,
    /// Lowest HLE scratch allocation for the next synthetic call (see `alloc_scratch`).
    scratch_low: Option<u32>,
    /// Direct-mapped decode cache: (pc, fetched word, decoded). Checked against the
    /// fetched bytes on every use, so modified code is always re-decoded.
    dcache: Box<[(u32, u32, Insn)]>,
    /// Statistics (diagnostics only).
    pub window_overflows: u64,
    pub window_underflows: u64,
    pub interrupts_taken: u64,
}

impl Default for Xtensa {
    fn default() -> Self {
        Self::new()
    }
}

#[inline(always)]
fn sext(v: u32, bits: u32) -> u32 {
    let s = 32 - bits;
    (((v << s) as i32) >> s) as u32
}

impl Xtensa {
    pub fn new() -> Self {
        let mut c = Xtensa {
            ar: [0; 64],
            pc: RESET_PC,
            wb: 0,
            ws: 1,
            ps: 0x1f,
            sar: 0,
            lbeg: 0,
            lend: 0,
            lcount: 0,
            br: 0,
            scompare1: 0,
            acclo: 0,
            acchi: 0,
            m: [0; 4],
            vecbase: VECBASE_RESET,
            epc: [0; 8],
            eps: [0; 8],
            excsave: [0; 8],
            depc: 0,
            exccause: 0,
            excvaddr: 0,
            debugcause: 0,
            cpenable: 0,
            intenable: 0,
            latched: 0,
            lines: 0,
            ccount: 0,
            ccompare: [0; 3],
            timer_left: 0,
            prid: 0xcdcd,
            icount: 0,
            icountlevel: 0,
            ibreakenable: 0,
            ibreaka: [0; 2],
            dbreaka: [0; 2],
            dbreakc: [0; 2],
            ddr: 0,
            misc: [0; 4],
            atomctl: 0x28,
            memctl: 0,
            threadptr: 0,
            fcr: 0,
            fsr: 0,
            f: [0; 16],
            cp3: Cp3::default(),
            itlb_attr: [2; 8],
            dtlb_attr: [2; 8],
            win_free: 3,
            pend_level: 0,
            irq_armed: false,
            pending_call: None,
            scratch_low: None,
            dcache: vec![(1, 0, decode(0)); DCACHE_SIZE].into_boxed_slice(),
            window_overflows: 0,
            window_underflows: 0,
            interrupts_taken: 0,
        };
        c.update_win();
        c.update_timer();
        c
    }

    /// Core reset: architectural state back to reset values (PRID and the external
    /// interrupt inputs are kept).
    pub fn reset(&mut self) {
        let prid = self.prid;
        let lines = self.lines;
        *self = Self::new();
        self.prid = prid;
        self.lines = lines;
        self.update_irq();
    }

    // ---- registers ------------------------------------------------------------------------------

    /// Address register `a<i>` of the current window.
    #[inline(always)]
    pub fn a(&self, i: u8) -> u32 {
        self.ar[((self.wb * 4 + i as u32) & 63) as usize]
    }

    #[inline(always)]
    pub fn set_a(&mut self, i: u8, v: u32) {
        self.ar[((self.wb * 4 + i as u32) & 63) as usize] = v;
    }

    pub fn windowbase(&self) -> u32 {
        self.wb
    }
    pub fn windowstart(&self) -> u32 {
        self.ws
    }
    pub fn set_window(&mut self, wb: u32, ws: u32) {
        self.wb = wb & 15;
        self.ws = ws & 0xffff;
        self.update_win();
    }
    pub fn ps(&self) -> u32 {
        self.ps
    }
    pub fn set_ps(&mut self, v: u32) {
        self.ps = v & ps::WRITABLE;
        self.update_win();
        self.update_armed();
    }
    fn callinc(&self) -> u32 {
        (self.ps >> ps::CALLINC_SHIFT) & 3
    }

    fn update_win(&mut self) {
        self.win_free =
            if self.ps & ps::WOE != 0 && self.ps & ps::EXCM == 0 { self.free_quads().min(3) as u8 } else { 3 };
    }

    /// Number of quads above WINDOWBASE before the next live frame (0..=15).
    fn free_quads(&self) -> u32 {
        let rep = self.ws | (self.ws << 16);
        (rep >> (self.wb + 1)).trailing_zeros().min(15)
    }

    // ---- interrupts -----------------------------------------------------------------------------

    /// The INTERRUPT special register.
    pub fn interrupt(&self) -> u32 {
        self.latched | (self.lines & INT_EXTERN_LEVEL)
    }

    pub fn intenable(&self) -> u32 {
        self.intenable
    }

    pub fn set_intenable(&mut self, v: u32) {
        self.intenable = v;
        self.update_irq();
    }

    /// Drive the external interrupt inputs (bit n = interrupt n; only bits configured as
    /// external level/edge or NMI are used). Level lines are sampled continuously; edge
    /// lines and the NMI latch on a 0->1 transition.
    pub fn set_irq_lines(&mut self, mask: u32) {
        let rising = mask & !self.lines;
        self.latched |= rising & (INT_EXTERN_EDGE | INT_NMI);
        self.lines = mask;
        self.update_irq();
    }

    pub fn irq_lines(&self) -> u32 {
        self.lines
    }

    /// Current interrupt mask level (PS.INTLEVEL, raised to EXCM_LEVEL while PS.EXCM).
    pub fn cintlevel(&self) -> u32 {
        let il = self.ps & ps::INTLEVEL;
        if self.ps & ps::EXCM != 0 { il.max(EXCM_LEVEL) } else { il }
    }

    fn update_irq(&mut self) {
        let pend = self.interrupt() & (self.intenable | INT_NMI);
        self.pend_level = 0;
        if pend != 0 {
            for l in (1..=7).rev() {
                if LEVEL_MASK[l] & pend != 0 {
                    self.pend_level = l as u32;
                    break;
                }
            }
        }
        self.update_armed();
    }

    #[inline(always)]
    fn update_armed(&mut self) {
        // The NMI (level 7, edge) is not maskable by PS.INTLEVEL / PS.EXCM.
        self.irq_armed = self.pend_level > self.cintlevel() || self.latched & INT_NMI != 0;
    }

    /// Is an interrupt deliverable right now?
    pub fn irq_pending(&self) -> bool {
        self.irq_armed
    }

    fn take_interrupt(&mut self) {
        let level = self.pend_level;
        self.interrupts_taken += 1;
        let pc = self.pc;
        if level == 1 {
            self.exccause = cause::LEVEL1_INTERRUPT;
            self.epc[1] = pc;
            let vec = if self.ps & ps::UM != 0 { USER_VEC } else { KERNEL_VEC };
            self.ps |= ps::EXCM;
            self.pc = self.vecbase.wrapping_add(vec);
        } else {
            let l = level as usize;
            self.epc[l] = pc;
            self.eps[l] = self.ps;
            self.ps = (self.ps & !ps::INTLEVEL) | level | ps::EXCM;
            self.pc = self.vecbase.wrapping_add(LEVEL_VEC[l]);
            if level == NMI_LEVEL {
                self.latched &= !INT_NMI;
            }
        }
        self.update_win();
        self.update_irq();
    }

    // ---- timers ---------------------------------------------------------------------------------

    pub fn ccount(&self) -> u32 {
        self.ccount
    }

    pub fn set_ccount(&mut self, v: u32) {
        self.ccount = v;
        self.update_timer();
    }

    pub fn ccompare(&self, i: usize) -> u32 {
        self.ccompare[i]
    }

    /// Cycles until the next CCOMPARE match (1..=2^32).
    pub fn cycles_to_timer(&self) -> u64 {
        self.timer_left
    }

    /// Advance CCOUNT by `n` without executing (e.g. while parked in WAITI), raising
    /// the timer interrupts of any CCOMPARE values passed.
    pub fn advance_ccount(&mut self, n: u64) {
        if n == 0 {
            return;
        }
        let mut fired = false;
        for i in 0..3 {
            let d = self.ccompare[i].wrapping_sub(self.ccount);
            let d = if d == 0 { 1u64 << 32 } else { d as u64 };
            if d <= n {
                self.latched |= 1 << TIMER_INT[i];
                fired = true;
            }
        }
        self.ccount = self.ccount.wrapping_add(n as u32);
        self.update_timer();
        if fired {
            self.update_irq();
        }
    }

    fn update_timer(&mut self) {
        let mut m = 1u64 << 32;
        for i in 0..3 {
            let d = self.ccompare[i].wrapping_sub(self.ccount);
            let d = if d == 0 { 1u64 << 32 } else { d as u64 };
            m = m.min(d);
        }
        self.timer_left = m;
    }

    #[cold]
    fn timer_match(&mut self) {
        for i in 0..3 {
            if self.ccompare[i] == self.ccount {
                self.latched |= 1 << TIMER_INT[i];
            }
        }
        self.update_timer();
        self.update_irq();
    }

    // ---- exceptions -----------------------------------------------------------------------------

    /// Raise a general (level-1 class) exception at `self.pc`, dispatching to the guest
    /// vector exactly as the hardware does.
    pub fn raise_exception(&mut self, cause: u32, vaddr: u32) -> Exception {
        let pc = self.pc;
        self.exccause = cause;
        if matches!(cause, 2 | 3 | 9 | 12..=29) {
            self.excvaddr = vaddr;
        }
        let double = self.ps & ps::EXCM != 0;
        if double {
            self.depc = pc;
            self.pc = self.vecbase.wrapping_add(DOUBLE_VEC);
        } else {
            self.epc[1] = pc;
            let vec = if self.ps & ps::UM != 0 { USER_VEC } else { KERNEL_VEC };
            self.pc = self.vecbase.wrapping_add(vec);
        }
        self.ps |= ps::EXCM;
        self.update_win();
        self.update_armed();
        Exception { cause, pc, vaddr, double }
    }

    /// Take a debug exception (level 6) at the current PC with the given DEBUGCAUSE, as
    /// the hardware would for BREAK (DEBUGCAUSE bit 3 = BREAK, bit 4 = BREAK.N).
    pub fn take_debug_exception(&mut self, debugcause: u32) {
        let l = DEBUG_LEVEL as usize;
        self.debugcause = debugcause;
        self.epc[l] = self.pc;
        self.eps[l] = self.ps;
        self.ps = (self.ps & !ps::INTLEVEL) | DEBUG_LEVEL | ps::EXCM;
        self.pc = self.vecbase.wrapping_add(LEVEL_VEC[l]);
        self.update_win();
        self.update_armed();
    }

    /// Dispatch a window overflow for the oldest live frame above WINDOWBASE; the
    /// handler returns (RFWO) to `pc`, which then re-executes.
    fn window_overflow(&mut self, pc: u32) {
        let rep = self.ws | (self.ws << 16);
        let x = rep >> (self.wb + 1);
        let n = x.trailing_zeros() + 1;
        let owb = self.wb;
        self.window_overflows += 1;
        let vec = match (x >> n).trailing_zeros() {
            0 => WINDOW_OF4,
            1 => WINDOW_OF8,
            _ => WINDOW_OF12,
        };
        self.wb = (self.wb + n) & 15;
        self.ps = (self.ps & !ps::OWB) | (owb << ps::OWB_SHIFT) | ps::EXCM;
        self.epc[1] = pc;
        self.pc = self.vecbase.wrapping_add(vec);
        self.update_win();
        self.update_armed();
    }

    /// Dispatch a window underflow returning by `n` quads from the frame at WINDOWBASE.
    fn window_underflow(&mut self, pc: u32, n: u32) {
        let owb = self.wb;
        self.window_underflows += 1;
        self.wb = (self.wb.wrapping_sub(n)) & 15;
        self.ps = (self.ps & !ps::OWB) | (owb << ps::OWB_SHIFT) | ps::EXCM;
        self.epc[1] = pc;
        let vec = match n {
            1 => WINDOW_UF4,
            2 => WINDOW_UF8,
            _ => WINDOW_UF12,
        };
        self.pc = self.vecbase.wrapping_add(vec);
        self.update_win();
        self.update_armed();
    }

    // ---- special registers ----------------------------------------------------------------------

    /// RSR. `None` for registers the S3 does not have.
    pub fn read_sr(&self, n: u32) -> Option<u32> {
        Some(match n {
            0 => self.lbeg,
            1 => self.lend,
            2 => self.lcount,
            3 => self.sar,
            4 => self.br,
            12 => self.scompare1,
            16 => self.acclo,
            17 => self.acchi,
            32..=35 => self.m[(n - 32) as usize],
            72 => self.wb,
            73 => self.ws,
            96 => self.ibreakenable,
            97 => self.memctl,
            99 => self.atomctl,
            104 => self.ddr,
            128 | 129 => self.ibreaka[(n - 128) as usize],
            144 | 145 => self.dbreaka[(n - 144) as usize],
            160 | 161 => self.dbreakc[(n - 160) as usize],
            176 => CONFIGID0,
            177..=183 => self.epc[(n - 176) as usize],
            192 => self.depc,
            194..=199 => self.eps[(n - 192) as usize],
            208 => CONFIGID1,
            209..=215 => self.excsave[(n - 208) as usize],
            224 => self.cpenable,
            226 => self.interrupt(),
            228 => self.intenable,
            230 => self.ps,
            231 => self.vecbase,
            232 => self.exccause,
            233 => self.debugcause,
            234 => self.ccount,
            235 => self.prid,
            236 => self.icount,
            237 => self.icountlevel,
            238 => self.excvaddr,
            240..=242 => self.ccompare[(n - 240) as usize],
            244..=247 => self.misc[(n - 244) as usize],
            _ => return None,
        })
    }

    /// WSR. Returns false for registers the S3 does not have (or read-only ones).
    pub fn write_sr(&mut self, n: u32, v: u32) -> bool {
        match n {
            0 => self.lbeg = v,
            1 => self.lend = v,
            2 => self.lcount = v,
            3 => self.sar = v & 63,
            4 => self.br = v & 0xffff,
            12 => self.scompare1 = v,
            16 => self.acclo = v,
            17 => self.acchi = v as u8 as i8 as i32 as u32,
            32..=35 => self.m[(n - 32) as usize] = v,
            72 => {
                self.wb = v & 15;
                self.update_win();
            }
            73 => {
                self.ws = v & 0xffff;
                self.update_win();
            }
            96 => self.ibreakenable = v & 3,
            97 => self.memctl = v,
            99 => self.atomctl = v & 0x3f,
            104 => self.ddr = v,
            128 | 129 => self.ibreaka[(n - 128) as usize] = v,
            144 | 145 => self.dbreaka[(n - 144) as usize] = v,
            160 | 161 => self.dbreakc[(n - 160) as usize] = v & 0xc000_003f,
            176 | 208 => {} // CONFIGID: writes ignored
            177..=183 => self.epc[(n - 176) as usize] = v,
            192 => self.depc = v,
            194..=199 => self.eps[(n - 192) as usize] = v & ps::WRITABLE,
            209..=215 => self.excsave[(n - 208) as usize] = v,
            224 => self.cpenable = v & 0xff,
            226 => {
                // INTSET: software interrupts only
                self.latched |= v & INT_SOFTWARE;
                self.update_irq();
            }
            227 => {
                // INTCLEAR: software and edge-triggered
                self.latched &= !(v & (INT_SOFTWARE | INT_EXTERN_EDGE));
                self.update_irq();
            }
            228 => self.set_intenable(v),
            230 => self.set_ps(v),
            231 => self.vecbase = v & !0x3ff,
            232 => self.exccause = v & 63,
            233 => self.debugcause = v,
            234 => self.set_ccount(v),
            236 => self.icount = v,
            237 => self.icountlevel = v & 15,
            238 => self.excvaddr = v,
            240..=242 => {
                let i = (n - 240) as usize;
                self.ccompare[i] = v;
                self.latched &= !(1 << TIMER_INT[i]);
                self.update_timer();
                self.update_irq();
            }
            244..=247 => self.misc[(n - 244) as usize] = v,
            _ => return false,
        }
        true
    }

    /// Coprocessor that owns user register `n`, if any.
    fn ur_cp(n: u32) -> Option<u32> {
        match n {
            0..=18 => Some(3),
            232 | 233 => Some(0),
            _ => None,
        }
    }

    pub fn read_ur(&self, n: u32) -> Option<u32> {
        let c = &self.cp3;
        Some(match n {
            0 => c.accx[0],
            1 => c.accx[1],
            2..=6 => c.qacc_h[(n - 2) as usize],
            7..=11 => c.qacc_l[(n - 7) as usize],
            13 => c.sar_byte,
            14 => c.fft_bit_width,
            15..=18 => c.ua_state[(n - 15) as usize],
            231 => self.threadptr,
            232 => self.fcr,
            233 => self.fsr,
            _ => return None,
        })
    }

    pub fn write_ur(&mut self, n: u32, v: u32) -> bool {
        let c = &mut self.cp3;
        match n {
            0 => c.accx[0] = v,
            1 => c.accx[1] = v & 0xff,
            2..=6 => c.qacc_h[(n - 2) as usize] = v,
            7..=11 => c.qacc_l[(n - 7) as usize] = v,
            13 => c.sar_byte = v & 15,
            14 => c.fft_bit_width = v & 15,
            15..=18 => c.ua_state[(n - 15) as usize] = v,
            231 => self.threadptr = v,
            232 => self.fcr = v & 0x7f,
            233 => self.fsr = v & 0xf80,
            _ => return false,
        }
        true
    }

    // ---- the interpreter ------------------------------------------------------------------------

    /// Take a pending interrupt or fetch, decode and execute one instruction.
    #[inline(always)]
    pub fn step<B: MemBus>(&mut self, bus: &mut B) -> Step {
        if self.irq_armed && self.pc != SYNTH_CALL_PC {
            self.take_interrupt();
            return Step::Ok;
        }
        let pc = self.pc;
        let w = match bus.fetch32(pc) {
            Ok(w) => w,
            Err(_) => match self.fetch_slow(bus, pc) {
                Some(w) => w,
                None => return self.fetch_fault(bus, pc),
            },
        };
        let slot = ((pc >> 1) as usize) & (DCACHE_SIZE - 1);
        let e = &mut self.dcache[slot];
        let insn = if e.0 == pc && e.1 == w {
            e.2
        } else {
            let i = decode(w);
            *e = (pc, w, i);
            i
        };
        bus.tick();
        self.ccount = self.ccount.wrapping_add(1);
        self.timer_left -= 1;
        if self.timer_left == 0 {
            self.timer_match();
        }
        match self.exec(bus, &insn, pc) {
            Ok(()) => Step::Ok,
            Err(e) => self.finish(e),
        }
    }

    #[cold]
    fn finish(&mut self, e: Ex) -> Step {
        match e {
            Ex::Gen(cause, vaddr) => Step::Exception(self.raise_exception(cause, vaddr)),
            Ex::Trap(t) => Step::Trap(t),
            Ex::Wfi => Step::Wfi,
            Ex::Done => Step::Ok,
        }
    }

    /// Instruction bytes near the end of a mapped region: fetch only what is needed.
    #[cold]
    fn fetch_slow<B: MemBus>(&mut self, bus: &mut B, pc: u32) -> Option<u32> {
        let lo = bus.fetch16(pc).ok()? as u32;
        match insn_len(lo as u8) {
            2 => Some(lo),
            3 => {
                let hi = bus.fetch16(pc.wrapping_add(1)).ok()? as u32;
                Some(lo | ((hi >> 8) << 16))
            }
            _ => None,
        }
    }

    #[cold]
    fn fetch_fault<B: MemBus>(&mut self, bus: &mut B, pc: u32) -> Step {
        if pc == SYNTH_CALL_PC {
            return self.synth_call(bus);
        }
        if pc == SYNTH_RET_PC {
            return self.synth_ret(bus);
        }
        Step::Exception(self.raise_exception(cause::INSTR_PROHIBITED, pc))
    }

    #[inline(always)]
    fn ld8<B: MemBus>(bus: &mut B, a: u32) -> Result<u32, Ex> {
        bus.read8(a).map(|v| v as u32).map_err(|_| Ex::Gen(cause::LOAD_PROHIBITED, a))
    }
    #[inline(always)]
    fn ld16<B: MemBus>(bus: &mut B, a: u32) -> Result<u32, Ex> {
        bus.read16(a).map(|v| v as u32).map_err(|_| Ex::Gen(cause::LOAD_PROHIBITED, a))
    }
    #[inline(always)]
    fn ld32<B: MemBus>(bus: &mut B, a: u32) -> Result<u32, Ex> {
        bus.read32(a).map_err(|_| Ex::Gen(cause::LOAD_PROHIBITED, a))
    }
    #[inline(always)]
    fn st8<B: MemBus>(bus: &mut B, a: u32, v: u32) -> Result<(), Ex> {
        bus.write8(a, v as u8).map_err(|_| Ex::Gen(cause::STORE_PROHIBITED, a))
    }
    #[inline(always)]
    fn st16<B: MemBus>(bus: &mut B, a: u32, v: u32) -> Result<(), Ex> {
        bus.write16(a, v as u16).map_err(|_| Ex::Gen(cause::STORE_PROHIBITED, a))
    }
    #[inline(always)]
    fn st32<B: MemBus>(bus: &mut B, a: u32, v: u32) -> Result<(), Ex> {
        bus.write32(a, v).map_err(|_| Ex::Gen(cause::STORE_PROHIBITED, a))
    }

    #[inline(always)]
    fn cwoe(&self) -> bool {
        self.ps & (ps::WOE | ps::EXCM) == ps::WOE
    }

    #[inline(always)]
    fn fp_on(&self) -> Result<(), Ex> {
        if self.cpenable & 1 != 0 { Ok(()) } else { Err(Ex::Gen(cause::CP0_DISABLED, 0)) }
    }

    #[inline(always)]
    fn cp3_on(&self) -> Result<(), Ex> {
        if self.cpenable & 8 != 0 { Ok(()) } else { Err(Ex::Gen(cause::CP3_DISABLED, 0)) }
    }

    #[inline(always)]
    fn b(&self, i: u8) -> bool {
        self.br >> i & 1 != 0
    }

    #[inline(always)]
    fn set_b(&mut self, i: u8, v: bool) {
        self.br = (self.br & !(1 << i)) | ((v as u32) << i);
    }

    fn acc(&self) -> i64 {
        ((self.acchi as u8 as i8 as i64) << 32) | self.acclo as i64
    }

    fn set_acc(&mut self, v: i64) {
        self.acclo = v as u32;
        self.acchi = (v >> 32) as u8 as i8 as i32 as u32;
    }

    #[inline(always)]
    fn exec<B: MemBus>(&mut self, bus: &mut B, i: &Insn, pc: u32) -> Result<(), Ex> {
        if i.wq > self.win_free {
            self.window_overflow(pc);
            return Err(Ex::Done);
        }
        let (r, s, t) = (i.r, i.s, i.t);
        let fall = pc.wrapping_add(i.len as u32);
        macro_rules! jump {
            ($target:expr) => {{
                self.pc = $target;
                return Ok(());
            }};
        }
        macro_rules! branch {
            ($cond:expr) => {{
                if $cond {
                    jump!(pc.wrapping_add(i.imm as u32));
                }
            }};
        }
        let rnd = || fpu::Round::from_fcr(self.fcr);
        match i.op {
            Op::Add | Op::AddN => self.set_a(r, self.a(s).wrapping_add(self.a(t))),
            Op::Addx2 => self.set_a(r, (self.a(s) << 1).wrapping_add(self.a(t))),
            Op::Addx4 => self.set_a(r, (self.a(s) << 2).wrapping_add(self.a(t))),
            Op::Addx8 => self.set_a(r, (self.a(s) << 3).wrapping_add(self.a(t))),
            Op::Sub => self.set_a(r, self.a(s).wrapping_sub(self.a(t))),
            Op::Subx2 => self.set_a(r, (self.a(s) << 1).wrapping_sub(self.a(t))),
            Op::Subx4 => self.set_a(r, (self.a(s) << 2).wrapping_sub(self.a(t))),
            Op::Subx8 => self.set_a(r, (self.a(s) << 3).wrapping_sub(self.a(t))),
            Op::And => self.set_a(r, self.a(s) & self.a(t)),
            Op::Or => self.set_a(r, self.a(s) | self.a(t)),
            Op::Xor => self.set_a(r, self.a(s) ^ self.a(t)),
            Op::Min => self.set_a(r, (self.a(s) as i32).min(self.a(t) as i32) as u32),
            Op::Max => self.set_a(r, (self.a(s) as i32).max(self.a(t) as i32) as u32),
            Op::Minu => self.set_a(r, self.a(s).min(self.a(t))),
            Op::Maxu => self.set_a(r, self.a(s).max(self.a(t))),
            Op::Salt => self.set_a(r, ((self.a(s) as i32) < (self.a(t) as i32)) as u32),
            Op::Saltu => self.set_a(r, (self.a(s) < self.a(t)) as u32),
            Op::Mull => self.set_a(r, self.a(s).wrapping_mul(self.a(t))),
            Op::Muluh => self.set_a(r, ((self.a(s) as u64 * self.a(t) as u64) >> 32) as u32),
            Op::Mulsh => self.set_a(r, ((self.a(s) as i32 as i64 * self.a(t) as i32 as i64) >> 32) as u32),
            Op::Mul16u => self.set_a(r, (self.a(s) & 0xffff) * (self.a(t) & 0xffff)),
            Op::Mul16s => self.set_a(r, ((self.a(s) as i16 as i32) * (self.a(t) as i16 as i32)) as u32),
            Op::Quou | Op::Quos | Op::Remu | Op::Rems => {
                let (x, y) = (self.a(s), self.a(t));
                if y == 0 {
                    return Err(Ex::Gen(cause::DIVIDE_BY_ZERO, 0));
                }
                let v = match i.op {
                    Op::Quou => x / y,
                    Op::Remu => x % y,
                    Op::Quos => (x as i32).wrapping_div(y as i32) as u32,
                    _ => (x as i32).wrapping_rem(y as i32) as u32,
                };
                self.set_a(r, v);
            }
            Op::Moveqz => {
                if self.a(t) == 0 {
                    self.set_a(r, self.a(s))
                }
            }
            Op::Movnez => {
                if self.a(t) != 0 {
                    self.set_a(r, self.a(s))
                }
            }
            Op::Movltz => {
                if (self.a(t) as i32) < 0 {
                    self.set_a(r, self.a(s))
                }
            }
            Op::Movgez => {
                if (self.a(t) as i32) >= 0 {
                    self.set_a(r, self.a(s))
                }
            }
            Op::Movf => {
                if !self.b(t) {
                    self.set_a(r, self.a(s))
                }
            }
            Op::Movt => {
                if self.b(t) {
                    self.set_a(r, self.a(s))
                }
            }
            Op::Neg => self.set_a(r, self.a(t).wrapping_neg()),
            Op::Abs => self.set_a(r, (self.a(t) as i32).unsigned_abs()),
            Op::Sext => self.set_a(r, sext(self.a(s), i.imm as u32 + 1)),
            Op::Clamps => {
                let n = i.imm as u32;
                let lo = -(1i64 << n);
                let hi = (1i64 << n) - 1;
                self.set_a(r, (self.a(s) as i32 as i64).clamp(lo, hi) as u32);
            }
            Op::Nsa => {
                let x = self.a(s);
                let v = if (x as i32) < 0 { !x } else { x };
                self.set_a(t, if v == 0 { 31 } else { v.leading_zeros() - 1 });
            }
            Op::Nsau => self.set_a(t, self.a(s).leading_zeros()),
            Op::Sll => self.set_a(r, (((self.a(s) as u64) << 32) >> (self.sar & 63)) as u32),
            Op::Srl => self.set_a(r, ((self.a(t) as u64) >> (self.sar & 63)) as u32),
            Op::Sra => self.set_a(r, ((self.a(t) as i32 as i64) >> (self.sar & 63)) as u32),
            Op::Src => {
                let v = ((self.a(s) as u64) << 32) | self.a(t) as u64;
                self.set_a(r, (v >> (self.sar & 63)) as u32);
            }
            Op::Slli => self.set_a(r, ((self.a(s) as u64) << i.imm) as u32),
            Op::Srli => self.set_a(r, self.a(t) >> i.imm),
            Op::Srai => self.set_a(r, ((self.a(t) as i32) >> i.imm) as u32),
            Op::Ssr => self.sar = self.a(s) & 31,
            Op::Ssl => self.sar = 32 - (self.a(s) & 31),
            Op::Ssa8l => self.sar = (self.a(s) & 3) << 3,
            Op::Ssa8b => self.sar = 32 - ((self.a(s) & 3) << 3),
            Op::Ssai => self.sar = i.imm as u32,
            Op::Extui => self.set_a(r, (self.a(t) >> i.imm2) & i.imm as u32),

            Op::L8ui => {
                let v = Self::ld8(bus, self.a(s).wrapping_add(i.imm as u32))?;
                self.set_a(t, v);
            }
            Op::L16ui => {
                let v = Self::ld16(bus, self.a(s).wrapping_add(i.imm as u32))?;
                self.set_a(t, v);
            }
            Op::L16si => {
                let v = Self::ld16(bus, self.a(s).wrapping_add(i.imm as u32))?;
                self.set_a(t, v as u16 as i16 as i32 as u32);
            }
            Op::L32i | Op::L32iN | Op::L32ai | Op::L32e => {
                let v = Self::ld32(bus, self.a(s).wrapping_add(i.imm as u32))?;
                self.set_a(t, v);
            }
            Op::S8i => Self::st8(bus, self.a(s).wrapping_add(i.imm as u32), self.a(t))?,
            Op::S16i => Self::st16(bus, self.a(s).wrapping_add(i.imm as u32), self.a(t))?,
            Op::S32i | Op::S32iN | Op::S32ri | Op::S32e | Op::S32nb => {
                Self::st32(bus, self.a(s).wrapping_add(i.imm as u32), self.a(t))?
            }
            Op::S32c1i => {
                let addr = self.a(s).wrapping_add(i.imm as u32);
                let old = Self::ld32(bus, addr)?;
                if old == self.scompare1 {
                    Self::st32(bus, addr, self.a(t))?;
                }
                self.set_a(t, old);
            }
            Op::L32r => {
                let addr = (pc.wrapping_add(3) & !3).wrapping_add(i.imm as u32);
                let v = Self::ld32(bus, addr)?;
                self.set_a(t, v);
            }
            Op::Addi | Op::Addmi => self.set_a(t, self.a(s).wrapping_add(i.imm as u32)),
            Op::AddiN => self.set_a(r, self.a(s).wrapping_add(i.imm as u32)),
            Op::Movi => self.set_a(t, i.imm as u32),
            Op::MoviN => self.set_a(s, i.imm as u32),
            Op::MovN => self.set_a(t, self.a(s)),

            Op::Beqz | Op::BeqzN => branch!(self.a(s) == 0),
            Op::Bnez | Op::BnezN => branch!(self.a(s) != 0),
            Op::Bltz => branch!((self.a(s) as i32) < 0),
            Op::Bgez => branch!((self.a(s) as i32) >= 0),
            Op::Beqi => branch!(self.a(s) == i.imm2 as u32),
            Op::Bnei => branch!(self.a(s) != i.imm2 as u32),
            Op::Blti => branch!((self.a(s) as i32) < i.imm2),
            Op::Bgei => branch!((self.a(s) as i32) >= i.imm2),
            Op::Bltui => branch!(self.a(s) < i.imm2 as u32),
            Op::Bgeui => branch!(self.a(s) >= i.imm2 as u32),
            Op::Beq => branch!(self.a(s) == self.a(t)),
            Op::Bne => branch!(self.a(s) != self.a(t)),
            Op::Blt => branch!((self.a(s) as i32) < (self.a(t) as i32)),
            Op::Bge => branch!((self.a(s) as i32) >= (self.a(t) as i32)),
            Op::Bltu => branch!(self.a(s) < self.a(t)),
            Op::Bgeu => branch!(self.a(s) >= self.a(t)),
            Op::Bany => branch!(self.a(s) & self.a(t) != 0),
            Op::Bnone => branch!(self.a(s) & self.a(t) == 0),
            Op::Ball => branch!(!self.a(s) & self.a(t) == 0),
            Op::Bnall => branch!(!self.a(s) & self.a(t) != 0),
            Op::Bbc => branch!(self.a(s) >> (self.a(t) & 31) & 1 == 0),
            Op::Bbs => branch!(self.a(s) >> (self.a(t) & 31) & 1 != 0),
            Op::Bbci => branch!(self.a(s) >> i.imm2 & 1 == 0),
            Op::Bbsi => branch!(self.a(s) >> i.imm2 & 1 != 0),
            Op::Bf => branch!(!self.b(s)),
            Op::Bt => branch!(self.b(s)),

            Op::J => jump!(pc.wrapping_add(i.imm as u32)),
            Op::Jx => jump!(self.a(s)),
            Op::Call0 => {
                self.set_a(0, fall);
                jump!((pc & !3).wrapping_add(i.imm as u32));
            }
            Op::Callx0 => {
                let target = self.a(s);
                self.set_a(0, fall);
                jump!(target);
            }
            Op::Calln | Op::Callxn => {
                let n = i.imm2 as u32;
                let target = if i.op == Op::Calln { (pc & !3).wrapping_add(i.imm as u32) } else { self.a(s) };
                self.ps = (self.ps & !ps::CALLINC) | (n << ps::CALLINC_SHIFT);
                self.set_a((n * 4) as u8, (n << 30) | (fall & 0x3fff_ffff));
                jump!(target);
            }
            Op::Ret | Op::RetN => jump!(self.a(0)),
            Op::Retw | Op::RetwN => return self.retw(pc),
            Op::Entry => {
                if s > 3 || !self.cwoe() {
                    return Err(Ex::Gen(cause::ILLEGAL, 0));
                }
                let n = self.callinc();
                if n as u8 > self.win_free {
                    self.window_overflow(pc);
                    return Err(Ex::Done);
                }
                let v = self.a(s).wrapping_sub(i.imm as u32);
                self.ar[(((self.wb + n) * 4 + s as u32) & 63) as usize] = v;
                self.wb = (self.wb + n) & 15;
                self.ws |= 1 << self.wb;
                self.update_win();
            }
            Op::Movsp => {
                let m = (self.ws | (self.ws << 16)) >> (self.wb + 16 - 3) & 7;
                if m == 0 {
                    return Err(Ex::Gen(cause::ALLOCA, 0));
                }
                self.set_a(t, self.a(s));
            }
            Op::Rotw => {
                self.wb = (self.wb as i32 + i.imm) as u32 & 15;
                self.update_win();
            }
            Op::Loop | Op::Loopnez | Op::Loopgtz => {
                let v = self.a(s);
                self.lcount = v.wrapping_sub(1);
                self.lbeg = fall;
                self.lend = pc.wrapping_add(i.imm as u32);
                let skip = match i.op {
                    Op::Loopnez => v == 0,
                    Op::Loopgtz => (v as i32) <= 0,
                    _ => false,
                };
                jump!(if skip { self.lend } else { fall });
            }

            Op::Rsr => {
                let v = self.read_sr(i.imm as u32).ok_or(Ex::Gen(cause::ILLEGAL, 0))?;
                self.set_a(t, v);
            }
            Op::Wsr => {
                if !self.write_sr(i.imm as u32, self.a(t)) {
                    return Err(Ex::Gen(cause::ILLEGAL, 0));
                }
            }
            Op::Xsr => {
                let n = i.imm as u32;
                let old = self.read_sr(n).ok_or(Ex::Gen(cause::ILLEGAL, 0))?;
                if n == 226 || !self.write_sr(n, self.a(t)) {
                    return Err(Ex::Gen(cause::ILLEGAL, 0));
                }
                self.set_a(t, old);
            }
            Op::Rur => {
                let n = i.imm as u32;
                match Self::ur_cp(n) {
                    Some(0) => self.fp_on()?,
                    Some(_) => self.cp3_on()?,
                    None => {}
                }
                let v = self.read_ur(n).ok_or(Ex::Gen(cause::ILLEGAL, 0))?;
                self.set_a(r, v);
            }
            Op::Wur => {
                let n = i.imm as u32;
                match Self::ur_cp(n) {
                    Some(0) => self.fp_on()?,
                    Some(_) => self.cp3_on()?,
                    None => {}
                }
                if !self.write_ur(n, self.a(t)) {
                    return Err(Ex::Gen(cause::ILLEGAL, 0));
                }
            }
            Op::Rsil => {
                let old = self.ps;
                self.set_ps((self.ps & !ps::INTLEVEL) | s as u32);
                self.set_a(t, old);
            }
            Op::Waiti => {
                self.set_ps((self.ps & !ps::INTLEVEL) | s as u32);
                self.pc = fall;
                return if self.irq_armed { Ok(()) } else { Err(Ex::Wfi) };
            }
            Op::Rfe | Op::Rfue => {
                self.set_ps(self.ps & !ps::EXCM);
                jump!(self.epc[1]);
            }
            // Returns into the handler that took the double exception, which was running
            // with PS.EXCM set: EXCM is left alone (as in QEMU).
            Op::Rfde => jump!(self.depc),
            Op::Rfwo | Op::Rfwu => {
                if i.op == Op::Rfwo {
                    self.ws &= !(1 << self.wb);
                } else {
                    self.ws |= 1 << self.wb;
                }
                self.wb = (self.ps & ps::OWB) >> ps::OWB_SHIFT;
                self.set_ps(self.ps & !ps::EXCM);
                jump!(self.epc[1]);
            }
            Op::Rfi => {
                let l = s as usize;
                if !(2..=7).contains(&l) {
                    return Err(Ex::Gen(cause::ILLEGAL, 0));
                }
                self.set_ps(self.eps[l]);
                jump!(self.epc[l]);
            }
            Op::Rfdo | Op::Rfdd => {
                let l = DEBUG_LEVEL as usize;
                self.set_ps(self.eps[l]);
                jump!(self.epc[l]);
            }
            Op::Syscall => return Err(Ex::Gen(cause::SYSCALL, 0)),
            Op::Simcall => {
                // Simulator call (no simulator attached): a2 = -1 (ENOSYS-ish), as QEMU
                // does for unknown calls without semihosting.
                self.set_a(2, u32::MAX);
            }
            Op::Break => return Err(Ex::Trap(Trap::Break { pc, s, t, narrow: false })),
            Op::BreakN => return Err(Ex::Trap(Trap::Break { pc, s, t: 0, narrow: true })),
            Op::Ill | Op::IllN => return Err(Ex::Gen(cause::ILLEGAL, 0)),
            Op::Sync | Op::NopN | Op::Cache => {}
            Op::Rer => self.set_a(t, 0),
            Op::Wer => {}
            Op::Tlb => {
                let va = self.a(s);
                let idx = (va >> 29) as usize;
                match r {
                    6 => self.itlb_attr[idx] = self.a(t) & 15,
                    14 => self.dtlb_attr[idx] = self.a(t) & 15,
                    3 | 11 => self.set_a(t, va & 0xe000_0000),
                    7 => self.set_a(t, self.itlb_attr[idx]),
                    15 => self.set_a(t, self.dtlb_attr[idx]),
                    5 | 13 => self.set_a(t, (va & 0xe000_0000) | 1),
                    _ => {}
                }
            }

            Op::Andb => self.set_b(r, self.b(s) & self.b(t)),
            Op::Andbc => self.set_b(r, self.b(s) & !self.b(t)),
            Op::Orb => self.set_b(r, self.b(s) | self.b(t)),
            Op::Orbc => self.set_b(r, self.b(s) | !self.b(t)),
            Op::Xorb => self.set_b(r, self.b(s) ^ self.b(t)),
            Op::Any4 | Op::All4 | Op::Any8 | Op::All8 => {
                let w = if matches!(i.op, Op::Any4 | Op::All4) { 4 } else { 8 };
                let m = ((1u32 << w) - 1) << (s & !(w - 1));
                let v = match i.op {
                    Op::Any4 | Op::Any8 => self.br & m != 0,
                    _ => self.br & m == m,
                };
                self.set_b(t, v);
            }

            Op::Mac16 => self.mac16(bus, i)?,
            Op::Ldinc | Op::Lddec => {
                let addr = if i.op == Op::Ldinc { self.a(s).wrapping_add(4) } else { self.a(s).wrapping_sub(4) };
                let v = Self::ld32(bus, addr)?;
                self.m[(r & 3) as usize] = v;
                self.set_a(s, addr);
            }

            // ---- FPU ----
            Op::Lsi | Op::Lsiu => {
                self.fp_on()?;
                let addr = self.a(s).wrapping_add(i.imm as u32);
                self.f[t as usize] = Self::ld32(bus, addr)?;
                if i.op == Op::Lsiu {
                    self.set_a(s, addr);
                }
            }
            Op::Ssi | Op::Ssiu => {
                self.fp_on()?;
                let addr = self.a(s).wrapping_add(i.imm as u32);
                Self::st32(bus, addr, self.f[t as usize])?;
                if i.op == Op::Ssiu {
                    self.set_a(s, addr);
                }
            }
            Op::Lsx | Op::Lsxu => {
                self.fp_on()?;
                let addr = self.a(s).wrapping_add(self.a(t));
                self.f[r as usize] = Self::ld32(bus, addr)?;
                if i.op == Op::Lsxu {
                    self.set_a(s, addr);
                }
            }
            Op::Ssx | Op::Ssxu => {
                self.fp_on()?;
                let addr = self.a(s).wrapping_add(self.a(t));
                Self::st32(bus, addr, self.f[r as usize])?;
                if i.op == Op::Ssxu {
                    self.set_a(s, addr);
                }
            }
            Op::AddS | Op::SubS | Op::MulS | Op::MaddS | Op::MsubS | Op::MaddnS => {
                self.fp_on()?;
                let (fr, fs, ft) = (self.f[r as usize], self.f[s as usize], self.f[t as usize]);
                self.f[r as usize] = match i.op {
                    Op::AddS => fpu::add(fs, ft, rnd()),
                    Op::SubS => fpu::sub(fs, ft, rnd()),
                    Op::MulS => fpu::mul(fs, ft, rnd()),
                    Op::MaddS => fpu::madd(fr, fs, ft, false, rnd()),
                    Op::MsubS => fpu::madd(fr, fs, ft, true, rnd()),
                    _ => fpu::madd(fr, fs, ft, false, fpu::Round::NearestEven),
                };
            }
            // Division/square-root helper steps: the final result is produced exactly by
            // MKDADJ.S / MKSADJ.S and moved into place by ADDEXPM.S (same model as QEMU),
            // so the Newton-Raphson steps in between have no architectural effect.
            Op::DivnS | Op::Div0S | Op::Sqrt0S | Op::Nexp01S | Op::AddexpS => self.fp_on()?,
            Op::AddexpmS | Op::MovS => {
                self.fp_on()?;
                self.f[r as usize] = self.f[s as usize];
            }
            Op::MkdadjS => {
                self.fp_on()?;
                self.f[r as usize] = fpu::div(self.f[s as usize], self.f[r as usize]);
            }
            Op::MksadjS => {
                self.fp_on()?;
                self.f[r as usize] = fpu::sqrt(self.f[s as usize]);
            }
            Op::Recip0S => {
                self.fp_on()?;
                self.f[r as usize] = fpu::recip(self.f[s as usize]);
            }
            Op::Rsqrt0S => {
                self.fp_on()?;
                self.f[r as usize] = fpu::rsqrt(self.f[s as usize]);
            }
            Op::AbsS => {
                self.fp_on()?;
                self.f[r as usize] = self.f[s as usize] & 0x7fff_ffff;
            }
            Op::NegS => {
                self.fp_on()?;
                self.f[r as usize] = self.f[s as usize] ^ 0x8000_0000;
            }
            Op::ConstS => {
                self.fp_on()?;
                self.f[r as usize] = fpu::const_s(i.imm as u32);
            }
            Op::Rfr => {
                self.fp_on()?;
                self.set_a(r, self.f[s as usize]);
            }
            Op::Wfr => {
                self.fp_on()?;
                self.f[r as usize] = self.a(s);
            }
            Op::RoundS | Op::TruncS | Op::FloorS | Op::CeilS => {
                self.fp_on()?;
                let how = match i.op {
                    Op::RoundS => fpu::ToInt::Nearest,
                    Op::TruncS => fpu::ToInt::Trunc,
                    Op::FloorS => fpu::ToInt::Floor,
                    _ => fpu::ToInt::Ceil,
                };
                self.set_a(r, fpu::to_int(self.f[s as usize], i.imm as u32, how));
            }
            Op::UtruncS => {
                self.fp_on()?;
                self.set_a(r, fpu::to_uint_trunc(self.f[s as usize], i.imm as u32));
            }
            Op::FloatS | Op::UfloatS => {
                self.fp_on()?;
                self.f[r as usize] = fpu::from_int(self.a(s), i.op == Op::FloatS, i.imm as u32, rnd());
            }
            Op::UnS | Op::OeqS | Op::UeqS | Op::OltS | Op::UltS | Op::OleS | Op::UleS => {
                self.fp_on()?;
                let (x, y) = (self.f[s as usize], self.f[t as usize]);
                let un = fpu::un(x, y);
                let v = match i.op {
                    Op::UnS => un,
                    Op::OeqS => fpu::oeq(x, y),
                    Op::UeqS => un || fpu::oeq(x, y),
                    Op::OltS => fpu::olt(x, y),
                    Op::UltS => un || fpu::olt(x, y),
                    Op::OleS => fpu::ole(x, y),
                    _ => un || fpu::ole(x, y),
                };
                self.set_b(r, v);
            }
            Op::MoveqzS | Op::MovnezS | Op::MovltzS | Op::MovgezS | Op::MovfS | Op::MovtS => {
                self.fp_on()?;
                let c = match i.op {
                    Op::MoveqzS => self.a(t) == 0,
                    Op::MovnezS => self.a(t) != 0,
                    Op::MovltzS => (self.a(t) as i32) < 0,
                    Op::MovgezS => (self.a(t) as i32) >= 0,
                    Op::MovfS => !self.b(t),
                    _ => self.b(t),
                };
                if c {
                    self.f[r as usize] = self.f[s as usize];
                }
            }

            // ---- cop_ai ----
            Op::LdQr | Op::StQr => {
                self.cp3_on()?;
                let addr = self.a(t).wrapping_add(i.imm as u32) & !15;
                let q = i.imm2 as usize;
                for k in 0..4u32 {
                    let a = addr.wrapping_add(k * 4);
                    if i.op == Op::LdQr {
                        self.cp3.q[q][k as usize] = Self::ld32(bus, a)?;
                    } else {
                        Self::st32(bus, a, self.cp3.q[q][k as usize])?;
                    }
                }
            }

            Op::Unknown => return Err(Ex::Trap(Trap::Unimplemented { pc, raw: i.raw, len: i.len })),
        }
        // Sequential fall-through: zero-overhead loop back-edge.
        self.pc = if fall == self.lend && self.lcount != 0 && self.ps & ps::EXCM == 0 {
            self.lcount -= 1;
            self.lbeg
        } else {
            fall
        };
        Ok(())
    }

    fn retw(&mut self, pc: u32) -> Result<(), Ex> {
        let a0 = self.a(0);
        let n = a0 >> 30;
        if n == 0 || !self.cwoe() {
            return Err(Ex::Gen(cause::ILLEGAL, 0));
        }
        let target_wb = self.wb.wrapping_sub(n) & 15;
        if self.ws & (1 << target_wb) == 0 {
            self.window_underflow(pc, n);
            return Err(Ex::Done);
        }
        self.ws &= !(1 << self.wb);
        self.wb = target_wb;
        self.update_win();
        self.pc = (pc & 0xc000_0000) | (a0 & 0x3fff_ffff);
        Ok(())
    }

    fn mac16<B: MemBus>(&mut self, bus: &mut B, i: &Insn) -> Result<(), Ex> {
        let (r, s, t) = (i.r, i.s, i.t);
        let op1 = i.imm as u32;
        let op2 = i.imm2;
        let mx = self.m[((r >> 2) & 1) as usize];
        let my = self.m[(2 + ((t >> 2) & 1)) as usize];
        let (x, y) = match op2 {
            0..=2 => (mx, my),
            3 => (self.a(s), my),
            4..=6 => (mx, self.a(t)),
            _ => (self.a(s), self.a(t)),
        };
        let kind = op1 >> 2;
        let half = |v: u32, hi: bool| if hi { v >> 16 } else { v & 0xffff };
        let hx = half(x, op1 & 1 != 0);
        let hy = half(y, op1 & 2 != 0);
        // the load (if any) happens after the operands were read
        if matches!(op2, 0 | 1 | 4 | 5) {
            let addr = if op2 & 1 == 0 { self.a(s).wrapping_add(4) } else { self.a(s).wrapping_sub(4) };
            let v = Self::ld32(bus, addr)?;
            self.m[(r & 3) as usize] = v;
            self.set_a(s, addr);
        }
        let sp = (hx as u16 as i16 as i64) * (hy as u16 as i16 as i64);
        match kind {
            0 => self.set_acc((hx * hy) as i64),
            1 => self.set_acc(sp),
            2 => self.set_acc(self.acc().wrapping_add(sp)),
            _ => self.set_acc(self.acc().wrapping_sub(sp)),
        }
        Ok(())
    }

    // ---- HLE call synthesis ---------------------------------------------------------------------

    #[cold]
    fn synth_call<B: MemBus>(&mut self, bus: &mut B) -> Step {
        let pc = self.pc;
        let Some(call) = self.pending_call.clone() else {
            return Step::Trap(Trap::HleCall { pc });
        };
        let n = call.rot;
        // Quads WB+1 ..= WB+n+3 (the synthetic frame and the callee's a0..a7) must not hold
        // a live frame: spill through the guest's overflow handler until they are free.
        if self.free_quads() < n + 3 && self.ws != 1 << self.wb {
            if !self.cwoe() {
                return Step::Trap(Trap::HleCall { pc });
            }
            self.window_overflow(pc);
            return Step::Ok;
        }
        let sp_x = self.a(1);
        let mut sp_h = sp_x.wrapping_sub(80) & !15;
        // Scratch from alloc_scratch sits between the save areas and our descriptor.
        if let Some(low) = self.scratch_low.take() {
            sp_h = sp_h.min(low.wrapping_sub(48) & !15);
        }
        let meta = [call.return_to, call.callinc, SYNTH_COOKIE, n];
        for (k, v) in meta.iter().enumerate() {
            if bus.write32(sp_h + 32 + 4 * k as u32, *v).is_err() {
                return Step::Trap(Trap::HleCall { pc });
            }
        }
        for k in 6..call.nargs {
            if bus.write32(sp_h + 4 * (k as u32 - 6), call.args[k]).is_err() {
                return Step::Trap(Trap::HleCall { pc });
            }
        }
        // virtual ENTRY of the hooked function (rotation n), then CALL8 func from it
        self.ar[(((self.wb + n) * 4 + 1) & 63) as usize] = sp_h;
        self.wb = (self.wb + n) & 15;
        self.ws |= 1 << self.wb;
        self.set_a(8, (2 << 30) | (SYNTH_RET_PC & 0x3fff_ffff));
        for k in 0..call.nargs.min(6) {
            self.set_a(10 + k as u8, call.args[k]);
        }
        self.ps = (self.ps & !ps::CALLINC) | (2 << ps::CALLINC_SHIFT);
        self.update_win();
        self.pending_call = None;
        self.pc = call.func;
        Step::Ok
    }

    #[cold]
    fn synth_ret<B: MemBus>(&mut self, bus: &mut B) -> Step {
        let pc = self.pc;
        let sp_h = self.a(1);
        let mut meta = [0u32; 4];
        for (k, m) in meta.iter_mut().enumerate() {
            match bus.read32(sp_h + 32 + 4 * k as u32) {
                Ok(v) => *m = v,
                Err(_) => return Step::Trap(Trap::HleCall { pc }),
            }
        }
        let [return_to, callinc, cookie, n] = meta;
        if cookie != SYNTH_COOKIE || !(1..=3).contains(&n) {
            return Step::Trap(Trap::HleCall { pc });
        }
        // callee's a2 -> our a2, which is the hook context's a(4n+2)
        self.set_a(2, self.a(10));
        let target_wb = self.wb.wrapping_sub(n) & 15;
        if self.ws & (1 << target_wb) == 0 {
            if !self.cwoe() {
                return Step::Trap(Trap::HleCall { pc });
            }
            self.window_underflow(pc, n);
            return Step::Ok;
        }
        let _ = bus.write32(sp_h + 40, 0);
        self.ws &= !(1 << self.wb);
        self.wb = target_wb;
        self.ps = (self.ps & !ps::CALLINC) | (callinc << ps::CALLINC_SHIFT);
        self.update_win();
        self.pc = return_to;
        Step::Ok
    }

    /// Hook-context register base: 4 * PS.CALLINC.
    fn hook_base(&self) -> u8 {
        (self.callinc() * 4) as u8
    }

    /// Disassemble the instruction at `pc` (for diagnostics).
    pub fn disasm_at<B: MemBus>(bus: &mut B, pc: u32) -> String {
        match bus.fetch32(pc).or_else(|_| bus.fetch16(pc).map(|v| v as u32)) {
            Ok(w) => decode(w).disasm(pc),
            Err(_) => "<unmapped>".into(),
        }
    }
}

/// HLE view. All accessors assume the "hook context": the CPU sits at the first
/// instruction of a windowed function that was just called with CALLn/CALLXn (n =
/// PS.CALLINC), before its ENTRY. With n = 0 (a CALL0-ABI function) the same accessors
/// degrade to the CALL0 convention (args in a2.., return address in a0), except for
/// `begin_call`, which needs a windowed context.
impl GuestCpu for Xtensa {
    fn pc(&self) -> u32 {
        self.pc
    }
    fn set_pc(&mut self, pc: u32) {
        self.pc = pc;
    }
    fn arg(&self, n: usize) -> u32 {
        assert!(n < 6, "stack-passed args not supported");
        self.a(self.hook_base() + 2 + n as u8)
    }
    fn set_arg(&mut self, n: usize, v: u32) {
        assert!(n < 6, "stack-passed args not supported");
        self.set_a(self.hook_base() + 2 + n as u8, v);
    }
    fn ret_val(&self) -> u32 {
        self.a(self.hook_base() + 2)
    }
    fn sp(&self) -> u32 {
        self.a(1)
    }
    fn set_sp(&mut self, v: u32) {
        self.set_a(1, v);
    }
    fn return_address(&self) -> u32 {
        let ra = self.a(self.hook_base());
        if self.callinc() == 0 { ra } else { (self.pc & 0xc000_0000) | (ra & 0x3fff_ffff) }
    }
    /// Emulates the callee running `ENTRY ...; RETW`: the value lands in the caller's
    /// a(4n+2) and execution resumes at the decoded return address. The window is
    /// unchanged (ENTRY and RETW cancel out).
    /// Scratch lives in the synthetic frame `begin_call` builds, below the hook context's
    /// stack pointer. That pointer itself must not move: on the windowed ABI the caller's
    /// registers spill to addresses derived from it, so a temporarily lowered a1 makes a
    /// spill and its reload disagree.
    fn alloc_scratch(&mut self, n: u32) -> u32 {
        // Stay clear of the save areas in the 32 bytes below SP.
        let base = self.scratch_low.unwrap_or(self.a(1).wrapping_sub(32));
        let p = base.wrapping_sub(n) & !15;
        self.scratch_low = Some(p);
        p
    }
    fn return_from_hook(&mut self, ret: Option<u32>) {
        self.scratch_low = None;
        if let Some(v) = ret {
            self.set_a(self.hook_base() + 2, v);
        }
        self.pc = self.return_address();
    }
    /// Synthesise a call to `func` that returns to `return_to` with the CPU back in the
    /// same hook context (same window, same PS.CALLINC) and the callee's result
    /// readable via `ret_val()`.
    ///
    /// Mechanism: the PC is set to [`SYNTH_CALL_PC`]; the next `step()` spills old frames
    /// through the guest's overflow handler if needed, performs a virtual ENTRY of the
    /// hooked function with an 80-byte frame below the current SP (reserving the ABI
    /// save areas, a 16-byte descriptor holding `return_to`/CALLINC, and room for up to
    /// 8 stack arguments), and then a CALL8 to `func` whose return address is
    /// [`SYNTH_RET_PC`]. When `func` executes RETW, the step at SYNTH_RET_PC undoes the
    /// virtual ENTRY (via the guest's underflow handler if the caller's frame was
    /// spilled in the meantime), copies the result to a(4n+2) and jumps to `return_to`.
    /// Everything needed after the call lives in guest registers/memory, so it survives
    /// context switches and migration between cores. Interrupts are held off only while
    /// the call is being set up.
    fn begin_call(&mut self, func: u32, args: &[u32], return_to: u32) {
        assert!(args.len() <= 14, "at most 14 arguments supported");
        let callinc = self.callinc();
        if callinc == 0 {
            log::warn!("xtensa: begin_call from a CALL0 context (pc={:#x}); a12-a15 may be clobbered", self.pc);
        }
        let mut a = [0u32; 14];
        a[..args.len()].copy_from_slice(args);
        self.pending_call = Some(PendingCall {
            func,
            args: a,
            nargs: args.len(),
            return_to,
            rot: if callinc == 0 { 3 } else { callinc },
            callinc,
        });
        self.pc = SYNTH_CALL_PC;
    }
    /// Level-1 interrupts are deliverable.
    fn irq_enabled(&self) -> bool {
        self.cintlevel() == 0
    }
    /// Raise interrupt `line`: level-triggered lines are asserted (the SoC must deassert
    /// them with [`Xtensa::set_irq_lines`]); edge/software/NMI lines are latched. The core
    /// takes it on the next `step()` if its configured level (core-isa tables; `level` is
    /// ignored) is above the current mask. SoCs should normally just use `set_irq_lines`.
    fn enter_interrupt(&mut self, line: u32, _level: u32) {
        let bit = 1 << (line & 31);
        if bit & INT_EXTERN_LEVEL != 0 {
            self.set_irq_lines(self.lines | bit);
        } else {
            self.latched |= bit;
            self.update_irq();
        }
    }
    fn gpr_dump(&self) -> String {
        let mut s = format!(
            "pc={:08x} ps={:08x} wb={} ws={:04x} epc1={:08x} exccause={} excvaddr={:08x} sar={} lcount={:08x}\n",
            self.pc, self.ps, self.wb, self.ws, self.epc[1], self.exccause, self.excvaddr, self.sar, self.lcount
        );
        for i in 0..16u8 {
            s += &format!("{:>4}={:08x}{}", format!("a{i}"), self.a(i), if i % 4 == 3 { "\n" } else { " " });
        }
        s
    }
}
