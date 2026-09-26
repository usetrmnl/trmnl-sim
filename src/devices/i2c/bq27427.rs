//! TI BQ27427 single-cell fuel gauge (TRMNL X: 0x55), as used by
//! lib/BQ27427/src/BQ27427.cpp and src/battery/bq27427_battery.cpp.
//!
//! Protocol (all via Arduino Wire, write + STOP then a separate read):
//! - standard commands: `readWord(cmd)` = write `[cmd]`, read 2 bytes LE;
//! - control subcommands: write `[0x00, lo, hi]`, then read 2 bytes from 0x00;
//! - data memory: 0x61 BlockDataControl, 0x3E DataClass, 0x3F DataBlock,
//!   0x40..0x5F the selected 32-byte block (RAM copy), 0x60 BlockDataChecksum.
//!   Writing the checksum commits the RAM block if it equals
//!   `255 - (sum & 0xFF)`; selecting a class/block (re)loads it, so reading
//!   0x60 after reselecting echoes the stored block's checksum
//!   (`goldenFileWriteBlock`'s verification).
//!
//! The default state is an already-configured gauge (ITPOR = 0, golden-file
//! data memory for the chosen cell count), so `connectAndConfigure()` takes the
//! no-config path: `Flags.ITPOR == 0`, IT Cfg (class 80) byte 81 =
//! Design Energy Scale 1 (1 cell) or 10 (2 cells), CC Cal (class 105) byte 5
//! bit 7 = 0. [`Bq27427::unconfigured`] / [`Bq27427::por`] model a fresh
//! power-on instead (ITPOR = 1, factory data memory), which drives the firmware
//! through the golden-file path.

use std::collections::HashMap;

use super::I2cDevice;

pub const ADDR: u8 = 0x55;

pub const CMD_CONTROL: u8 = 0x00;
pub const CMD_TEMP: u8 = 0x02;
pub const CMD_VOLTAGE: u8 = 0x04;
pub const CMD_FLAGS: u8 = 0x06;
pub const CMD_NOM_CAPACITY: u8 = 0x08;
pub const CMD_AVAIL_CAPACITY: u8 = 0x0A;
pub const CMD_REM_CAPACITY: u8 = 0x0C;
pub const CMD_FULL_CAPACITY: u8 = 0x0E;
pub const CMD_AVG_CURRENT: u8 = 0x10;
pub const CMD_STDBY_CURRENT: u8 = 0x12;
pub const CMD_MAX_CURRENT: u8 = 0x14;
pub const CMD_AVG_POWER: u8 = 0x18;
pub const CMD_SOC: u8 = 0x1C;
pub const CMD_INT_TEMP: u8 = 0x1E;
pub const CMD_SOH: u8 = 0x20;
pub const CMD_REM_CAP_UNFL: u8 = 0x28;
pub const CMD_REM_CAP_FIL: u8 = 0x2A;
pub const CMD_FULL_CAP_UNFL: u8 = 0x2C;
pub const CMD_FULL_CAP_FIL: u8 = 0x2E;
pub const CMD_SOC_UNFL: u8 = 0x30;
pub const EXT_DATACLASS: u8 = 0x3E;
pub const EXT_DATABLOCK: u8 = 0x3F;
pub const EXT_BLOCKDATA: u8 = 0x40;
pub const EXT_CHECKSUM: u8 = 0x60;
pub const EXT_CONTROL: u8 = 0x61;

pub const CTL_STATUS: u16 = 0x0000;
pub const CTL_DEVICE_TYPE: u16 = 0x0001;
pub const CTL_FW_VERSION: u16 = 0x0002;
pub const CTL_DM_CODE: u16 = 0x0004;
pub const CTL_PREV_MACWRITE: u16 = 0x0007;
pub const CTL_CHEM_ID: u16 = 0x0008;
pub const CTL_SET_CFGUPDATE: u16 = 0x0013;
pub const CTL_SEALED: u16 = 0x0020;
pub const CTL_CHEM_A: u16 = 0x0030;
pub const CTL_CHEM_B: u16 = 0x0031;
pub const CTL_CHEM_C: u16 = 0x0032;
pub const CTL_RESET: u16 = 0x0041;
pub const CTL_SOFT_RESET: u16 = 0x0042;

pub const DEVICE_TYPE: u16 = 0x0427;
pub const FW_VERSION: u16 = 0x0202;

pub const STATUS_SS: u16 = 1 << 13;
pub const STATUS_INITCOMP: u16 = 1 << 7;
pub const STATUS_VOK: u16 = 1 << 1;

pub const FLAG_OT: u16 = 1 << 15;
pub const FLAG_UT: u16 = 1 << 14;
pub const FLAG_FC: u16 = 1 << 9;
pub const FLAG_CHG: u16 = 1 << 8;
pub const FLAG_ITPOR: u16 = 1 << 5;
pub const FLAG_CFGUPMODE: u16 = 1 << 4;
pub const FLAG_BAT_DET: u16 = 1 << 3;
pub const FLAG_SOC1: u16 = 1 << 2;
pub const FLAG_SOCF: u16 = 1 << 1;
pub const FLAG_DSG: u16 = 1 << 0;

/// Data classes the firmware touches.
pub const CLASS_IT_CFG: u8 = 80;
pub const CLASS_STATE: u8 = 82;
pub const CLASS_CC_CAL: u8 = 105;

pub struct Bq27427 {
    // I2C
    ptr: u8,
    expect_cmd: bool,

    // Control()
    ctl_lo: u8,
    ctl_response: u16,
    last_subcommand: u16,
    pub sealed: bool,
    chem_id: u16,

    itpor: bool,
    cfgupmode: bool,

    // Data memory
    blocks: HashMap<(u8, u8), [u8; 32]>,
    block_ctl: u8,
    class: u8,
    block: u8,
    ram: [u8; 32],

    // Battery (set by the board)
    voltage_mv: u16,
    charging: bool,
    soc: u8,
    soh: u8,
    current_ma: Option<i16>,
    temp_dk: u16,
    /// Number of commits of a data-memory block (tests / diagnostics).
    pub block_commits: u32,
}

impl Bq27427 {
    /// A gauge already configured for a 1- or 2-cell pack (no-config boot path).
    pub fn new(cells: u8) -> Self {
        let two = cells >= 2;
        let mut b = Self::blank();
        for g in GOLDEN_BLOCKS.iter().filter(|g| g.cells.applies(two)) {
            b.blocks.insert((g.class, g.block), g.data);
        }
        b
    }

    /// A freshly powered gauge: ITPOR set, factory data memory
    /// (Design Energy Scale 1, design capacity 1340 mAh).
    pub fn unconfigured() -> Self {
        let mut b = Self::blank();
        b.load_factory();
        b
    }

    fn blank() -> Self {
        Bq27427 {
            ptr: 0,
            expect_cmd: false,
            ctl_lo: 0,
            ctl_response: 0,
            last_subcommand: 0,
            sealed: false,
            chem_id: 0x3230,
            itpor: false,
            cfgupmode: false,
            blocks: HashMap::new(),
            block_ctl: 0,
            class: 0,
            block: 0,
            ram: [0; 32],
            voltage_mv: 4000,
            charging: false,
            soc: 80,
            soh: 100,
            current_ma: None,
            temp_dk: 2982,
            block_commits: 0,
        }
    }

    fn load_factory(&mut self) {
        self.blocks.clear();
        let mut it_cfg2 = [0u8; 32];
        it_cfg2[17] = 1; // Design Energy Scale
        self.blocks.insert((CLASS_IT_CFG, 2), it_cfg2);
        let mut state = [0u8; 32];
        state[6..8].copy_from_slice(&1340u16.to_be_bytes()); // Design Capacity
        self.blocks.insert((CLASS_STATE, 0), state);
        self.itpor = true;
        self.cfgupmode = false;
    }

    /// Power-on reset: the BQ27427 keeps data memory in RAM, so it reverts to
    /// factory defaults with ITPOR set.
    pub fn por(&mut self) {
        let keep = (self.voltage_mv, self.charging, self.soc, self.soh, self.current_ma, self.temp_dk, self.sealed);
        let commits = self.block_commits;
        *self = Self::unconfigured();
        (self.voltage_mv, self.charging, self.soc, self.soh, self.current_ma, self.temp_dk, self.sealed) = keep;
        self.block_commits = commits;
    }

    /// Board input: pack voltage, whether it is charging, state of charge (%).
    pub fn set_battery(&mut self, mv: u16, charging: bool, soc: u8) {
        self.voltage_mv = mv;
        self.charging = charging;
        self.soc = soc.min(100);
    }

    /// Override AverageCurrent (mA, negative = discharging). Default: +500 mA while
    /// charging below 100 %, 0 when full, -50 mA discharging.
    pub fn set_current_ma(&mut self, ma: Option<i16>) {
        self.current_ma = ma;
    }

    pub fn set_temperature_c(&mut self, c: f32) {
        self.temp_dk = ((c + 273.15) * 10.0).round().clamp(0.0, 65535.0) as u16;
    }

    pub fn set_soh(&mut self, pct: u8) {
        self.soh = pct.min(100);
    }

    pub fn itpor(&self) -> bool {
        self.itpor
    }

    pub fn cfgupmode(&self) -> bool {
        self.cfgupmode
    }

    /// Stored data-memory block (None if never written / not present).
    pub fn block(&self, class: u8, block: u8) -> Option<&[u8; 32]> {
        self.blocks.get(&(class, block))
    }

    /// Data-memory byte by class and absolute offset (as `readExtendedData`).
    pub fn dm_byte(&self, class: u8, offset: u8) -> u8 {
        self.blocks.get(&(class, offset / 32)).map_or(0, |b| b[(offset % 32) as usize])
    }

    fn energy_scale(&self) -> u16 {
        (self.dm_byte(CLASS_IT_CFG, 81) as u16).max(1)
    }

    fn design_capacity(&self) -> u16 {
        let c = u16::from_be_bytes([self.dm_byte(CLASS_STATE, 6), self.dm_byte(CLASS_STATE, 7)]);
        if c == 0 { 1340 } else { c }
    }

    pub fn flags(&self) -> u16 {
        let mut f = FLAG_BAT_DET;
        if self.itpor {
            f |= FLAG_ITPOR;
        }
        if self.cfgupmode {
            f |= FLAG_CFGUPMODE;
        }
        if self.charging {
            if self.soc >= 100 {
                f |= FLAG_FC;
            } else {
                f |= FLAG_CHG;
            }
        } else {
            f |= FLAG_DSG;
        }
        if self.soc <= 10 {
            f |= FLAG_SOC1;
        }
        if self.soc <= 2 {
            f |= FLAG_SOCF;
        }
        f
    }

    fn control_status(&self) -> u16 {
        let mut s = STATUS_INITCOMP | STATUS_VOK;
        if self.sealed {
            s |= STATUS_SS;
        }
        s
    }

    fn current(&self) -> i16 {
        self.current_ma.unwrap_or(if self.charging { if self.soc >= 100 { 0 } else { 500 } } else { -50 })
    }

    fn full_capacity(&self) -> u16 {
        (self.design_capacity() as u32 * self.soh as u32 / 100) as u16
    }

    fn remaining_capacity(&self) -> u16 {
        (self.full_capacity() as u32 * self.soc as u32 / 100) as u16
    }

    /// Standard command word at an even address.
    fn word(&self, cmd: u8) -> u16 {
        match cmd {
            CMD_CONTROL => self.ctl_response,
            CMD_TEMP | CMD_INT_TEMP => self.temp_dk,
            CMD_VOLTAGE => self.voltage_mv,
            CMD_FLAGS => self.flags(),
            CMD_NOM_CAPACITY | CMD_REM_CAPACITY | CMD_REM_CAP_UNFL | CMD_REM_CAP_FIL => self.remaining_capacity(),
            CMD_AVAIL_CAPACITY | CMD_FULL_CAPACITY | CMD_FULL_CAP_UNFL | CMD_FULL_CAP_FIL => self.full_capacity(),
            CMD_AVG_CURRENT => self.current() as u16,
            CMD_STDBY_CURRENT => -10i16 as u16,
            CMD_MAX_CURRENT => -200i16 as u16,
            CMD_AVG_POWER => (self.current() as i32 * self.voltage_mv as i32 / 1000) as i16 as u16,
            CMD_SOC | CMD_SOC_UNFL => self.soc as u16,
            CMD_SOH => u16::from_le_bytes([self.soh, 0x03]), // percent, status 3 = ready
            _ => 0,
        }
    }

    fn checksum(data: &[u8; 32]) -> u8 {
        255 - data.iter().fold(0u8, |a, &b| a.wrapping_add(b))
    }

    fn load_block(&mut self) {
        self.ram = self.blocks.get(&(self.class, self.block)).copied().unwrap_or([0; 32]);
    }

    fn subcommand(&mut self, sub: u16) {
        let prev = std::mem::replace(&mut self.last_subcommand, sub);
        self.ctl_response = match sub {
            CTL_STATUS => self.control_status(),
            CTL_DEVICE_TYPE => DEVICE_TYPE,
            CTL_FW_VERSION => FW_VERSION,
            CTL_DM_CODE => 0x0000,
            CTL_PREV_MACWRITE => prev,
            CTL_CHEM_ID => self.chem_id,
            _ => {
                match sub {
                    CTL_SET_CFGUPDATE if !self.sealed => self.cfgupmode = true,
                    CTL_SOFT_RESET if self.cfgupmode => {
                        // Leaving CONFIG UPDATE re-initialises gauging; ITPOR clears.
                        self.cfgupmode = false;
                        self.itpor = false;
                    }
                    CTL_RESET => self.por(),
                    CTL_SEALED => self.sealed = true,
                    CTL_CHEM_A | CTL_CHEM_B | CTL_CHEM_C if self.cfgupmode => self.chem_id = 0x3230 + (sub - 0x30),
                    // Unseal: two-word key 0x8000/0x8000 (the library) or 0x0414/0x3672 (golden file).
                    0x8000 if prev == 0x8000 => self.sealed = false,
                    0x3672 if prev == 0x0414 => self.sealed = false,
                    _ => {}
                }
                // Reading Control() after a command subcommand returns CONTROL_STATUS
                // (nonzero, which `unseal()` relies on).
                self.control_status()
            }
        };
    }

    fn write_byte_at(&mut self, addr: u8, v: u8) {
        match addr {
            0x00 => self.ctl_lo = v,
            0x01 => {
                let sub = u16::from_le_bytes([self.ctl_lo, v]);
                self.subcommand(sub);
            }
            EXT_DATACLASS => {
                self.class = v;
                self.block = 0;
                self.load_block();
            }
            EXT_DATABLOCK => {
                self.block = v;
                self.load_block();
            }
            0x40..=0x5F => self.ram[(addr - EXT_BLOCKDATA) as usize] = v,
            // A matching checksum commits the RAM block (a mismatch discards it).
            EXT_CHECKSUM if !self.sealed && v == Self::checksum(&self.ram) => {
                self.blocks.insert((self.class, self.block), self.ram);
                self.block_commits += 1;
            }
            EXT_CONTROL => self.block_ctl = v,
            _ => {}
        }
    }

    fn read_byte_at(&self, addr: u8) -> u8 {
        match addr {
            EXT_DATACLASS => self.class,
            EXT_DATABLOCK => self.block,
            0x40..=0x5F => self.ram[(addr - EXT_BLOCKDATA) as usize],
            EXT_CHECKSUM => Self::checksum(&self.ram),
            EXT_CONTROL => self.block_ctl,
            a if a < 0x3E => self.word(a & !1).to_le_bytes()[(a & 1) as usize],
            _ => 0,
        }
    }
}

impl I2cDevice for Bq27427 {
    fn address(&self) -> u8 {
        ADDR
    }

    fn start(&mut self, _now: u64, read: bool) -> bool {
        self.expect_cmd = !read;
        true
    }

    fn write(&mut self, _now: u64, byte: u8) -> bool {
        if self.expect_cmd {
            self.expect_cmd = false;
            self.ptr = byte;
        } else {
            self.write_byte_at(self.ptr, byte);
            self.ptr = self.ptr.wrapping_add(1);
        }
        true
    }

    fn read(&mut self, _now: u64, _ack: bool) -> u8 {
        let v = self.read_byte_at(self.ptr);
        self.ptr = self.ptr.wrapping_add(1);
        v
    }

    fn stop(&mut self, _now: u64) {
        self.expect_cmd = false;
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Cells {
    Both,
    One,
    Two,
}

impl Cells {
    fn applies(self, two: bool) -> bool {
        match self {
            Cells::Both => true,
            Cells::One => !two,
            Cells::Two => two,
        }
    }
}

/// One golden-file block from `BQ27427::applyGoldenFile()` (fw 2.02).
pub struct GoldenBlock {
    pub cells: Cells,
    pub class: u8,
    pub block: u8,
    pub checksum: u8,
    pub data: [u8; 32],
}

// Generated from lib/BQ27427/src/BQ27427.cpp applyGoldenFile(); every checksum
// equals 255 - (sum(data) & 0xFF).
pub const GOLDEN_BLOCKS: [GoldenBlock; 15] = [
    GoldenBlock {
        cells: Cells::Both,
        class: 0x02,
        block: 0x00,
        checksum: 0xA5,
        data: [
            0x02, 0x26, 0x00, 0x00, 0x32, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        ],
    },
    GoldenBlock {
        cells: Cells::Both,
        class: 0x24,
        block: 0x00,
        checksum: 0x69,
        data: [
            0x00, 0x19, 0x28, 0x63, 0x5F, 0xFF, 0x62, 0x00, 0x32, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        ],
    },
    GoldenBlock {
        cells: Cells::Both,
        class: 0x31,
        block: 0x00,
        checksum: 0xDF,
        data: [
            0x0A, 0x0F, 0x02, 0x05, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        ],
    },
    GoldenBlock {
        cells: Cells::Both,
        class: 0x40,
        block: 0x00,
        checksum: 0x06,
        data: [
            0x64, 0x78, 0x0F, 0x9F, 0x23, 0x00, 0x00, 0x14, 0x04, 0x00, 0x09, 0x04, 0x27, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        ],
    },
    GoldenBlock {
        cells: Cells::Both,
        class: 0x44,
        block: 0x00,
        checksum: 0xF9,
        data: [
            0x00, 0x32, 0x01, 0xC2, 0x30, 0x00, 0x03, 0x08, 0x98, 0x01, 0x00, 0x3C, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        ],
    },
    GoldenBlock {
        cells: Cells::Both,
        class: 0x50,
        block: 0x00,
        checksum: 0xE4,
        data: [
            0x01, 0xF4, 0x00, 0x1E, 0xC8, 0x14, 0x08, 0x00, 0x3C, 0x0E, 0x10, 0x00, 0x0A, 0x46, 0x05, 0x14, 0x05, 0x0F,
            0x03, 0x20, 0x7F, 0xFF, 0x00, 0xF0, 0x46, 0x50, 0x18, 0x01, 0x90, 0x00, 0x64, 0x19,
        ],
    },
    GoldenBlock {
        cells: Cells::Both,
        class: 0x50,
        block: 0x01,
        checksum: 0xDB,
        data: [
            0xDC, 0x5C, 0x60, 0x00, 0x7D, 0x00, 0x04, 0x03, 0x19, 0x25, 0x0F, 0x14, 0x0A, 0x78, 0x60, 0x28, 0x01, 0xF4,
            0x00, 0x00, 0x00, 0x00, 0x43, 0x80, 0x04, 0x01, 0x14, 0x00, 0x08, 0x0B, 0xB8, 0x01,
        ],
    },
    GoldenBlock {
        cells: Cells::One,
        class: 0x50,
        block: 0x02,
        checksum: 0x47,
        data: [
            0x2C, 0x0A, 0x01, 0x0A, 0x00, 0x00, 0x00, 0xC8, 0x00, 0x64, 0x02, 0x00, 0x00, 0x00, 0x00, 0x07, 0xD0, 0x01,
            0x03, 0x5A, 0x14, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        ],
    },
    GoldenBlock {
        cells: Cells::Two,
        class: 0x50,
        block: 0x02,
        checksum: 0x3E,
        data: [
            0x2C, 0x0A, 0x01, 0x0A, 0x00, 0x00, 0x00, 0xC8, 0x00, 0x64, 0x02, 0x00, 0x00, 0x00, 0x00, 0x07, 0xD0, 0x0A,
            0x03, 0x5A, 0x14, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        ],
    },
    GoldenBlock {
        cells: Cells::Both,
        class: 0x51,
        block: 0x00,
        checksum: 0x04,
        data: [
            0x01, 0xC2, 0x00, 0x64, 0x00, 0x64, 0x00, 0x3C, 0x3C, 0x01, 0xB3, 0xB3, 0x01, 0x90, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        ],
    },
    GoldenBlock {
        cells: Cells::One,
        class: 0x52,
        block: 0x00,
        checksum: 0xF7,
        data: [
            0x40, 0x00, 0x00, 0x00, 0x00, 0x81, 0x17, 0x70, 0x5A, 0x3C, 0x0B, 0xB8, 0x00, 0xC8, 0x00, 0x32, 0x00, 0x14,
            0x03, 0xE8, 0x01, 0x00, 0xC8, 0x00, 0x0A, 0xFF, 0xCE, 0xFF, 0xCE, 0x00, 0x01, 0x00,
        ],
    },
    GoldenBlock {
        cells: Cells::Two,
        class: 0x52,
        block: 0x00,
        checksum: 0x7F,
        data: [
            0x40, 0x00, 0x03, 0x00, 0x00, 0x81, 0x04, 0xB0, 0x12, 0x0C, 0x0B, 0xB8, 0x00, 0xC8, 0x00, 0x32, 0x00, 0x14,
            0x03, 0xE8, 0x01, 0x01, 0x90, 0x00, 0x0A, 0xFF, 0xF6, 0xFF, 0x9D, 0x00, 0x01, 0x00,
        ],
    },
    GoldenBlock {
        cells: Cells::Both,
        class: 0x59,
        block: 0x00,
        checksum: 0x79,
        data: [
            0x00, 0x4E, 0x00, 0x23, 0x00, 0x27, 0x00, 0x2D, 0x00, 0x2A, 0x00, 0x24, 0x00, 0x27, 0x00, 0x24, 0x00, 0x23,
            0x00, 0x25, 0x00, 0x26, 0x00, 0x28, 0x00, 0x2E, 0x00, 0x36, 0x00, 0x2E, 0x00, 0x00,
        ],
    },
    GoldenBlock {
        cells: Cells::Both,
        class: 0x6D,
        block: 0x00,
        checksum: 0x81,
        data: [
            0x09, 0x22, 0x0E, 0xE3, 0x0E, 0xA6, 0x10, 0xF4, 0x10, 0x9A, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        ],
    },
    GoldenBlock {
        cells: Cells::Both,
        class: 0x70,
        block: 0x00,
        checksum: 0xFF,
        data: [
            0x80, 0x00, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        ],
    },
];

#[cfg(test)]
mod tests {
    use super::super::I2cBus;
    use super::*;

    /// The library's Wire-level helpers.
    struct Lib<'a> {
        bus: &'a mut I2cBus,
        now: u64,
    }

    impl Lib<'_> {
        fn i2c_read_bytes(&mut self, sub: u8, n: usize) -> Vec<u8> {
            assert!(self.bus.write_txn(self.now, ADDR, &[sub]));
            self.bus.read_txn(self.now, ADDR, n).unwrap()
        }
        fn i2c_write_bytes(&mut self, sub: u8, src: &[u8]) {
            let mut v = vec![sub];
            v.extend_from_slice(src);
            assert!(self.bus.write_txn(self.now, ADDR, &v));
        }
        fn read_word(&mut self, cmd: u8) -> u16 {
            let d = self.i2c_read_bytes(cmd, 2);
            u16::from_le_bytes([d[0], d[1]])
        }
        fn read_control_word(&mut self, f: u16) -> u16 {
            self.i2c_write_bytes(0, &f.to_le_bytes());
            self.read_word(0)
        }
        fn flags(&mut self) -> u16 {
            self.read_word(CMD_FLAGS)
        }
        /// readExtendedData(classID, offset)
        fn read_extended_data(&mut self, class: u8, offset: u8) -> u8 {
            self.i2c_write_bytes(EXT_CONTROL, &[0]);
            self.i2c_write_bytes(EXT_DATACLASS, &[class]);
            self.now += 200_000;
            self.i2c_write_bytes(EXT_DATABLOCK, &[offset / 32]);
            self.now += 5_000_000;
            self.i2c_read_bytes(EXT_BLOCKDATA + offset % 32, 1)[0]
        }
        fn design_energy_scale(&mut self) -> u8 {
            let s = self.read_extended_data(80, 81);
            if s > 0 { s } else { 1 }
        }
        fn current_polarity(&mut self) -> bool {
            self.read_extended_data(105, 5) & 0x80 != 0
        }
        /// goldenFileWriteBlock()
        fn golden_write_block(&mut self, class: u8, block: u8, data: &[u8; 32], csum: u8) -> bool {
            self.i2c_write_bytes(EXT_DATACLASS, &[class]);
            self.i2c_write_bytes(EXT_DATABLOCK, &[block]);
            let _ = self.i2c_read_bytes(EXT_BLOCKDATA, 32);
            self.i2c_write_bytes(EXT_BLOCKDATA, data);
            self.i2c_write_bytes(EXT_CHECKSUM, &[csum]);
            self.i2c_write_bytes(EXT_DATACLASS, &[class]);
            self.i2c_write_bytes(EXT_DATABLOCK, &[block]);
            self.i2c_read_bytes(EXT_CHECKSUM, 1)[0] == csum
        }
        /// applyGoldenFile()
        fn apply_golden_file(&mut self, two: bool) -> bool {
            self.i2c_write_bytes(0, &[0x01, 0x00]);
            if self.i2c_read_bytes(0, 2) != [0x27, 0x04] {
                return false;
            }
            self.i2c_write_bytes(0, &[0x02, 0x00]);
            assert_eq!(self.i2c_read_bytes(0, 2), [0x02, 0x02]);
            for k in [[0x14, 0x04], [0x72, 0x36], [0xFF, 0xFF], [0xFF, 0xFF], [0x00, 0x80], [0x00, 0x80]] {
                self.i2c_write_bytes(0, &k);
            }
            self.i2c_write_bytes(0, &[0x13, 0x00]);
            if self.flags() & FLAG_CFGUPMODE == 0 {
                return false;
            }
            for g in GOLDEN_BLOCKS.iter().filter(|g| g.cells.applies(two)) {
                if !self.golden_write_block(g.class, g.block, &g.data, g.checksum) {
                    return false;
                }
            }
            self.i2c_write_bytes(0, &[0x00, 0x00]);
            self.i2c_write_bytes(0, &[0x42, 0x00]);
            true
        }
        /// connectAndConfigure(); returns (initialized, configured).
        fn connect_and_configure(&mut self, one_cell: bool) -> (bool, bool) {
            let alive = self.read_control_word(CTL_DEVICE_TYPE) == DEVICE_TYPE;
            let mut configured = false;
            if alive {
                let expected = if one_cell { 1 } else { 10 };
                let needs =
                    self.flags() & FLAG_ITPOR != 0 || self.design_energy_scale() != expected || self.current_polarity();
                if needs {
                    configured = self.apply_golden_file(!one_cell);
                }
            }
            (alive && self.flags() & FLAG_ITPOR == 0, configured)
        }
    }

    fn bq(bus: &mut I2cBus) -> &mut Bq27427 {
        bus.device_mut::<Bq27427>().unwrap()
    }

    #[test]
    fn golden_table_checksums() {
        for g in &GOLDEN_BLOCKS {
            assert_eq!(Bq27427::checksum(&g.data), g.checksum, "{:#x}/{}", g.class, g.block);
        }
    }

    #[test]
    fn connect_and_configure_no_config_path_and_snapshot() {
        for (cells, scale, full) in [(1u8, 1u16, 6000u16), (2, 10, 1200)] {
            let mut bus = I2cBus::new();
            bus.add(Box::new(Bq27427::new(cells)));
            bq(&mut bus).set_battery(3950, false, 72);
            let mut lib = Lib { bus: &mut bus, now: 0 };
            assert_eq!(lib.read_control_word(CTL_FW_VERSION), FW_VERSION);
            assert_eq!(lib.read_control_word(CTL_STATUS) & STATUS_SS, 0);
            assert_eq!(lib.connect_and_configure(cells == 1), (true, false));
            // readSnapshot()
            assert_eq!(lib.design_energy_scale() as u16, scale);
            let flags = lib.flags();
            assert_eq!(flags & (FLAG_ITPOR | FLAG_CFGUPMODE | FLAG_CHG | FLAG_FC), 0);
            assert_ne!(flags & FLAG_DSG, 0);
            assert_eq!(lib.read_word(CMD_SOC), 72);
            assert_eq!(lib.read_word(CMD_VOLTAGE), 3950);
            assert_eq!(lib.read_word(CMD_AVG_CURRENT) as i16, -50);
            assert_eq!(lib.read_word(CMD_TEMP), 2982);
            assert_eq!(lib.read_word(CMD_FULL_CAPACITY), full);
            assert_eq!(lib.read_word(CMD_REM_CAPACITY) as u32, full as u32 * 72 / 100);
            assert_eq!(lib.read_word(CMD_SOH) & 0xff, 100);
            assert_eq!(bq(&mut bus).block_commits, 0);
        }
    }

    #[test]
    fn charging_flags() {
        let mut bus = I2cBus::new();
        bus.add(Box::new(Bq27427::new(1)));
        bq(&mut bus).set_battery(4150, true, 90);
        let mut lib = Lib { bus: &mut bus, now: 0 };
        let f = lib.flags();
        assert!(f & FLAG_CHG != 0 && f & (FLAG_FC | FLAG_DSG) == 0);
        assert!(lib.read_word(CMD_AVG_CURRENT) as i16 > 0);
        bq(&mut bus).set_battery(4200, true, 100);
        let mut lib = Lib { bus: &mut bus, now: 0 };
        assert!(lib.flags() & FLAG_FC != 0);
        bq(&mut bus).set_battery(3300, false, 2);
        let mut lib = Lib { bus: &mut bus, now: 0 };
        assert_eq!(lib.flags() & (FLAG_SOC1 | FLAG_SOCF | FLAG_DSG), FLAG_SOC1 | FLAG_SOCF | FLAG_DSG);
    }

    #[test]
    fn golden_file_path_from_por_with_checksum_echo() {
        for cells in [1u8, 2] {
            let mut bus = I2cBus::new();
            bus.add(Box::new(Bq27427::unconfigured()));
            let mut lib = Lib { bus: &mut bus, now: 0 };
            assert_ne!(lib.flags() & FLAG_ITPOR, 0);
            assert_eq!(lib.connect_and_configure(cells == 1), (true, true), "cells={cells}");
            let expected_blocks = GOLDEN_BLOCKS.iter().filter(|g| g.cells.applies(cells == 2)).count() as u32;
            assert_eq!(bq(&mut bus).block_commits, expected_blocks);
            for g in GOLDEN_BLOCKS.iter().filter(|g| g.cells.applies(cells == 2)) {
                assert_eq!(bq(&mut bus).block(g.class, g.block), Some(&g.data));
            }
            // SOFT_RESET left CONFIG UPDATE and cleared ITPOR; next boot takes the no-config path.
            let mut lib = Lib { bus: &mut bus, now: 0 };
            assert_eq!(lib.flags() & (FLAG_ITPOR | FLAG_CFGUPMODE), 0);
            assert_eq!(lib.connect_and_configure(cells == 1), (true, false));
        }
    }

    #[test]
    fn bad_checksum_is_not_committed_and_readback_mismatches() {
        let mut bus = I2cBus::new();
        bus.add(Box::new(Bq27427::new(1)));
        let mut lib = Lib { bus: &mut bus, now: 0 };
        lib.i2c_write_bytes(0, &[0x13, 0x00]);
        let g = &GOLDEN_BLOCKS[0];
        let mut data = g.data;
        data[5] = 0x77;
        // Wrong checksum for the new data: rejected, reselect reloads the old block,
        // whose checksum differs from what was written -> the library reports failure.
        let bad = Bq27427::checksum(&data) ^ 0x01;
        assert!(!lib.golden_write_block(g.class, g.block, &data, bad));
        assert_eq!(bq(&mut bus).block(g.class, g.block), Some(&g.data));
        // Same with the *old* checksum: also rejected, but the readback echoes the
        // stored block's checksum, so goldenFileWriteBlock() cannot tell.
        let mut lib = Lib { bus: &mut bus, now: 0 };
        assert!(lib.golden_write_block(g.class, g.block, &data, g.checksum));
        assert_eq!(bq(&mut bus).block(g.class, g.block), Some(&g.data));
    }

    #[test]
    fn write_extended_data_with_library_checksum() {
        // writeExtendedData(CC_CAL, 5, [0x80]): per-byte block write, recompute checksum.
        let mut bus = I2cBus::new();
        bus.add(Box::new(Bq27427::new(1)));
        let mut lib = Lib { bus: &mut bus, now: 0 };
        lib.i2c_write_bytes(0, &[0x13, 0x00]); // enterConfig
        assert_ne!(lib.flags() & FLAG_CFGUPMODE, 0);
        lib.i2c_write_bytes(EXT_CONTROL, &[0]);
        lib.i2c_write_bytes(EXT_DATACLASS, &[CLASS_CC_CAL]);
        lib.i2c_write_bytes(EXT_DATABLOCK, &[0]);
        let before: u8 = lib.i2c_read_bytes(EXT_BLOCKDATA, 32).iter().fold(0u8, |a, &b| a.wrapping_add(b));
        let _old = lib.i2c_read_bytes(EXT_CHECKSUM, 1)[0];
        lib.i2c_write_bytes(EXT_BLOCKDATA + 5, &[0x80]);
        let sum: u8 = lib.i2c_read_bytes(EXT_BLOCKDATA, 32).iter().fold(0u8, |a, &b| a.wrapping_add(b));
        assert_eq!(sum, before.wrapping_add(0x80));
        lib.i2c_write_bytes(EXT_CHECKSUM, &[255 - sum]);
        lib.i2c_write_bytes(0, &[0x42, 0x00]); // exitConfig
        assert!(lib.current_polarity());
        assert_eq!(lib.flags() & FLAG_CFGUPMODE, 0);
    }

    #[test]
    fn sealed_gauge_needs_unseal_keys() {
        let mut bus = I2cBus::new();
        let mut g = Bq27427::new(1);
        g.sealed = true;
        bus.add(Box::new(g));
        let mut lib = Lib { bus: &mut bus, now: 0 };
        assert_ne!(lib.read_control_word(CTL_STATUS) & STATUS_SS, 0);
        lib.i2c_write_bytes(0, &[0x13, 0x00]);
        assert_eq!(lib.flags() & FLAG_CFGUPMODE, 0, "no CONFIG UPDATE while sealed");
        // unseal(): readControlWord(0x8000) must be nonzero, twice.
        assert_ne!(lib.read_control_word(0x8000), 0);
        assert_ne!(lib.read_control_word(0x8000), 0);
        assert_eq!(lib.read_control_word(CTL_STATUS) & STATUS_SS, 0);
    }
}
