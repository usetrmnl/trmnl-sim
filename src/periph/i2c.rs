//! I2C master controller (ESP32-C3/S3 I2C_EXTn). Executes the hardware command
//! list (COMD0..7) when TRANS_START is set, turning it into bus events
//! (START / byte / STOP) delivered to the board's devices.

use std::collections::VecDeque;

use crate::board::Board;

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
    /// Controller number, passed to the board with every event.
    pub bus: u8,
    pub tx: VecDeque<u8>,
    pub rx: VecDeque<u8>,
    pub raw: u32,
    /// The next written byte is an address byte (after START / repeated START).
    expect_addr: bool,
    /// A transaction is open (START sent, no STOP yet).
    active: bool,
}

impl I2c {
    pub fn new(bus: u8) -> Self {
        I2c { bus, ..Default::default() }
    }

    /// Run the command list. `cmds` are COMD0..7; done bits are set as commands complete.
    pub fn execute(&mut self, now: u64, cmds: &mut [u32; 8], board: &mut dyn Board) {
        for c in cmds.iter_mut() {
            let op = (*c >> 11) & 7;
            let n = (*c & 0xff) as usize;
            let ack_check = *c & (1 << 8) != 0;
            let ack_value = *c & (1 << 10) != 0; // level the master sends after read bytes (1 = NACK)
            *c |= 1 << 31;
            match op {
                OP_RSTART => {
                    self.expect_addr = true;
                }
                OP_WRITE => {
                    for _ in 0..n {
                        let b = self.tx.pop_front().unwrap_or(0xff);
                        let ack = if self.expect_addr {
                            self.expect_addr = false;
                            self.active = true;
                            board.i2c_start(now, self.bus, b >> 1, b & 1 != 0)
                        } else {
                            board.i2c_write(now, self.bus, b)
                        };
                        if !ack && ack_check {
                            self.raw |= INT_NACK;
                            return;
                        }
                    }
                }
                OP_READ => {
                    for i in 0..n {
                        let last = i + 1 == n;
                        let master_acks = !(last && ack_value);
                        self.rx.push_back(board.i2c_read(now, self.bus, master_acks));
                    }
                }
                OP_STOP => {
                    if self.active {
                        board.i2c_stop(now, self.bus);
                        self.active = false;
                    }
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
