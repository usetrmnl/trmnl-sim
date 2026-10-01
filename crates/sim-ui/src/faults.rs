//! The left panel's Faults tab: one-click versions of the common faults
//! (`Command::SetFaults`, `Command::SetWifiAvailable`); the control API has the full set.

use egui::Ui;
use sim_api::{BoardInfo, Command, CutPoint, DnsFault, Faults, FlashOp, PowerLoss};

use crate::{Pending, SimApp, section};

/// BQ27427 fuel gauge (TRMNL X).
const FUEL_GAUGE: u8 = 0x55;

impl SimApp {
    /// Whether any fault is on, including ones only the control API sets (the tab is starred).
    pub(crate) fn faults_on(&mut self) -> bool {
        let f = Pending::resolve(&mut self.faults_pending, self.status.faults.clone());
        let wifi = Pending::resolve(&mut self.wifi_pending, self.status.wifi_available);
        !wifi || !f.is_empty()
    }

    pub(crate) fn faults_tab(&mut self, ui: &mut Ui) {
        let board = self.board();
        let current = Pending::resolve(&mut self.faults_pending, self.status.faults.clone());
        let mut f = current.clone();
        let wifi_before = Pending::resolve(&mut self.wifi_pending, self.status.wifi_available);
        let mut wifi = wifi_before;

        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            network(ui, &mut f, &mut wifi);
            self.nvs(ui, &mut f);
            panel(ui, &mut f, &board);
            if board.has_fuel_gauge {
                section(ui, "Fuel gauge");
                let mut absent = f.i2c_absent.contains(&FUEL_GAUGE);
                if ui.checkbox(&mut absent, "Absent").on_hover_text("The BQ27427 NACKs every I2C transfer").changed() {
                    f.i2c_absent.retain(|&a| a != FUEL_GAUGE);
                    if absent {
                        f.i2c_absent.push(FUEL_GAUGE);
                    }
                }
            }
            if board.has_5ghz {
                section(ui, "Modem");
                ui.checkbox(&mut f.modem_unresponsive, "Unresponsive")
                    .on_hover_text("The 5 GHz modem stops answering AT commands");
            }
            if (!f.is_empty() || !wifi) && ui.small_button("Clear all faults").clicked() {
                f = Faults::default();
                wifi = true;
            }
        });

        if wifi != wifi_before {
            self.send(Command::SetWifiAvailable(wifi));
            Pending::set(&mut self.wifi_pending, wifi);
        }
        if f != current {
            self.send(Command::SetFaults(f.clone()));
            Pending::set(&mut self.faults_pending, f);
        }
    }

    /// Cut power in the middle of the next NVS write.
    fn nvs(&self, ui: &mut Ui, f: &mut Faults) {
        section(ui, "NVS");
        ui.horizontal_wrapped(|ui| {
            if f.power_loss.is_some() {
                if ui.button("Cancel power loss").on_hover_text("Disarm the pending power-loss trigger").clicked() {
                    f.power_loss = None;
                }
                ui.weak("armed");
            } else if ui
                .button("⚡ Cut power on next NVS write")
                .on_hover_text("Cut power in the middle of the next NVS flash write (leaving it torn), then restore it")
                .clicked()
            {
                f.power_loss = Some(PowerLoss {
                    op: FlashOp::Any,
                    partition: Some("nvs".into()),
                    cut: CutPoint::Torn,
                    ..Default::default()
                });
            }
        });
        let power_losses = self.status.power_losses;
        if power_losses > 0 {
            ui.weak(format!("Power lost {power_losses} time(s) so far"));
        }
    }
}

/// The access point and the user-mode network.
fn network(ui: &mut Ui, f: &mut Faults, wifi: &mut bool) {
    section(ui, "Network");
    ui.checkbox(wifi, "Access point available")
        .on_hover_text("Off: the device's WiFi network disappears (out of range)");
    let n = &mut f.net;
    let mut offline = n.offline;
    if ui.checkbox(&mut offline, "Internet down").on_hover_text("Only the host (10.0.2.2) is reachable").changed() {
        n.offline = offline;
    }
    ui.checkbox(&mut n.no_internet, "No internet behind the AP")
        .on_hover_text("WiFi and DHCP work, but DNS and every connection time out");
    let mut slow = n.latency_ms > 0 || n.bandwidth_bps.is_some();
    if ui.checkbox(&mut slow, "Slow (300 ms, 16 kB/s)").on_hover_text("Added latency and a bandwidth limit").changed() {
        (n.latency_ms, n.bandwidth_bps) = if slow { (300, Some(16_000)) } else { (0, None) };
    }
    let mut lossy = n.loss > 0.0;
    if ui.checkbox(&mut lossy, "Lossy (10% packet loss)").changed() {
        n.loss = if lossy { 0.1 } else { 0.0 };
    }
    let mut dns = n.dns.is_some();
    if ui.checkbox(&mut dns, "DNS fails").on_hover_text("Every lookup answers SERVFAIL").changed() {
        n.dns = dns.then_some(DnsFault::ServFail);
    }
}

fn panel(ui: &mut Ui, f: &mut Faults, board: &BoardInfo) {
    section(ui, "Panel");
    let (label, hover) = if board.has_touchbar {
        ("Power fails", "The PMIC never reports power good (the X's panel has no BUSY line)")
    } else {
        ("BUSY stuck", "The panel controller holds BUSY low forever")
    };
    ui.checkbox(&mut f.panel_busy_stuck, label).on_hover_text(hover);
}
