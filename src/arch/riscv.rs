//! RV32IMAC interpreter (machine mode only), as found in the ESP32-C3.

use super::{GuestCpu, MemBus, ReturnPatch};

pub const MSTATUS_MIE: u32 = 1 << 3;
pub const MSTATUS_MPIE: u32 = 1 << 7;
pub const MSTATUS_MPP: u32 = 3 << 11;

pub mod cause {
    pub const INSN_ACCESS: u32 = 1;
    pub const ILLEGAL: u32 = 2;
    pub const BREAKPOINT: u32 = 3;
    pub const LOAD_ACCESS: u32 = 5;
    pub const STORE_ACCESS: u32 = 7;
    pub const ECALL_M: u32 = 11;
}

#[derive(Debug, Clone, Copy)]
pub struct Trap {
    pub cause: u32,
    pub tval: u32,
}

impl Trap {
    pub fn new(cause: u32, tval: u32) -> Self {
        Trap { cause, tval }
    }
}

pub enum Step {
    Ok,
    /// `wfi` executed with nothing pending: caller may fast-forward time.
    Wfi,
    Trap(Trap),
}

#[derive(Default, Clone)]
pub struct Csrs {
    pub mstatus: u32,
    pub mtvec: u32,
    pub mepc: u32,
    pub mcause: u32,
    pub mtval: u32,
    pub mscratch: u32,
    pub mie: u32,
    /// Offset subtracted from the cycle counter to implement writes to mpccr.
    pub pccr_base: u64,
    pub mpcer: u32,
    pub mpcmr: u32,
    pub other: std::collections::HashMap<u16, u32>,
}

#[derive(Clone)]
pub struct Rv32 {
    pub x: [u32; 32],
    pub pc: u32,
    pub csr: Csrs,
    pub reservation: Option<u32>,
}

impl Default for Rv32 {
    fn default() -> Self {
        Self::new()
    }
}

#[inline(always)]
fn sext(v: u32, bits: u32) -> u32 {
    let s = 32 - bits;
    (((v << s) as i32) >> s) as u32
}

impl Rv32 {
    pub fn new() -> Self {
        Rv32 {
            x: [0; 32],
            pc: 0x4000_0000,
            csr: Csrs { mstatus: MSTATUS_MPP, ..Default::default() },
            reservation: None,
        }
    }

    #[inline(always)]
    fn wr(&mut self, rd: usize, v: u32) {
        if rd != 0 {
            self.x[rd] = v;
        }
    }

    /// Enter a trap (exception or interrupt). Vectored mode applies to interrupts only.
    pub fn take_trap(&mut self, cause: u32, tval: u32, epc: u32) {
        self.csr.mepc = epc;
        self.csr.mcause = cause;
        self.csr.mtval = tval;
        let mie = self.csr.mstatus & MSTATUS_MIE != 0;
        self.csr.mstatus &= !(MSTATUS_MIE | MSTATUS_MPIE);
        if mie {
            self.csr.mstatus |= MSTATUS_MPIE;
        }
        self.csr.mstatus |= MSTATUS_MPP;
        let base = self.csr.mtvec & !3;
        let vectored = self.csr.mtvec & 1 != 0;
        self.pc =
            if vectored && cause & 0x8000_0000 != 0 { base.wrapping_add(4 * (cause & 0x7fff_ffff)) } else { base };
        self.reservation = None;
    }

    pub fn read_csr(&self, n: u16, cycles: u64) -> Option<u32> {
        Some(match n {
            0x300 => self.csr.mstatus,
            0x301 => 0x4000_1104, // RV32IMC (+A)
            0x304 => self.csr.mie,
            0x305 => self.csr.mtvec,
            0x340 => self.csr.mscratch,
            0x341 => self.csr.mepc,
            0x342 => self.csr.mcause,
            0x343 => self.csr.mtval,
            0x344 => 0, // mip
            0xF11..=0xF13 => 0,
            0xF14 => 0, // mhartid
            0x7E0 | 0x800 => self.csr.mpcer,
            0x7E1 | 0x801 => self.csr.mpcmr,
            0x7E2 | 0x802 => cycles.wrapping_sub(self.csr.pccr_base) as u32,
            0xB00 | 0xC00 => cycles as u32,
            0xB80 | 0xC80 => (cycles >> 32) as u32,
            _ => *self.csr.other.get(&n).unwrap_or(&0),
        })
    }

    pub fn write_csr(&mut self, n: u16, v: u32, cycles: u64) {
        match n {
            0x300 => self.csr.mstatus = (v & (MSTATUS_MIE | MSTATUS_MPIE)) | MSTATUS_MPP,
            0x304 => self.csr.mie = v,
            0x305 => self.csr.mtvec = v,
            0x340 => self.csr.mscratch = v,
            0x341 => self.csr.mepc = v & !1,
            0x342 => self.csr.mcause = v,
            0x343 => self.csr.mtval = v,
            0x7E0 | 0x800 => self.csr.mpcer = v,
            0x7E1 | 0x801 => self.csr.mpcmr = v,
            0x7E2 | 0x802 => self.csr.pccr_base = cycles.wrapping_sub(v as u64),
            _ => {
                self.csr.other.insert(n, v);
            }
        }
    }

    /// Fetch, decode and execute a single instruction.
    #[inline(always)]
    pub fn step<B: MemBus>(&mut self, bus: &mut B) -> Step {
        let pc = self.pc;
        let lo = match bus.fetch16(pc) {
            Ok(v) => v as u32,
            Err(_) => return Step::Trap(Trap::new(cause::INSN_ACCESS, pc)),
        };
        bus.tick();
        if lo & 3 != 3 {
            return match self.exec_c(bus, lo) {
                Ok(s) => s,
                Err(t) => Step::Trap(t),
            };
        }
        let hi = match bus.fetch16(pc.wrapping_add(2)) {
            Ok(v) => v as u32,
            Err(_) => return Step::Trap(Trap::new(cause::INSN_ACCESS, pc)),
        };
        let insn = lo | (hi << 16);
        match self.exec32(bus, insn) {
            Ok(s) => s,
            Err(t) => Step::Trap(t),
        }
    }

    fn load<B: MemBus>(&mut self, bus: &mut B, addr: u32, width: u32, signed: bool) -> Result<u32, Trap> {
        let r = match width {
            1 => bus.read8(addr).map(|v| if signed { sext(v as u32, 8) } else { v as u32 }),
            2 => bus.read16(addr).map(|v| if signed { sext(v as u32, 16) } else { v as u32 }),
            _ => bus.read32(addr),
        };
        r.map_err(|_| Trap::new(cause::LOAD_ACCESS, addr))
    }

    fn store<B: MemBus>(&mut self, bus: &mut B, addr: u32, width: u32, v: u32) -> Result<(), Trap> {
        if let Some(r) = self.reservation
            && r & !3 == addr & !3
        {
            self.reservation = None;
        }
        let r = match width {
            1 => bus.write8(addr, v as u8),
            2 => bus.write16(addr, v as u16),
            _ => bus.write32(addr, v),
        };
        r.map_err(|_| Trap::new(cause::STORE_ACCESS, addr))
    }

    fn exec32<B: MemBus>(&mut self, bus: &mut B, i: u32) -> Result<Step, Trap> {
        let rd = ((i >> 7) & 31) as usize;
        let rs1 = ((i >> 15) & 31) as usize;
        let rs2 = ((i >> 20) & 31) as usize;
        let f3 = (i >> 12) & 7;
        let f7 = i >> 25;
        let a = self.x[rs1];
        let b = self.x[rs2];
        let imm_i = sext(i >> 20, 12);
        let pc = self.pc;
        let mut next = pc.wrapping_add(4);
        let illegal = Trap::new(cause::ILLEGAL, i);
        match i & 0x7f {
            0x37 => self.wr(rd, i & 0xffff_f000),
            0x17 => self.wr(rd, pc.wrapping_add(i & 0xffff_f000)),
            0x6f => {
                let imm = sext(
                    ((i >> 31) << 20)
                        | (((i >> 12) & 0xff) << 12)
                        | (((i >> 20) & 1) << 11)
                        | (((i >> 21) & 0x3ff) << 1),
                    21,
                );
                self.wr(rd, next);
                next = pc.wrapping_add(imm);
            }
            0x67 => {
                let t = a.wrapping_add(imm_i) & !1;
                self.wr(rd, next);
                next = t;
            }
            0x63 => {
                let imm = sext(
                    ((i >> 31) << 12) | (((i >> 7) & 1) << 11) | (((i >> 25) & 0x3f) << 5) | (((i >> 8) & 0xf) << 1),
                    13,
                );
                let take = match f3 {
                    0 => a == b,
                    1 => a != b,
                    4 => (a as i32) < (b as i32),
                    5 => (a as i32) >= (b as i32),
                    6 => a < b,
                    7 => a >= b,
                    _ => return Err(illegal),
                };
                if take {
                    next = pc.wrapping_add(imm);
                }
            }
            0x03 => {
                let addr = a.wrapping_add(imm_i);
                let v = match f3 {
                    0 => self.load(bus, addr, 1, true)?,
                    1 => self.load(bus, addr, 2, true)?,
                    2 => self.load(bus, addr, 4, false)?,
                    4 => self.load(bus, addr, 1, false)?,
                    5 => self.load(bus, addr, 2, false)?,
                    _ => return Err(illegal),
                };
                self.wr(rd, v);
            }
            0x23 => {
                let imm = sext(((i >> 25) << 5) | ((i >> 7) & 31), 12);
                let addr = a.wrapping_add(imm);
                match f3 {
                    0 => self.store(bus, addr, 1, b)?,
                    1 => self.store(bus, addr, 2, b)?,
                    2 => self.store(bus, addr, 4, b)?,
                    _ => return Err(illegal),
                }
            }
            0x13 => {
                let sh = (i >> 20) & 31;
                let v = match f3 {
                    0 => a.wrapping_add(imm_i),
                    2 => ((a as i32) < (imm_i as i32)) as u32,
                    3 => (a < imm_i) as u32,
                    4 => a ^ imm_i,
                    6 => a | imm_i,
                    7 => a & imm_i,
                    1 => a << sh,
                    5 => {
                        if f7 & 0x20 != 0 {
                            ((a as i32) >> sh) as u32
                        } else {
                            a >> sh
                        }
                    }
                    _ => unreachable!(),
                };
                self.wr(rd, v);
            }
            0x33 => {
                let v = if f7 == 1 {
                    match f3 {
                        0 => a.wrapping_mul(b),
                        1 => (((a as i32 as i64) * (b as i32 as i64)) >> 32) as u32,
                        2 => (((a as i32 as i64) * (b as u64 as i64)) >> 32) as u32,
                        3 => (((a as u64) * (b as u64)) >> 32) as u32,
                        4 => {
                            if b == 0 {
                                u32::MAX
                            } else if a == 0x8000_0000 && b == u32::MAX {
                                a
                            } else {
                                ((a as i32) / (b as i32)) as u32
                            }
                        }
                        5 => a.checked_div(b).unwrap_or(u32::MAX),
                        6 => {
                            if b == 0 {
                                a
                            } else if a == 0x8000_0000 && b == u32::MAX {
                                0
                            } else {
                                ((a as i32) % (b as i32)) as u32
                            }
                        }
                        7 => {
                            if b == 0 {
                                a
                            } else {
                                a % b
                            }
                        }
                        _ => unreachable!(),
                    }
                } else {
                    match (f3, f7) {
                        (0, 0) => a.wrapping_add(b),
                        (0, 0x20) => a.wrapping_sub(b),
                        (1, 0) => a << (b & 31),
                        (2, 0) => ((a as i32) < (b as i32)) as u32,
                        (3, 0) => (a < b) as u32,
                        (4, 0) => a ^ b,
                        (5, 0) => a >> (b & 31),
                        (5, 0x20) => ((a as i32) >> (b & 31)) as u32,
                        (6, 0) => a | b,
                        (7, 0) => a & b,
                        _ => return Err(illegal),
                    }
                };
                self.wr(rd, v);
            }
            0x0f => {} // fence / fence.i
            0x2f => {
                // A extension (the C3 lacks it, but it is cheap to support)
                if f3 != 2 {
                    return Err(illegal);
                }
                let op = i >> 27;
                match op {
                    0x02 => {
                        let v = self.load(bus, a, 4, false)?;
                        self.reservation = Some(a);
                        self.wr(rd, v);
                    }
                    0x03 => {
                        if self.reservation == Some(a) {
                            self.store(bus, a, 4, b)?;
                            self.wr(rd, 0);
                        } else {
                            self.wr(rd, 1);
                        }
                        self.reservation = None;
                    }
                    _ => {
                        let old = self.load(bus, a, 4, false)?;
                        let new = match op {
                            0x01 => b,
                            0x00 => old.wrapping_add(b),
                            0x04 => old ^ b,
                            0x0c => old & b,
                            0x08 => old | b,
                            0x10 => (old as i32).min(b as i32) as u32,
                            0x14 => (old as i32).max(b as i32) as u32,
                            0x18 => old.min(b),
                            0x1c => old.max(b),
                            _ => return Err(illegal),
                        };
                        self.store(bus, a, 4, new)?;
                        self.wr(rd, old);
                    }
                }
            }
            0x73 => {
                if f3 == 0 {
                    match i {
                        0x0000_0073 => return Err(Trap::new(cause::ECALL_M, 0)),
                        0x0010_0073 => return Err(Trap::new(cause::BREAKPOINT, pc)),
                        0x3020_0073 => {
                            // mret
                            let mpie = self.csr.mstatus & MSTATUS_MPIE != 0;
                            self.csr.mstatus &= !MSTATUS_MIE;
                            if mpie {
                                self.csr.mstatus |= MSTATUS_MIE;
                            }
                            self.csr.mstatus |= MSTATUS_MPIE;
                            self.pc = self.csr.mepc;
                            return Ok(Step::Ok);
                        }
                        0x1050_0073 => {
                            self.pc = next;
                            return Ok(Step::Wfi);
                        }
                        _ => {} // sfence etc.
                    }
                } else {
                    let n = (i >> 20) as u16;
                    let old = self.read_csr(n, bus.cycles()).ok_or(illegal)?;
                    let src = if f3 & 4 != 0 { rs1 as u32 } else { a };
                    let new = match f3 & 3 {
                        1 => Some(src),
                        2 => (rs1 != 0).then_some(old | src),
                        3 => (rs1 != 0).then_some(old & !src),
                        _ => return Err(illegal),
                    };
                    self.wr(rd, old);
                    if let Some(v) = new {
                        self.write_csr(n, v, bus.cycles());
                    }
                }
            }
            _ => return Err(illegal),
        }
        self.pc = next;
        Ok(Step::Ok)
    }

    fn exec_c<B: MemBus>(&mut self, bus: &mut B, i: u32) -> Result<Step, Trap> {
        let pc = self.pc;
        let mut next = pc.wrapping_add(2);
        let illegal = Trap::new(cause::ILLEGAL, i);
        let f3 = i >> 13;
        let rdp = (((i >> 2) & 7) + 8) as usize;
        let rs1p = (((i >> 7) & 7) + 8) as usize;
        let rd = ((i >> 7) & 31) as usize;
        let rs2 = ((i >> 2) & 31) as usize;
        match (i & 3, f3) {
            (0, 0) => {
                // c.addi4spn
                let imm = ((i >> 7) & 0x30) | ((i >> 1) & 0x3c0) | ((i >> 4) & 4) | ((i >> 2) & 8);
                if imm == 0 {
                    return Err(illegal);
                }
                self.wr(rdp, self.x[2].wrapping_add(imm));
            }
            (0, 2) => {
                let imm = ((i >> 7) & 0x38) | ((i >> 4) & 4) | ((i << 1) & 0x40);
                let v = self.load(bus, self.x[rs1p].wrapping_add(imm), 4, false)?;
                self.wr(rdp, v);
            }
            (0, 6) => {
                let imm = ((i >> 7) & 0x38) | ((i >> 4) & 4) | ((i << 1) & 0x40);
                self.store(bus, self.x[rs1p].wrapping_add(imm), 4, self.x[rdp])?;
            }
            (1, 0) => {
                let imm = sext(((i >> 7) & 0x20) | ((i >> 2) & 0x1f), 6);
                self.wr(rd, self.x[rd].wrapping_add(imm));
            }
            (1, 1) | (1, 5) => {
                // c.jal / c.j
                let imm = sext(
                    ((i >> 1) & 0x800)
                        | ((i << 2) & 0x400)
                        | ((i >> 1) & 0x300)
                        | ((i << 1) & 0x80)
                        | ((i >> 1) & 0x40)
                        | ((i << 3) & 0x20)
                        | ((i >> 7) & 0x10)
                        | ((i >> 2) & 0xe),
                    12,
                );
                if f3 == 1 {
                    self.x[1] = next;
                }
                next = pc.wrapping_add(imm);
            }
            (1, 2) => {
                let imm = sext(((i >> 7) & 0x20) | ((i >> 2) & 0x1f), 6);
                self.wr(rd, imm);
            }
            (1, 3) => {
                if rd == 2 {
                    let imm = sext(
                        ((i >> 3) & 0x200)
                            | ((i >> 2) & 0x10)
                            | ((i << 1) & 0x40)
                            | ((i << 4) & 0x180)
                            | ((i << 3) & 0x20),
                        10,
                    );
                    if imm == 0 {
                        return Err(illegal);
                    }
                    self.x[2] = self.x[2].wrapping_add(imm);
                } else {
                    let imm = sext(((i << 5) & 0x20000) | ((i << 10) & 0x1f000), 18);
                    self.wr(rd, imm);
                }
            }
            (1, 4) => {
                let sh = ((i >> 7) & 0x20) | ((i >> 2) & 0x1f);
                let a = self.x[rs1p];
                let v = match (i >> 10) & 3 {
                    0 => a >> sh,
                    1 => ((a as i32) >> sh) as u32,
                    2 => a & sext(sh, 6),
                    _ => {
                        let b = self.x[rdp];
                        match ((i >> 12) & 1, (i >> 5) & 3) {
                            (0, 0) => a.wrapping_sub(b),
                            (0, 1) => a ^ b,
                            (0, 2) => a | b,
                            (0, 3) => a & b,
                            _ => return Err(illegal),
                        }
                    }
                };
                self.x[rs1p] = v;
            }
            (1, 6) | (1, 7) => {
                let imm = sext(
                    ((i >> 4) & 0x100) | ((i << 1) & 0xc0) | ((i << 3) & 0x20) | ((i >> 7) & 0x18) | ((i >> 2) & 6),
                    9,
                );
                let z = self.x[rs1p] == 0;
                if z == (f3 == 6) {
                    next = pc.wrapping_add(imm);
                }
            }
            (2, 0) => {
                let sh = ((i >> 7) & 0x20) | ((i >> 2) & 0x1f);
                self.wr(rd, self.x[rd] << sh);
            }
            (2, 2) => {
                let imm = ((i >> 7) & 0x20) | ((i >> 2) & 0x1c) | ((i << 4) & 0xc0);
                let v = self.load(bus, self.x[2].wrapping_add(imm), 4, false)?;
                self.wr(rd, v);
            }
            (2, 4) => {
                let bit12 = (i >> 12) & 1;
                match (bit12, rd, rs2) {
                    (0, 0, _) => return Err(illegal),
                    (0, _, 0) => next = self.x[rd] & !1,   // c.jr
                    (0, _, _) => self.wr(rd, self.x[rs2]), // c.mv
                    (1, 0, 0) => return Err(Trap::new(cause::BREAKPOINT, pc)),
                    (1, _, 0) => {
                        // c.jalr
                        let t = self.x[rd] & !1;
                        self.x[1] = next;
                        next = t;
                    }
                    _ => self.wr(rd, self.x[rd].wrapping_add(self.x[rs2])), // c.add
                }
            }
            (2, 6) => {
                let imm = ((i >> 7) & 0x3c) | ((i >> 1) & 0xc0);
                self.store(bus, self.x[2].wrapping_add(imm), 4, self.x[rs2])?;
            }
            _ => return Err(illegal),
        }
        self.pc = next;
        Ok(Step::Ok)
    }
}

/// ABI registers.
const RA: usize = 1;
const SP: usize = 2;
const A0: usize = 10;

/// Whether `v` looks like a return address: in code (ROM, IRAM or flash) and right after
/// a `jal ra`, `jalr ra`, `c.jal` or `c.jalr`.
fn is_return_address(read: &dyn Fn(u32) -> Option<u32>, v: u32) -> bool {
    if v & 1 != 0 || !matches!(v >> 24, 0x40 | 0x42) {
        return false;
    }
    let jal = |w: u32| (w & 0xfff) == 0x0ef || (w & 0x7fff) == 0x00e7;
    let cjal = |h: u32| (h & 0xf07f) == 0x9002 && (h >> 7) & 0x1f != 0 || (h & 0xe003) == 0x2001;
    read(v.wrapping_sub(4)).is_some_and(jal) || read(v.wrapping_sub(2)).is_some_and(|w| cjal(w & 0xffff))
}

impl GuestCpu for Rv32 {
    fn pc(&self) -> u32 {
        self.pc
    }
    fn set_pc(&mut self, pc: u32) {
        self.pc = pc;
    }
    fn arg(&self, n: usize) -> u32 {
        assert!(n < 8, "stack-passed args not supported");
        self.x[A0 + n]
    }
    fn set_arg(&mut self, n: usize, v: u32) {
        self.x[A0 + n] = v;
    }
    fn ret_val(&self) -> u32 {
        self.x[A0]
    }
    fn sp(&self) -> u32 {
        self.x[SP]
    }
    fn set_sp(&mut self, v: u32) {
        self.x[SP] = v;
    }
    fn return_from_hook(&mut self, ret: Option<u32>) {
        if let Some(v) = ret {
            self.x[A0] = v;
        }
        self.pc = self.x[RA];
    }
    fn return_address(&self) -> u32 {
        self.x[RA]
    }
    fn begin_call(&mut self, func: u32, args: &[u32], return_to: u32) {
        for (i, a) in args.iter().enumerate() {
            self.set_arg(i, *a);
        }
        self.x[RA] = return_to;
        self.pc = func;
    }
    fn restore_after_call(&mut self, return_address: u32) {
        self.x[RA] = return_address;
    }
    fn redirect_return(&mut self, to: u32) -> ReturnPatch {
        let ra = self.x[RA];
        self.x[RA] = to;
        ReturnPatch { return_to: ra, ra_reg: RA as u8, ra, ret_reg: A0 as u8 }
    }
    fn finish_return(&mut self, p: &ReturnPatch) -> u32 {
        self.x[p.ra_reg as usize] = p.ra;
        self.pc = p.return_to;
        self.x[p.ret_reg as usize]
    }
    /// Without frame pointers this is a heuristic: `ra`, then words on the stack that
    /// are return addresses (the instruction before them is a `jal`/`jalr` to `ra`).
    fn backtrace(&self, read: &dyn Fn(u32) -> Option<u32>, max: usize) -> Vec<u32> {
        let mut out = vec![self.pc];
        let push = |v: u32, out: &mut Vec<u32>| {
            if out.len() < max && out.last() != Some(&v) && is_return_address(read, v) {
                out.push(v);
            }
        };
        push(self.x[RA], &mut out);
        let sp = self.x[SP];
        for i in 0..256 {
            if out.len() >= max {
                break;
            }
            match read(sp.wrapping_add(4 * i)) {
                Some(v) => push(v, &mut out),
                None => break,
            }
        }
        out
    }
    fn irq_enabled(&self) -> bool {
        self.csr.mstatus & MSTATUS_MIE != 0
    }
    fn enter_interrupt(&mut self, line: u32, _level: u32) {
        let pc = self.pc;
        self.take_trap(0x8000_0000 | line, 0, pc);
    }
    fn gpr_dump(&self) -> String {
        const N: [&str; 32] = [
            "zero", "ra", "sp", "gp", "tp", "t0", "t1", "t2", "s0", "s1", "a0", "a1", "a2", "a3", "a4", "a5", "a6",
            "a7", "s2", "s3", "s4", "s5", "s6", "s7", "s8", "s9", "s10", "s11", "t3", "t4", "t5", "t6",
        ];
        let mut s = format!(
            "pc={:08x} mstatus={:08x} mcause={:08x} mepc={:08x} mtval={:08x}\n",
            self.pc, self.csr.mstatus, self.csr.mcause, self.csr.mepc, self.csr.mtval
        );
        for (i, n) in N.iter().enumerate() {
            s += &format!("{:>4}={:08x}{}", n, self.x[i], if i % 4 == 3 { "\n" } else { " " });
        }
        s
    }
}
