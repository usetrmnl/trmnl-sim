//! The left panel's Portal tab: the device's own setup page, served by the firmware through the
//! host's portal client, in an embedded system webview (no separate browser needed). The tab
//! exists while the portal is up; `SimApp::sync_portal` opens and closes it.

use egui::{RichText, Ui};

pub(crate) struct PortalView {
    url: String,
    browser: Browser,
}

impl PortalView {
    pub fn new(url: &str) -> Self {
        PortalView { url: url.to_string(), browser: Browser::default() }
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    pub fn ui(&mut self, ui: &mut Ui, frame: &eframe::Frame) {
        ui.horizontal(|ui| {
            // Buttons first: the address gets (and truncates to) what they leave.
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.small_button("Open in browser").clicked() {
                    ui.ctx().open_url(egui::OpenUrl::new_tab(&self.url));
                }
                self.browser.reload_button(ui);
                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    ui.add(egui::Label::new(RichText::new(&self.url).monospace()).truncate());
                });
            });
        });
        ui.add_space(2.0);
        self.browser.show(ui, frame, &self.url);
    }

    /// Another tab is showing, or the panel is collapsed.
    pub fn hidden(&mut self) {
        self.browser.hide();
    }
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
use embedded::Browser;

#[cfg(any(target_os = "macos", target_os = "windows"))]
mod embedded {
    use egui::{Rect, Sense, Ui};
    use wry::dpi::{LogicalPosition, LogicalSize};

    /// A native child view floating over the egui window, kept on top of the area egui left for
    /// it. Created when the tab first shows.
    #[derive(Default)]
    pub struct Browser {
        webview: Option<wry::WebView>,
        error: Option<String>,
        bounds: Option<Rect>,
        visible: bool,
    }

    impl Browser {
        pub fn reload_button(&self, ui: &mut Ui) {
            if let Some(w) = &self.webview
                && ui.small_button("⟳ Reload").clicked()
            {
                let _ = w.reload();
            }
        }

        pub fn show(&mut self, ui: &mut Ui, frame: &eframe::Frame, url: &str) {
            let rect = ui.available_rect_before_wrap();
            ui.allocate_rect(rect, Sense::hover());
            if self.webview.is_none() && self.error.is_none() {
                match build(frame, url) {
                    Ok(w) => self.webview = Some(w),
                    Err(e) => self.error = Some(e.to_string()),
                }
            }
            let Some(webview) = &self.webview else {
                let error = self.error.as_deref().unwrap_or_default();
                ui.scope_builder(egui::UiBuilder::new().max_rect(rect), |ui| {
                    ui.colored_label(ui.visuals().error_fg_color, format!("No embedded browser: {error}"));
                });
                return;
            };
            // Points are the window's logical units scaled by egui's own zoom.
            let zoom = ui.ctx().zoom_factor();
            if self.bounds != Some(rect) || !self.visible {
                let _ = webview.set_bounds(wry::Rect {
                    position: LogicalPosition::new(rect.min.x * zoom, rect.min.y * zoom).into(),
                    size: LogicalSize::new(rect.width() * zoom, rect.height() * zoom).into(),
                });
                self.bounds = Some(rect);
            }
            if !self.visible {
                let _ = webview.set_visible(true);
                self.visible = true;
            }
            // A click in the page never reaches egui, so a press egui sees is outside it: give
            // the keyboard back (Space is the device's button).
            if ui.input(|i| i.pointer.any_pressed()) {
                let _ = webview.focus_parent();
            }
        }

        pub fn hide(&mut self) {
            if let Some(w) = &self.webview
                && self.visible
            {
                let _ = w.focus_parent();
                let _ = w.set_visible(false);
                self.visible = false;
            }
        }
    }

    impl Drop for Browser {
        /// A first responder removed from the window leaves the window without keyboard input.
        fn drop(&mut self) {
            self.hide();
        }
    }

    fn build(frame: &eframe::Frame, url: &str) -> wry::Result<wry::WebView> {
        let webview = wry::WebViewBuilder::new()
            .with_url(url)
            .with_visible(false)
            .with_focused(false)
            // Each run is its own device: nothing cached from another firmware's page.
            .with_incognito(true)
            // A file dropped on the page would otherwise navigate away from it.
            .with_drag_drop_handler(|_| true)
            .build_as_child(frame)?;
        // Opening by itself mustn't take the keyboard from the window.
        let _ = webview.focus_parent();
        Ok(webview)
    }
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
use no_webview::Browser;

/// Without a webview the tab only links to the page.
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
mod no_webview {
    use egui::Ui;

    #[derive(Default)]
    pub struct Browser {}

    impl Browser {
        pub fn reload_button(&self, _ui: &mut Ui) {}

        pub fn show(&mut self, ui: &mut Ui, _frame: &eframe::Frame, _url: &str) {
            ui.weak("The setup page can't be embedded on this platform: open it in your browser.");
        }

        pub fn hide(&mut self) {}
    }
}
