//! I2C master controller (I2C_EXT0). Executes the hardware command list
//! (COMD0..7) against the board's I2C devices when TRANS_START is set.

use std::collections::VecDeque;

use crate::board::Board;

pub const BASE: u32 = 0x6001_3000;

const OP_WRITE: u32 = 1;
const OP_STOP: u32 = 2;
const OP_READ: u32 = 3;
const OP_END: u32 = 4;
const OP_RSTART: u32 = 6;

const INT_END_DETECT: u32 = 1 << 3;
const INT_TRANS_COMPLETE: u32 = 1 << 7;
const INT_NACK: u32 = 1 << 10;

#[derive(Default)]
pub struct I2c {
    pub tx: VecDeque<u8>,
    pub rx: VecDeque<u8>,
    pub raw: u32,
    /// Current transaction: 7-bit address and direction once the address byte went out.
    addr: Option<(u8, bool)>,
    /// Bytes written to the addressed device, delivered at STOP / repeated START.
    pending_write: Vec<u8>,
    expect_addr: bool,
}

impl I2c {
    fn flush_write(&mut self, board: &mut dyn Board) {
        if let Some((addr, false)) = self.addr
            && !self.pending_write.is_empty()
        {
            board.i2c_write(addr, &self.pending_write);
        }
        self.pending_write.clear();
    }

    /// Run the command list. `cmds` are COMD0..7; returns the updated command words.
    pub fn execute(&mut self, cmds: &mut [u32; 8], board: &mut dyn Board) {
        for c in cmds.iter_mut() {
            let op = (*c >> 11) & 7;
            let n = (*c & 0xff) as usize;
            let ack_check = *c & (1 << 8) != 0;
            *c |= 1 << 31;
            match op {
                OP_RSTART => {
                    self.flush_write(board);
                    self.addr = None;
                    self.expect_addr = true;
                }
                OP_WRITE => {
                    for _ in 0..n {
                        let b = self.tx.pop_front().unwrap_or(0xff);
                        if self.expect_addr {
                            self.expect_addr = false;
                            let (addr, read) = (b >> 1, b & 1 != 0);
                            let ack = board.i2c_probe(addr);
                            self.addr = Some((addr, read));
                            if !ack && ack_check {
                                self.addr = None;
                                self.raw |= INT_NACK;
                                return;
                            }
                        } else {
                            self.pending_write.push(b);
                        }
                    }
                }
                OP_READ => {
                    let data = match self.addr {
                        Some((addr, true)) => board.i2c_read(addr, n),
                        _ => None,
                    };
                    let data = data.unwrap_or_else(|| vec![0xff; n]);
                    self.rx.extend(data.into_iter().take(n));
                }
                OP_STOP => {
                    self.flush_write(board);
                    self.addr = None;
                    self.raw |= INT_TRANS_COMPLETE;
                    return;
                }
                OP_END => {
                    self.raw |= INT_END_DETECT;
                    return;
                }
                _ => {}
            }
        }
    }
}
