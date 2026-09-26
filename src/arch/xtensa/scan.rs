//! Static opcode-coverage scan: recursive-descent disassembly of an ELF's code using
//! this crate's decoder, starting from every function symbol (and the exception vector
//! table). Linear disassembly (objdump -d) is useless for this on Xtensa because
//! literal pools and alignment padding live inside `.text`; following control flow
//! only visits bytes that can actually execute.

use super::decode::{Insn, Op, decode};
use object::{Object, ObjectSection, ObjectSymbol};
use std::collections::{BTreeMap, HashSet};

pub struct Code {
    segs: Vec<(u32, Vec<u8>)>,
    pub roots: Vec<(u32, String)>,
}

impl Code {
    pub fn from_elf(data: &[u8]) -> Code {
        let obj = object::File::parse(data).expect("parse elf");
        let mut segs = Vec::new();
        for sec in obj.sections() {
            let exec = sec.kind() == object::SectionKind::Text;
            if exec
                && sec.size() > 0
                && let Ok(d) = sec.data()
            {
                segs.push((sec.address() as u32, d.to_vec()));
            }
        }
        let mut roots = Vec::new();
        for sym in obj.symbols() {
            if sym.kind() == object::SymbolKind::Text && sym.address() != 0 {
                roots.push((sym.address() as u32, sym.name().unwrap_or("?").to_string()));
            }
        }
        Code { segs, roots }
    }

    pub fn fetch(&self, a: u32) -> Option<u32> {
        for (base, d) in &self.segs {
            if a >= *base && ((a - base) as usize) < d.len() {
                let o = (a - base) as usize;
                let mut w = 0u32;
                for k in 0..4 {
                    w |= (*d.get(o + k).unwrap_or(&0) as u32) << (8 * k);
                }
                return Some(w);
            }
        }
        None
    }

    pub fn contains(&self, a: u32) -> bool {
        self.segs.iter().any(|(b, d)| a >= *b && ((a - b) as usize) < d.len())
    }

    pub fn code_bytes(&self) -> usize {
        self.segs.iter().map(|(_, d)| d.len()).sum()
    }
}

pub struct ScanResult {
    /// Every reachable instruction.
    pub insns: BTreeMap<u32, Insn>,
    /// Reachable undecodable instructions: (pc, raw, len, root it came from).
    pub unknown: Vec<(u32, u32, u8, String)>,
    /// Undecodable bytes reached only by falling through a call (the callee did not
    /// return, e.g. `call8 abort` followed by padding): not counted as errors.
    pub after_noreturn: usize,
}

impl ScanResult {
    pub fn histogram(&self) -> BTreeMap<String, usize> {
        let mut h = BTreeMap::new();
        for i in self.insns.values() {
            *h.entry(i.mnemonic()).or_insert(0) += 1;
        }
        h
    }
}

pub fn scan(code: &Code, extra_roots: &[(u32, String)]) -> ScanResult {
    let mut insns = BTreeMap::new();
    let mut unknown = Vec::new();
    let mut seen = HashSet::new();
    let mut after_noreturn = 0;
    // (pc, root index, speculative: only reachable by falling through a call)
    let mut work: Vec<(u32, usize, bool)> = Vec::new();
    let mut names: Vec<String> = Vec::new();
    for (a, n) in code.roots.iter().chain(extra_roots) {
        names.push(n.clone());
        work.push((*a, names.len() - 1, false));
    }
    while let Some((mut pc, root, spec)) = work.pop() {
        loop {
            if !seen.insert(pc) {
                break;
            }
            let Some(w) = code.fetch(pc) else { break };
            let i = decode(w);
            insns.insert(pc, i);
            let target = pc.wrapping_add(i.imm as u32);
            let fall = pc.wrapping_add(i.len as u32);
            let mut push = |a: u32| {
                if code.contains(a) {
                    work.push((a, root, spec));
                }
            };
            match i.op {
                Op::Unknown => {
                    if spec {
                        after_noreturn += 1;
                    } else {
                        unknown.push((pc, i.raw, i.len, names[root].clone()));
                    }
                    break;
                }
                Op::J => {
                    push(target);
                    break;
                }
                Op::Jx
                | Op::Ret
                | Op::RetN
                | Op::Retw
                | Op::RetwN
                | Op::Rfe
                | Op::Rfue
                | Op::Rfde
                | Op::Rfwo
                | Op::Rfwu
                | Op::Rfi
                | Op::Rfdo
                | Op::Rfdd
                | Op::Ill
                | Op::IllN => break,
                Op::Call0 | Op::Calln | Op::Callx0 | Op::Callxn => {
                    if matches!(i.op, Op::Call0 | Op::Calln) {
                        push((pc & !3).wrapping_add(i.imm as u32));
                    }
                    if code.contains(fall) {
                        work.push((fall, root, true));
                    }
                    break;
                }
                Op::Beqz
                | Op::Bnez
                | Op::Bltz
                | Op::Bgez
                | Op::Beqi
                | Op::Bnei
                | Op::Blti
                | Op::Bgei
                | Op::Bltui
                | Op::Bgeui
                | Op::Beq
                | Op::Bne
                | Op::Blt
                | Op::Bge
                | Op::Bltu
                | Op::Bgeu
                | Op::Bany
                | Op::Bnone
                | Op::Ball
                | Op::Bnall
                | Op::Bbc
                | Op::Bbs
                | Op::Bbci
                | Op::Bbsi
                | Op::Bf
                | Op::Bt
                | Op::BeqzN
                | Op::BnezN
                | Op::Loopnez
                | Op::Loopgtz => push(target),
                _ => {}
            }
            pc = fall;
        }
    }
    ScanResult { insns, unknown, after_noreturn }
}
