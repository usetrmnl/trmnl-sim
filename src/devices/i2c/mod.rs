//! I2C bus devices (device side). A SoC-side I2C controller (`periph::i2c`) or a
//! bit-banged GPIO master turns its activity into bus *events* — START (with
//! address), byte written, byte read, STOP — and feeds them to an [`I2cBus`],
//! which routes them to the addressed [`I2cDevice`].
//!
//! Event granularity (instead of whole write/read transactions) matters for
//! devices whose behaviour hangs off transaction boundaries, e.g. the IQS323
//! closing its communication window at the STOP that ends a transaction, or
//! register pointers that latch across STOP.
//!
//! [`BitBangI2cSlave`] decodes open-drain SDA/SCL pin levels into the same
//! events, for firmware that bit-bangs I2C (the TRMNL X deep-sleep wake stub).

// Nothing is wired into a board yet; the integrator will use these.
#![allow(dead_code)]

use std::any::Any;

use crate::savepoint::{StateReader, StateWriter};

pub mod axp2101;
pub mod bq27220;
pub mod bq27427;
pub mod env_sensors;
pub mod iqs323;
pub mod m5_py32;
pub mod m5ioe1;
pub mod tca9535;
pub mod tps65185;

/// One device on an I2C bus, driven by bus events. Times are virtual nanoseconds.
pub trait I2cDevice: Any + Send {
    /// 7-bit address.
    fn address(&self) -> u8;
    /// START (or repeated START) addressed to this device. Return ACK.
    fn start(&mut self, now_ns: u64, read: bool) -> bool;
    /// Master wrote a byte; return ACK.
    fn write(&mut self, now_ns: u64, byte: u8) -> bool;
    /// Master reads a byte (`ack` = master will ACK it, i.e. wants more).
    fn read(&mut self, now_ns: u64, ack: bool) -> u8;
    /// STOP condition (end of transaction).
    fn stop(&mut self, now_ns: u64);
    /// Time-driven behaviour (e.g. RDY windows, ATI completion).
    fn update(&mut self, _now_ns: u64) {}
    /// Earliest future time at which the device changes state on its own.
    fn next_event_ns(&self, _now_ns: u64) -> Option<u64> {
        None
    }
    /// Save point state (see `savepoint`): what the chip keeps while the board stays
    /// powered (registers, configuration). Never called with a transaction open.
    fn save_state(&self, _w: &mut StateWriter) {}
    /// Load `save_state` output; a transaction in progress is dropped.
    fn restore_state(&mut self, _r: &mut StateReader) -> anyhow::Result<()> {
        Ok(())
    }
}

/// Routes bus events to devices by address. Unknown addresses NACK and read 0xFF.
#[derive(Default)]
pub struct I2cBus {
    devices: Vec<Box<dyn I2cDevice>>,
    /// Device addressed by the most recent START / repeated START (None = NACKed).
    target: Option<usize>,
    /// Devices addressed since the last STOP; all of them see the STOP.
    participants: Vec<usize>,
    /// Log every event at `trace` level.
    pub trace: bool,
    /// Fault: addresses whose device is gone (never ACKs).
    pub absent: Vec<u8>,
}

impl I2cBus {
    pub fn new() -> Self {
        Self::default()
    }

    /// Attach a device; returns its index.
    pub fn add(&mut self, dev: Box<dyn I2cDevice>) -> usize {
        self.devices.push(dev);
        self.devices.len() - 1
    }

    /// Typed access to the first attached device of type `T`.
    pub fn device<T: I2cDevice>(&self) -> Option<&T> {
        self.devices.iter().find_map(|d| (d.as_ref() as &dyn Any).downcast_ref::<T>())
    }

    /// Typed mutable access to the first attached device of type `T`.
    pub fn device_mut<T: I2cDevice>(&mut self) -> Option<&mut T> {
        self.devices.iter_mut().find_map(|d| (d.as_mut() as &mut dyn Any).downcast_mut::<T>())
    }

    /// Is a device listening at `addr`?
    pub fn has(&self, addr: u8) -> bool {
        self.devices.iter().any(|d| d.address() == addr)
    }

    /// A transaction is open (START seen, no STOP yet).
    pub fn busy(&self) -> bool {
        !self.participants.is_empty() || self.target.is_some()
    }

    /// START or repeated START with a 7-bit address. Returns ACK.
    pub fn start(&mut self, now: u64, addr: u8, read: bool) -> bool {
        self.target = self.devices.iter().position(|d| d.address() == addr && !self.absent.contains(&addr));
        let ack = match self.target {
            Some(i) => {
                if !self.participants.contains(&i) {
                    self.participants.push(i);
                }
                self.devices[i].start(now, read)
            }
            None => false,
        };
        if self.trace {
            log::trace!("i2c {now}: START {addr:#04x} {} -> {}", if read { "R" } else { "W" }, ack_str(ack));
        }
        if !ack {
            self.target = None;
        }
        ack
    }

    /// Master wrote a data byte. Returns ACK.
    pub fn write(&mut self, now: u64, byte: u8) -> bool {
        let ack = match self.target {
            Some(i) => self.devices[i].write(now, byte),
            None => false,
        };
        if self.trace {
            log::trace!("i2c {now}: W {byte:#04x} -> {}", ack_str(ack));
        }
        ack
    }

    /// Master reads a data byte; `ack` = the master will ACK it (wants more).
    pub fn read(&mut self, now: u64, ack: bool) -> u8 {
        let b = match self.target {
            Some(i) => self.devices[i].read(now, ack),
            None => 0xff,
        };
        if self.trace {
            log::trace!("i2c {now}: R {b:#04x} ({})", if ack { "ACK" } else { "NACK" });
        }
        b
    }

    /// STOP: delivered to every device addressed since the previous STOP.
    pub fn stop(&mut self, now: u64) {
        if self.trace {
            log::trace!("i2c {now}: STOP");
        }
        for i in std::mem::take(&mut self.participants) {
            self.devices[i].stop(now);
        }
        self.target = None;
    }

    pub fn update(&mut self, now: u64) {
        for d in &mut self.devices {
            d.update(now);
        }
    }

    pub fn next_event_ns(&self, now: u64) -> Option<u64> {
        self.devices.iter().filter_map(|d| d.next_event_ns(now)).min()
    }

    /// Every device's save point state, in attachment order.
    pub fn save_state(&self, w: &mut StateWriter) {
        self.save_state_where(w, |_| true);
    }

    pub fn restore_state(&mut self, r: &mut StateReader) -> anyhow::Result<()> {
        self.restore_state_where(r, |_| true)
    }

    /// The save point state of the devices `keep` selects (the others, e.g. optional
    /// add-ons, may differ between saving and restoring).
    pub fn save_state_where(&self, w: &mut StateWriter, keep: impl Fn(&dyn I2cDevice) -> bool) {
        let kept: Vec<&Box<dyn I2cDevice>> = self.devices.iter().filter(|d| keep(d.as_ref())).collect();
        w.u32(kept.len() as u32);
        for d in kept {
            w.u8(d.address());
            w.section(|w| d.save_state(w));
        }
    }

    pub fn restore_state_where(
        &mut self,
        r: &mut StateReader,
        keep: impl Fn(&dyn I2cDevice) -> bool,
    ) -> anyhow::Result<()> {
        let n = self.devices.iter().filter(|d| keep(d.as_ref())).count();
        if r.u32()? as usize != n {
            anyhow::bail!("save point has a different set of I2C devices");
        }
        self.target = None;
        self.participants.clear();
        for d in self.devices.iter_mut().filter(|d| keep(d.as_ref())) {
            if r.u8()? != d.address() {
                anyhow::bail!("save point has a different set of I2C devices");
            }
            r.section(|r| d.restore_state(r))?;
        }
        Ok(())
    }

    // ---- Whole-transaction conveniences (tests, simple callers) ----

    /// START(W), bytes, STOP. Returns false if the address or any byte was NACKed
    /// (the transaction is still terminated with STOP, like Arduino Wire).
    pub fn write_txn(&mut self, now: u64, addr: u8, data: &[u8]) -> bool {
        let mut ok = self.start(now, addr, false);
        if ok {
            for &b in data {
                if !self.write(now, b) {
                    ok = false;
                    break;
                }
            }
        }
        self.stop(now);
        ok
    }

    /// START(R), `n` bytes (ACK all but the last), STOP. None if NACKed.
    pub fn read_txn(&mut self, now: u64, addr: u8, n: usize) -> Option<Vec<u8>> {
        let ok = self.start(now, addr, true);
        let out = ok.then(|| (0..n).map(|i| self.read(now, i + 1 < n)).collect());
        self.stop(now);
        out
    }

    /// START(W), `wr`, repeated START(R), `n` bytes, STOP.
    pub fn write_read(&mut self, now: u64, addr: u8, wr: &[u8], n: usize) -> Option<Vec<u8>> {
        let mut ok = self.start(now, addr, false);
        if ok {
            ok = wr.iter().all(|&b| self.write(now, b));
        }
        if ok {
            ok = self.start(now, addr, true);
        }
        let out = ok.then(|| (0..n).map(|i| self.read(now, i + 1 < n)).collect());
        self.stop(now);
        out
    }
}

fn ack_str(ack: bool) -> &'static str {
    if ack { "ACK" } else { "NACK" }
}

// ---------------------------------------------------------------------------
// Bit-banged slave-side decoder
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Bb {
    /// No transaction in progress (or not addressed to anyone we know).
    Idle,
    /// Shifting in the address byte.
    Addr,
    /// Shifting in a data byte (write direction).
    WriteData,
    /// Slave ACK/NACK bit on SDA; `next` is the state after the ACK clock.
    AckOut { next: AfterAck },
    /// Shifting out a data byte (read direction).
    ReadData,
    /// Master's ACK/NACK after a read byte.
    ReadAck,
    /// NACKed / done: wait for START or STOP.
    Ignore,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum AfterAck {
    Write,
    Read,
    Ignore,
}

/// Decodes a bit-banged I2C master from open-drain SDA/SCL levels and turns it
/// into [`I2cBus`] events. Call [`pins`](Self::pins) whenever the master's pin
/// drive changes; it returns whether the *slave side* is pulling SDA low (ACK
/// bits, zero data bits), which the SoC must see in its GPIO input register.
///
/// Slaves change SDA only right after SCL falling edges, as real devices do,
/// so the master sampling on SCL high always sees stable data. Reads fetch
/// each byte from the bus at the start of its first bit, before the master's
/// ACK/NACK for it is known, so the device's `read` is always passed `ack = true`.
pub struct BitBangI2cSlave {
    state: Bb,
    /// Line levels as of the previous call (after the slave's own drive).
    sda: bool,
    scl: bool,
    shift: u8,
    bits: u8,
    master_ack: bool,
    /// We are pulling SDA low.
    slave_low: bool,
    /// Bus events were produced since the last START (so a STOP must be delivered).
    in_txn: bool,
}

impl Default for BitBangI2cSlave {
    fn default() -> Self {
        Self::new()
    }
}

impl BitBangI2cSlave {
    pub fn new() -> Self {
        BitBangI2cSlave {
            state: Bb::Idle,
            sda: true,
            scl: true,
            shift: 0,
            bits: 0,
            master_ack: false,
            slave_low: false,
            in_txn: false,
        }
    }

    /// Whether the slave side currently pulls SDA low.
    pub fn sda_low(&self) -> bool {
        self.slave_low
    }

    /// New master drive state. `sda_master_low`: the master pulls SDA low (for an
    /// open-drain GPIO: output-enabled and output 0). `scl`: SCL line level (the
    /// master is the only SCL driver; no clock stretching). Returns whether the
    /// slave side pulls SDA low afterwards.
    pub fn pins(&mut self, bus: &mut I2cBus, now: u64, sda_master_low: bool, scl: bool) -> bool {
        let sda = !(sda_master_low || self.slave_low);
        let (prev_sda, prev_scl) = (self.sda, self.scl);
        if prev_scl && scl && sda != prev_sda {
            // SDA moved while SCL is high: START (falling) or STOP (rising).
            if !sda {
                self.on_start();
            } else {
                self.on_stop(bus, now);
            }
        } else if !prev_scl && scl {
            self.on_rise(sda);
        } else if prev_scl && !scl {
            self.on_fall(bus, now);
        }
        self.scl = scl;
        self.sda = !(sda_master_low || self.slave_low);
        self.slave_low
    }

    fn on_start(&mut self) {
        // Repeated START keeps the bus transaction open (the bus handles it).
        self.state = Bb::Addr;
        self.shift = 0;
        self.bits = 0;
        self.slave_low = false;
    }

    fn on_stop(&mut self, bus: &mut I2cBus, now: u64) {
        if self.in_txn {
            bus.stop(now);
            self.in_txn = false;
        }
        self.state = Bb::Idle;
        self.slave_low = false;
    }

    fn on_rise(&mut self, sda: bool) {
        match self.state {
            Bb::Addr | Bb::WriteData if self.bits < 8 => {
                self.shift = self.shift << 1 | sda as u8;
                self.bits += 1;
            }
            Bb::ReadData if self.bits < 8 => self.bits += 1,
            Bb::ReadAck => self.master_ack = !sda,
            _ => {}
        }
    }

    fn on_fall(&mut self, bus: &mut I2cBus, now: u64) {
        match self.state {
            Bb::Addr if self.bits == 8 => {
                let (addr, read) = (self.shift >> 1, self.shift & 1 != 0);
                self.in_txn = true;
                let ack = bus.start(now, addr, read);
                let next = match (ack, read) {
                    (false, _) => AfterAck::Ignore,
                    (true, true) => AfterAck::Read,
                    (true, false) => AfterAck::Write,
                };
                self.slave_low = ack;
                self.state = Bb::AckOut { next };
            }
            Bb::WriteData if self.bits == 8 => {
                let ack = bus.write(now, self.shift);
                self.slave_low = ack;
                self.state = Bb::AckOut { next: if ack { AfterAck::Write } else { AfterAck::Ignore } };
            }
            Bb::AckOut { next } => {
                self.slave_low = false;
                match next {
                    AfterAck::Write => {
                        self.state = Bb::WriteData;
                        self.shift = 0;
                        self.bits = 0;
                    }
                    AfterAck::Read => self.load_read_byte(bus, now),
                    AfterAck::Ignore => self.state = Bb::Ignore,
                }
            }
            Bb::ReadData => {
                if self.bits < 8 {
                    self.slave_low = self.shift >> (7 - self.bits) & 1 == 0;
                } else {
                    self.slave_low = false; // master drives the ACK bit
                    self.state = Bb::ReadAck;
                    self.master_ack = false;
                }
            }
            Bb::ReadAck => {
                if self.master_ack {
                    self.load_read_byte(bus, now);
                } else {
                    self.state = Bb::Ignore;
                }
            }
            _ => {}
        }
    }

    fn load_read_byte(&mut self, bus: &mut I2cBus, now: u64) {
        self.shift = bus.read(now, true);
        self.bits = 0;
        self.state = Bb::ReadData;
        self.slave_low = self.shift & 0x80 == 0;
    }
}

#[cfg(test)]
pub(crate) mod testutil {
    //! A bit-banging master that mirrors the firmware's wake stub
    //! (lib/trmnl_x/src/rtc_wake_stub_i2c.cpp) step for step, including its delays.
    use super::*;

    pub struct BbMaster<'a> {
        pub slave: &'a mut BitBangI2cSlave,
        pub bus: &'a mut I2cBus,
        pub now: u64,
        sda_low: bool,
        scl: bool,
    }

    impl<'a> BbMaster<'a> {
        pub fn new(slave: &'a mut BitBangI2cSlave, bus: &'a mut I2cBus, now: u64) -> Self {
            let mut m = BbMaster { slave, bus, now, sda_low: false, scl: true };
            m.apply();
            m
        }
        fn apply(&mut self) {
            self.slave.pins(self.bus, self.now, self.sda_low, self.scl);
        }
        fn delay_us(&mut self, us: u64) {
            self.now += us * 1000;
            self.bus.update(self.now);
        }
        fn sda(&mut self, high: bool) {
            self.sda_low = !high;
            self.apply();
        }
        fn scl(&mut self, high: bool) {
            self.scl = high;
            self.apply();
        }
        fn sda_read(&self) -> bool {
            !(self.sda_low || self.slave.sda_low())
        }
        pub fn start(&mut self) {
            self.sda(true);
            self.scl(true);
            self.delay_us(10);
            self.sda(false);
            self.delay_us(10);
            self.scl(false);
            self.delay_us(10);
        }
        pub fn stop(&mut self) {
            self.sda(false);
            self.scl(false);
            self.delay_us(10);
            self.scl(true);
            self.delay_us(10);
            self.sda(true);
            self.delay_us(10);
        }
        pub fn write_byte(&mut self, data: u8) -> bool {
            for i in (0..8).rev() {
                self.scl(false);
                self.delay_us(5);
                self.sda(data >> i & 1 != 0);
                self.delay_us(5);
                self.scl(true);
                self.delay_us(10);
            }
            self.scl(false);
            self.delay_us(5);
            self.sda(true);
            self.delay_us(5);
            self.scl(true);
            self.delay_us(5);
            let ack = !self.sda_read();
            self.delay_us(5);
            self.scl(false);
            self.delay_us(10);
            ack
        }
        pub fn read_byte(&mut self, send_ack: bool) -> u8 {
            let mut data = 0u8;
            self.sda(true);
            for i in (0..8).rev() {
                self.scl(false);
                self.delay_us(10);
                self.scl(true);
                self.delay_us(5);
                if self.sda_read() {
                    data |= 1 << i;
                }
                self.delay_us(5);
            }
            self.scl(false);
            self.delay_us(5);
            self.sda(!send_ack);
            self.delay_us(5);
            self.scl(true);
            self.delay_us(10);
            self.scl(false);
            self.delay_us(10);
            data
        }
        /// `wake_stub_i2c_read()`: returns None where the stub would bail out.
        pub fn read_reg(&mut self, addr7: u8, reg: u8, len: usize) -> Option<Vec<u8>> {
            self.start();
            if !self.write_byte(addr7 << 1) {
                self.stop();
                return None;
            }
            if !self.write_byte(reg) {
                self.stop();
                return None;
            }
            self.start();
            if !self.write_byte(addr7 << 1 | 1) {
                self.stop();
                return None;
            }
            let out = (0..len).map(|i| self.read_byte(i < len - 1)).collect();
            self.stop();
            Some(out)
        }
        /// `wake_stub_i2c_write()`.
        pub fn write(&mut self, addr7: u8, data: &[u8]) -> bool {
            self.start();
            if !self.write_byte(addr7 << 1) {
                self.stop();
                return false;
            }
            let mut ok = true;
            for &b in data {
                if !self.write_byte(b) {
                    ok = false;
                    break;
                }
            }
            self.stop();
            ok
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testutil::BbMaster;
    use super::*;

    /// Records every event; register file with a latching pointer.
    #[derive(Default)]
    struct Probe {
        addr: u8,
        events: Vec<String>,
        regs: [u8; 16],
        ptr: usize,
        first: bool,
    }

    impl I2cDevice for Probe {
        fn address(&self) -> u8 {
            self.addr
        }
        fn start(&mut self, _: u64, read: bool) -> bool {
            self.events.push(format!("S{}", if read { 'R' } else { 'W' }));
            self.first = !read;
            true
        }
        fn write(&mut self, _: u64, b: u8) -> bool {
            self.events.push(format!("W{b:02x}"));
            if self.first {
                self.ptr = b as usize & 15;
                self.first = false;
            } else {
                self.regs[self.ptr] = b;
                self.ptr = (self.ptr + 1) & 15;
            }
            true
        }
        fn read(&mut self, _: u64, ack: bool) -> u8 {
            let b = self.regs[self.ptr];
            self.events.push(format!("R{b:02x}{}", if ack { "+" } else { "-" }));
            self.ptr = (self.ptr + 1) & 15;
            b
        }
        fn stop(&mut self, _: u64) {
            self.events.push("P".into());
        }
    }

    fn bus_with_probe() -> I2cBus {
        let mut bus = I2cBus::new();
        bus.add(Box::new(Probe { addr: 0x44, ..Default::default() }));
        bus
    }

    #[test]
    fn bus_routes_and_nacks_unknown() {
        let mut bus = bus_with_probe();
        assert!(!bus.start(0, 0x18, false)); // BMA530 is not modelled
        assert!(!bus.write(0, 0x00));
        assert_eq!(bus.read(0, false), 0xff);
        bus.stop(0);
        assert!(bus.write_txn(0, 0x44, &[0x02, 0xAA, 0xBB]));
        assert_eq!(bus.write_read(0, 0x44, &[0x02], 2), Some(vec![0xAA, 0xBB]));
        let p = bus.device::<Probe>().unwrap();
        assert_eq!(p.events.join(" "), "SW W02 Waa Wbb P SW W02 SR Raa+ Rbb- P");
        assert!(bus.device_mut::<Probe>().is_some());
    }

    #[test]
    fn bitbang_write_then_repeated_start_read() {
        let mut bus = bus_with_probe();
        let mut slave = BitBangI2cSlave::new();
        let mut m = BbMaster::new(&mut slave, &mut bus, 0);
        assert!(m.write(0x44, &[0x03, 0x5A, 0xC3, 0x00, 0xFF]));
        assert_eq!(m.read_reg(0x44, 0x03, 4), Some(vec![0x5A, 0xC3, 0x00, 0xFF]));
        // Unknown address: NACK, the master bails out with STOP.
        assert_eq!(m.read_reg(0x18, 0x00, 1), None);
        assert!(!slave.sda_low());
        let p = bus.device::<Probe>().unwrap();
        assert_eq!(p.events.join(" "), "SW W03 W5a Wc3 W00 Wff P SW W03 SR R5a+ Rc3+ R00+ Rff+ P");
    }

    #[test]
    fn bitbang_slave_drives_ack_and_data_bits() {
        let mut bus = bus_with_probe();
        bus.write_txn(0, 0x44, &[0x00, 0b1010_0110]);
        bus.write_txn(0, 0x44, &[0x00]); // pointer back to 0
        let mut s = BitBangI2cSlave::new();
        let mut t = 0;
        let mut pins = |s: &mut BitBangI2cSlave, bus: &mut I2cBus, sda_low: bool, scl: bool| {
            t += 1000;
            s.pins(bus, t, sda_low, scl)
        };
        // START
        pins(&mut s, &mut bus, true, true);
        pins(&mut s, &mut bus, true, false);
        // Address 0x44 read = 0x89
        for i in (0..8).rev() {
            let bit = 0x89u8 >> i & 1 != 0;
            pins(&mut s, &mut bus, !bit, false);
            pins(&mut s, &mut bus, !bit, true);
            pins(&mut s, &mut bus, !bit, false);
        }
        // ACK slot: master released SDA, slave must hold it low through the high phase.
        assert!(pins(&mut s, &mut bus, false, false));
        assert!(pins(&mut s, &mut bus, false, true));
        // Falling edge after ACK: slave presents bit 7 of 0xA6 (1 = released).
        let mut got = 0u8;
        pins(&mut s, &mut bus, false, false);
        for i in (0..8).rev() {
            if !pins(&mut s, &mut bus, false, true) {
                got |= 1 << i; // sampled while SCL is high
            }
            pins(&mut s, &mut bus, false, false);
        }
        assert_eq!(got, 0b1010_0110);
        assert!(!s.sda_low(), "slave releases SDA for the master's ACK bit");
        // Master NACK, then STOP.
        pins(&mut s, &mut bus, false, true);
        pins(&mut s, &mut bus, true, false);
        pins(&mut s, &mut bus, true, true);
        pins(&mut s, &mut bus, false, true);
        let p = bus.device::<Probe>().unwrap();
        assert!(p.events.join(" ").ends_with("SR Ra6+ P"), "{:?}", p.events);
    }
}
