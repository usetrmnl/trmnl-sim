//! SYSTIMER (ESP32-C3, ESP32-S3, ...): two 52-bit counters at 16 MHz and three
//! comparators. Owns its whole 0x100-byte register window; the SoC forwards
//! accesses with the offset and the current virtual time.

pub const SYSTIMER_HZ: u64 = 16_000_000;

const CONF: u32 = 0x00;
const INT_ENA: u32 = 0x64;
const INT_RAW: u32 = 0x68;
const INT_CLR: u32 = 0x6C;
const INT_ST: u32 = 0x70;

#[derive(Debug)]
pub struct Systimer {
    regs: [u32; 64],
    /// Counter value = ticks(now) + offset (while running).
    offset: [i64; 2],
    /// Active comparator state: target tick, period (0 = one-shot), unit, armed.
    target: [u64; 3],
    period: [u64; 3],
    unit: [usize; 3],
    armed: [bool; 3],
    raw: u32,
}

impl Default for Systimer {
    fn default() -> Self {
        let mut regs = [0u32; 64];
        regs[0] = 1 << 31 | 1 << 30 | 1 << 29; // clk_en, unit0/1 work_en
        Systimer { regs, offset: [0; 2], target: [0; 3], period: [0; 3], unit: [0; 3], armed: [false; 3], raw: 0 }
    }
}

impl Systimer {
    fn ticks(now_ns: u64) -> u64 {
        (now_ns as u128 * SYSTIMER_HZ as u128 / 1_000_000_000) as u64
    }

    /// Counters restart from zero, as after any digital-domain reset.
    pub fn reset_counters(&mut self, now_ns: u64) {
        let t = Self::ticks(now_ns) as i64;
        self.offset = [-t, -t];
    }

    pub fn counter(&self, unit: usize, now_ns: u64) -> u64 {
        (Self::ticks(now_ns) as i64 + self.offset[unit]) as u64 & ((1 << 52) - 1)
    }

    fn reg(&self, off: u32) -> u32 {
        self.regs[(off / 4) as usize & 63]
    }

    /// Interrupt outputs: bit n = comparator n (raw & enabled).
    pub fn irq_lines(&self) -> u32 {
        self.raw & self.reg(INT_ENA)
    }

    pub fn read(&mut self, off: u32, now: u64) -> u32 {
        self.update(now);
        match off {
            0x04 | 0x08 => self.reg(off) | 1 << 29, // VALUE_VALID
            INT_RAW => self.raw,
            INT_ST => self.irq_lines(),
            0x74..=0x88 => {
                // REAL_TARGETn_LO/HI (S3): the active comparator value
                let i = ((off - 0x74) / 8) as usize;
                let t = self.target[i];
                if (off - 0x74).is_multiple_of(8) { t as u32 } else { (t >> 32) as u32 & 0xfffff }
            }
            _ => self.reg(off),
        }
    }

    /// Returns true if interrupt outputs may have changed.
    pub fn write(&mut self, off: u32, v: u32, now: u64) -> bool {
        self.regs[(off / 4) as usize & 63] = v;
        match off {
            0x04 | 0x08 => {
                if v & (1 << 30) != 0 {
                    // UPDATE: snapshot the counter into VALUE_HI/LO
                    let u = (off / 4 - 1) as usize;
                    let c = self.counter(u, now);
                    self.regs[0x40 / 4 + 2 * u] = (c >> 32) as u32;
                    self.regs[0x44 / 4 + 2 * u] = c as u32;
                }
                false
            }
            0x5C | 0x60 => {
                if v & 1 != 0 {
                    // LOAD: counter := LOAD_HI:LOAD_LO
                    let u = ((off - 0x5C) / 4) as usize;
                    let hi = self.reg(0x0C + 8 * u as u32) as u64 & 0xfffff;
                    let lo = self.reg(0x10 + 8 * u as u32) as u64;
                    self.offset[u] = (hi << 32 | lo) as i64 - Self::ticks(now) as i64;
                }
                false
            }
            0x34..=0x3C => {
                // TARGETn_CONF: period mode is live; switching it on starts periodic alarms now.
                let i = ((off - 0x34) / 4) as usize;
                self.unit[i] = (v >> 31) as usize;
                if v & (1 << 30) != 0 && self.period[i] == 0 {
                    let p = ((v & 0x3ff_ffff) as u64).max(1);
                    self.period[i] = p;
                    self.target[i] = self.counter(self.unit[i], now) + p;
                    self.armed[i] = true;
                } else if v & (1 << 30) == 0 {
                    self.period[i] = 0;
                }
                true
            }
            0x50..=0x58 => {
                if v & 1 != 0 {
                    self.load_comparator(((off - 0x50) / 4) as usize, now);
                }
                true
            }
            INT_CLR => {
                self.raw &= !v;
                true
            }
            CONF | INT_ENA => true,
            _ => false,
        }
    }

    fn load_comparator(&mut self, i: usize, now: u64) {
        let conf = self.reg(0x34 + 4 * i as u32);
        let hi = self.reg(0x1C + 8 * i as u32) as u64 & 0xfffff;
        let lo = self.reg(0x20 + 8 * i as u32) as u64;
        self.unit[i] = (conf >> 31) as usize;
        let period = (conf & 0x3ff_ffff) as u64;
        if conf & (1 << 30) != 0 {
            self.period[i] = period.max(1);
            self.target[i] = self.counter(self.unit[i], now) + period.max(1);
        } else {
            self.period[i] = 0;
            self.target[i] = hi << 32 | lo;
        }
        self.armed[i] = true;
    }

    /// Fire comparators whose time has come. Returns true if raw status changed.
    pub fn update(&mut self, now_ns: u64) -> bool {
        let before = self.raw;
        let work_en = self.reg(CONF);
        for i in 0..3 {
            if !self.armed[i] || work_en & (1 << (24 - i)) == 0 {
                continue;
            }
            let c = self.counter(self.unit[i], now_ns);
            if c >= self.target[i] {
                self.raw |= 1 << i;
                if let Some(behind) = (c - self.target[i]).checked_div(self.period[i]) {
                    self.target[i] += (behind + 1) * self.period[i];
                } else {
                    self.armed[i] = false;
                }
            }
        }
        self.raw != before
    }

    /// Absolute ns of the next comparator firing.
    pub fn next_ns(&self, now_ns: u64) -> Option<u64> {
        let work_en = self.reg(CONF);
        (0..3)
            .filter(|&i| self.armed[i] && work_en & (1 << (24 - i)) != 0)
            .map(|i| {
                let c = self.counter(self.unit[i], now_ns);
                let dt = self.target[i].saturating_sub(c);
                now_ns + (dt as u128 * 1_000_000_000 / SYSTIMER_HZ as u128) as u64 + 1
            })
            .min()
    }
}
