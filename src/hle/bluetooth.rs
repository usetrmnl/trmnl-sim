//! ESP-IDF controller/VHCI boundary. The guest still runs NimBLE, ATT and protocomm.
use super::{Flow, HleCtx, Hooks, MAGIC_BASE};
use crate::firmware::Symbols;
use sim_api::{BluetoothOperation, BluetoothReply, BluetoothReplySender, BluetoothStatus};
use sim_bluetooth::Controller;
use std::collections::VecDeque;
use std::time::{Duration, Instant};

struct Pending {
    reply: BluetoothReplySender,
    connection: u64,
    opcode: u8,
    deadline_ns: u64,
    deadline: Instant,
}

const TASK_LOOP: u32 = MAGIC_BASE + 0x200;
const INVALID_STATE: u32 = 0x103;
const INVALID_ARG: u32 = 0x102;

pub struct BluetoothState {
    pub controller: Controller,
    pending: Option<Pending>,
    updates: VecDeque<Vec<u8>>,
    status: u32,
    callbacks: [u32; 2],
    task_created: bool,
    send_ready: bool,
}

impl BluetoothState {
    pub fn new(mac: [u8; 6]) -> Self {
        Self {
            controller: Controller::new(mac),
            pending: None,
            updates: VecDeque::new(),
            status: 0,
            callbacks: [0; 2],
            task_created: false,
            send_ready: false,
        }
    }
    pub fn reset(&mut self) {
        self.fail_pending("Bluetooth controller reset");
        self.updates.clear();
        self.controller.reset();
        self.status = 0;
        self.callbacks = [0; 2];
        self.task_created = false;
        self.send_ready = false;
    }
}

pub fn install(hooks: &mut Hooks, syms: &Symbols) {
    hooks.install(syms, "esp_bt_controller_init", init);
    hooks.install(syms, "esp_bt_controller_enable", enable);
    hooks.install(syms, "esp_bt_controller_disable", disable);
    hooks.install(syms, "esp_bt_controller_deinit", deinit);
    hooks.install(syms, "esp_bt_controller_get_status", status);
    hooks.install(syms, "esp_bt_controller_mem_release", release);
    hooks.install(syms, "esp_vhci_host_register_callback", register);
    hooks.install(syms, "esp_vhci_host_check_send_available", available);
    hooks.install(syms, "esp_vhci_host_send_packet", send);
    hooks.trampoline(TASK_LOOP, "sim_bluetooth task", task_loop);
}

fn result(code: u32) -> Flow {
    Flow::Return(Some(code))
}
fn init(c: &mut HleCtx) -> Flow {
    if c.state.bluetooth.status != 0 {
        return result(INVALID_STATE);
    }
    if c.cpu.arg(0) == 0 {
        return result(INVALID_ARG);
    }
    let Some(read_mac) = c.syms.addr("esp_read_mac") else {
        return result(0x106);
    };
    let saved_sp = c.cpu.sp();
    let mac = c.cpu.alloc_scratch(16);
    Flow::Call {
        func: read_mac,
        args: vec![mac, 2], // ESP_MAC_BT: let the guest derive its SoC/config-specific address.
        then: Box::new(move |c, ret| {
            let address = c.mem.read_bytes(mac, 6);
            c.cpu.set_sp(saved_sp);
            if ret != 0 {
                return result(ret);
            }
            let Some(address) = address else {
                return result(INVALID_ARG);
            };
            c.state.bluetooth.controller.set_address(address.try_into().unwrap());
            create_task(c)
        }),
    }
}

fn create_task(c: &mut HleCtx) -> Flow {
    let Some(create) = c.syms.addr("xTaskCreatePinnedToCore") else {
        return result(0x106);
    };
    if c.syms.addr("vTaskDelay").is_none() {
        return result(0x106);
    }
    if c.state.bluetooth.task_created {
        c.state.bluetooth.status = 1;
        return result(0);
    }
    let saved_sp = c.cpu.sp();
    let name = c.cpu.alloc_scratch(16);
    c.mem.write_bytes(name, b"sim_ble\0");
    Flow::Call {
        func: create,
        args: vec![TASK_LOOP, name, 4096, 0, 23, 0, 0x7fff_ffff],
        then: Box::new(move |c, ok| {
            c.cpu.set_sp(saved_sp);
            if ok != 1 {
                return result(0x101);
            }
            c.state.bluetooth.task_created = true;
            c.state.bluetooth.status = 1;
            c.env.console("bluetooth: mock controller initialized");
            result(0)
        }),
    }
}
fn enable(c: &mut HleCtx) -> Flow {
    if c.state.bluetooth.status != 1 {
        return result(INVALID_STATE);
    }
    if c.cpu.arg(0) != 1 {
        return result(INVALID_ARG);
    } // ESP_BT_MODE_BLE
    c.state.bluetooth.status = 2;
    result(0)
}
fn disable(c: &mut HleCtx) -> Flow {
    if c.state.bluetooth.status != 2 {
        return result(INVALID_STATE);
    }
    c.state.bluetooth.status = 1;
    c.state.bluetooth.controller.reset();
    c.state.bluetooth.send_ready = false;
    result(0)
}
fn deinit(c: &mut HleCtx) -> Flow {
    if c.state.bluetooth.status != 1 {
        return result(INVALID_STATE);
    }
    c.state.bluetooth.status = 0;
    c.state.bluetooth.callbacks = [0; 2];
    result(0)
}
fn status(c: &mut HleCtx) -> Flow {
    result(c.state.bluetooth.status)
}
fn release(c: &mut HleCtx) -> Flow {
    // No controller RAM is reserved by this backend. IDF calls this for Classic BT.
    result(if c.state.bluetooth.status == 0 { 0 } else { INVALID_STATE })
}
fn register(c: &mut HleCtx) -> Flow {
    let ptr = c.cpu.arg(0);
    let Some(data) = (if ptr == 0 { None } else { c.mem.read_bytes(ptr, 8) }) else {
        return result(INVALID_ARG);
    };
    let callbacks =
        [u32::from_le_bytes(data[..4].try_into().unwrap()), u32::from_le_bytes(data[4..].try_into().unwrap())];
    if callbacks.contains(&0) {
        return result(INVALID_ARG);
    }
    c.state.bluetooth.callbacks = callbacks;
    result(0)
}
fn available(c: &mut HleCtx) -> Flow {
    result(u32::from(c.state.bluetooth.status == 2))
}
fn send(c: &mut HleCtx) -> Flow {
    let len = c.cpu.arg(1) as usize;
    if c.state.bluetooth.status != 2 || len > 260 {
        return Flow::Return(None);
    }
    if let Some(packet) = c.mem.read_bytes(c.cpu.arg(0), len) {
        let was_advertising = c.state.bluetooth.controller.advertising();
        if let Err(e) = c.state.bluetooth.controller.receive(&packet, rand::random()) {
            c.env.console(&format!("bluetooth: rejected HCI packet: {e}"));
        }
        if c.state.bluetooth.controller.advertising() != was_advertising {
            c.env.console(if was_advertising {
                "bluetooth: advertising disabled"
            } else {
                "bluetooth: advertising enabled"
            });
        }
        c.state.bluetooth.send_ready = true;
    }
    Flow::Return(None)
}
fn back_to_loop(c: &mut HleCtx, _: u32) -> Flow {
    c.cpu.set_pc(TASK_LOOP);
    Flow::Redirected
}
fn task_loop(c: &mut HleCtx) -> Flow {
    let b = &mut c.state.bluetooth;
    if b.status == 2 && b.callbacks[0] != 0 && b.send_ready {
        b.send_ready = false;
        return Flow::Call { func: b.callbacks[0], args: vec![], then: Box::new(back_to_loop) };
    }
    if b.status == 2
        && b.callbacks[1] != 0
        && let Some(packet) = b.controller.pop_host_packet()
    {
        let callback = b.callbacks[1];
        let saved_sp = c.cpu.sp();
        let buf = c.cpu.alloc_scratch(packet.len() as u32);
        c.mem.write_bytes(buf, &packet);
        // IDF esp_nimble_hci.c:host_rcv_pkt copies into NimBLE-owned event/mbuf pools.
        return Flow::Call {
            func: callback,
            args: vec![buf, packet.len() as u32],
            then: Box::new(move |c, ret| {
                c.cpu.set_sp(saved_sp);
                back_to_loop(c, ret)
            }),
        };
    }
    Flow::Call { func: c.syms.addr("vTaskDelay").unwrap(), args: vec![1], then: Box::new(back_to_loop) }
}

impl BluetoothState {
    pub fn snapshot(&self) -> BluetoothStatus {
        BluetoothStatus {
            active: (self.status != 0).then_some("mock"),
            initialized: self.status == 2,
            advertising: self.controller.advertising(),
            connection: self.controller.connected().then(|| self.controller.generation()),
            advertisement: self.controller.advertisement().to_vec(),
            scan_response: self.controller.scan_response().to_vec(),
        }
    }

    fn fail_pending(&mut self, reason: &str) {
        if let Some(p) = self.pending.take() {
            let _ = p.reply.try_send(Err(reason.into()));
        }
    }

    pub fn operate(&mut self, operation: BluetoothOperation, reply: BluetoothReplySender, now: u64) {
        self.pump(now);
        let connecting = matches!(&operation, BluetoothOperation::Connect);
        let answer = (|| -> Result<BluetoothReply, String> {
            if self.status != 2 {
                return Err("Bluetooth is not enabled by the firmware".into());
            }
            let token = match &operation {
                BluetoothOperation::Connect => None,
                BluetoothOperation::Disconnect { connection }
                | BluetoothOperation::Exchange { connection, .. }
                | BluetoothOperation::Receive { connection } => Some(*connection),
            };
            if let Some(token) = token
                && (!self.controller.connected() || token != self.controller.generation())
            {
                return Err("stale or disconnected Bluetooth connection".into());
            }
            let mut result = BluetoothReply::default();
            match operation {
                BluetoothOperation::Connect => {
                    self.controller.connect()?;
                    self.updates.clear();
                    result.connection = Some(self.controller.generation());
                }
                BluetoothOperation::Disconnect { .. } => {
                    self.controller.disconnect(0x13)?;
                    self.fail_pending("Bluetooth central disconnected");
                    self.updates.clear();
                }
                BluetoothOperation::Receive { .. } => {
                    result.data = self.updates.pop_front().unwrap_or_default();
                }
                BluetoothOperation::Exchange { connection, data } => {
                    if self.pending.is_some() {
                        return Err("an ATT request is already pending".into());
                    }
                    let opcode = *data.first().ok_or("ATT request is empty")?;
                    if ![2, 4, 6, 8, 10, 12, 14, 16, 18, 22, 24, 0x52].contains(&opcode) {
                        return Err("unsupported ATT request opcode".into());
                    }
                    self.controller.send_att(&data)?;
                    if opcode != 0x52 {
                        self.pending = Some(Pending {
                            reply: reply.clone(),
                            connection,
                            opcode,
                            deadline_ns: now.saturating_add(5_000_000_000),
                            deadline: Instant::now() + Duration::from_secs(8),
                        });
                    }
                }
            }
            Ok(result)
        })();
        if answer.is_err() || self.pending.as_ref().is_none_or(|p| !p.reply.same_channel(&reply)) {
            let established = connecting && answer.is_ok();
            if reply.try_send(answer).is_err() && established {
                let _ = self.controller.disconnect(0x13);
                self.updates.clear();
            }
        }
    }

    pub fn pump(&mut self, now: u64) {
        if let Some(p) = &self.pending {
            if !self.controller.connected() || p.connection != self.controller.generation() {
                self.fail_pending("Bluetooth connection ended during ATT request");
            } else if now >= p.deadline_ns || Instant::now() >= p.deadline {
                self.fail_pending("ATT request timed out; connection discarded");
                let _ = self.controller.disconnect(0x13);
                self.updates.clear();
            }
        }
        while let Some(data) = self.controller.pop_att() {
            match data.first() {
                Some(0x1b | 0x1d) => {
                    if data[0] == 0x1d {
                        let _ = self.controller.send_att(&[0x1e]);
                    }
                    if self.updates.len() == 64 {
                        self.fail_pending("Bluetooth notification queue overflow");
                        let _ = self.controller.disconnect(0x13);
                        self.updates.clear();
                        break;
                    }
                    self.updates.push_back(data);
                }
                _ => {
                    if let Some(p) = self.pending.take() {
                        let valid = data.first() == Some(&(p.opcode + 1))
                            || (data.len() == 5 && data[0] == 1 && data[1] == p.opcode);
                        let response = if valid {
                            Ok(BluetoothReply { connection: Some(p.connection), data })
                        } else {
                            Err("unexpected ATT response; connection discarded".into())
                        };
                        if p.reply.try_send(response).is_err() || !valid {
                            let _ = self.controller.disconnect(0x13);
                            self.updates.clear();
                        }
                    } else {
                        // An uncorrelated response must not satisfy a later operation.
                        let _ = self.controller.disconnect(0x13);
                        self.updates.clear();
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn connected() -> BluetoothState {
        let mut b = BluetoothState::new([0; 6]);
        b.status = 2;
        b.controller.receive(&[1, 10, 32, 1, 1], [0; 8]).unwrap();
        b.controller.connect().unwrap();
        b
    }

    fn request(b: &mut BluetoothState, data: Vec<u8>) -> crossbeam_channel::Receiver<Result<BluetoothReply, String>> {
        let (tx, rx) = crossbeam_channel::bounded(1);
        b.operate(BluetoothOperation::Exchange { connection: b.controller.generation(), data }, tx, 0);
        rx
    }

    #[test]
    fn abandoned_connect_does_not_leave_an_orphaned_link() {
        let mut b = connected();
        b.controller.disconnect(0x13).unwrap();
        b.controller.receive(&[1, 10, 32, 1, 1], [0; 8]).unwrap();
        let (tx, rx) = crossbeam_channel::bounded(1);
        drop(rx);
        b.operate(BluetoothOperation::Connect, tx, 0);
        assert!(!b.controller.connected());
    }

    #[test]
    fn reset_fails_pending_once_and_clears_controller_status() {
        let mut b = connected();
        let token = b.controller.generation();
        let rx = request(&mut b, vec![10, 3, 0]);
        assert!(rx.try_recv().is_err());
        b.reset();
        assert!(rx.recv().unwrap().unwrap_err().contains("reset"));
        assert!(rx.try_recv().is_err());
        assert_ne!(token, b.controller.generation());
        assert_eq!(b.snapshot().active, None);
        assert!(!b.snapshot().initialized);
    }

    #[test]
    fn timeout_disconnects_without_replaying_request() {
        let mut b = connected();
        let rx = request(&mut b, vec![10, 3, 0]);
        b.pump(5_000_000_000);
        assert!(rx.recv().unwrap().unwrap_err().contains("timed out"));
        assert!(!b.controller.connected());
        while let Some(packet) = b.controller.pop_host_packet() {
            assert_ne!(packet[0], 2);
        }
    }

    #[test]
    fn att_error_is_returned_without_disconnecting() {
        let mut b = connected();
        let rx = request(&mut b, vec![10, 3, 0]);
        // ATT Error Response to Read Request: invalid handle.
        b.controller.receive(&[2, 1, 0, 9, 0, 5, 0, 4, 0, 1, 10, 3, 0, 1], [0; 8]).unwrap();
        b.pump(1);
        assert_eq!(rx.recv().unwrap().unwrap().data, [1, 10, 3, 0, 1]);
        assert!(b.controller.connected());
    }

    #[test]
    fn overlapping_and_stale_requests_are_rejected() {
        let mut b = connected();
        let _rx = request(&mut b, vec![10, 3, 0]);
        let second = request(&mut b, vec![10, 4, 0]);
        assert!(second.recv().unwrap().unwrap_err().contains("pending"));
        let (tx, rx) = crossbeam_channel::bounded(1);
        b.operate(BluetoothOperation::Disconnect { connection: b.controller.generation() + 1 }, tx, 0);
        assert!(rx.recv().unwrap().unwrap_err().contains("stale"));
        assert!(b.controller.connected());
    }
}
