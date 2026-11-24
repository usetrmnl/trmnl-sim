//! What a USB device plugged into a simulated host port answers, one transaction at a time.
//! The host controller (`periph::usb_dwc`) runs the bus: it sends SETUP packets and data
//! stages to endpoint 0, polls IN endpoints and hands over OUT data, and maps the answers
//! (data, NAK, STALL) onto its own registers and descriptors.

/// A device's answer to a transaction on a non-control endpoint.
#[derive(Debug, PartialEq)]
pub enum Reply {
    /// IN: the data of one transfer (shorter than asked for ends it; empty is a zero-length
    /// packet). OUT: accepted.
    Data(Vec<u8>),
    /// Not ready (no data to send, or can't take data now): the host retries later.
    Nak,
    Stall,
}

/// A standard request's SETUP packet (USB 2.0 §9.3).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Setup {
    pub request_type: u8,
    pub request: u8,
    pub value: u16,
    pub index: u16,
    pub length: u16,
}

impl Setup {
    pub fn parse(b: &[u8]) -> Option<Setup> {
        let b: &[u8; 8] = b.get(..8)?.try_into().ok()?;
        Some(Setup {
            request_type: b[0],
            request: b[1],
            value: u16::from_le_bytes([b[2], b[3]]),
            index: u16::from_le_bytes([b[4], b[5]]),
            length: u16::from_le_bytes([b[6], b[7]]),
        })
    }
}

pub trait UsbDevice: Send {
    /// Bus reset (the host drove SE0): default state, address 0, unconfigured.
    fn bus_reset(&mut self);

    /// The address the device answers on.
    fn address(&self) -> u8;

    /// Endpoint 0: carry out a control request. Called once per request, when the device has
    /// what it needs: at the data stage for IN requests (answer: up to `length` bytes) and for
    /// OUT requests with data (`data` is the data stage), at the status stage for requests
    /// without data. `None` STALLs the request. A SET_ADDRESS takes effect on return (its
    /// status stage went to the old address).
    fn control(&mut self, setup: &Setup, data: &[u8]) -> Option<Vec<u8>>;

    /// An IN transaction on endpoint `ep` (number, no direction bit) taking up to `max` bytes.
    fn data_in(&mut self, ep: u8, max: usize) -> Reply;

    /// An OUT transfer to endpoint `ep`: `data` is what the host sent, and `ends` whether it
    /// ended with a short or zero-length packet (otherwise more of the same frame may follow).
    fn data_out(&mut self, ep: u8, data: &[u8], ends: bool) -> Reply;

    /// Let time pass: the device may have new data to send afterwards.
    fn poll(&mut self, _now_ns: u64) {}

    /// Host-side work is in flight (simulated time must keep to wall-clock time).
    fn net_busy(&self) -> bool {
        false
    }

    /// Notes for the console (what the device saw that a person debugging would want to know).
    fn take_messages(&mut self) -> Vec<String> {
        Vec::new()
    }

    /// The device is a USB Ethernet adapter.
    fn ethernet(&mut self) -> Option<&mut super::usb_ethernet::UsbEthernet> {
        None
    }

    fn ethernet_ref(&self) -> Option<&super::usb_ethernet::UsbEthernet> {
        None
    }
}
