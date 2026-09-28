//! The side panel's "Faults" section: one-click versions of the common faults
//! (`Command::SetFaults`); the control API has the full set.

use egui::Ui;
use sim_api::{Command, CutPoint, DnsFault, Faults, FlashOp, PowerLoss};

use crate::{Pending, SimApp, section};

/// BQ27427 fuel gauge (TRMNL X).
const FUEL_GAUGE: u8 = 0x55;

impl SimApp {
    pub(crate) fn faults_section(&mut self, ui: &mut Ui) {
        let board = self.board();
        let current = Pending::resolve(&mut self.faults_pending, self.status.faults.clone());
        let mut f = current.clone();

        section(ui, "Faults");
        // The network error modes, collapsed (the header says how many are on).
        let mut wifi = Pending::resolve(&mut self.wifi_pending, self.status.wifi_available);
        let n = &mut f.net;
        let slow = n.latency_ms > 0 || n.bandwidth_bps.is_some();
        let active =
            [!wifi, n.offline, n.no_internet, slow, n.loss > 0.0, n.dns.is_some()].iter().filter(|&&on| on).count();
        let title =
            if active > 0 { format!("WiFi / network errors ({active} on)") } else { "WiFi / network errors".into() };
        let mut wifi_changed = false;
        egui::CollapsingHeader::new(title).id_salt("network_faults").default_open(false).show(ui, |ui| {
            wifi_changed = ui
                .checkbox(&mut wifi, "Access point available")
                .on_hover_text("Off: the device's WiFi network disappears (out of range)")
                .changed();
            let mut offline = n.offline;
            if ui
                .checkbox(&mut offline, "Internet down")
                .on_hover_text("Only the host (10.0.2.2) is reachable")
                .changed()
            {
                n.offline = offline;
            }
            ui.checkbox(&mut n.no_internet, "No internet behind the AP")
                .on_hover_text("WiFi and DHCP work, but DNS and every connection time out");
            let mut slow = n.latency_ms > 0 || n.bandwidth_bps.is_some();
            if ui
                .checkbox(&mut slow, "Slow (300 ms, 16 kB/s)")
                .on_hover_text("Added latency and a bandwidth limit")
                .changed()
            {
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
        });
        if wifi_changed {
            self.send(Command::SetWifiAvailable(wifi));
            Pending::set(&mut self.wifi_pending, wifi);
        }

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
        if self.status.power_losses > 0 {
            ui.weak(format!("Power lost {} time(s) so far", self.status.power_losses));
        }

        if board.has_fuel_gauge {
            let mut absent = f.i2c_absent.contains(&FUEL_GAUGE);
            if ui
                .checkbox(&mut absent, "Fuel gauge absent")
                .on_hover_text("The BQ27427 NACKs every I2C transfer")
                .changed()
            {
                f.i2c_absent.retain(|&a| a != FUEL_GAUGE);
                if absent {
                    f.i2c_absent.push(FUEL_GAUGE);
                }
            }
        }
        let (label, hover) = if board.has_touchbar {
            ("Panel power fails", "The PMIC never reports power good (the X's panel has no BUSY line)")
        } else {
            ("Panel BUSY stuck", "The panel controller holds BUSY low forever")
        };
        ui.checkbox(&mut f.panel_busy_stuck, label).on_hover_text(hover);
        if board.has_5ghz {
            ui.checkbox(&mut f.modem_unresponsive, "Modem unresponsive")
                .on_hover_text("The 5 GHz modem stops answering AT commands");
        }
        if !f.is_empty() && ui.small_button("Clear all faults").clicked() {
            f = Faults::default();
        }

        if f != current {
            self.send(Command::SetFaults(f.clone()));
            Pending::set(&mut self.faults_pending, f);
        }
    }
}
