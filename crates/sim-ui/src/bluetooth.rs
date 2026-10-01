//! The left panel's Bluetooth tab: the mock controller's state, the firmware's decoded
//! advertisement, and a manual central (connect, raw ATT exchanges, notifications) over
//! `Command::Bluetooth`, the same single central the control API's `/bluetooth/*` drives.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, TryRecvError};
use egui::{Color32, RichText, Ui};
use sim_api::{BluetoothOperation, BluetoothReply, BluetoothStatus, Command, SimHandle};

use crate::{dot, section};

/// Log rows kept (newest last).
const LOG_ROWS: usize = 200;
/// How often notifications are fetched on the panel's own connection.
const RECEIVE_EVERY: Duration = Duration::from_millis(250);
/// The control API waits 10 s; the emulator bounds requests to 5 s virtual / 8 s wall.
const REPLY_TIMEOUT: Duration = Duration::from_secs(10);

/// One-click ATT requests: (label, bytes, hover).
const PRESETS: [(&str, &[u8], &str); 3] = [
    ("Exchange MTU", &[0x02, 0x00, 0x02], "Exchange MTU Request, client MTU 512"),
    ("Services", &[0x10, 0x01, 0x00, 0xff, 0xff, 0x00, 0x28], "Read By Group Type 0x0001-0xffff, primary services"),
    ("Characteristics", &[0x08, 0x01, 0x00, 0xff, 0xff, 0x03, 0x28], "Read By Type 0x0001-0xffff, characteristics"),
];

type Reply = Result<BluetoothReply, String>;

#[derive(Clone, Copy, PartialEq)]
enum Dir {
    Sent,
    Reply,
    Notify,
    Info,
    Error,
}

struct Row {
    dir: Dir,
    text: String,
}

enum Kind {
    Connect,
    Disconnect,
    Exchange,
    Receive,
}

#[derive(Default)]
pub(crate) struct BluetoothPanel {
    pending: Option<(Kind, Receiver<Reply>, Instant)>,
    /// The connection this panel opened; notifications are only fetched on it, so the panel
    /// never takes them from a control API client's connection.
    own: Option<u64>,
    last_receive: Option<Instant>,
    hex: String,
    log: VecDeque<Row>,
}

impl BluetoothPanel {
    /// Handle replies and fetch notifications; call once per frame.
    pub fn poll(&mut self, h: &SimHandle, bt: &BluetoothStatus) {
        let result = self.pending.as_ref().and_then(|(_, rx, sent)| match rx.try_recv() {
            Ok(r) => Some(r),
            Err(TryRecvError::Disconnected) => Some(Err("the simulator dropped the request".into())),
            Err(TryRecvError::Empty) if sent.elapsed() > REPLY_TIMEOUT => Some(Err("timed out".into())),
            Err(TryRecvError::Empty) => None,
        });
        if let Some(result) = result
            && let Some((kind, ..)) = self.pending.take()
        {
            self.finished(kind, result);
        }
        if self.own.is_some() && bt.connection != self.own {
            self.own = None;
            self.push(Dir::Info, "Disconnected".into());
        }
        if let Some(connection) = self.own
            && self.pending.is_none()
            && self.last_receive.is_none_or(|t| t.elapsed() >= RECEIVE_EVERY)
        {
            self.last_receive = Some(Instant::now());
            self.request(h, Kind::Receive, BluetoothOperation::Receive { connection });
        }
    }

    fn finished(&mut self, kind: Kind, result: Reply) {
        match (kind, result) {
            (Kind::Connect, Ok(r)) => {
                self.own = r.connection;
                self.push(Dir::Info, "Connected".into());
            }
            (Kind::Disconnect, Ok(_)) => {
                self.own = None;
                self.push(Dir::Info, "Disconnected".into());
            }
            (Kind::Exchange, Ok(r)) => self.push(Dir::Reply, describe_att(&r.data)),
            (Kind::Receive, Ok(r)) if r.data.is_empty() => {}
            (Kind::Receive, Ok(r)) => self.push(Dir::Notify, describe_att(&r.data)),
            (_, Err(e)) => self.push(Dir::Error, e),
        }
    }

    fn request(&mut self, h: &SimHandle, kind: Kind, operation: BluetoothOperation) {
        let (reply, rx) = crossbeam_channel::bounded(1);
        h.send(Command::Bluetooth { operation, reply });
        self.pending = Some((kind, rx, Instant::now()));
    }

    fn push(&mut self, dir: Dir, text: String) {
        if self.log.len() == LOG_ROWS {
            self.log.pop_front();
        }
        self.log.push_back(Row { dir, text });
    }

    fn send_att(&mut self, h: &SimHandle, connection: u64, data: Vec<u8>) {
        self.push(Dir::Sent, describe_att(&data));
        self.request(h, Kind::Exchange, BluetoothOperation::Exchange { connection, data });
    }

    pub fn ui(&mut self, ui: &mut Ui, h: &SimHandle, bt: &BluetoothStatus) {
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 6.0;
            self.status_section(ui, bt);
            self.central_section(ui, h, bt);
            self.log_section(ui);
        });
    }

    fn status_section(&mut self, ui: &mut Ui, bt: &BluetoothStatus) {
        section(ui, "Controller");
        let dark = ui.visuals().dark_mode;
        let green = if dark { Color32::from_rgb(0x5c, 0xc8, 0x6c) } else { Color32::from_rgb(0x1f, 0x8a, 0x34) };
        let blue = if dark { Color32::from_rgb(0x7a, 0xb4, 0xf0) } else { Color32::from_rgb(0x1d, 0x5f, 0xb0) };
        let weak = ui.visuals().weak_text_color();
        ui.horizontal(|ui| {
            let (color, text) = if bt.connection.is_some() {
                (green, "Connected")
            } else if bt.advertising {
                (blue, "Advertising")
            } else if bt.initialized {
                (weak, "Initialized, not advertising")
            } else {
                (weak, "Off (the firmware hasn't started Bluetooth)")
            };
            dot(ui, color, bt.initialized);
            ui.label(RichText::new(text).color(color));
        });

        if bt.advertisement.is_empty() && bt.scan_response.is_empty() {
            return;
        }
        section(ui, "Advertisement");
        let fields: Vec<_> = parse_ad(&bt.advertisement).into_iter().chain(parse_ad(&bt.scan_response)).collect();
        egui::Grid::new("bt_ad").num_columns(2).spacing([12.0, 4.0]).show(ui, |ui| {
            for (name, value) in fields {
                ui.weak(name);
                ui.add(egui::Label::new(RichText::new(value).monospace()).wrap());
                ui.end_row();
            }
        });
        egui::CollapsingHeader::new("Raw bytes").id_salt("bt_raw").show(ui, |ui| {
            for (label, bytes) in [("Advertising data", &bt.advertisement), ("Scan response", &bt.scan_response)] {
                if !bytes.is_empty() {
                    ui.weak(label);
                    ui.add(egui::Label::new(RichText::new(hex(bytes)).monospace()).wrap());
                }
            }
        });
    }

    fn central_section(&mut self, ui: &mut Ui, h: &SimHandle, bt: &BluetoothStatus) {
        section(ui, "Central");
        let idle = self.pending.is_none();
        ui.horizontal(|ui| {
            match bt.connection {
                Some(connection) => {
                    if ui.add_enabled(idle, egui::Button::new("Disconnect")).clicked() {
                        self.request(h, Kind::Disconnect, BluetoothOperation::Disconnect { connection });
                    }
                    if self.own != Some(connection) {
                        ui.weak("(opened by the control API)");
                    }
                }
                None => {
                    let can = idle && bt.advertising;
                    if ui
                        .add_enabled(can, egui::Button::new("Connect"))
                        .on_disabled_hover_text("The firmware isn't advertising")
                        .clicked()
                    {
                        self.request(h, Kind::Connect, BluetoothOperation::Connect);
                    }
                }
            }
            if !idle {
                ui.spinner();
            }
        });

        let Some(connection) = bt.connection else { return };
        ui.horizontal_wrapped(|ui| {
            for (label, bytes, hover) in PRESETS {
                if ui.add_enabled(idle, egui::Button::new(label).small()).on_hover_text(hover).clicked() {
                    self.send_att(h, connection, bytes.to_vec());
                }
            }
        });
        ui.horizontal(|ui| {
            let parsed = parse_hex(&self.hex);
            let send = ui.add_enabled(idle && parsed.is_some(), egui::Button::new("Send"));
            let edit = ui.add(
                egui::TextEdit::singleline(&mut self.hex)
                    .hint_text("ATT bytes in hex, e.g. 0a 0300")
                    .font(egui::TextStyle::Monospace)
                    .desired_width(f32::INFINITY),
            );
            let enter = edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if let Some(data) = parsed
                && idle
                && (send.clicked() || enter)
            {
                self.send_att(h, connection, data);
            }
        });
        if self.own == Some(connection) {
            ui.weak("Notifications and indications are fetched automatically.");
        }
    }

    fn log_section(&mut self, ui: &mut Ui) {
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            let title = format!("LOG ({})", self.log.len());
            ui.label(RichText::new(title).small().strong().color(ui.visuals().weak_text_color()));
            if !self.log.is_empty() && ui.small_button("Clear").clicked() {
                self.log.clear();
            }
        });
        if self.log.is_empty() {
            ui.weak("None yet.");
        }
        let v = ui.visuals().clone();
        for row in self.log.iter().rev() {
            let (arrow, color) = match row.dir {
                Dir::Sent => ("→", v.text_color()),
                Dir::Reply => ("←", v.text_color()),
                Dir::Notify => ("⇠", v.selection.bg_fill),
                Dir::Info => ("·", v.weak_text_color()),
                Dir::Error => ("!", v.error_fg_color),
            };
            ui.horizontal_top(|ui| {
                ui.label(RichText::new(arrow).monospace().color(color));
                ui.add(egui::Label::new(RichText::new(&row.text).monospace().color(color)).wrap());
            });
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(" ")
}

/// Hex bytes, whitespace optional ("0a 0300" = [0x0a, 0x03, 0x00]); `None` unless 1..=517
/// whole bytes (the ATT limit the control API also applies).
fn parse_hex(s: &str) -> Option<Vec<u8>> {
    let digits: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    if digits.is_empty() || !digits.len().is_multiple_of(2) || digits.len() / 2 > 517 {
        return None;
    }
    (0..digits.len()).step_by(2).map(|i| u8::from_str_radix(&digits[i..i + 2], 16).ok()).collect()
}

/// A UUID from little-endian AD/ATT bytes: 16-bit as `0x180a`, 128-bit in canonical form.
fn uuid(le: &[u8]) -> String {
    match le.len() {
        2 => format!("0x{:04x}", u16::from_le_bytes([le[0], le[1]])),
        16 => {
            let b: Vec<u8> = le.iter().rev().copied().collect();
            let h = hex(&b).replace(' ', "");
            format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32])
        }
        _ => hex(le),
    }
}

/// Advertising data structures (Core Spec Supplement, Part A) as (field, value) rows.
fn parse_ad(data: &[u8]) -> Vec<(&'static str, String)> {
    let mut out = Vec::new();
    let mut rest = data;
    while let [len, tail @ ..] = rest {
        let len = *len as usize;
        if len == 0 || tail.len() < len {
            break;
        }
        let (ty, value) = (tail[0], &tail[1..len]);
        rest = &tail[len..];
        let uuids = |w: usize| value.chunks_exact(w).map(uuid).collect::<Vec<_>>().join(", ");
        out.push(match ty {
            0x01 => ("Flags", format!("0x{:02x}", value.first().copied().unwrap_or(0))),
            0x02 | 0x03 => ("Services", uuids(2)),
            0x06 | 0x07 => ("Services", uuids(16)),
            0x08 => ("Short name", String::from_utf8_lossy(value).into()),
            0x09 => ("Name", String::from_utf8_lossy(value).into()),
            0x0a => ("TX power", format!("{} dBm", value.first().map_or(0, |&p| p as i8))),
            0x19 => ("Appearance", hex(value)),
            0xff => ("Manufacturer", hex(value)),
            _ => ("AD type", format!("0x{ty:02x}: {}", hex(value))),
        });
    }
    out
}

/// An ATT PDU for the log: opcode name, a decoded summary where useful, then the raw bytes.
fn describe_att(data: &[u8]) -> String {
    let Some(&op) = data.first() else { return "(empty)".into() };
    let handle = |at: usize| data.get(at..at + 2).map(|b| u16::from_le_bytes([b[0], b[1]]));
    let name = match op {
        0x01 => "Error",
        0x02 => "MTU req",
        0x03 => "MTU rsp",
        0x04 => "Find info req",
        0x05 => "Find info rsp",
        0x08 => "Read by type req",
        0x09 => "Read by type rsp",
        0x0a => "Read req",
        0x0b => "Read rsp",
        0x10 => "Read by group req",
        0x11 => "Read by group rsp",
        0x12 => "Write req",
        0x13 => "Write rsp",
        0x1b => "Notification",
        0x1d => "Indication",
        0x52 => "Write cmd",
        _ => "ATT",
    };
    let detail = match op {
        // Error: request opcode, handle, error code.
        0x01 if data.len() >= 5 => {
            format!(" op 0x{:02x} handle 0x{:04x} code 0x{:02x}", data[1], handle(2).unwrap_or(0), data[4])
        }
        0x02 | 0x03 => handle(1).map(|m| format!(" {m}")).unwrap_or_default(),
        // Read By Group Type rsp: entry length, then (start, end, uuid) entries.
        0x11 if data.len() >= 2 && data[1] >= 6 => {
            let entries: Vec<_> = data[2..]
                .chunks_exact(data[1] as usize)
                .map(|e| {
                    let (s, end) = (u16::from_le_bytes([e[0], e[1]]), u16::from_le_bytes([e[2], e[3]]));
                    format!("{s:04x}-{end:04x} {}", uuid(&e[4..]))
                })
                .collect();
            format!(" {}", entries.join(", "))
        }
        0x0a | 0x12 | 0x1b | 0x1d | 0x52 => handle(1).map(|h| format!(" 0x{h:04x}")).unwrap_or_default(),
        _ => String::new(),
    };
    format!("{name}{detail} · {}", hex(data))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_parsing() {
        assert_eq!(parse_hex("0a 0300"), Some(vec![0x0a, 0x03, 0x00]));
        assert_eq!(parse_hex("0A0B"), Some(vec![0x0a, 0x0b]));
        assert_eq!(parse_hex("0a0"), None);
        assert_eq!(parse_hex("zz"), None);
        assert_eq!(parse_hex("  "), None);
    }

    #[test]
    fn advertisement() {
        // Flags, complete name "TRMNL", 16-bit service 0xffff.
        let ad = [2, 0x01, 0x06, 6, 0x09, b'T', b'R', b'M', b'N', b'L', 3, 0x03, 0xff, 0xff];
        let f = parse_ad(&ad);
        assert_eq!(f[0], ("Flags", "0x06".into()));
        assert_eq!(f[1], ("Name", "TRMNL".into()));
        assert_eq!(f[2], ("Services", "0xffff".into()));
        // A truncated structure stops parsing.
        assert_eq!(parse_ad(&[5, 0x09, b'a']).len(), 0);
        let u128: Vec<u8> = (0..16).collect();
        assert_eq!(uuid(&u128), "0f0e0d0c-0b0a-0908-0706-050403020100");
    }

    #[test]
    fn att_descriptions() {
        assert_eq!(describe_att(&[0x03, 0x00, 0x02]), "MTU rsp 512 · 03 00 02");
        assert_eq!(
            describe_att(&[0x11, 0x06, 0x01, 0x00, 0x05, 0x00, 0x00, 0x18]),
            "Read by group rsp 0001-0005 0x1800 · 11 06 01 00 05 00 00 18"
        );
        assert_eq!(
            describe_att(&[0x01, 0x10, 0x01, 0x00, 0x0a]),
            "Error op 0x10 handle 0x0001 code 0x0a · 01 10 01 00 0a"
        );
    }
}
