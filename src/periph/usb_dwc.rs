//! Synopsys DesignWare USB 2.0 OTG core (DWC2, v4.00a) in host mode, as on the ESP32-S3
//! (USB_DWC at 0x6008_0000): one full-speed root port, eight host channels, and the
//! scatter/gather DMA mode ESP-IDF's HCD uses (hcd_dwc.c over hal/usb_dwc_hal.c). Each
//! channel walks a list of 64-bit QTDs (status word, buffer pointer) from HCDMAn; the
//! simulated device behind the port answers each transfer at once, or NAKs it, in which case
//! the channel keeps retrying (on every poll) until the device has data or the host halts it.
//!
//! Modelled: the core and hardware-config registers the HAL reads, core soft reset and FIFO
//! flushes (complete at once), the interrupt tree (HCINTn → HAINT → GINTSTS.HCHINT, HPRT →
//! GINTSTS.PRTINT, DISCONNINT) behind GAHBCFG's global mask, the root port (power,
//! connect detect, reset, enable, disable), and control, bulk and interrupt channels. Not
//! modelled: device mode, slave (FIFO) mode, isochronous transfers, split transactions, the
//! periodic frame list's timing (an interrupt channel is polled like a bulk one), suspend and
//! resume signalling, and the packet-level errors of a real cable.

use crate::devices::usb::{Reply, Setup, UsbDevice};

pub const SIZE: u32 = 0x1000;

const GAHBCFG: u32 = 0x008;
const GUSBCFG: u32 = 0x00C;
const GRSTCTL: u32 = 0x010;
const GINTSTS: u32 = 0x014;
const GINTMSK: u32 = 0x018;
const GSNPSID: u32 = 0x040;
const GHWCFG1: u32 = 0x044;
const GHWCFG2: u32 = 0x048;
const GHWCFG3: u32 = 0x04C;
const GHWCFG4: u32 = 0x050;
const HFNUM: u32 = 0x408;
const HAINT: u32 = 0x414;
const HAINTMSK: u32 = 0x418;
const HPRT: u32 = 0x440;
const CHANNELS_BASE: u32 = 0x500;
const CHANNEL_STRIDE: u32 = 0x20;
const CHANNELS: usize = 8;

/// The ESP32-S3's values (TRM, USB OTG chapter; TinyUSB's dwc2_esp32): v4.00a, internal DMA,
/// a full-speed PHY only, 8 host channels, a 200-word data FIFO.
const SNPSID: u32 = 0x4F54_400A;
const HWCFG1: u32 = 0x0000_0000;
const HWCFG2: u32 = 0x224D_D930;
const HWCFG3: u32 = 0x00C8_04B5;
const HWCFG4: u32 = 0xD3F0_A030;

// GINTSTS
const INT_CURMOD: u32 = 1 << 0;
const INT_PRTINT: u32 = 1 << 24;
const INT_HCHINT: u32 = 1 << 25;
const INT_DISCONN: u32 = 1 << 29;

// HPRT
const PRT_CONNSTS: u32 = 1 << 0;
const PRT_CONNDET: u32 = 1 << 1;
const PRT_ENA: u32 = 1 << 2;
const PRT_ENCHNG: u32 = 1 << 3;
const PRT_RES: u32 = 1 << 6;
const PRT_SUSP: u32 = 1 << 7;
const PRT_RST: u32 = 1 << 8;
const PRT_PWR: u32 = 1 << 12;
/// prtspd 1: full speed.
const PRT_SPD_FS: u32 = 1 << 17;
/// prtlnsts: D+ high, the full-speed idle (J) state.
const PRT_LNSTS_J: u32 = 1 << 10;

// HCCHARn
const CHAR_CHDIS: u32 = 1 << 30;
const CHAR_CHENA: u32 = 1 << 31;

// HCINTn
const CH_XFERCOMPL: u32 = 1 << 0;
const CH_CHHLTD: u32 = 1 << 1;
const CH_STALL: u32 = 1 << 3;
const CH_XACTERR: u32 = 1 << 7;
const CH_BNAINTR: u32 = 1 << 11;
const CH_XCS_XACT_ERR: u32 = 1 << 12;

// QTD status word (usb_dwc_ll_dma_qtd_t, non-isochronous)
const QTD_SIZE: u32 = 0x1_FFFF;
const QTD_SETUP: u32 = 1 << 24;
const QTD_IOC: u32 = 1 << 25;
const QTD_EOL: u32 = 1 << 26;
const QTD_STS_SHIFT: u32 = 28;
const QTD_STS_PKTERR: u32 = 1;
const QTD_ACTIVE: u32 = 1 << 31;

/// Memory the controller reads and writes by DMA (QTD lists and transfer buffers).
pub trait Dma {
    fn dma_read(&self, addr: u32, len: usize) -> Option<Vec<u8>>;
    fn dma_write(&mut self, addr: u32, data: &[u8]) -> bool;
}

#[derive(Default, Clone, Copy)]
struct Channel {
    enabled: bool,
    hcint: u32,
    /// The device NAKed the current QTD: try it again on the next poll.
    waiting: bool,
}

/// The control request in progress on endpoint 0.
#[derive(Clone, Copy)]
struct Control {
    setup: Setup,
    /// The device has carried it out (at the data stage).
    done: bool,
}

enum Outcome {
    /// The QTD completed with this many bytes not transferred.
    Done(u32),
    Nak,
    Stall,
    /// Nobody answered (no device at the address, port off).
    NoResponse,
}

pub struct UsbDwc {
    regs: Box<[u32; (SIZE / 4) as usize]>,
    chans: [Channel; CHANNELS],
    /// W1C bits of GINTSTS that are latched (DISCONNINT); the rest are derived.
    gintsts: u32,
    powered: bool,
    connected: bool,
    enabled: bool,
    conndet: bool,
    enchng: bool,
    control: Option<Control>,
    device: Option<Box<dyn UsbDevice>>,
    messages: Vec<String>,
}

impl Default for UsbDwc {
    fn default() -> Self {
        UsbDwc {
            regs: Box::new([0; (SIZE / 4) as usize]),
            chans: [Channel::default(); CHANNELS],
            gintsts: 0,
            powered: false,
            connected: false,
            enabled: false,
            conndet: false,
            enchng: false,
            control: None,
            device: None,
            messages: Vec::new(),
        }
    }
}

impl UsbDwc {
    /// Chip reset: the core's registers are back at their reset values and the port is off
    /// (so a plugged device lost VBUS and starts over); the device stays plugged.
    pub fn chip_reset(&mut self) {
        let device = self.device.take();
        let messages = std::mem::take(&mut self.messages);
        *self = UsbDwc { device, messages, ..Default::default() };
        if let Some(d) = self.device.as_mut() {
            d.bus_reset();
        }
    }

    pub fn device(&self) -> Option<&dyn UsbDevice> {
        self.device.as_deref()
    }

    pub fn device_mut(&mut self) -> Option<&mut (dyn UsbDevice + 'static)> {
        self.device.as_deref_mut()
    }

    /// Plug a device into the port (a connect, if the port is powered).
    pub fn attach(&mut self, mut device: Box<dyn UsbDevice>) {
        self.detach();
        device.bus_reset();
        self.device = Some(device);
        self.connect();
    }

    /// Unplug the device: the port sees a disconnect.
    pub fn detach(&mut self) -> Option<Box<dyn UsbDevice>> {
        let d = self.device.take();
        if self.connected {
            self.connected = false;
            self.enabled = false;
            self.gintsts |= INT_DISCONN;
            // Transfers in flight get no answer: after three tries the channel halts.
            for c in self.chans.iter_mut().filter(|c| c.enabled) {
                *c = Channel {
                    enabled: false,
                    waiting: false,
                    hcint: c.hcint | CH_XACTERR | CH_XCS_XACT_ERR | CH_CHHLTD,
                };
            }
        }
        d
    }

    /// Console notes from the controller and the device.
    pub fn take_messages(&mut self) -> Vec<String> {
        let mut m = std::mem::take(&mut self.messages);
        if let Some(d) = self.device.as_mut() {
            m.extend(d.take_messages());
        }
        m
    }

    pub fn net_busy(&self) -> bool {
        self.device.as_ref().is_some_and(|d| d.net_busy())
    }

    fn connect(&mut self) {
        if self.powered && self.device.is_some() && !self.connected {
            self.connected = true;
            self.conndet = true;
        }
    }

    /// The interrupt line: GAHBCFG.GlblIntrMsk and an unmasked GINTSTS bit.
    pub fn irq(&self) -> bool {
        self.reg(GAHBCFG) & 1 != 0 && self.gintsts_value() & self.reg(GINTMSK) != 0
    }

    fn reg(&self, off: u32) -> u32 {
        self.regs[(off / 4) as usize]
    }

    fn set_reg(&mut self, off: u32, v: u32) {
        self.regs[(off / 4) as usize] = v;
    }

    fn haint(&self) -> u32 {
        (0..CHANNELS)
            .filter(|&n| self.chans[n].hcint & self.reg(Self::chan_off(n) + 0x0C) != 0)
            .fold(0, |m, n| m | 1 << n)
    }

    fn gintsts_value(&self) -> u32 {
        let mut v = self.gintsts | INT_CURMOD;
        if self.conndet || self.enchng {
            v |= INT_PRTINT;
        }
        if self.haint() & self.reg(HAINTMSK) != 0 {
            v |= INT_HCHINT;
        }
        v
    }

    fn hprt_value(&self) -> u32 {
        let mut v = self.reg(HPRT) & (PRT_RES | PRT_SUSP | PRT_RST | PRT_PWR | 0xF << 13);
        if self.connected {
            v |= PRT_CONNSTS | PRT_SPD_FS;
            if !self.reg(HPRT) & PRT_RST != 0 {
                v |= PRT_LNSTS_J;
            }
        }
        if self.conndet {
            v |= PRT_CONNDET;
        }
        if self.enabled {
            v |= PRT_ENA;
        }
        if self.enchng {
            v |= PRT_ENCHNG;
        }
        v
    }

    fn chan_off(n: usize) -> u32 {
        CHANNELS_BASE + CHANNEL_STRIDE * n as u32
    }

    /// The channel and register (0x00 HCCHAR ... 0x1C HCDMAB) at `off`.
    fn chan_reg(off: u32) -> Option<(usize, u32)> {
        let rel = off.checked_sub(CHANNELS_BASE)?;
        let n = (rel / CHANNEL_STRIDE) as usize;
        (n < CHANNELS).then_some((n, rel % CHANNEL_STRIDE))
    }

    pub fn read(&self, off: u32, now_ns: u64) -> u32 {
        match off {
            GRSTCTL => self.reg(GRSTCTL) & !0x3F | 1 << 31, // resets and flushes done, AHB idle
            GINTSTS => self.gintsts_value(),
            GSNPSID => SNPSID,
            GHWCFG1 => HWCFG1,
            GHWCFG2 => HWCFG2,
            GHWCFG3 => HWCFG3,
            GHWCFG4 => HWCFG4,
            // Full-speed frames are 1 ms.
            HFNUM => ((now_ns / 1_000_000) & 0x3FFF) as u32,
            HAINT => self.haint(),
            HPRT => self.hprt_value(),
            _ => match Self::chan_reg(off) {
                Some((n, 0x00)) => {
                    let v = self.reg(off) & !(CHAR_CHENA | CHAR_CHDIS);
                    if self.chans[n].enabled { v | CHAR_CHENA } else { v }
                }
                Some((n, 0x08)) => self.chans[n].hcint,
                _ => self.reg(off),
            },
        }
    }

    pub fn write(&mut self, off: u32, v: u32, dma: &mut dyn Dma) {
        match off {
            GRSTCTL => {
                self.set_reg(off, v);
                if v & 1 != 0 {
                    self.soft_reset();
                }
            }
            GINTSTS => self.gintsts &= !v,
            HAINT | GSNPSID | GHWCFG1 | GHWCFG2 | GHWCFG3 | GHWCFG4 | HFNUM => {}
            HPRT => self.write_hprt(v),
            GUSBCFG | GAHBCFG | GINTMSK | HAINTMSK => self.set_reg(off, v),
            _ => match Self::chan_reg(off) {
                Some((n, 0x00)) => self.write_hcchar(n, v, dma),
                Some((n, 0x08)) => self.chans[n].hcint &= !v,
                _ => self.set_reg(off, v),
            },
        }
    }

    /// GRSTCTL.CSftRst: the core's state machines and interrupts start over; the port is
    /// unpowered until the HCD powers it again (hcd_port_recover, then a power-on).
    fn soft_reset(&mut self) {
        self.chans = [Channel::default(); CHANNELS];
        self.gintsts = 0;
        self.conndet = false;
        self.enchng = false;
        self.enabled = false;
        self.control = None;
        let hprt = self.reg(HPRT);
        self.set_reg(HPRT, hprt & !PRT_PWR & !PRT_RST);
        self.power(false);
    }

    fn power(&mut self, on: bool) {
        if on == self.powered {
            return;
        }
        self.powered = on;
        if on {
            self.connect();
        } else {
            // VBUS off: the device goes away (and comes back, reset, on the next power-on).
            if self.connected {
                self.connected = false;
                self.enabled = false;
                self.gintsts |= INT_DISCONN;
            }
            if let Some(d) = self.device.as_mut() {
                d.bus_reset();
            }
        }
    }

    fn write_hprt(&mut self, v: u32) {
        let old = self.reg(HPRT);
        // W1C: connect detect, enable change (no overcurrent here); a 1 to PrtEna disables.
        if v & PRT_CONNDET != 0 {
            self.conndet = false;
        }
        if v & PRT_ENCHNG != 0 {
            self.enchng = false;
        }
        if v & PRT_ENA != 0 && self.enabled {
            self.enabled = false;
            self.enchng = true;
        }
        self.set_reg(HPRT, v & (PRT_RES | PRT_SUSP | PRT_RST | PRT_PWR | 0xF << 13));
        if (v ^ old) & PRT_PWR != 0 {
            self.power(v & PRT_PWR != 0);
        }
        if (v ^ old) & PRT_RST != 0 {
            if v & PRT_RST != 0 {
                // Driving reset disables an enabled port.
                if self.enabled {
                    self.enabled = false;
                    self.enchng = true;
                }
                self.control = None;
                if self.connected
                    && let Some(d) = self.device.as_mut()
                {
                    d.bus_reset();
                }
            } else if self.connected {
                // Reset done: the full-speed device is enabled.
                self.enabled = true;
                self.enchng = true;
            }
        }
    }

    fn write_hcchar(&mut self, n: usize, v: u32, dma: &mut dyn Dma) {
        let off = Self::chan_off(n);
        self.set_reg(off, v & !(CHAR_CHENA | CHAR_CHDIS));
        let c = &mut self.chans[n];
        if v & CHAR_CHDIS != 0 {
            // A halt request; only an enabled channel halts (and interrupts).
            if c.enabled {
                *c = Channel { enabled: false, waiting: false, hcint: c.hcint | CH_CHHLTD };
            }
        } else if v & CHAR_CHENA != 0 && !c.enabled {
            c.enabled = true;
            c.waiting = false;
            self.run(n, dma);
        }
    }

    /// Let time pass: the device may have data now for channels it NAKed.
    pub fn poll(&mut self, now_ns: u64, dma: &mut dyn Dma) {
        if let Some(d) = self.device.as_mut() {
            d.poll(now_ns);
        }
        self.retry(dma);
    }

    fn retry(&mut self, dma: &mut dyn Dma) {
        for n in 0..CHANNELS {
            if self.chans[n].enabled && self.chans[n].waiting {
                self.run(n, dma);
            }
        }
    }

    /// Run channel `n` through its QTD list from HCDMAn's current index until it halts,
    /// stops on a short packet, or the device NAKs.
    fn run(&mut self, n: usize, dma: &mut dyn Dma) {
        let off = Self::chan_off(n);
        let hcchar = self.reg(off);
        let ntd = ((self.reg(off + 0x10) >> 8) & 0xFF) + 1;
        let hcdma = self.reg(off + 0x14);
        let list = hcdma & !0x1FF;
        let mut idx = (hcdma >> 3) & 0x3F;
        let mps = (hcchar & 0x7FF).max(1);
        let ep = ((hcchar >> 11) & 0xF) as u8;
        let is_in = hcchar & (1 << 15) != 0;
        let eptype = (hcchar >> 18) & 3;
        let addr = ((hcchar >> 22) & 0x7F) as u8;
        let mut wrote_out = false;
        for _ in 0..ntd {
            let qtd_addr = list + 8 * idx;
            let Some(q) = dma.dma_read(qtd_addr, 8) else {
                self.halt(n, CH_BNAINTR, false);
                return;
            };
            let status = u32::from_le_bytes(q[0..4].try_into().unwrap());
            let buf = u32::from_le_bytes(q[4..8].try_into().unwrap());
            if status & QTD_ACTIVE == 0 {
                // A QTD the core can't use: BNA, without a channel halted interrupt.
                self.halt(n, CH_BNAINTR, false);
                return;
            }
            let size = status & QTD_SIZE;
            let outcome = self.transact(eptype, ep, is_in, addr, mps, status & QTD_SETUP != 0, buf, size, dma);
            match outcome {
                Outcome::Nak => {
                    self.chans[n].waiting = true;
                    self.set_hcdma(n, idx);
                    if wrote_out {
                        self.retry_others(n, dma);
                    }
                    return;
                }
                Outcome::Stall | Outcome::NoResponse => {
                    let sts = QTD_STS_PKTERR << QTD_STS_SHIFT;
                    dma.dma_write(qtd_addr, &((status & !QTD_ACTIVE) | sts).to_le_bytes());
                    let bits = match outcome {
                        Outcome::Stall => CH_STALL,
                        _ => CH_XACTERR | CH_XCS_XACT_ERR,
                    };
                    self.set_hcdma(n, idx);
                    self.halt(n, bits, true);
                    return;
                }
                Outcome::Done(rem) => {
                    let done = (status & !(QTD_ACTIVE | QTD_SIZE | 3 << QTD_STS_SHIFT)) | rem;
                    dma.dma_write(qtd_addr, &done.to_le_bytes());
                    wrote_out |= !is_in;
                    idx = (idx + 1) % ntd;
                    self.set_hcdma(n, idx);
                    let ioc = if status & QTD_IOC != 0 { CH_XFERCOMPL } else { 0 };
                    if status & QTD_EOL != 0 {
                        self.halt(n, ioc, true);
                        break;
                    }
                    if is_in && rem > 0 {
                        // A short packet ends the transfer: transfer complete, the channel
                        // stays enabled until the host halts it.
                        let c = &mut self.chans[n];
                        c.hcint |= CH_XFERCOMPL;
                        c.waiting = false;
                        break;
                    }
                }
            }
        }
        if wrote_out {
            self.retry_others(n, dma);
        }
    }

    /// Data the host just sent may have answers waiting (ARP, DHCP, TCP): retry the other
    /// channels' NAKed transfers now rather than at the next poll.
    fn retry_others(&mut self, except: usize, dma: &mut dyn Dma) {
        for m in 0..CHANNELS {
            if m != except && self.chans[m].enabled && self.chans[m].waiting {
                self.run(m, dma);
            }
        }
    }

    fn set_hcdma(&mut self, n: usize, idx: u32) {
        let off = Self::chan_off(n) + 0x14;
        let v = self.reg(off) & !(0x3F << 3) | (idx & 0x3F) << 3;
        self.set_reg(off, v);
    }

    fn halt(&mut self, n: usize, bits: u32, halted: bool) {
        let c = &mut self.chans[n];
        c.enabled = false;
        c.waiting = false;
        c.hcint |= bits | if halted { CH_CHHLTD } else { 0 };
    }

    /// One QTD's transfer with the device.
    #[allow(clippy::too_many_arguments)]
    fn transact(
        &mut self,
        eptype: u32,
        ep: u8,
        is_in: bool,
        addr: u8,
        mps: u32,
        setup: bool,
        buf: u32,
        size: u32,
        dma: &mut dyn Dma,
    ) -> Outcome {
        if !self.enabled {
            return Outcome::NoResponse;
        }
        let Some(dev) = self.device.as_mut().filter(|d| d.address() == addr) else {
            return Outcome::NoResponse;
        };
        if eptype == 0 {
            // Control: the HCD runs each stage as its own one-QTD activation.
            if setup {
                let Some(s) = dma.dma_read(buf, 8).and_then(|b| Setup::parse(&b)) else {
                    return Outcome::NoResponse;
                };
                self.control = Some(Control { setup: s, done: false });
                return Outcome::Done(0);
            }
            let Some(ctl) = self.control.as_mut() else { return Outcome::Stall };
            if size == 0 {
                // Status stage: a request without a data stage is carried out now.
                if !ctl.done {
                    ctl.done = true;
                    if dev.control(&ctl.setup, &[]).is_none() {
                        return Outcome::Stall;
                    }
                }
                return Outcome::Done(0);
            }
            if ctl.done {
                return Outcome::Stall;
            }
            ctl.done = true;
            return if is_in {
                match dev.control(&ctl.setup, &[]) {
                    Some(d) => {
                        let n = d.len().min(size as usize);
                        dma.dma_write(buf, &d[..n]);
                        Outcome::Done(size - n as u32)
                    }
                    None => Outcome::Stall,
                }
            } else {
                let data = dma.dma_read(buf, size as usize).unwrap_or_default();
                match dev.control(&ctl.setup, &data) {
                    Some(_) => Outcome::Done(0),
                    None => Outcome::Stall,
                }
            };
        }
        if is_in {
            match dev.data_in(ep, size as usize) {
                Reply::Data(d) => {
                    let n = d.len().min(size as usize);
                    dma.dma_write(buf, &d[..n]);
                    Outcome::Done(size - n as u32)
                }
                Reply::Nak => Outcome::Nak,
                Reply::Stall => Outcome::Stall,
            }
        } else {
            let data = if size > 0 { dma.dma_read(buf, size as usize).unwrap_or_default() } else { Vec::new() };
            // A short packet (or a zero-length one) ends the device's transfer.
            let ends = !size.is_multiple_of(mps) || size == 0;
            match dev.data_out(ep, &data, ends) {
                Reply::Data(_) => Outcome::Done(0),
                Reply::Nak => Outcome::Nak,
                Reply::Stall => Outcome::Stall,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devices::usb_ethernet::{Link, UsbEthernet};

    /// Guest memory for the DMA: a flat buffer at address 0.
    struct Mem(Vec<u8>);

    impl Dma for Mem {
        fn dma_read(&self, addr: u32, len: usize) -> Option<Vec<u8>> {
            self.0.get(addr as usize..addr as usize + len).map(<[u8]>::to_vec)
        }
        fn dma_write(&mut self, addr: u32, data: &[u8]) -> bool {
            let a = addr as usize;
            self.0[a..a + data.len()].copy_from_slice(data);
            true
        }
    }

    const QTDS: u32 = 0x200;
    const BUF: u32 = 0x1000;

    fn ethernet() -> Box<UsbEthernet> {
        let net = vnet::NetConfig { offline: true, ..Default::default() };
        Box::new(UsbEthernet::new(Link { up: true, dhcp: true }, &net, Default::default(), [0; 6]))
    }

    /// A powered, reset and enabled port, as hub.c leaves it.
    fn port_up(c: &mut UsbDwc, m: &mut Mem) {
        c.write(GAHBCFG, 1, m);
        c.write(GINTMSK, INT_PRTINT | INT_HCHINT | INT_DISCONN, m);
        c.write(HPRT, PRT_PWR, m);
        assert!(c.irq() && c.read(HPRT, 0) & (PRT_CONNSTS | PRT_CONNDET) == PRT_CONNSTS | PRT_CONNDET);
        c.write(HPRT, PRT_PWR | PRT_CONNDET, m);
        c.write(HPRT, PRT_PWR | PRT_RST, m);
        c.write(HPRT, PRT_PWR, m);
        assert_eq!(c.read(HPRT, 0) & (PRT_ENA | PRT_ENCHNG | 3 << 17), PRT_ENA | PRT_ENCHNG | PRT_SPD_FS);
        c.write(HPRT, PRT_PWR | PRT_ENCHNG, m);
        assert!(!c.irq());
    }

    fn qtd(m: &mut Mem, i: u32, status: u32, buf: u32) {
        m.dma_write(QTDS + 8 * i, &status.to_le_bytes());
        m.dma_write(QTDS + 8 * i + 4, &buf.to_le_bytes());
    }

    fn qtd_status(m: &Mem, i: u32) -> u32 {
        u32::from_le_bytes(m.dma_read(QTDS + 8 * i, 4).unwrap().try_into().unwrap())
    }

    /// Start channel 0 on endpoint `ep` of device `addr` at QTD `idx` of a `ntd`-long list.
    #[allow(clippy::too_many_arguments)]
    fn start(c: &mut UsbDwc, m: &mut Mem, ep: u32, is_in: bool, eptype: u32, addr: u32, idx: u32, ntd: u32) {
        c.write(CHANNELS_BASE + 0x0C, CH_XFERCOMPL | CH_CHHLTD | CH_BNAINTR, m);
        c.write(HAINTMSK, 1, m);
        c.write(CHANNELS_BASE + 0x10, (ntd - 1) << 8 | 0xFF, m);
        c.write(CHANNELS_BASE + 0x14, QTDS | idx << 3, m);
        let hcchar = 64 | ep << 11 | (is_in as u32) << 15 | eptype << 18 | addr << 22;
        c.write(CHANNELS_BASE, hcchar | CHAR_CHENA, m);
    }

    /// One control transfer as hcd_dwc.c runs it: three one-QTD stages, each halting.
    fn control(c: &mut UsbDwc, m: &mut Mem, addr: u32, setup: [u8; 8], data_len: u32) -> Option<Vec<u8>> {
        let is_in = setup[0] & 0x80 != 0;
        m.dma_write(BUF, &setup);
        qtd(m, 0, QTD_ACTIVE | QTD_SETUP | QTD_IOC | QTD_EOL | 8, BUF);
        qtd(m, 1, QTD_ACTIVE | QTD_IOC | QTD_EOL | data_len, BUF + 8);
        qtd(m, 2, QTD_ACTIVE | QTD_IOC | QTD_EOL, 0);
        let mut stages = vec![(0, false)];
        if data_len > 0 {
            stages.push((1, is_in));
        }
        stages.push((2, !(is_in && data_len > 0)));
        for (idx, dir_in) in stages {
            start(c, m, 0, dir_in, 0, addr, idx, 3);
            let hcint = c.read(CHANNELS_BASE + 0x08, 0);
            assert!(c.irq() && c.read(GINTSTS, 0) & INT_HCHINT != 0, "stage {idx}: no channel interrupt");
            c.write(CHANNELS_BASE + 0x08, hcint, m);
            // A STALL, or no answer (nothing at that address).
            if hcint & (CH_STALL | CH_XCS_XACT_ERR) != 0 {
                return None;
            }
            assert_eq!(hcint, CH_XFERCOMPL | CH_CHHLTD, "stage {idx}");
        }
        let rem = qtd_status(m, 1) & QTD_SIZE;
        Some(m.dma_read(BUF + 8, (data_len - rem) as usize).unwrap())
    }

    #[test]
    fn enumerates_a_device_and_reads_its_descriptors() {
        let mut c = UsbDwc::default();
        let mut m = Mem(vec![0; 0x4000]);
        assert_eq!(c.read(GSNPSID, 0), SNPSID);
        assert_eq!((c.read(GHWCFG2, 0) >> 14 & 0xF) + 1, 8);
        c.attach(ethernet());
        port_up(&mut c, &mut m);
        // GET_DESCRIPTOR(device, 8) at address 0, into a 64-byte buffer: a short packet.
        let d = control(&mut c, &mut m, 0, [0x80, 6, 0, 1, 0, 0, 8, 0], 64).unwrap();
        assert_eq!(d.len(), 8);
        assert!(!c.irq());
        control(&mut c, &mut m, 0, [0x00, 5, 7, 0, 0, 0, 0, 0], 0).unwrap();
        // Now only address 7 answers.
        assert!(control(&mut c, &mut m, 0, [0x80, 6, 0, 1, 0, 0, 18, 0], 64).is_none());
        let d = control(&mut c, &mut m, 7, [0x80, 6, 0, 1, 0, 0, 18, 0], 64).unwrap();
        assert_eq!((d.len(), d[17]), (18, 2));
        // An unknown request STALLs.
        assert!(control(&mut c, &mut m, 7, [0xC0, 0x05, 0, 0, 0, 0, 2, 0], 64).is_none());
    }

    #[test]
    fn a_naked_in_transfer_completes_when_data_arrives_and_halts_on_request() {
        let mut c = UsbDwc::default();
        let mut m = Mem(vec![0; 0x4000]);
        c.attach(ethernet());
        port_up(&mut c, &mut m);
        control(&mut c, &mut m, 0, [0x00, 5, 1, 0, 0, 0, 0, 0], 0).unwrap();
        control(&mut c, &mut m, 1, [0x00, 9, 2, 0, 0, 0, 0, 0], 0).unwrap();
        // Bulk IN before the data interface is enabled: NAK, the channel keeps trying.
        qtd(&mut m, 0, QTD_ACTIVE | QTD_IOC | QTD_EOL | 1536, BUF);
        start(&mut c, &mut m, 1, true, 2, 1, 0, 1);
        c.poll(0, &mut m);
        assert_eq!(c.read(CHANNELS_BASE + 0x08, 0), 0);
        assert_ne!(c.read(CHANNELS_BASE, 0) & CHAR_CHENA, 0);
        // The host halts it.
        c.write(CHANNELS_BASE, c.read(CHANNELS_BASE, 0) | CHAR_CHDIS, &mut m);
        assert_eq!(c.read(CHANNELS_BASE + 0x08, 0), CH_CHHLTD);
        assert_eq!(c.read(CHANNELS_BASE, 0) & CHAR_CHENA, 0);
        c.write(CHANNELS_BASE + 0x08, CH_CHHLTD, &mut m);
        // A notification IN waits for SET_INTERFACE, then gets NETWORK_CONNECTION (8 of 16 bytes).
        qtd(&mut m, 0, QTD_ACTIVE | QTD_IOC | QTD_EOL | 16, BUF);
        start(&mut c, &mut m, 3, true, 3, 1, 0, 1);
        assert_eq!(c.read(CHANNELS_BASE + 0x08, 0), 0);
        // SET_INTERFACE(data interface, alternate setting 1), straight to the device; the
        // next poll retries the waiting channel.
        let dev = c.device_mut().unwrap();
        let s = Setup { request_type: 0x01, request: 0x0B, value: 1, index: 1, length: 0 };
        dev.control(&s, &[]).unwrap();
        c.poll(0, &mut m);
        assert_eq!(c.read(CHANNELS_BASE + 0x08, 0), CH_XFERCOMPL | CH_CHHLTD);
        assert_eq!(qtd_status(&m, 0) & (QTD_ACTIVE | QTD_SIZE), 16 - 8);
        assert_eq!(m.dma_read(BUF, 2).unwrap(), [0xA1, 0x00]);
    }

    #[test]
    fn unplugging_halts_transfers_in_flight_and_reports_a_disconnect() {
        let mut c = UsbDwc::default();
        let mut m = Mem(vec![0; 0x4000]);
        c.attach(ethernet());
        port_up(&mut c, &mut m);
        qtd(&mut m, 0, QTD_ACTIVE | QTD_IOC | QTD_EOL | 16, BUF);
        start(&mut c, &mut m, 3, true, 3, 0, 0, 1);
        assert!(c.detach().is_some());
        assert_ne!(c.read(GINTSTS, 0) & INT_DISCONN, 0);
        assert_eq!(c.read(HPRT, 0) & (PRT_CONNSTS | PRT_ENA), 0);
        let hcint = c.read(CHANNELS_BASE + 0x08, 0);
        assert_eq!(hcint & (CH_CHHLTD | CH_XCS_XACT_ERR), CH_CHHLTD | CH_XCS_XACT_ERR);
        // Plugged in again: a new connection once the port is powered.
        c.write(GINTSTS, INT_DISCONN, &mut m);
        c.attach(ethernet());
        assert_ne!(c.read(HPRT, 0) & PRT_CONNDET, 0);
        // A soft reset (hcd_port_recover) leaves the port unpowered.
        c.write(GRSTCTL, 1, &mut m);
        assert_eq!(c.read(GRSTCTL, 0) & 1, 0);
        assert_eq!(c.read(HPRT, 0) & (PRT_PWR | PRT_CONNSTS), 0);
    }
}
