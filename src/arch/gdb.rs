//! The cores' registers as GDB numbers them, for the debugger (`--gdb`).
//!
//! RISC-V: x0..x31 and pc in the `g` packet, CSRs at 65 + their number, with a target
//! description so GDB knows there is no FPU. Xtensa: GDB has the ESP32-S3's register map
//! built in (`xtensa-esp32s3-elf-gdb`) and reads no target description; [`XTENSA_REGS`] is
//! that map in OpenOCD's order (`target/xtensa-core-esp32s3.cfg`), which GDB's matches.

use sim_api::RegValue;

use super::riscv::Rv32;
use super::xtensa::Xtensa;

// ---- RISC-V ---------------------------------------------------------------------------------

const RISCV_PC: usize = 32;
const RISCV_FIRST_CSR: usize = 65;

pub const RISCV_TARGET_XML: &str = r#"<?xml version="1.0"?>
<!DOCTYPE target SYSTEM "gdb-target.dtd">
<target version="1.0">
  <architecture>riscv:rv32</architecture>
  <feature name="org.gnu.gdb.riscv.cpu">
    <reg name="zero" bitsize="32" type="int" regnum="0"/>
    <reg name="ra" bitsize="32" type="code_ptr"/>
    <reg name="sp" bitsize="32" type="data_ptr"/>
    <reg name="gp" bitsize="32" type="data_ptr"/>
    <reg name="tp" bitsize="32" type="data_ptr"/>
    <reg name="t0" bitsize="32" type="int"/>
    <reg name="t1" bitsize="32" type="int"/>
    <reg name="t2" bitsize="32" type="int"/>
    <reg name="fp" bitsize="32" type="data_ptr"/>
    <reg name="s1" bitsize="32" type="int"/>
    <reg name="a0" bitsize="32" type="int"/>
    <reg name="a1" bitsize="32" type="int"/>
    <reg name="a2" bitsize="32" type="int"/>
    <reg name="a3" bitsize="32" type="int"/>
    <reg name="a4" bitsize="32" type="int"/>
    <reg name="a5" bitsize="32" type="int"/>
    <reg name="a6" bitsize="32" type="int"/>
    <reg name="a7" bitsize="32" type="int"/>
    <reg name="s2" bitsize="32" type="int"/>
    <reg name="s3" bitsize="32" type="int"/>
    <reg name="s4" bitsize="32" type="int"/>
    <reg name="s5" bitsize="32" type="int"/>
    <reg name="s6" bitsize="32" type="int"/>
    <reg name="s7" bitsize="32" type="int"/>
    <reg name="s8" bitsize="32" type="int"/>
    <reg name="s9" bitsize="32" type="int"/>
    <reg name="s10" bitsize="32" type="int"/>
    <reg name="s11" bitsize="32" type="int"/>
    <reg name="t3" bitsize="32" type="int"/>
    <reg name="t4" bitsize="32" type="int"/>
    <reg name="t5" bitsize="32" type="int"/>
    <reg name="t6" bitsize="32" type="int"/>
    <reg name="pc" bitsize="32" type="code_ptr"/>
  </feature>
  <feature name="org.gnu.gdb.riscv.csr">
    <reg name="mstatus" bitsize="32" regnum="833"/>
    <reg name="misa" bitsize="32" regnum="834"/>
    <reg name="mie" bitsize="32" regnum="837"/>
    <reg name="mtvec" bitsize="32" regnum="838"/>
    <reg name="mscratch" bitsize="32" regnum="897"/>
    <reg name="mepc" bitsize="32" regnum="898"/>
    <reg name="mcause" bitsize="32" regnum="899"/>
    <reg name="mtval" bitsize="32" regnum="900"/>
    <reg name="mip" bitsize="32" regnum="901"/>
  </feature>
</target>
"#;

fn word(v: u32) -> RegValue {
    RegValue::Value(v.to_le_bytes().to_vec())
}

fn le32(b: &[u8]) -> Option<u32> {
    Some(u32::from_le_bytes(b.try_into().ok()?))
}

pub fn riscv_registers(cpu: &Rv32) -> Vec<RegValue> {
    cpu.x.iter().map(|&v| word(v)).chain([word(cpu.pc)]).collect()
}

/// `cycles`: the current cycle count (the performance counter CSRs read it).
pub fn riscv_register(cpu: &Rv32, n: usize, cycles: u64) -> Option<RegValue> {
    match n {
        0..32 => Some(word(cpu.x[n])),
        RISCV_PC => Some(word(cpu.pc)),
        _ => {
            let csr = n.checked_sub(RISCV_FIRST_CSR).filter(|&c| c < 4096)?;
            Some(cpu.read_csr(csr as u16, cycles).map_or(RegValue::Unavailable(4), word))
        }
    }
}

pub fn riscv_set_register(cpu: &mut Rv32, n: usize, value: &[u8], cycles: u64) -> bool {
    let Some(v) = le32(value) else { return false };
    match n {
        0 => {}
        1..32 => cpu.x[n] = v,
        RISCV_PC => cpu.pc = v,
        _ => match n.checked_sub(RISCV_FIRST_CSR).filter(|&c| c < 4096) {
            Some(csr) => cpu.write_csr(csr as u16, v, cycles),
            None => return false,
        },
    }
    true
}

// ---- Xtensa ---------------------------------------------------------------------------------

/// GDB's ESP32-S3 registers in GDB's order: (name, OpenOCD's register number, which says
/// what it is: 0x0020 pc, 0x01nn physical ARnn, 0x02nn special register nn, 0x03nn user
/// register nn, 0x003n FPU fn, 0x100n vector qn, 0x000n windowed an, 0x20nn debug-module
/// registers the simulator doesn't have).
pub const XTENSA_REGS: &[(&str, u16)] = &[
    ("pc", 0x0020),
    ("ar0", 0x0100),
    ("ar1", 0x0101),
    ("ar2", 0x0102),
    ("ar3", 0x0103),
    ("ar4", 0x0104),
    ("ar5", 0x0105),
    ("ar6", 0x0106),
    ("ar7", 0x0107),
    ("ar8", 0x0108),
    ("ar9", 0x0109),
    ("ar10", 0x010a),
    ("ar11", 0x010b),
    ("ar12", 0x010c),
    ("ar13", 0x010d),
    ("ar14", 0x010e),
    ("ar15", 0x010f),
    ("ar16", 0x0110),
    ("ar17", 0x0111),
    ("ar18", 0x0112),
    ("ar19", 0x0113),
    ("ar20", 0x0114),
    ("ar21", 0x0115),
    ("ar22", 0x0116),
    ("ar23", 0x0117),
    ("ar24", 0x0118),
    ("ar25", 0x0119),
    ("ar26", 0x011a),
    ("ar27", 0x011b),
    ("ar28", 0x011c),
    ("ar29", 0x011d),
    ("ar30", 0x011e),
    ("ar31", 0x011f),
    ("ar32", 0x0120),
    ("ar33", 0x0121),
    ("ar34", 0x0122),
    ("ar35", 0x0123),
    ("ar36", 0x0124),
    ("ar37", 0x0125),
    ("ar38", 0x0126),
    ("ar39", 0x0127),
    ("ar40", 0x0128),
    ("ar41", 0x0129),
    ("ar42", 0x012a),
    ("ar43", 0x012b),
    ("ar44", 0x012c),
    ("ar45", 0x012d),
    ("ar46", 0x012e),
    ("ar47", 0x012f),
    ("ar48", 0x0130),
    ("ar49", 0x0131),
    ("ar50", 0x0132),
    ("ar51", 0x0133),
    ("ar52", 0x0134),
    ("ar53", 0x0135),
    ("ar54", 0x0136),
    ("ar55", 0x0137),
    ("ar56", 0x0138),
    ("ar57", 0x0139),
    ("ar58", 0x013a),
    ("ar59", 0x013b),
    ("ar60", 0x013c),
    ("ar61", 0x013d),
    ("ar62", 0x013e),
    ("ar63", 0x013f),
    ("lbeg", 0x0200),
    ("lend", 0x0201),
    ("lcount", 0x0202),
    ("sar", 0x0203),
    ("windowbase", 0x0248),
    ("windowstart", 0x0249),
    ("configid0", 0x02b0),
    ("configid1", 0x02d0),
    ("ps", 0x02e6),
    ("threadptr", 0x03e7),
    ("br", 0x0204),
    ("scompare1", 0x020c),
    ("acclo", 0x0210),
    ("acchi", 0x0211),
    ("m0", 0x0220),
    ("m1", 0x0221),
    ("m2", 0x0222),
    ("m3", 0x0223),
    ("gpio_out", 0x030c),
    ("f0", 0x0030),
    ("f1", 0x0031),
    ("f2", 0x0032),
    ("f3", 0x0033),
    ("f4", 0x0034),
    ("f5", 0x0035),
    ("f6", 0x0036),
    ("f7", 0x0037),
    ("f8", 0x0038),
    ("f9", 0x0039),
    ("f10", 0x003a),
    ("f11", 0x003b),
    ("f12", 0x003c),
    ("f13", 0x003d),
    ("f14", 0x003e),
    ("f15", 0x003f),
    ("fcr", 0x03e8),
    ("fsr", 0x03e9),
    ("accx_0", 0x0300),
    ("accx_1", 0x0301),
    ("qacc_h_0", 0x0302),
    ("qacc_h_1", 0x0303),
    ("qacc_h_2", 0x0304),
    ("qacc_h_3", 0x0305),
    ("qacc_h_4", 0x0306),
    ("qacc_l_0", 0x0307),
    ("qacc_l_1", 0x0308),
    ("qacc_l_2", 0x0309),
    ("qacc_l_3", 0x030a),
    ("qacc_l_4", 0x030b),
    ("sar_byte", 0x030d),
    ("fft_bit_width", 0x030e),
    ("ua_state_0", 0x030f),
    ("ua_state_1", 0x0310),
    ("ua_state_2", 0x0311),
    ("ua_state_3", 0x0312),
    ("q0", 0x1008),
    ("q1", 0x1009),
    ("q2", 0x100a),
    ("q3", 0x100b),
    ("q4", 0x100c),
    ("q5", 0x100d),
    ("q6", 0x100e),
    ("q7", 0x100f),
    ("mmid", 0x0259),
    ("ibreakenable", 0x0260),
    ("memctl", 0x0261),
    ("atomctl", 0x0263),
    ("ddr", 0x0268),
    ("ibreaka0", 0x0280),
    ("ibreaka1", 0x0281),
    ("dbreaka0", 0x0290),
    ("dbreaka1", 0x0291),
    ("dbreakc0", 0x02a0),
    ("dbreakc1", 0x02a1),
    ("epc1", 0x02b1),
    ("epc2", 0x02b2),
    ("epc3", 0x02b3),
    ("epc4", 0x02b4),
    ("epc5", 0x02b5),
    ("epc6", 0x02b6),
    ("epc7", 0x02b7),
    ("depc", 0x02c0),
    ("eps2", 0x02c2),
    ("eps3", 0x02c3),
    ("eps4", 0x02c4),
    ("eps5", 0x02c5),
    ("eps6", 0x02c6),
    ("eps7", 0x02c7),
    ("excsave1", 0x02d1),
    ("excsave2", 0x02d2),
    ("excsave3", 0x02d3),
    ("excsave4", 0x02d4),
    ("excsave5", 0x02d5),
    ("excsave6", 0x02d6),
    ("excsave7", 0x02d7),
    ("cpenable", 0x02e0),
    ("interrupt", 0x02e2),
    ("intset", 0x02e2),
    ("intclear", 0x02e3),
    ("intenable", 0x02e4),
    ("vecbase", 0x02e7),
    ("exccause", 0x02e8),
    ("debugcause", 0x02e9),
    ("ccount", 0x02ea),
    ("prid", 0x02eb),
    ("icount", 0x02ec),
    ("icountlevel", 0x02ed),
    ("excvaddr", 0x02ee),
    ("ccompare0", 0x02f0),
    ("ccompare1", 0x02f1),
    ("ccompare2", 0x02f2),
    ("misc0", 0x02f4),
    ("misc1", 0x02f5),
    ("misc2", 0x02f6),
    ("misc3", 0x02f7),
    ("pwrctl", 0x2028),
    ("pwrstat", 0x2029),
    ("eristat", 0x202a),
    ("cs_itctrl", 0x202b),
    ("cs_claimset", 0x202c),
    ("cs_claimclr", 0x202d),
    ("cs_lockaccess", 0x202e),
    ("cs_lockstatus", 0x202f),
    ("cs_authstatus", 0x2030),
    ("fault_info", 0x203f),
    ("trax_id", 0x2040),
    ("trax_control", 0x2041),
    ("trax_status", 0x2042),
    ("trax_data", 0x2043),
    ("trax_address", 0x2044),
    ("trax_pctrigger", 0x2045),
    ("trax_pcmatch", 0x2046),
    ("trax_delay", 0x2047),
    ("trax_memstart", 0x2048),
    ("trax_memend", 0x2049),
    ("pmg", 0x2057),
    ("pmpc", 0x2058),
    ("pm0", 0x2059),
    ("pm1", 0x205a),
    ("pmctrl0", 0x205b),
    ("pmctrl1", 0x205c),
    ("pmstat0", 0x205d),
    ("pmstat1", 0x205e),
    ("ocdid", 0x205f),
    ("ocd_dcrclr", 0x2060),
    ("ocd_dcrset", 0x2061),
    ("ocd_dsr", 0x2062),
    ("a0", 0x0000),
    ("a1", 0x0001),
    ("a2", 0x0002),
    ("a3", 0x0003),
    ("a4", 0x0004),
    ("a5", 0x0005),
    ("a6", 0x0006),
    ("a7", 0x0007),
    ("a8", 0x0008),
    ("a9", 0x0009),
    ("a10", 0x000a),
    ("a11", 0x000b),
    ("a12", 0x000c),
    ("a13", 0x000d),
    ("a14", 0x000e),
    ("a15", 0x000f),
];

/// Registers in the `g` packet: pc through m3, all 32-bit (GDB asks for the rest with `p`).
const XTENSA_G_REGS: usize = 83;

pub fn xtensa_registers(cpu: &Xtensa) -> Vec<RegValue> {
    (0..XTENSA_G_REGS).filter_map(|n| xtensa_register(cpu, n)).collect()
}

pub fn xtensa_register(cpu: &Xtensa, n: usize) -> Option<RegValue> {
    let &(_, id) = XTENSA_REGS.get(n)?;
    let (hi, lo) = (id >> 8, (id & 0xff) as u32);
    let v = match (hi, lo) {
        (0x00, 0x20) => Some(cpu.pc),
        (0x00, 0x30..=0x3f) => Some(cpu.f[(lo - 0x30) as usize]),
        (0x00, 0..=15) => {
            let wb = cpu.read_sr(72).unwrap_or(0);
            Some(cpu.ar[((wb * 4 + lo) % 64) as usize])
        }
        (0x01, _) => cpu.ar.get(lo as usize).copied(),
        (0x02, _) => cpu.read_sr(lo),
        (0x03, _) => cpu.read_ur(lo),
        (0x10, 0..=7) => {
            let q = cpu.cp3.q[lo as usize];
            return Some(RegValue::Value(q.iter().flat_map(|w| w.to_le_bytes()).collect()));
        }
        _ => None,
    };
    Some(v.map_or(RegValue::Unavailable(4), word))
}

pub fn xtensa_set_register(cpu: &mut Xtensa, n: usize, value: &[u8]) -> bool {
    let Some(&(_, id)) = XTENSA_REGS.get(n) else { return false };
    let (hi, lo) = (id >> 8, (id & 0xff) as u32);
    if hi == 0x10 && lo < 8 {
        let Ok(b) = <[u8; 16]>::try_from(value) else { return false };
        for (i, w) in b.chunks(4).enumerate() {
            cpu.cp3.q[lo as usize][i] = le32(w).unwrap_or(0);
        }
        return true;
    }
    let Some(v) = le32(value) else { return false };
    match (hi, lo) {
        (0x00, 0x20) => cpu.pc = v,
        (0x00, 0x30..=0x3f) => cpu.f[(lo - 0x30) as usize] = v,
        (0x00, 0..=15) => {
            let wb = cpu.read_sr(72).unwrap_or(0);
            cpu.ar[((wb * 4 + lo) % 64) as usize] = v;
        }
        (0x01, 0..=63) => cpu.ar[lo as usize] = v,
        (0x02, _) => return cpu.write_sr(lo, v),
        (0x03, _) => return cpu.write_ur(lo, v),
        _ => return false,
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xtensa_map_is_gdbs_esp32s3_order() {
        let index = |name| XTENSA_REGS.iter().position(|r| r.0 == name).unwrap();
        assert_eq!(XTENSA_REGS.len(), 228);
        assert_eq!((index("pc"), index("ar0"), index("windowbase"), index("ps"), index("a0")), (0, 1, 69, 73, 212));
        assert_eq!(XTENSA_REGS[XTENSA_G_REGS - 1].0, "m3");
    }

    #[test]
    fn xtensa_windowed_registers_follow_windowbase() {
        let mut cpu = Xtensa::new();
        cpu.ar[8] = 0x1234;
        assert!(cpu.write_sr(72, 2));
        let a0 = XTENSA_REGS.iter().position(|r| r.0 == "a0").unwrap();
        assert_eq!(xtensa_register(&cpu, a0), Some(word(0x1234)));
        assert!(xtensa_set_register(&mut cpu, a0 + 1, &7u32.to_le_bytes()));
        assert_eq!(cpu.ar[9], 7);
        assert_eq!(xtensa_registers(&cpu).len(), XTENSA_G_REGS);
    }

    #[test]
    fn riscv_registers_and_csrs() {
        let mut cpu = Rv32::new();
        cpu.x[10] = 42;
        cpu.pc = 0x4200_0000;
        let g = riscv_registers(&cpu);
        assert_eq!((g.len(), &g[10], &g[32]), (33, &word(42), &word(0x4200_0000)));
        assert!(riscv_set_register(&mut cpu, RISCV_FIRST_CSR + 0x341, &0x4200_0100u32.to_le_bytes(), 0));
        assert_eq!(riscv_register(&cpu, RISCV_FIRST_CSR + 0x341, 0), Some(word(0x4200_0100)));
        assert!(riscv_set_register(&mut cpu, 0, &[1, 0, 0, 0], 0) && cpu.x[0] == 0);
    }
}
