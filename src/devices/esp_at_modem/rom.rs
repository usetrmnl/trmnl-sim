//! ESP32-C5 ROM serial bootloader (UART download mode), as much as esp-serial-flasher needs to
//! detect the chip, size the flash and write an image without a stub.
//!
//! Wire format (SLIP framed, 0xC0 delimiters, DB DC / DB DD escapes):
//! * request:  `dir=0x00, cmd, size: u16, checksum: u32, data[size]`
//! * response: `dir=0x01, cmd, size: u16, value: u32, data..., status[4]`
//!   where `status = {failed, error, 0, 0}` and `size = data.len() + 4`. esp-serial-flasher reads
//!   `status` from the last two bytes of its (truncated) receive buffer, which for the ROM's
//!   4-byte status lands exactly on `{failed, error}`.

use std::collections::HashMap;

use super::{ModemStats, ModemTiming};

pub(super) const SYNC: u8 = 0x08;
pub(super) const FLASH_BEGIN: u8 = 0x02;
pub(super) const FLASH_DATA: u8 = 0x03;
pub(super) const FLASH_END: u8 = 0x04;
pub(super) const MEM_BEGIN: u8 = 0x05;
pub(super) const MEM_END: u8 = 0x06;
pub(super) const MEM_DATA: u8 = 0x07;
pub(super) const WRITE_REG: u8 = 0x09;
pub(super) const READ_REG: u8 = 0x0a;
pub(super) const SPI_SET_PARAMS: u8 = 0x0b;
pub(super) const SPI_ATTACH: u8 = 0x0d;
pub(super) const CHANGE_BAUDRATE: u8 = 0x0f;
pub(super) const GET_SECURITY_INFO: u8 = 0x14;

pub(super) const ERR_INVALID_COMMAND: u8 = 0x05;
pub(super) const ERR_COMMAND_FAILED: u8 = 0x06;
pub(super) const ERR_INVALID_CRC: u8 = 0x07;

pub(super) const SPI_BASE: u32 = 0x6000_3000;
pub(super) const SPI_CMD: u32 = SPI_BASE;
pub(super) const SPI_USR2: u32 = SPI_BASE + 0x20;
pub(super) const SPI_W0: u32 = SPI_BASE + 0x58;
const SPI_CMD_USR: u32 = 1 << 18;
pub(super) const CHIP_DETECT_MAGIC_REG: u32 = 0x4000_1000;
pub(super) const CHIP_MAGIC: u32 = 0x1101_406f;
pub(super) const EFUSE_MAC0: u32 = 0x600b_4800 + 0x44;
pub(super) const CHIP_ID: u32 = 23;

const MAX_FRAME: usize = 64 * 1024;

/// SLIP receive state machine. Bytes outside frames (e.g. text) are dropped.
#[derive(Default)]
pub(super) struct Slip {
    in_frame: bool,
    esc: bool,
    buf: Vec<u8>,
}

impl Slip {
    pub fn feed(&mut self, data: &[u8]) -> Vec<Vec<u8>> {
        let mut frames = Vec::new();
        for &b in data {
            if b == 0xc0 {
                // A delimiter both ends a frame and may start the next one.
                if self.in_frame && !self.buf.is_empty() {
                    frames.push(std::mem::take(&mut self.buf));
                }
                self.in_frame = true;
                self.esc = false;
                self.buf.clear();
                continue;
            }
            if !self.in_frame {
                continue;
            }
            if self.esc {
                self.esc = false;
                match b {
                    0xdc => self.buf.push(0xc0),
                    0xdd => self.buf.push(0xdb),
                    _ => {
                        // Protocol violation: drop the frame.
                        self.in_frame = false;
                        self.buf.clear();
                    }
                }
            } else if b == 0xdb {
                self.esc = true;
            } else {
                self.buf.push(b);
            }
            if self.buf.len() > MAX_FRAME {
                self.in_frame = false;
                self.buf.clear();
            }
        }
        frames
    }
}

pub(super) fn slip_encode(p: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(p.len() + 4);
    out.push(0xc0);
    for &b in p {
        match b {
            0xc0 => out.extend_from_slice(&[0xdb, 0xdc]),
            0xdb => out.extend_from_slice(&[0xdb, 0xdd]),
            _ => out.push(b),
        }
    }
    out.push(0xc0);
    out
}

/// Raw (unframed) ROM response packet.
pub(super) fn response(cmd: u8, value: u32, data: &[u8], error: Option<u8>) -> Vec<u8> {
    let mut p = Vec::with_capacity(12 + data.len());
    p.push(0x01);
    p.push(cmd);
    p.extend_from_slice(&((data.len() + 4) as u16).to_le_bytes());
    p.extend_from_slice(&value.to_le_bytes());
    p.extend_from_slice(data);
    p.extend_from_slice(&[u8::from(error.is_some()), error.unwrap_or(0), 0, 0]);
    p
}

/// `0xEF ^ xor(data)`, the ROM's data-packet checksum.
pub(super) fn checksum(data: &[u8]) -> u32 {
    u32::from(data.iter().fold(0xefu8, |a, &b| a ^ b))
}

fn word(d: &[u8], i: usize) -> Option<u32> {
    d.get(i * 4..i * 4 + 4).map(|b| u32::from_le_bytes(b.try_into().unwrap()))
}

struct FlashWrite {
    offset: u32,
    packet_size: u32,
    written: u32,
}

/// Everything outside the loader the command handler touches.
pub(super) struct RomCtx<'a> {
    pub flash: &'a mut Option<Vec<u8>>,
    pub flash_size_id: u8,
    pub mac: [u8; 6],
    pub stats: &'a mut ModemStats,
    pub timing: &'a ModemTiming,
}

pub(super) struct RomReply {
    /// SLIP-encoded response frame(s).
    pub bytes: Vec<u8>,
    /// Time the ROM spends on the command before replying.
    pub cost_ns: u64,
    /// FLASH_END asked to run the flashed application.
    pub reboot: bool,
}

#[derive(Default)]
pub(super) struct RomLoader {
    pub slip: Slip,
    regs: HashMap<u32, u32>,
    flash_write: Option<FlashWrite>,
    mem_active: bool,
}

impl RomLoader {
    fn read_reg(&self, addr: u32, mac: &[u8; 6]) -> u32 {
        match addr {
            CHIP_DETECT_MAGIC_REG => CHIP_MAGIC,
            SPI_CMD => self.regs.get(&addr).copied().unwrap_or(0) & !SPI_CMD_USR,
            EFUSE_MAC0 => u32::from_be_bytes([mac[2], mac[3], mac[4], mac[5]]),
            a if a == EFUSE_MAC0 + 4 => u32::from(mac[0]) << 8 | u32::from(mac[1]),
            _ => self.regs.get(&addr).copied().unwrap_or(0),
        }
    }

    fn write_reg(&mut self, addr: u32, value: u32, mask: u32, size_id: u8) {
        let old = self.regs.get(&addr).copied().unwrap_or(0);
        let new = (old & !mask) | (value & mask);
        self.regs.insert(addr, new);
        if addr == SPI_CMD && new & SPI_CMD_USR != 0 {
            // A SPI "user" command to the flash chip; it completes immediately.
            let opcode = self.regs.get(&SPI_USR2).copied().unwrap_or(0) & 0xff;
            if opcode == 0x9f {
                // READ_ID: manufacturer (GigaDevice 0xC8), memory type 0x40, capacity code.
                self.regs.insert(SPI_W0, u32::from(size_id) << 16 | 0x40 << 8 | 0xc8);
            }
            self.regs.insert(addr, new & !SPI_CMD_USR);
        }
    }

    /// Handle one de-SLIPped request. `None` = not a request (ignored, like the ROM does).
    pub fn handle(&mut self, pkt: &[u8], ctx: RomCtx<'_>) -> Option<RomReply> {
        if pkt.len() < 8 || pkt[0] != 0x00 {
            return None;
        }
        let cmd = pkt[1];
        let size = u16::from_le_bytes([pkt[2], pkt[3]]) as usize;
        let chk = u32::from_le_bytes(pkt[4..8].try_into().unwrap());
        let data = &pkt[8..];
        ctx.stats.rom_packets += 1;
        let t = ctx.timing;
        let mut cost = t.rom_cmd_ns;
        let mut reboot = false;
        let one = |value: u32, data: &[u8], err: Option<u8>| slip_encode(&response(cmd, value, data, err));
        if data.len() != size {
            return Some(RomReply { bytes: one(0, &[], Some(ERR_INVALID_COMMAND)), cost_ns: cost, reboot });
        }
        let bytes = match cmd {
            SYNC => {
                // The ROM answers every SYNC with a burst of 8 responses; the flasher reads all 8.
                let r = one(0, &[], None);
                r.repeat(8)
            }
            GET_SECURITY_INFO => {
                let mut d = Vec::with_capacity(20);
                d.extend_from_slice(&0u32.to_le_bytes()); // flags
                d.push(0); // flash_crypt_cnt
                d.extend_from_slice(&[0; 7]); // key_purposes
                d.extend_from_slice(&CHIP_ID.to_le_bytes());
                d.extend_from_slice(&0u32.to_le_bytes()); // eco_version
                one(0, &d, None)
            }
            READ_REG => match word(data, 0) {
                Some(a) => one(self.read_reg(a, &ctx.mac), &[], None),
                None => one(0, &[], Some(ERR_INVALID_COMMAND)),
            },
            WRITE_REG => match (word(data, 0), word(data, 1), word(data, 2)) {
                (Some(a), Some(v), Some(m)) => {
                    self.write_reg(a, v, m, ctx.flash_size_id);
                    one(0, &[], None)
                }
                _ => one(0, &[], Some(ERR_INVALID_COMMAND)),
            },
            SPI_ATTACH | SPI_SET_PARAMS | CHANGE_BAUDRATE => one(0, &[], None),
            FLASH_BEGIN => match (word(data, 0), word(data, 1), word(data, 2), word(data, 3)) {
                (Some(erase_size), Some(_count), Some(packet_size), Some(offset)) if packet_size > 0 => {
                    let flash_len = 1usize << ctx.flash_size_id.min(28);
                    let flash = ctx.flash.get_or_insert_with(|| vec![0xff; flash_len]);
                    let start = (offset as usize).min(flash.len());
                    let end = (offset as usize).saturating_add(erase_size as usize).min(flash.len());
                    flash[start..end].fill(0xff);
                    // The ROM erases 64 KiB blocks where it can, 4 KiB sectors otherwise.
                    let sectors = u64::from(erase_size).div_ceil(4096);
                    cost += sectors / 16 * t.rom_erase_block_ns + sectors % 16 * t.rom_erase_sector_ns;
                    self.flash_write = Some(FlashWrite { offset, packet_size, written: 0 });
                    ctx.stats.flash_begins += 1;
                    one(0, &[], None)
                }
                _ => one(0, &[], Some(ERR_INVALID_COMMAND)),
            },
            FLASH_DATA | MEM_DATA => {
                let hdr_ok = data.len() >= 16 && word(data, 0) == Some((data.len() - 16) as u32);
                let payload = data.get(16..).unwrap_or(&[]);
                if !hdr_ok {
                    one(0, &[], Some(ERR_INVALID_COMMAND))
                } else if checksum(payload) != chk {
                    ctx.stats.rom_checksum_errors += 1;
                    one(0, &[], Some(ERR_INVALID_CRC))
                } else if cmd == MEM_DATA {
                    if self.mem_active { one(0, &[], None) } else { one(0, &[], Some(ERR_COMMAND_FAILED)) }
                } else if let Some(fw) = &mut self.flash_write {
                    let flash = ctx.flash.get_or_insert_with(|| vec![0xff; 1usize << ctx.flash_size_id.min(28)]);
                    let addr = fw.offset as usize + fw.written as usize * fw.packet_size as usize;
                    let n = payload.len().min(flash.len().saturating_sub(addr));
                    // NOR programming only clears bits.
                    let start = addr.min(flash.len());
                    for (d, s) in flash[start..][..n].iter_mut().zip(payload) {
                        *d &= *s;
                    }
                    fw.written += 1;
                    ctx.stats.flash_data_packets += 1;
                    ctx.stats.flash_bytes_written += payload.len() as u64;
                    cost += t.rom_write_ns_per_kb * payload.len() as u64 / 1024;
                    one(0, &[], None)
                } else {
                    one(0, &[], Some(ERR_COMMAND_FAILED))
                }
            }
            FLASH_END => {
                self.flash_write = None;
                ctx.stats.flash_ends += 1;
                // stay_in_loader == 0: leave the loader and run the application.
                reboot = word(data, 0) == Some(0);
                one(0, &[], None)
            }
            MEM_BEGIN => {
                self.mem_active = true;
                one(0, &[], None)
            }
            MEM_END => {
                // We cannot run uploaded code (e.g. a flasher stub); just acknowledge.
                self.mem_active = false;
                one(0, &[], None)
            }
            _ => {
                ctx.stats.rom_unknown_commands += 1;
                one(0, &[], Some(ERR_INVALID_COMMAND))
            }
        };
        Some(RomReply { bytes, cost_ns: cost, reboot })
    }
}
