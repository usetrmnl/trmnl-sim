//! In-memory smoltcp `Device`: frames/packets are pushed into `rx` by the owner and
//! whatever smoltcp transmits accumulates in `tx`.

use std::collections::VecDeque;

use smoltcp::phy::{self, Device, DeviceCapabilities, Medium};
use smoltcp::time::Instant;

pub(crate) struct QueueDevice {
    pub rx: VecDeque<Vec<u8>>,
    pub tx: Vec<Vec<u8>>,
    medium: Medium,
    mtu: usize,
}

impl QueueDevice {
    pub fn new(medium: Medium, mtu: usize) -> Self {
        Self { rx: VecDeque::new(), tx: Vec::new(), medium, mtu }
    }
}

pub(crate) struct RxToken(Vec<u8>);
pub(crate) struct TxToken<'a>(&'a mut Vec<Vec<u8>>);

impl phy::RxToken for RxToken {
    fn consume<R, F: FnOnce(&[u8]) -> R>(self, f: F) -> R {
        f(&self.0)
    }
}

impl phy::TxToken for TxToken<'_> {
    fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, len: usize, f: F) -> R {
        let mut buf = vec![0u8; len];
        let r = f(&mut buf);
        self.0.push(buf);
        r
    }
}

impl Device for QueueDevice {
    type RxToken<'a> = RxToken;
    type TxToken<'a> = TxToken<'a>;

    fn receive(&mut self, _ts: Instant) -> Option<(RxToken, TxToken<'_>)> {
        let p = self.rx.pop_front()?;
        Some((RxToken(p), TxToken(&mut self.tx)))
    }

    fn transmit(&mut self, _ts: Instant) -> Option<TxToken<'_>> {
        Some(TxToken(&mut self.tx))
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.medium = self.medium;
        caps.max_transmission_unit = self.mtu;
        caps
    }
}
