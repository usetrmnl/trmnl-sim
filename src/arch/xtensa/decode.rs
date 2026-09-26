//! Xtensa instruction decoder for the ESP32-S3 configuration (LX7 core ISA plus the
//! options the S3 has: density, windowed registers, loops, MUL16/32, DIV32, MAC16,
//! booleans, single-precision FPU with the DFPU division/sqrt helpers, and the few
//! "cop_ai" (PIE/SIMD) instructions that ESP-IDF itself uses).
//!
//! Decoding is pure: [`decode`] turns up to four instruction bytes into an [`Insn`]
//! with all fields extracted, and [`Insn::mnemonic`] / [`Insn::disasm`] render it the
//! way GNU objdump does (the coverage test compares against objdump).

/// Operation. Operands live in [`Insn`] fields; the comment says which.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Op {
    /// Not decodable (an unimplemented TIE/PIE instruction or a reserved encoding).
    Unknown,

    // ---- RRR arithmetic: ar = f(as, at) ----
    Add,
    Addx2,
    Addx4,
    Addx8,
    Sub,
    Subx2,
    Subx4,
    Subx8,
    And,
    Or,
    Xor,
    Min,
    Max,
    Minu,
    Maxu,
    Salt,
    Saltu,
    Mull,
    Muluh,
    Mulsh,
    Mul16u,
    Mul16s,
    Quou,
    Quos,
    Remu,
    Rems,
    Moveqz,
    Movnez,
    Movltz,
    Movgez,
    /// ar = as if !b[t]
    Movf,
    Movt,
    /// ar = -at
    Neg,
    Abs,
    /// ar = sext(as, imm) (imm = t + 7)
    Sext,
    Clamps,
    /// at = nsa(as)
    Nsa,
    Nsau,

    // ---- shifts ----
    Sll,
    Srl,
    Sra,
    Src,
    /// ar = as << imm
    Slli,
    /// ar = at >> imm
    Srli,
    Srai,
    Ssr,
    Ssl,
    Ssa8l,
    Ssa8b,
    Ssai,
    /// ar = (at >> s) & imm
    Extui,

    // ---- loads / stores / immediates ----
    L8ui,
    L16ui,
    L16si,
    L32i,
    S8i,
    S16i,
    S32i,
    L32ai,
    S32ri,
    S32c1i,
    S32nb,
    L32e,
    S32e,
    L32r,
    Addi,
    Addmi,
    Movi,

    // ---- density ----
    AddN,
    AddiN,
    MovN,
    MoviN,
    L32iN,
    S32iN,
    RetN,
    RetwN,
    BreakN,
    NopN,
    IllN,
    BeqzN,
    BnezN,

    // ---- branches (target = pc + imm) ----
    Beqz,
    Bnez,
    Bltz,
    Bgez,
    /// as == imm2
    Beqi,
    Bnei,
    Blti,
    Bgei,
    Bltui,
    Bgeui,
    Beq,
    Bne,
    Blt,
    Bge,
    Bltu,
    Bgeu,
    Bany,
    Bnone,
    Ball,
    Bnall,
    Bbc,
    Bbs,
    /// bit index in imm2
    Bbci,
    Bbsi,
    /// b[s]
    Bf,
    Bt,

    // ---- jumps / calls / windows / loops ----
    J,
    Jx,
    Call0,
    /// window increment n in imm2
    Calln,
    Callx0,
    Callxn,
    Ret,
    Retw,
    /// as = s, frame bytes = imm
    Entry,
    /// at = as
    Movsp,
    Rotw,
    Loop,
    Loopnez,
    Loopgtz,

    // ---- system ----
    /// sr number in imm, register in t
    Rsr,
    Wsr,
    Xsr,
    /// ur number in imm; Rur -> ar, Wur <- at
    Rur,
    Wur,
    Rsil,
    Waiti,
    Rfe,
    Rfue,
    Rfde,
    Rfwo,
    Rfwu,
    Rfi,
    Rfdo,
    Rfdd,
    Syscall,
    Simcall,
    Break,
    Ill,
    /// ISYNC/RSYNC/ESYNC/DSYNC/EXCW/MEMW/EXTW/NOP (kind in t)
    Sync,
    Rer,
    Wer,
    /// TLB management: t = result reg for the read/probe forms, s = address reg (kind in r)
    Tlb,
    /// Cache operations (no caches in the S3 core; they are no-ops). Kind in imm2.
    Cache,

    // ---- booleans ----
    Andb,
    Andbc,
    Orb,
    Orbc,
    Xorb,
    Any4,
    All4,
    Any8,
    All8,

    // ---- MAC16 (kind = op1 in imm, op2 in imm2) ----
    Mac16,
    Ldinc,
    Lddec,

    // ---- FPU ----
    AddS,
    SubS,
    MulS,
    MaddS,
    MsubS,
    MaddnS,
    DivnS,
    RoundS,
    TruncS,
    FloorS,
    CeilS,
    FloatS,
    UfloatS,
    UtruncS,
    MovS,
    AbsS,
    ConstS,
    Rfr,
    Wfr,
    NegS,
    Div0S,
    Recip0S,
    Sqrt0S,
    Rsqrt0S,
    Nexp01S,
    MksadjS,
    MkdadjS,
    AddexpS,
    AddexpmS,
    UnS,
    OeqS,
    UeqS,
    OltS,
    UltS,
    OleS,
    UleS,
    MoveqzS,
    MovnezS,
    MovltzS,
    MovgezS,
    MovfS,
    MovtS,
    Lsi,
    Ssi,
    Lsiu,
    Ssiu,
    Lsx,
    Ssx,
    Lsxu,
    Ssxu,

    // ---- ESP32-S3 cop_ai (PIE) subset ----
    /// q register in imm2, base in t, offset in imm
    LdQr,
    StQr,
}

/// A decoded instruction.
#[derive(Clone, Copy, Debug)]
pub struct Insn {
    pub op: Op,
    /// Length in bytes (2, 3 or 4).
    pub len: u8,
    pub r: u8,
    pub s: u8,
    pub t: u8,
    /// Highest address-register quad (a0-3 = 0 .. a12-15 = 3) the instruction names:
    /// used for the window-overflow check.
    pub wq: u8,
    pub imm: i32,
    pub imm2: i32,
    /// The raw instruction bits (little-endian, `len` bytes significant).
    pub raw: u32,
}

const B4CONST: [i32; 16] = [-1, 1, 2, 3, 4, 5, 6, 7, 8, 10, 12, 16, 32, 64, 128, 256];
const B4CONSTU: [i32; 16] = [32768, 65536, 2, 3, 4, 5, 6, 7, 8, 10, 12, 16, 32, 64, 128, 256];

#[inline(always)]
fn sext(v: u32, bits: u32) -> i32 {
    let s = 32 - bits;
    ((v << s) as i32) >> s
}

/// Instruction length from the first byte (op0).
#[inline(always)]
pub fn insn_len(b0: u8) -> u8 {
    match b0 & 15 {
        0x8..=0xd => 2,
        0xe | 0xf => 4,
        _ => 3,
    }
}

// Field-usage flags for the window check.
const R: u8 = 1;
const S: u8 = 2;
const T: u8 = 4;

impl Insn {
    #[inline(always)]
    fn new(op: Op, w: u32, len: u8, regs: u8) -> Insn {
        let t = ((w >> 4) & 15) as u8;
        let s = ((w >> 8) & 15) as u8;
        let r = ((w >> 12) & 15) as u8;
        let mut wq = 0;
        if regs & R != 0 {
            wq = wq.max(r >> 2);
        }
        if regs & S != 0 {
            wq = wq.max(s >> 2);
        }
        if regs & T != 0 {
            wq = wq.max(t >> 2);
        }
        Insn { op, len, r, s, t, wq, imm: 0, imm2: 0, raw: w }
    }
    #[inline(always)]
    fn imm(mut self, v: i32) -> Insn {
        self.imm = v;
        self
    }
    #[inline(always)]
    fn imm2(mut self, v: i32) -> Insn {
        self.imm2 = v;
        self
    }
    #[inline(always)]
    fn wq(mut self, q: u8) -> Insn {
        self.wq = self.wq.max(q);
        self
    }
}

/// Decode the instruction whose first bytes are `w` (little-endian; bytes beyond the
/// instruction's length are ignored).
#[inline(always)]
pub fn decode(w: u32) -> Insn {
    let op0 = w & 15;
    match op0 {
        0x8..=0xd => decode_narrow(w & 0xffff),
        0xe | 0xf => Insn::new(Op::Unknown, w, 4, 0),
        _ => decode24(w & 0xff_ffff),
    }
}

fn decode_narrow(w: u32) -> Insn {
    let t = (w >> 4) & 15;
    let r = (w >> 12) & 15;
    let n = |op, regs| Insn::new(op, w, 2, regs);
    match w & 15 {
        0x8 => n(Op::L32iN, S | T).imm((r << 2) as i32),
        0x9 => n(Op::S32iN, S | T).imm((r << 2) as i32),
        0xa => n(Op::AddN, R | S | T),
        0xb => n(Op::AddiN, R | S).imm(if t == 0 { -1 } else { t as i32 }),
        0xc => {
            if t & 8 == 0 {
                let v = ((t & 7) << 4) | r;
                n(Op::MoviN, S).imm(if v >= 96 { v as i32 - 128 } else { v as i32 })
            } else {
                let off = (((t & 3) << 4) | r) as i32 + 4;
                n(if t & 4 == 0 { Op::BeqzN } else { Op::BnezN }, S).imm(off)
            }
        }
        _ => match r {
            0 => n(Op::MovN, S | T),
            15 => match t {
                0 => n(Op::RetN, 0),
                1 => n(Op::RetwN, 0),
                2 => n(Op::BreakN, 0),
                3 => n(Op::NopN, 0),
                6 => n(Op::IllN, 0),
                _ => n(Op::Unknown, 0),
            },
            _ => n(Op::Unknown, 0),
        },
    }
}

fn decode24(w: u32) -> Insn {
    let t = (w >> 4) & 15;
    let s = (w >> 8) & 15;
    let r = (w >> 12) & 15;
    let op1 = (w >> 16) & 15;
    let op2 = (w >> 20) & 15;
    let n = |op, regs| Insn::new(op, w, 3, regs);
    let unk = Insn::new(Op::Unknown, w, 3, 0);
    let imm8 = (w >> 16) & 0xff;
    match w & 15 {
        0 => match op1 {
            0 => match op2 {
                0 => match r {
                    0 => {
                        let m = t >> 2;
                        let nn = t & 3;
                        match (m, nn) {
                            (0, 0) => n(Op::Ill, 0),
                            (2, 0) => n(Op::Ret, 0),
                            (2, 1) => n(Op::Retw, 0),
                            (2, 2) => n(Op::Jx, S),
                            (3, 0) => n(Op::Callx0, S),
                            (3, _) => n(Op::Callxn, S).imm2(nn as i32).wq(nn as u8),
                            _ => unk,
                        }
                    }
                    1 => n(Op::Movsp, S | T),
                    2 => match t {
                        0 | 1 | 2 | 3 | 8 | 12 | 13 | 15 => n(Op::Sync, 0),
                        _ => unk,
                    },
                    3 => match t {
                        0 => match s {
                            0 => n(Op::Rfe, 0),
                            1 => n(Op::Rfue, 0),
                            2 => n(Op::Rfde, 0),
                            4 => n(Op::Rfwo, 0),
                            5 => n(Op::Rfwu, 0),
                            _ => unk,
                        },
                        1 => n(Op::Rfi, 0),
                        _ => unk,
                    },
                    4 => n(Op::Break, 0),
                    5 => match s {
                        0 if t == 0 => n(Op::Syscall, 0),
                        1 if t == 0 => n(Op::Simcall, 0),
                        _ => unk,
                    },
                    6 => n(Op::Rsil, T),
                    7 => n(Op::Waiti, 0),
                    8 => n(Op::Any4, 0),
                    9 => n(Op::All4, 0),
                    10 => n(Op::Any8, 0),
                    11 => n(Op::All8, 0),
                    _ => unk,
                },
                1 => n(Op::And, R | S | T),
                2 => n(Op::Or, R | S | T),
                3 => n(Op::Xor, R | S | T),
                4 => match r {
                    0 => n(Op::Ssr, S),
                    1 => n(Op::Ssl, S),
                    2 => n(Op::Ssa8l, S),
                    3 => n(Op::Ssa8b, S),
                    4 if t & 14 == 0 => n(Op::Ssai, 0).imm((s | ((t & 1) << 4)) as i32),
                    6 => n(Op::Rer, S | T),
                    7 => n(Op::Wer, S | T),
                    8 if s == 0 => n(Op::Rotw, 0).imm(sext(t, 4)),
                    14 => n(Op::Nsa, S | T),
                    15 => n(Op::Nsau, S | T),
                    _ => unk,
                },
                5 => match r {
                    3 | 7 | 11 | 15 | 5 | 13 => n(Op::Tlb, S | T),
                    4 | 6 | 12 | 14 => n(Op::Tlb, S | if r & 2 != 0 { T } else { 0 }),
                    _ => unk,
                },
                6 => match s {
                    0 => n(Op::Neg, R | T),
                    1 => n(Op::Abs, R | T),
                    _ => unk,
                },
                8 => n(Op::Add, R | S | T),
                9 => n(Op::Addx2, R | S | T),
                10 => n(Op::Addx4, R | S | T),
                11 => n(Op::Addx8, R | S | T),
                12 => n(Op::Sub, R | S | T),
                13 => n(Op::Subx2, R | S | T),
                14 => n(Op::Subx4, R | S | T),
                15 => n(Op::Subx8, R | S | T),
                _ => unk,
            },
            1 => match op2 {
                0 | 1 => n(Op::Slli, R | S).imm(32 - (((op2 & 1) << 4) | t) as i32),
                2 | 3 => n(Op::Srai, R | T).imm((((op2 & 1) << 4) | s) as i32),
                4 => n(Op::Srli, R | T).imm(s as i32),
                6 => n(Op::Xsr, T).imm(((r << 4) | s) as i32),
                8 => n(Op::Src, R | S | T),
                9 if s == 0 => n(Op::Srl, R | T),
                10 if t == 0 => n(Op::Sll, R | S),
                11 if s == 0 => n(Op::Sra, R | T),
                12 => n(Op::Mul16u, R | S | T),
                13 => n(Op::Mul16s, R | S | T),
                15 => match r {
                    14 => match t {
                        0 => n(Op::Rfdo, 0),
                        1 => n(Op::Rfdd, 0),
                        _ => unk,
                    },
                    _ => unk,
                },
                _ => unk,
            },
            2 => match op2 {
                0 => n(Op::Andb, 0),
                1 => n(Op::Andbc, 0),
                2 => n(Op::Orb, 0),
                3 => n(Op::Orbc, 0),
                4 => n(Op::Xorb, 0),
                6 => n(Op::Saltu, R | S | T),
                7 => n(Op::Salt, R | S | T),
                8 => n(Op::Mull, R | S | T),
                10 => n(Op::Muluh, R | S | T),
                11 => n(Op::Mulsh, R | S | T),
                12 => n(Op::Quou, R | S | T),
                13 => n(Op::Quos, R | S | T),
                14 => n(Op::Remu, R | S | T),
                15 => n(Op::Rems, R | S | T),
                _ => unk,
            },
            3 => match op2 {
                0 => n(Op::Rsr, T).imm(((r << 4) | s) as i32),
                1 => n(Op::Wsr, T).imm(((r << 4) | s) as i32),
                2 => n(Op::Sext, R | S).imm(t as i32 + 7),
                3 => n(Op::Clamps, R | S).imm(t as i32 + 7),
                4 => n(Op::Min, R | S | T),
                5 => n(Op::Max, R | S | T),
                6 => n(Op::Minu, R | S | T),
                7 => n(Op::Maxu, R | S | T),
                8 => n(Op::Moveqz, R | S | T),
                9 => n(Op::Movnez, R | S | T),
                10 => n(Op::Movltz, R | S | T),
                11 => n(Op::Movgez, R | S | T),
                12 => n(Op::Movf, R | S),
                13 => n(Op::Movt, R | S),
                14 => n(Op::Rur, R).imm(((s << 4) | t) as i32),
                15 => n(Op::Wur, T).imm(((r << 4) | s) as i32),
                _ => unk,
            },
            4 | 5 => {
                n(Op::Extui, R | T).imm(((1u64 << (op2 + 1)) - 1) as u32 as i32).imm2((s | ((op1 & 1) << 4)) as i32)
            }
            8 => match op2 {
                0 => n(Op::Lsx, S | T),
                1 => n(Op::Lsxu, S | T),
                4 => n(Op::Ssx, S | T),
                5 => n(Op::Ssxu, S | T),
                _ => unk,
            },
            9 => match op2 {
                0 => n(Op::L32e, S | T).imm(((r << 2) as i32) - 64),
                4 => n(Op::S32e, S | T).imm(((r << 2) as i32) - 64),
                5 => n(Op::S32nb, S | T).imm((r << 2) as i32),
                _ => unk,
            },
            10 => match op2 {
                0 => n(Op::AddS, 0),
                1 => n(Op::SubS, 0),
                2 => n(Op::MulS, 0),
                4 => n(Op::MaddS, 0),
                5 => n(Op::MsubS, 0),
                6 => n(Op::MaddnS, 0),
                7 => n(Op::DivnS, 0),
                8 => n(Op::RoundS, R).imm(t as i32),
                9 => n(Op::TruncS, R).imm(t as i32),
                10 => n(Op::FloorS, R).imm(t as i32),
                11 => n(Op::CeilS, R).imm(t as i32),
                12 => n(Op::FloatS, S).imm(t as i32),
                13 => n(Op::UfloatS, S).imm(t as i32),
                14 => n(Op::UtruncS, R).imm(t as i32),
                15 => match t {
                    0 => n(Op::MovS, 0),
                    1 => n(Op::AbsS, 0),
                    3 => n(Op::ConstS, 0).imm(s as i32),
                    4 => n(Op::Rfr, R),
                    5 => n(Op::Wfr, S),
                    6 => n(Op::NegS, 0),
                    7 => n(Op::Div0S, 0),
                    8 => n(Op::Recip0S, 0),
                    9 => n(Op::Sqrt0S, 0),
                    10 => n(Op::Rsqrt0S, 0),
                    11 => n(Op::Nexp01S, 0),
                    12 => n(Op::MksadjS, 0),
                    13 => n(Op::MkdadjS, 0),
                    14 => n(Op::AddexpS, 0),
                    15 => n(Op::AddexpmS, 0),
                    _ => unk,
                },
                _ => unk,
            },
            11 => match op2 {
                1 => n(Op::UnS, 0),
                2 => n(Op::OeqS, 0),
                3 => n(Op::UeqS, 0),
                4 => n(Op::OltS, 0),
                5 => n(Op::UltS, 0),
                6 => n(Op::OleS, 0),
                7 => n(Op::UleS, 0),
                8 => n(Op::MoveqzS, T),
                9 => n(Op::MovnezS, T),
                10 => n(Op::MovltzS, T),
                11 => n(Op::MovgezS, T),
                12 => n(Op::MovfS, 0),
                13 => n(Op::MovtS, 0),
                _ => unk,
            },
            _ => unk,
        },
        1 => n(Op::L32r, T).imm(((0xffff_0000 | (w >> 8)) << 2) as i32),
        2 => match r {
            0 => n(Op::L8ui, S | T).imm(imm8 as i32),
            1 => n(Op::L16ui, S | T).imm((imm8 << 1) as i32),
            2 => n(Op::L32i, S | T).imm((imm8 << 2) as i32),
            4 => n(Op::S8i, S | T).imm(imm8 as i32),
            5 => n(Op::S16i, S | T).imm((imm8 << 1) as i32),
            6 => n(Op::S32i, S | T).imm((imm8 << 2) as i32),
            7 => {
                // CACHE group: DPFR/DPFW/DHWB/DHI/.../IHI/III, DPFL/DHU/... (op1 in t=8/12)
                n(Op::Cache, S).imm(imm8 as i32).imm2(t as i32)
            }
            9 => n(Op::L16si, S | T).imm((imm8 << 1) as i32),
            10 => n(Op::Movi, T).imm(sext((s << 8) | imm8, 12)),
            11 => n(Op::L32ai, S | T).imm((imm8 << 2) as i32),
            12 => n(Op::Addi, S | T).imm(sext(imm8, 8)),
            13 => n(Op::Addmi, S | T).imm(sext(imm8, 8) << 8),
            14 => n(Op::S32c1i, S | T).imm((imm8 << 2) as i32),
            15 => n(Op::S32ri, S | T).imm((imm8 << 2) as i32),
            _ => unk,
        },
        3 => match r {
            0 => n(Op::Lsi, S).imm((imm8 << 2) as i32),
            4 => n(Op::Ssi, S).imm((imm8 << 2) as i32),
            8 => n(Op::Lsiu, S).imm((imm8 << 2) as i32),
            12 => n(Op::Ssiu, S).imm((imm8 << 2) as i32),
            _ => unk,
        },
        4 => decode_mac16(w),
        5 => {
            let nn = (w >> 4) & 3;
            let off = (sext(w >> 6, 18) << 2) + 4;
            if nn == 0 { n(Op::Call0, 0).imm(off) } else { n(Op::Calln, 0).imm(off).imm2(nn as i32).wq(nn as u8) }
        }
        6 => {
            let nn = (w >> 4) & 3;
            let m = (w >> 6) & 3;
            let off8 = sext(imm8, 8) + 4;
            let off12 = sext(w >> 12, 12) + 4;
            match nn {
                0 => n(Op::J, 0).imm(sext(w >> 6, 18) + 4),
                1 => n([Op::Beqz, Op::Bnez, Op::Bltz, Op::Bgez][m as usize], S).imm(off12),
                2 => n([Op::Beqi, Op::Bnei, Op::Blti, Op::Bgei][m as usize], S).imm(off8).imm2(B4CONST[r as usize]),
                _ => match m {
                    0 => n(Op::Entry, S).imm(((w >> 12) << 3) as i32),
                    1 => match r {
                        0 => n(Op::Bf, 0).imm(off8),
                        1 => n(Op::Bt, 0).imm(off8),
                        8 => n(Op::Loop, S).imm(imm8 as i32 + 4),
                        9 => n(Op::Loopnez, S).imm(imm8 as i32 + 4),
                        10 => n(Op::Loopgtz, S).imm(imm8 as i32 + 4),
                        _ => unk,
                    },
                    2 => n(Op::Bltui, S).imm(off8).imm2(B4CONSTU[r as usize]),
                    _ => n(Op::Bgeui, S).imm(off8).imm2(B4CONSTU[r as usize]),
                },
            }
        }
        7 => {
            let off8 = sext(imm8, 8) + 4;
            let op = match r {
                0 => Op::Bnone,
                1 => Op::Beq,
                2 => Op::Blt,
                3 => Op::Bltu,
                4 => Op::Ball,
                5 => Op::Bbc,
                6 | 7 => Op::Bbci,
                8 => Op::Bany,
                9 => Op::Bne,
                10 => Op::Bge,
                11 => Op::Bgeu,
                12 => Op::Bnall,
                13 => Op::Bbs,
                _ => Op::Bbsi,
            };
            if matches!(op, Op::Bbci | Op::Bbsi) {
                n(op, S).imm(off8).imm2((((r & 1) << 4) | t) as i32)
            } else {
                n(op, S | T).imm(off8)
            }
        }
        _ => unk,
    }
}

fn decode_mac16(w: u32) -> Insn {
    let t = (w >> 4) & 15;
    let s = (w >> 8) & 15;
    let r = (w >> 12) & 15;
    let op1 = (w >> 16) & 15;
    let op2 = (w >> 20) & 15;
    let n = |op, regs| Insn::new(op, w, 3, regs);
    let unk = Insn::new(Op::Unknown, w, 3, 0);
    let kind = op1 >> 2;
    match op2 {
        // MULA.DD.*.LDINC/LDDEC, MULA.DA.*.LDINC/LDDEC
        0 | 1 | 4 | 5 if kind == 2 && r & 8 == 0 && (op2 >= 4 || t & 11 == 0) => {
            let regs = if op2 >= 4 { S | T } else { S };
            n(Op::Mac16, regs).imm(op1 as i32).imm2(op2 as i32)
        }
        // MUL*.DD: r = mx (bit 2), t = my (bit 2)
        2 if kind != 0 && s == 0 && r & 11 == 0 && t & 11 == 0 => n(Op::Mac16, 0).imm(op1 as i32).imm2(2),
        // MUL*.AD: as, my
        3 if kind != 0 && r == 0 && t & 11 == 0 => n(Op::Mac16, S).imm(op1 as i32).imm2(3),
        // MUL*.DA: mx, at
        6 if kind != 0 && s == 0 && r & 11 == 0 => n(Op::Mac16, T).imm(op1 as i32).imm2(6),
        // MUL*.AA: as, at (UMUL only exists for AA)
        7 if r == 0 => n(Op::Mac16, S | T).imm(op1 as i32).imm2(7),
        8 if op1 == 0 && r & 12 == 0 && t == 0 => n(Op::Ldinc, S),
        9 if op1 == 0 && r & 12 == 0 && t == 0 => n(Op::Lddec, S),
        // cop_ai LD.QR / ST.QR: op1 = 0xD, r[2:0] = 2 (ld) / 6 (st), q = {op2[1:0], r[3]}
        12..=15 if op1 == 13 && (r & 7 == 2 || r & 7 == 6) => {
            let q = ((op2 & 3) << 1) | (r >> 3);
            n(if r & 7 == 2 { Op::LdQr } else { Op::StQr }, T).imm(sext(s, 4) << 4).imm2(q as i32)
        }
        _ => unk,
    }
}

// ------------------------------------------------------------------------------------------------
// Disassembly (objdump-compatible spelling)
// ------------------------------------------------------------------------------------------------

/// Name of a special register, as objdump spells it in `rsr.<name>`.
pub fn sr_name(n: u32) -> Option<&'static str> {
    Some(match n {
        0 => "lbeg",
        1 => "lend",
        2 => "lcount",
        3 => "sar",
        4 => "br",
        5 => "litbase",
        12 => "scompare1",
        16 => "acclo",
        17 => "acchi",
        32 => "m0",
        33 => "m1",
        34 => "m2",
        35 => "m3",
        72 => "windowbase",
        73 => "windowstart",
        83 => "ptevaddr",
        89 => "mmid",
        90 => "rasid",
        91 => "itlbcfg",
        92 => "dtlbcfg",
        96 => "ibreakenable",
        97 => "memctl",
        98 => "cacheadrdis",
        99 => "atomctl",
        104 => "ddr",
        106 => "mepc",
        107 => "meps",
        108 => "mesave",
        109 => "mesr",
        110 => "mecr",
        111 => "mevaddr",
        128 => "ibreaka0",
        129 => "ibreaka1",
        144 => "dbreaka0",
        145 => "dbreaka1",
        160 => "dbreakc0",
        161 => "dbreakc1",
        176 => "configid0",
        177 => "epc1",
        178 => "epc2",
        179 => "epc3",
        180 => "epc4",
        181 => "epc5",
        182 => "epc6",
        183 => "epc7",
        192 => "depc",
        194 => "eps2",
        195 => "eps3",
        196 => "eps4",
        197 => "eps5",
        198 => "eps6",
        199 => "eps7",
        208 => "configid1",
        209 => "excsave1",
        210 => "excsave2",
        211 => "excsave3",
        212 => "excsave4",
        213 => "excsave5",
        214 => "excsave6",
        215 => "excsave7",
        224 => "cpenable",
        226 => "interrupt",
        227 => "intclear",
        228 => "intenable",
        230 => "ps",
        231 => "vecbase",
        232 => "exccause",
        233 => "debugcause",
        234 => "ccount",
        235 => "prid",
        236 => "icount",
        237 => "icountlevel",
        238 => "excvaddr",
        240 => "ccompare0",
        241 => "ccompare1",
        242 => "ccompare2",
        244 => "misc0",
        245 => "misc1",
        246 => "misc2",
        247 => "misc3",
        _ => return None,
    })
}

/// Name of a user register (RUR/WUR), as objdump spells it.
pub fn ur_name(n: u32) -> Option<&'static str> {
    Some(match n {
        0 => "accx_0",
        1 => "accx_1",
        2 => "qacc_h_0",
        3 => "qacc_h_1",
        4 => "qacc_h_2",
        5 => "qacc_h_3",
        6 => "qacc_h_4",
        7 => "qacc_l_0",
        8 => "qacc_l_1",
        9 => "qacc_l_2",
        10 => "qacc_l_3",
        11 => "qacc_l_4",
        13 => "sar_byte",
        14 => "fft_bit_width",
        15 => "ua_state_0",
        16 => "ua_state_1",
        17 => "ua_state_2",
        18 => "ua_state_3",
        231 => "threadptr",
        232 => "fcr",
        233 => "fsr",
        _ => return None,
    })
}

impl Insn {
    /// The objdump mnemonic.
    pub fn mnemonic(&self) -> String {
        use Op::*;
        let s = match self.op {
            Unknown => return format!(".unknown({:#x})", self.raw),
            Rsr | Wsr | Xsr => {
                let p = match self.op {
                    Rsr => "rsr",
                    Wsr => "wsr",
                    _ => "xsr",
                };
                return match sr_name(self.imm as u32) {
                    Some(n) => format!("{p}.{n}"),
                    None => p.to_string(),
                };
            }
            Rur | Wur => {
                let p = if self.op == Rur { "rur" } else { "wur" };
                return match ur_name(self.imm as u32) {
                    Some(n) => format!("{p}.{n}"),
                    None => p.to_string(),
                };
            }
            Sync => match self.t {
                0 => "isync",
                1 => "rsync",
                2 => "esync",
                3 => "dsync",
                8 => "excw",
                12 => "memw",
                13 => "extw",
                _ => "nop",
            },
            Tlb => match self.r {
                3 => "ritlb0",
                4 => "iitlb",
                5 => "pitlb",
                6 => "witlb",
                7 => "ritlb1",
                11 => "rdtlb0",
                12 => "idtlb",
                13 => "pdtlb",
                14 => "wdtlb",
                _ => "rdtlb1",
            },
            Cache => match self.imm2 {
                0 => "dpfr",
                1 => "dpfw",
                2 => "dpfro",
                3 => "dpfwo",
                4 => "dhwb",
                5 => "dhwbi",
                6 => "dhi",
                7 => "dii",
                8 => "dcache",
                12 => "ipf",
                13 => "icache",
                14 => "ihi",
                15 => "iii",
                _ => "cache",
            },
            Mac16 => return self.mac16_name(),
            Calln => {
                return format!("call{}", 4 * self.imm2);
            }
            Callxn => {
                return format!("callx{}", 4 * self.imm2);
            }
            Add => "add",
            Addx2 => "addx2",
            Addx4 => "addx4",
            Addx8 => "addx8",
            Sub => "sub",
            Subx2 => "subx2",
            Subx4 => "subx4",
            Subx8 => "subx8",
            And => "and",
            Or => "or",
            Xor => "xor",
            Min => "min",
            Max => "max",
            Minu => "minu",
            Maxu => "maxu",
            Salt => "salt",
            Saltu => "saltu",
            Mull => "mull",
            Muluh => "muluh",
            Mulsh => "mulsh",
            Mul16u => "mul16u",
            Mul16s => "mul16s",
            Quou => "quou",
            Quos => "quos",
            Remu => "remu",
            Rems => "rems",
            Moveqz => "moveqz",
            Movnez => "movnez",
            Movltz => "movltz",
            Movgez => "movgez",
            Movf => "movf",
            Movt => "movt",
            Neg => "neg",
            Abs => "abs",
            Sext => "sext",
            Clamps => "clamps",
            Nsa => "nsa",
            Nsau => "nsau",
            Sll => "sll",
            Srl => "srl",
            Sra => "sra",
            Src => "src",
            Slli => "slli",
            Srli => "srli",
            Srai => "srai",
            Ssr => "ssr",
            Ssl => "ssl",
            Ssa8l => "ssa8l",
            Ssa8b => "ssa8b",
            Ssai => "ssai",
            Extui => "extui",
            L8ui => "l8ui",
            L16ui => "l16ui",
            L16si => "l16si",
            L32i => "l32i",
            S8i => "s8i",
            S16i => "s16i",
            S32i => "s32i",
            L32ai => "l32ai",
            S32ri => "s32ri",
            S32c1i => "s32c1i",
            S32nb => "s32nb",
            L32e => "l32e",
            S32e => "s32e",
            L32r => "l32r",
            Addi => "addi",
            Addmi => "addmi",
            Movi => "movi",
            AddN => "add.n",
            AddiN => "addi.n",
            MovN => "mov.n",
            MoviN => "movi.n",
            L32iN => "l32i.n",
            S32iN => "s32i.n",
            RetN => "ret.n",
            RetwN => "retw.n",
            BreakN => "break.n",
            NopN => "nop.n",
            IllN => "ill.n",
            BeqzN => "beqz.n",
            BnezN => "bnez.n",
            Beqz => "beqz",
            Bnez => "bnez",
            Bltz => "bltz",
            Bgez => "bgez",
            Beqi => "beqi",
            Bnei => "bnei",
            Blti => "blti",
            Bgei => "bgei",
            Bltui => "bltui",
            Bgeui => "bgeui",
            Beq => "beq",
            Bne => "bne",
            Blt => "blt",
            Bge => "bge",
            Bltu => "bltu",
            Bgeu => "bgeu",
            Bany => "bany",
            Bnone => "bnone",
            Ball => "ball",
            Bnall => "bnall",
            Bbc => "bbc",
            Bbs => "bbs",
            Bbci => "bbci",
            Bbsi => "bbsi",
            Bf => "bf",
            Bt => "bt",
            J => "j",
            Jx => "jx",
            Call0 => "call0",
            Callx0 => "callx0",
            Ret => "ret",
            Retw => "retw",
            Entry => "entry",
            Movsp => "movsp",
            Rotw => "rotw",
            Loop => "loop",
            Loopnez => "loopnez",
            Loopgtz => "loopgtz",
            Rsil => "rsil",
            Waiti => "waiti",
            Rfe => "rfe",
            Rfue => "rfue",
            Rfde => "rfde",
            Rfwo => "rfwo",
            Rfwu => "rfwu",
            Rfi => "rfi",
            Rfdo => "rfdo",
            Rfdd => "rfdd",
            Syscall => "syscall",
            Simcall => "simcall",
            Break => "break",
            Ill => "ill",
            Rer => "rer",
            Wer => "wer",
            Andb => "andb",
            Andbc => "andbc",
            Orb => "orb",
            Orbc => "orbc",
            Xorb => "xorb",
            Any4 => "any4",
            All4 => "all4",
            Any8 => "any8",
            All8 => "all8",
            Ldinc => "ldinc",
            Lddec => "lddec",
            AddS => "add.s",
            SubS => "sub.s",
            MulS => "mul.s",
            MaddS => "madd.s",
            MsubS => "msub.s",
            MaddnS => "maddn.s",
            DivnS => "divn.s",
            RoundS => "round.s",
            TruncS => "trunc.s",
            FloorS => "floor.s",
            CeilS => "ceil.s",
            FloatS => "float.s",
            UfloatS => "ufloat.s",
            UtruncS => "utrunc.s",
            MovS => "mov.s",
            AbsS => "abs.s",
            ConstS => "const.s",
            Rfr => "rfr",
            Wfr => "wfr",
            NegS => "neg.s",
            Div0S => "div0.s",
            Recip0S => "recip0.s",
            Sqrt0S => "sqrt0.s",
            Rsqrt0S => "rsqrt0.s",
            Nexp01S => "nexp01.s",
            MksadjS => "mksadj.s",
            MkdadjS => "mkdadj.s",
            AddexpS => "addexp.s",
            AddexpmS => "addexpm.s",
            UnS => "un.s",
            OeqS => "oeq.s",
            UeqS => "ueq.s",
            OltS => "olt.s",
            UltS => "ult.s",
            OleS => "ole.s",
            UleS => "ule.s",
            MoveqzS => "moveqz.s",
            MovnezS => "movnez.s",
            MovltzS => "movltz.s",
            MovgezS => "movgez.s",
            MovfS => "movf.s",
            MovtS => "movt.s",
            Lsi => "lsi",
            Ssi => "ssi",
            Lsiu => "lsip",
            Ssiu => "ssip",
            Lsx => "lsx",
            Ssx => "ssx",
            Lsxu => "lsxp",
            Ssxu => "ssxp",
            LdQr => "ld.qr",
            StQr => "st.qr",
        };
        s.to_string()
    }

    fn mac16_name(&self) -> String {
        let op1 = self.imm as u32;
        let op2 = self.imm2;
        let kind = ["umul", "mul", "mula", "muls"][(op1 >> 2) as usize];
        let h = |b: u32| if op1 & b != 0 { 'h' } else { 'l' };
        let src = match op2 {
            0..=2 => "dd",
            3 => "ad",
            4..=6 => "da",
            _ => "aa",
        };
        let ld = match op2 {
            0 | 4 => ".ldinc",
            1 | 5 => ".lddec",
            _ => "",
        };
        format!("{kind}.{src}.{}{}{ld}", h(1), h(2))
    }

    /// Full disassembly in objdump style (branch targets as absolute addresses).
    pub fn disasm(&self, pc: u32) -> String {
        use Op::*;
        let a = |n: u8| format!("a{n}");
        let f = |n: u8| format!("f{n}");
        let b = |n: u8| format!("b{n}");
        let num = |v: i32| if v > -256 && v < 256 { format!("{v}") } else { format!("{:#x}", v as u32) };
        let tgt = |off: i32| format!("{:x}", pc.wrapping_add(off as u32));
        let (r, s, t) = (self.r, self.s, self.t);
        let ops: Vec<String> = match self.op {
            Unknown => vec![],
            Add | Addx2 | Addx4 | Addx8 | Sub | Subx2 | Subx4 | Subx8 | And | Or | Xor | Min | Max | Minu | Maxu
            | Salt | Saltu | Mull | Muluh | Mulsh | Mul16u | Mul16s | Quou | Quos | Remu | Rems | Moveqz | Movnez
            | Movltz | Movgez | AddN | Src => vec![a(r), a(s), a(t)],
            Movf | Movt => vec![a(r), a(s), b(t)],
            Neg | Abs => vec![a(r), a(t)],
            Sext | Clamps => vec![a(r), a(s), num(self.imm)],
            Nsa | Nsau => vec![a(t), a(s)],
            Sll => vec![a(r), a(s)],
            Srl | Sra => vec![a(r), a(t)],
            Slli => vec![a(r), a(s), num(self.imm)],
            Srli | Srai => vec![a(r), a(t), num(self.imm)],
            Ssr | Ssl | Ssa8l | Ssa8b => vec![a(s)],
            Ssai => vec![num(self.imm)],
            Extui => vec![a(r), a(t), num(self.imm2), num((self.imm as u32).count_ones() as i32)],
            L8ui | L16ui | L16si | L32i | S8i | S16i | S32i | L32ai | S32ri | S32c1i | S32nb | L32e | S32e | L32iN
            | S32iN => vec![a(t), a(s), num(self.imm)],
            Addi | Addmi => vec![a(t), a(s), num(self.imm)],
            Movi => vec![a(t), num(self.imm)],
            L32r => vec![a(t), format!("{:x}", ((pc.wrapping_add(3)) & !3).wrapping_add(self.imm as u32))],
            AddiN => vec![a(r), a(s), num(self.imm)],
            MovN => vec![a(t), a(s)],
            MoviN => vec![a(s), num(self.imm)],
            BeqzN | BnezN | Beqz | Bnez | Bltz | Bgez => vec![a(s), tgt(self.imm)],
            Beqi | Bnei | Blti | Bgei | Bltui | Bgeui | Bbci | Bbsi => vec![a(s), num(self.imm2), tgt(self.imm)],
            Beq | Bne | Blt | Bge | Bltu | Bgeu | Bany | Bnone | Ball | Bnall | Bbc | Bbs => {
                vec![a(s), a(t), tgt(self.imm)]
            }
            Bf | Bt => vec![b(s), tgt(self.imm)],
            J => vec![tgt(self.imm)],
            Call0 | Calln => vec![format!("{:x}", (pc & !3).wrapping_add(self.imm as u32))],
            Jx | Callx0 | Callxn => vec![a(s)],
            Entry => vec![a(s), num(self.imm)],
            Movsp => vec![a(t), a(s)],
            Rotw => vec![num(self.imm)],
            Loop | Loopnez | Loopgtz => vec![a(s), tgt(self.imm)],
            Rsr | Wsr | Xsr => {
                if sr_name(self.imm as u32).is_some() {
                    vec![a(t)]
                } else {
                    vec![a(t), num(self.imm)]
                }
            }
            Rur => {
                if ur_name(self.imm as u32).is_some() {
                    vec![a(r)]
                } else {
                    vec![a(r), num(self.imm)]
                }
            }
            Wur => {
                if ur_name(self.imm as u32).is_some() {
                    vec![a(t)]
                } else {
                    vec![a(t), num(self.imm)]
                }
            }
            Rsil => vec![a(t), num(s as i32)],
            Waiti | Rfi => vec![num(s as i32)],
            Rfdo => vec![num(s as i32)],
            Break => vec![num(s as i32), num(t as i32)],
            BreakN => vec![num(s as i32)],
            Rer | Wer => vec![a(t), a(s)],
            Tlb => match r {
                4 | 12 => vec![a(s)],
                _ => vec![a(t), a(s)],
            },
            Cache => vec![a(s), num(self.imm)],
            Andb | Andbc | Orb | Orbc | Xorb => vec![b(r), b(s), b(t)],
            Any4 | All4 | Any8 | All8 => vec![b(t), b(s)],
            Mac16 => self.mac16_ops(),
            Ldinc | Lddec => vec![format!("m{}", r & 3), a(s)],
            AddS | SubS | MulS | MaddS | MsubS | MaddnS | DivnS => vec![f(r), f(s), f(t)],
            RoundS | TruncS | FloorS | CeilS | UtruncS => vec![a(r), f(s), num(self.imm)],
            FloatS | UfloatS => vec![f(r), a(s), num(self.imm)],
            MovS | AbsS | NegS | Div0S | Recip0S | Sqrt0S | Rsqrt0S | Nexp01S | MksadjS | MkdadjS | AddexpS
            | AddexpmS => vec![f(r), f(s)],
            ConstS => vec![f(r), num(self.imm)],
            Rfr => vec![a(r), f(s)],
            Wfr => vec![f(r), a(s)],
            UnS | OeqS | UeqS | OltS | UltS | OleS | UleS => vec![b(r), f(s), f(t)],
            MoveqzS | MovnezS | MovltzS | MovgezS => vec![f(r), f(s), a(t)],
            MovfS | MovtS => vec![f(r), f(s), b(t)],
            Lsi | Ssi | Lsiu | Ssiu => vec![f(t), a(s), num(self.imm)],
            Lsx | Ssx | Lsxu | Ssxu => vec![f(r), a(s), a(t)],
            LdQr | StQr => vec![format!("q{}", self.imm2), a(t), num(self.imm)],
            _ => vec![],
        };
        let m = self.mnemonic();
        if ops.is_empty() { m } else { format!("{m}\t{}", ops.join(", ")) }
    }

    fn mac16_ops(&self) -> Vec<String> {
        let (r, s, t) = (self.r, self.s, self.t);
        let a = |n: u8| format!("a{n}");
        let mx = format!("m{}", (r >> 2) & 1);
        let my = format!("m{}", 2 + ((t >> 2) & 1));
        match self.imm2 {
            0 | 1 => vec![format!("m{}", r & 3), a(s), mx, my],
            4 | 5 => vec![format!("m{}", r & 3), a(s), mx, a(t)],
            2 => vec![mx, my],
            3 => vec![a(s), my],
            6 => vec![mx, a(t)],
            _ => vec![a(s), a(t)],
        }
    }
}
