//! Tests for the Xtensa core. The heavy lifting is done by self-checking guest programs
//! in `tests/xtensa/` (C + assembly, prebuilt ELFs checked in; rebuild with
//! `tests/xtensa/build.sh`).

use super::decode::{Op, decode};
use super::scan;
use std::path::PathBuf;

fn repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

// ------------------------------------------------------------------------------------------------
// Opcode coverage against real firmware
// ------------------------------------------------------------------------------------------------

fn coverage(path: &str, name: &str, extra_roots: &[(u32, String)]) -> Option<scan::ScanResult> {
    let data = std::fs::read(path).ok()?;
    let code = scan::Code::from_elf(&data);
    let res = scan::scan(&code, extra_roots);
    let bytes: usize = res.insns.values().map(|i| i.len as usize).sum();
    eprintln!(
        "{name}: {} reachable instructions ({} of {} code bytes) from {} roots, {} undecodable ({} more after noreturn calls, ignored)",
        res.insns.len(),
        bytes,
        code.code_bytes(),
        code.roots.len() + extra_roots.len(),
        res.unknown.len(),
        res.after_noreturn
    );
    // Machine-readable dump for tests/xtensa/scripts/compare_objdump.py and the report.
    let out = repo().join("target").join(format!("xtensa-coverage-{name}.txt"));
    let mut s = String::new();
    for (m, n) in res.histogram() {
        s += &format!("# {m} {n}\n");
    }
    let mut seen = std::collections::HashSet::new();
    for (pc, i) in &res.insns {
        let raw = i.raw & if i.len == 4 { u32::MAX } else { (1 << (8 * i.len)) - 1 };
        if seen.insert(raw) {
            s += &format!("{pc:08x} {} {:0w$x} {}\n", i.len, raw, i.disasm(*pc), w = 2 * i.len as usize);
        }
    }
    std::fs::write(&out, s).ok();
    // Blob of every distinct encoding in an 8-byte slot (padded with NOPs so objdump
    // stays in sync) plus our disassembly at the slot address, for
    // tests/xtensa/scripts/compare_objdump.py.
    let mut blob = Vec::new();
    let mut txt = String::new();
    let mut seen = std::collections::HashSet::new();
    for i in res.insns.values() {
        let len = i.len as usize;
        let raw = i.raw & if len == 4 { u32::MAX } else { (1 << (8 * len)) - 1 };
        if i.op == Op::Unknown || !seen.insert(raw) {
            continue;
        }
        let pc = blob.len() as u32;
        blob.extend_from_slice(&raw.to_le_bytes()[..len]);
        let pad: &[u8] = match len {
            2 => &[0xf0, 0x20, 0x00, 0xf0, 0x20, 0x00],
            3 => &[0xf0, 0x20, 0x00, 0x3d, 0xf0],
            _ => &[0x3d, 0xf0, 0x3d, 0xf0],
        };
        blob.extend_from_slice(pad);
        txt += &format!("{pc:x}\t{}\n", decode(raw).disasm(pc));
    }
    std::fs::write(repo().join("target").join(format!("xtensa-coverage-{name}.bin")), blob).ok();
    std::fs::write(repo().join("target").join(format!("xtensa-coverage-{name}.dis")), txt).ok();
    Some(res)
}

fn report_unknown(res: &scan::ScanResult) -> String {
    let mut s = String::new();
    for (pc, raw, len, root) in res.unknown.iter().take(50) {
        s += &format!("  {pc:08x}: {raw:0w$x} (reached from {root})\n", w = 2 * *len as usize);
    }
    s
}

/// Every instruction reachable from a symbol in the TRMNL firmware must decode.
/// Skipped unless XTENSA_FIRMWARE_ELF names an S3 build's firmware ELF.
#[test]
fn opcode_coverage_firmware() {
    let Ok(path) = std::env::var("XTENSA_FIRMWARE_ELF") else {
        eprintln!("skipping: set XTENSA_FIRMWARE_ELF");
        return;
    };
    // The vector table (VECBASE) is at the start of .iram0.vectors; the vectors
    // themselves are reached through VECBASE, not calls.
    let Some(res) = coverage(&path, "firmware", &[]) else {
        eprintln!("skipping: {path} not found");
        return;
    };
    assert!(res.unknown.is_empty(), "undecodable reachable instructions:\n{}", report_unknown(&res));
}

/// ... and its second-stage bootloader (XTENSA_BOOTLOADER_ELF).
#[test]
fn opcode_coverage_bootloader() {
    let Ok(path) = std::env::var("XTENSA_BOOTLOADER_ELF") else {
        eprintln!("skipping: set XTENSA_BOOTLOADER_ELF");
        return;
    };
    let Some(res) = coverage(&path, "bootloader", &[]) else {
        eprintln!("skipping: {path} not found");
        return;
    };
    assert!(res.unknown.is_empty(), "undecodable reachable instructions:\n{}", report_unknown(&res));
}

/// Same for the ESP32-S3 mask ROM.
#[test]
fn opcode_coverage_rom() {
    let path = std::env::var("XTENSA_ROM_ELF").unwrap_or_else(|_| {
        format!("{}/.platformio/packages/tool-esp-rom-elfs/esp32s3_rev0_rom.elf", std::env::var("HOME").unwrap())
    });
    let Some(res) = coverage(&path, "rom", &[]) else {
        eprintln!("skipping: {path} not found");
        return;
    };
    assert!(res.unknown.is_empty(), "undecodable reachable instructions:\n{}", report_unknown(&res));
}

/// Checked-in fixture: one example encoding of every mnemonic found in firmware.elf and
/// the ROM (generated by tests/xtensa/scripts/scan_opcodes.py). Runs without the ELFs.
#[test]
fn opcode_fixture_decodes() {
    let path = repo().join("tests/xtensa/opcodes.txt");
    let text = std::fs::read_to_string(&path).expect("tests/xtensa/opcodes.txt");
    let mut bad = Vec::new();
    for line in text.lines().filter(|l| !l.starts_with('#') && !l.trim().is_empty()) {
        let mut it = line.split_whitespace();
        let (Some(mn), Some(hex)) = (it.next(), it.next()) else { continue };
        let b = (0..hex.len()).step_by(2).map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap()).collect::<Vec<_>>();
        let mut w = 0u32;
        for (k, x) in b.iter().enumerate() {
            w |= (*x as u32) << (8 * k);
        }
        let i = decode(w);
        if i.op == Op::Unknown || i.len as usize != b.len() || i.mnemonic() != mn {
            bad.push(format!("{mn} {hex}: decoded as {} (len {})", i.mnemonic(), i.len));
        }
    }
    assert!(bad.is_empty(), "fixture mismatches:\n{}", bad.join("\n"));
}

// ------------------------------------------------------------------------------------------------
// Test bus and guest-program harness
// ------------------------------------------------------------------------------------------------

use super::{Step, Trap, Xtensa};
use crate::arch::{BusFault, BusResult, MemBus};

pub const RAM_BASE: u32 = 0x4000_0000;
pub const RAM_SIZE: usize = 0x8_0000;

/// Flat RAM at 0x4000_0000, a tiny MMIO block at 0x6000_0000 (see tests/xtensa/rt/rt.h),
/// and nothing else: every other access faults.
pub struct TestBus {
    pub ram: Vec<u8>,
    pub cycles: u64,
    pub exit: Option<u32>,
    pub out: String,
    pub irq_write: Option<u32>,
    pub actual: u32,
    pub expected: u32,
    /// Last value written to each MMIO word.
    pub mmio: [u32; 64],
}

impl TestBus {
    pub fn new() -> Self {
        TestBus {
            ram: vec![0; RAM_SIZE],
            cycles: 0,
            exit: None,
            out: String::new(),
            irq_write: None,
            actual: 0,
            expected: 0,
            mmio: [0; 64],
        }
    }
    #[inline(always)]
    fn off(&self, a: u32, n: u32) -> Option<usize> {
        let o = a.wrapping_sub(RAM_BASE);
        if (o as usize) + (n as usize) <= self.ram.len() { Some(o as usize) } else { None }
    }
    pub fn load_elf(&mut self, data: &[u8]) -> u32 {
        use object::{Object, ObjectSegment};
        let obj = object::File::parse(data).expect("elf");
        for seg in obj.segments() {
            let d = seg.data().unwrap();
            let a = seg.address() as u32;
            if d.is_empty() {
                continue;
            }
            let o = self.off(a, d.len() as u32).expect("segment outside RAM");
            self.ram[o..o + d.len()].copy_from_slice(d);
        }
        obj.entry() as u32
    }
    pub fn peek32(&self, a: u32) -> u32 {
        let o = self.off(a, 4).unwrap();
        u32::from_le_bytes(self.ram[o..o + 4].try_into().unwrap())
    }
    #[allow(clippy::match_overlapping_arm)] // specific registers first, then the rest of the page
    fn mmio_write(&mut self, a: u32, v: u32) -> BusResult<()> {
        if a & 0xffff_ff00 == 0x6000_0000 {
            self.mmio[((a & 0xff) / 4) as usize] = v;
        }
        match a {
            0x6000_0000 => self.exit = Some(v),
            0x6000_0004 => self.out.push(v as u8 as char),
            0x6000_0008 => self.irq_write = Some(v),
            0x6000_0010 => self.actual = v,
            0x6000_0014 => self.expected = v,
            0x6000_0000..=0x6000_00ff => {}
            _ => return Err(BusFault),
        }
        Ok(())
    }
}

impl MemBus for TestBus {
    #[inline(always)]
    fn fetch16(&mut self, a: u32) -> BusResult<u16> {
        let o = self.off(a, 2).ok_or(BusFault)?;
        Ok(u16::from_le_bytes([self.ram[o], self.ram[o + 1]]))
    }
    #[inline(always)]
    fn fetch32(&mut self, a: u32) -> BusResult<u32> {
        let o = self.off(a, 4).ok_or(BusFault)?;
        Ok(u32::from_le_bytes(self.ram[o..o + 4].try_into().unwrap()))
    }
    #[inline(always)]
    fn read8(&mut self, a: u32) -> BusResult<u8> {
        if let Some(o) = self.off(a, 1) {
            return Ok(self.ram[o]);
        }
        if a & 0xffff_ff00 == 0x6000_0000 { Ok(0) } else { Err(BusFault) }
    }
    #[inline(always)]
    fn read16(&mut self, a: u32) -> BusResult<u16> {
        if let Some(o) = self.off(a, 2) {
            return Ok(u16::from_le_bytes([self.ram[o], self.ram[o + 1]]));
        }
        if a & 0xffff_ff00 == 0x6000_0000 { Ok(0) } else { Err(BusFault) }
    }
    #[inline(always)]
    fn read32(&mut self, a: u32) -> BusResult<u32> {
        if let Some(o) = self.off(a, 4) {
            return Ok(u32::from_le_bytes(self.ram[o..o + 4].try_into().unwrap()));
        }
        if a & 0xffff_ff00 == 0x6000_0000 { Ok(0) } else { Err(BusFault) }
    }
    #[inline(always)]
    fn write8(&mut self, a: u32, v: u8) -> BusResult<()> {
        if let Some(o) = self.off(a, 1) {
            self.ram[o] = v;
            return Ok(());
        }
        self.mmio_write(a, v as u32)
    }
    #[inline(always)]
    fn write16(&mut self, a: u32, v: u16) -> BusResult<()> {
        if let Some(o) = self.off(a, 2) {
            self.ram[o..o + 2].copy_from_slice(&v.to_le_bytes());
            return Ok(());
        }
        self.mmio_write(a, v as u32)
    }
    #[inline(always)]
    fn write32(&mut self, a: u32, v: u32) -> BusResult<()> {
        if let Some(o) = self.off(a, 4) {
            self.ram[o..o + 4].copy_from_slice(&v.to_le_bytes());
            return Ok(());
        }
        self.mmio_write(a, v)
    }
    fn cycles(&self) -> u64 {
        self.cycles
    }
    #[inline(always)]
    fn tick(&mut self) {
        self.cycles += 1;
    }
}

pub struct Run {
    pub cpu: Xtensa,
    pub bus: TestBus,
    pub steps: u64,
    pub exceptions: Vec<super::Exception>,
    pub wfi: u64,
}

pub fn elf_path(name: &str) -> PathBuf {
    repo().join("tests/xtensa/elf").join(format!("{name}.elf"))
}

pub fn load(name: &str) -> Run {
    let data = std::fs::read(elf_path(name)).unwrap_or_else(|e| panic!("{name}.elf: {e}"));
    let mut bus = TestBus::new();
    let entry = bus.load_elf(&data);
    let mut cpu = Xtensa::new();
    assert_eq!(entry, super::config::RESET_PC);
    cpu.pc = entry;
    Run { cpu, bus, steps: 0, exceptions: Vec::new(), wfi: 0 }
}

impl Run {
    /// Step until the guest writes the exit register or `max` steps elapse. `hook` runs
    /// before every step and may redirect the CPU (return true to skip the step).
    pub fn run_with(&mut self, max: u64, mut hook: impl FnMut(&mut Xtensa, &mut TestBus) -> bool) -> u32 {
        while self.steps < max {
            if let Some(v) = self.bus.irq_write.take() {
                self.cpu.set_irq_lines(v);
            }
            if self.bus.exit.is_some() {
                break;
            }
            if hook(&mut self.cpu, &mut self.bus) {
                continue;
            }
            self.steps += 1;
            match self.cpu.step(&mut self.bus) {
                Step::Ok => {}
                Step::Wfi => {
                    self.wfi += 1;
                    let n = self.cpu.cycles_to_timer();
                    self.cpu.advance_ccount(n);
                }
                Step::Exception(e) => self.exceptions.push(e),
                Step::Trap(t) => panic!("trap {t:?}\n{}", crate::arch::GuestCpu::gpr_dump(&self.cpu)),
            }
        }
        match self.bus.exit {
            Some(c) => c,
            None => panic!("no exit after {} steps\n{}", self.steps, crate::arch::GuestCpu::gpr_dump(&self.cpu)),
        }
    }

    pub fn run(&mut self, max: u64) -> u32 {
        self.run_with(max, |_, _| false)
    }
}

/// Run a self-checking guest program; it exits with 0 on success or the failing line.
pub fn run_program(name: &str) -> Run {
    let mut r = load(name);
    let code = r.run(200_000_000);
    if code != 0 {
        panic!(
            "{name}: guest check failed at line {code} (actual {:#x}, expected {:#x}, aux {:#x} {:#x})\nout: {}\n{}",
            r.bus.actual,
            r.bus.expected,
            r.bus.mmio[6],
            r.bus.mmio[7],
            r.bus.out,
            crate::arch::GuestCpu::gpr_dump(&r.cpu)
        );
    }
    r
}

#[test]
fn guest_int_ops() {
    let r = run_program("int_ops");
    // four divide-by-zero exceptions were dispatched to the guest
    assert_eq!(r.exceptions.iter().filter(|e| e.cause == 6).count(), 4);
}

#[test]
fn guest_window() {
    let r = run_program("window");
    let allocas = r.exceptions.iter().filter(|e| e.cause == super::cause::ALLOCA).count();
    assert!(allocas > 0, "expected MOVSP alloca exceptions");
    assert!(r.exceptions.iter().all(|e| e.cause == super::cause::ALLOCA), "{:?}", r.exceptions);
    assert!(r.cpu.window_overflows > 1000 && r.cpu.window_underflows > 1000);
    eprintln!("window: {} overflows, {} underflows", r.cpu.window_overflows, r.cpu.window_underflows);
}

#[test]
fn guest_loops() {
    run_program("loops");
}

#[test]
fn guest_branches() {
    run_program("branches");
}

#[test]
fn guest_fpu() {
    run_program("fpu");
}

#[test]
fn guest_interrupts() {
    let r = run_program("interrupts");
    assert!(r.wfi > 0, "WAITI should have parked the core");
    eprintln!("interrupts taken: {}", r.cpu.interrupts_taken);
}

#[test]
fn guest_exceptions() {
    let r = run_program("exceptions");
    let causes: Vec<u32> = r.exceptions.iter().map(|e| e.cause).collect();
    assert_eq!(causes, vec![28, 29, 28, 20, 0, 0, 0, 0, 1, 28, 32, 32, 35]);
    assert!(r.exceptions[9].double);
}

#[test]
fn guest_atomics() {
    run_program("atomics");
}

#[test]
fn guest_mac16() {
    run_program("mac16");
}

fn elf_symbols(name: &str) -> std::collections::HashMap<String, u32> {
    use object::{Object, ObjectSymbol};
    let data = std::fs::read(elf_path(name)).unwrap();
    let obj = object::File::parse(&*data).unwrap();
    obj.symbols().filter_map(|s| Some((s.name().ok()?.to_string(), s.address() as u32))).collect()
}

/// Host side of tests/xtensa/src/hle.c: hooks before ENTRY, begin_call (with >6 args,
/// deep guest recursion inside, interrupts firing), return_from_hook, set_arg+set_pc,
/// and long chains of calls from one hook context.
#[test]
fn guest_hle() {
    use crate::arch::GuestCpu;
    let syms = elf_symbols("hle");
    let s = |n: &str| syms[n];
    let (hooked, hooked2, hooked3, add7, rsum) = (s("hooked"), s("hooked2"), s("hooked3"), s("add7"), s("rsum"));
    const M1: u32 = 0x7F10_0000;
    const M2: u32 = M1 + 2;
    const M3: u32 = M1 + 4;
    const M4: u32 = M1 + 6;
    let mut r = load("hle");
    // hook-context invariants captured at hook time: (windowbase, windowstart, callinc, sp)
    let mut ctx = (0u32, 0u32, 0u32, 0u32);
    let (mut a0, mut r1, mut ra, mut k, mut n, mut acc) = (0u32, 0u32, 0u32, 0u32, 0u32, 0u32);
    let mut hooks = 0;
    let snapshot = |c: &Xtensa| (c.windowbase(), c.windowstart(), (c.ps() >> 16) & 3, c.a(1));
    let code = r.run_with(50_000_000, |cpu, _bus| {
        let pc = cpu.pc;
        if pc == hooked || pc == hooked2 || pc == hooked3 {
            hooks += 1;
            ctx = snapshot(cpu);
            assert_ne!(ctx.2, 0, "hooked function reached with CALLINC=0");
        }
        if (M1..=M4).contains(&pc) {
            let now = snapshot(cpu);
            // same window and CALLINC; WINDOWSTART may differ only in bits below WB
            // (older frames the callee spilled)
            assert_eq!((now.0, now.2, now.3), (ctx.0, ctx.2, ctx.3), "hook context not restored at {pc:#x}");
            assert!(now.1 & (1 << now.0) != 0);
        }
        match pc {
            p if p == hooked => {
                a0 = cpu.arg(0);
                let (b, c) = (cpu.arg(1), cpu.arg(2));
                cpu.begin_call(add7, &[a0, b, c, a0 * 2, b * 2, c * 2, a0 + b + c], M1);
            }
            M1 => {
                r1 = cpu.ret_val();
                cpu.begin_call(rsum, &[(a0 & 31) + 10], M2);
            }
            M2 => {
                let r2 = cpu.ret_val();
                cpu.return_from_hook(Some(r1 ^ r2));
            }
            p if p == hooked2 => {
                ra = cpu.return_address();
                let x = cpu.arg(0);
                cpu.begin_call(rsum, &[x], M3);
            }
            M3 => {
                let v = cpu.ret_val();
                cpu.set_arg(0, v * 2);
                cpu.set_pc(ra);
            }
            p if p == hooked3 => {
                n = cpu.arg(0);
                k = 1;
                acc = 0;
                cpu.begin_call(add7, &[k, 0, 0, 0, 0, 0, 0], M4);
            }
            M4 => {
                acc += cpu.ret_val();
                k += 1;
                if k <= n {
                    cpu.begin_call(add7, &[k, 0, 0, 0, 0, 0, 0], M4);
                } else {
                    cpu.return_from_hook(Some(acc));
                }
            }
            _ => return false,
        }
        true
    });
    assert_eq!(
        code, 0,
        "guest check failed at line {code} (actual {:#x}, expected {:#x})",
        r.bus.actual, r.bus.expected
    );
    assert_eq!(hooks, 3 + 24 + 1);
    eprintln!(
        "hle: {} overflows, {} underflows, {} interrupts",
        r.cpu.window_overflows, r.cpu.window_underflows, r.cpu.interrupts_taken
    );
}

// ------------------------------------------------------------------------------------------------
// Direct unit tests (hand-placed instruction bytes)
// ------------------------------------------------------------------------------------------------

fn raw_cpu(code: &[u8]) -> (Xtensa, TestBus) {
    let mut bus = TestBus::new();
    let o = 0x1000;
    bus.ram[o..o + code.len()].copy_from_slice(code);
    let mut cpu = Xtensa::new();
    cpu.pc = RAM_BASE + o as u32;
    cpu.set_ps(ps::WOE | ps::UM);
    (cpu, bus)
}

use super::{cause, ps};

#[test]
fn break_is_reported_not_dispatched() {
    // break 1, 15 ; break.n 3
    let (mut cpu, mut bus) = raw_cpu(&[0xf0, 0x41, 0x00, 0x2d, 0xf3]);
    let pc = cpu.pc;
    match cpu.step(&mut bus) {
        Step::Trap(Trap::Break { pc: p, s: 1, t: 15, narrow: false }) => assert_eq!(p, pc),
        _ => panic!("expected BREAK trap"),
    }
    assert_eq!(cpu.pc, pc, "PC stays at the BREAK");
    cpu.pc += 3;
    assert!(matches!(cpu.step(&mut bus), Step::Trap(Trap::Break { s: 3, narrow: true, .. })));
    // emulating the debug exception is up to the caller
    cpu.take_debug_exception(1 << 4);
    assert_eq!(cpu.pc, cpu.vecbase + 0x280);
    assert_eq!(cpu.eps[6] & ps::EXCM, 0);
    assert_eq!(cpu.ps() & 0xf, 6);
}

#[test]
fn unimplemented_reports_raw_bytes() {
    // ee.zero.q q2 (24-bit, MAC16 opcode space) and a 32-bit op0=0xF TIE instruction
    let (mut cpu, mut bus) = raw_cpu(&[0xa4, 0x7f, 0xdd, 0x0f, 0x60, 0x00, 0xe6]);
    let pc = cpu.pc;
    match cpu.step(&mut bus) {
        Step::Trap(Trap::Unimplemented { pc: p, raw, len }) => {
            assert_eq!((p, raw, len), (pc, 0xdd7fa4, 3));
        }
        _ => panic!("expected unimplemented trap"),
    }
    cpu.pc += 3;
    match cpu.step(&mut bus) {
        Step::Trap(Trap::Unimplemented { raw, len, .. }) => assert_eq!((raw, len), (0xe600600f, 4)),
        _ => panic!("expected unimplemented trap"),
    }
    // the caller may hand it to the guest as an illegal instruction
    let e = cpu.raise_exception(cause::ILLEGAL, 0);
    assert_eq!(e.pc, pc + 3);
    assert_eq!(cpu.epc[1], pc + 3);
    assert_eq!(cpu.pc, cpu.vecbase + 0x340);
}

#[test]
fn ccount_and_timers() {
    let mut cpu = Xtensa::new();
    cpu.set_ps(0); // INTLEVEL 0, EXCM 0
    cpu.set_intenable(1 << 6 | 1 << 15);
    cpu.set_ccount(1000);
    cpu.write_sr(240, 1500);
    cpu.write_sr(241, 1200);
    assert_eq!(cpu.cycles_to_timer(), 200);
    cpu.advance_ccount(199);
    assert!(!cpu.irq_pending());
    cpu.advance_ccount(1);
    assert_eq!(cpu.interrupt(), 1 << 15);
    assert!(cpu.irq_pending());
    assert_eq!(cpu.cycles_to_timer(), 300);
    cpu.advance_ccount(10_000); // crosses 1500
    assert_eq!(cpu.interrupt(), 1 << 15 | 1 << 6);
    // rewriting CCOMPARE clears its interrupt
    cpu.write_sr(241, 0);
    assert_eq!(cpu.interrupt(), 1 << 6);
    // counts wrap: next match for CCOMPARE0 is a full period away
    assert_eq!(cpu.ccount(), 11_200);
    assert_eq!(cpu.cycles_to_timer(), (1u64 << 32) - 11_200);
}

#[test]
fn irq_lines_level_and_edge() {
    let mut cpu = Xtensa::new();
    cpu.set_ps(0);
    cpu.set_intenable(u32::MAX);
    cpu.set_irq_lines(1 << 0 | 1 << 10);
    assert_eq!(cpu.interrupt(), 1 << 0 | 1 << 10);
    cpu.set_irq_lines(0);
    assert_eq!(cpu.interrupt(), 1 << 10, "edge latched, level follows the line");
    cpu.write_sr(227, 1 << 10 | 1 << 0);
    assert_eq!(cpu.interrupt(), 0);
    // levels: line 19 is level 2, line 26 is level 5
    cpu.set_irq_lines(1 << 19);
    cpu.set_ps(1);
    assert!(cpu.irq_pending());
    cpu.set_ps(2);
    assert!(!cpu.irq_pending());
    cpu.set_ps(ps::EXCM); // EXCM masks up to level 3
    assert!(!cpu.irq_pending());
    cpu.set_irq_lines(1 << 26);
    assert!(cpu.irq_pending());
}

#[test]
fn reset_state() {
    let mut cpu = Xtensa::new();
    cpu.prid = 0xabab;
    cpu.set_a(5, 7);
    cpu.reset();
    assert_eq!(cpu.pc, 0x4000_0400);
    assert_eq!(cpu.ps(), 0x1f);
    assert_eq!(cpu.vecbase, 0x4000_0000);
    assert_eq!((cpu.windowbase(), cpu.windowstart()), (0, 1));
    assert_eq!(cpu.prid, 0xabab);
}

/// Emulator speed on a CRC/copy/recursion mix (prints MIPS; run with --release for a
/// meaningful number).
#[test]
fn bench_mips() {
    let mut r = load("bench");
    let t = std::time::Instant::now();
    let mut n = 0u64;
    let reps: u32 = std::env::var("BENCH_REPS").ok().and_then(|v| v.parse().ok()).unwrap_or(1);
    for _ in 1..reps {
        let mut b = load("bench");
        while b.bus.exit.is_none() {
            b.cpu.step(&mut b.bus);
        }
    }
    while r.bus.exit.is_none() && n < 500_000_000 {
        if let Step::Trap(t) = r.cpu.step(&mut r.bus) {
            panic!("{t:?}");
        }
        n += 1;
    }
    let dt = t.elapsed().as_secs_f64();
    assert_eq!(r.bus.exit, Some(0));
    eprintln!("bench: {n} instructions in {dt:.3}s = {:.1} MIPS", n as f64 / dt / 1e6);
}
