//! Serial console pane: a local, pre-processed mirror of `sim_api::Console`.

use std::collections::VecDeque;
use std::sync::Arc;

use egui::{Color32, RichText, TextStyle, Ui};
use parking_lot::Mutex;
use sim_api::Console;

const MAX_LINES: usize = 20_000;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LineKind {
    Plain,
    Sim,
    Error,
    Warn,
    Info,
    Debug,
}

pub struct Line {
    /// Absolute sequence number (index in the stream of all lines ever received).
    seq: u64,
    text: String,
    kind: LineKind,
}

pub struct ConsoleView {
    shared: Arc<Mutex<Console>>,
    /// `Console::total` value we have consumed up to.
    seen_total: u64,
    lines: VecDeque<Line>,
    /// Sequence numbers of lines that pass the filter (only maintained while a filter is set).
    filtered: VecDeque<u64>,
    filter: String,
    filter_lower: String,
    /// Scroll-to-bottom requested for the next frame.
    jump_to_bottom: bool,
    at_bottom: bool,
    /// Lines dropped from the front since last frame (for keeping the view stable while scrolled up).
    dropped_rows: usize,
    scroll_id: Option<egui::Id>,
}

impl ConsoleView {
    pub fn new(shared: Arc<Mutex<Console>>) -> Self {
        Self {
            shared,
            seen_total: 0,
            lines: VecDeque::new(),
            filtered: VecDeque::new(),
            filter: String::new(),
            filter_lower: String::new(),
            jump_to_bottom: true,
            at_bottom: true,
            dropped_rows: 0,
            scroll_id: None,
        }
    }

    /// Pull new lines from the shared console. Holds the lock only while cloning new lines.
    /// Returns true if anything new arrived.
    pub fn poll(&mut self) -> bool {
        let (total, new): (u64, Vec<String>) = {
            let c = self.shared.lock();
            if c.total == self.seen_total {
                return false;
            }
            if c.total < self.seen_total {
                // The console was replaced/reset underneath us.
                self.seen_total = 0;
            }
            let n_new = (c.total - self.seen_total).min(c.lines.len() as u64) as usize;
            let start = c.lines.len() - n_new;
            (c.total, c.lines.range(start..).cloned().collect())
        };
        let first_seq = total - new.len() as u64;
        self.seen_total = total;
        for (i, raw) in new.into_iter().enumerate() {
            let text = strip_ansi(&raw);
            let kind = classify(&text);
            let seq = first_seq + i as u64;
            if !self.filter_lower.is_empty() && matches(&text, &self.filter_lower) {
                self.filtered.push_back(seq);
            }
            self.lines.push_back(Line { seq, text, kind });
        }
        let mut dropped = 0;
        while self.lines.len() > MAX_LINES {
            let l = self.lines.pop_front().expect("nonempty");
            if self.filter_lower.is_empty() {
                dropped += 1;
            } else if self.filtered.front() == Some(&l.seq) {
                self.filtered.pop_front();
                dropped += 1;
            }
        }
        self.dropped_rows += dropped;
        true
    }

    pub fn clear(&mut self) {
        self.lines.clear();
        self.filtered.clear();
        self.jump_to_bottom = true;
    }

    fn set_filter(&mut self, f: String) {
        self.filter = f;
        self.filter_lower = self.filter.to_lowercase();
        self.filtered.clear();
        if !self.filter_lower.is_empty() {
            let fl = &self.filter_lower;
            self.filtered.extend(self.lines.iter().filter(|l| matches(&l.text, fl)).map(|l| l.seq));
        }
        self.jump_to_bottom = true;
    }

    fn row_count(&self) -> usize {
        if self.filter_lower.is_empty() { self.lines.len() } else { self.filtered.len() }
    }

    fn row(&self, r: usize) -> Option<&Line> {
        if self.filter_lower.is_empty() {
            self.lines.get(r)
        } else {
            let seq = *self.filtered.get(r)?;
            let base = self.lines.front()?.seq;
            self.lines.get((seq - base) as usize)
        }
    }

    fn all_text(&self) -> String {
        let n = self.row_count();
        let mut s = String::with_capacity(n * 64);
        for r in 0..n {
            if let Some(l) = self.row(r) {
                s.push_str(&l.text);
                s.push('\n');
            }
        }
        s
    }

    /// Toolbar row: filter, clear, copy, jump.
    pub fn toolbar(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            ui.label(RichText::new("Serial console").strong());
            ui.add_space(8.0);
            let mut f = self.filter.clone();
            let resp = ui.add(egui::TextEdit::singleline(&mut f).hint_text("Filter…").desired_width(220.0));
            if resp.changed() {
                self.set_filter(f);
            }
            if !self.filter.is_empty() && ui.small_button("✖").on_hover_text("Clear filter").clicked() {
                self.set_filter(String::new());
            }
            if ui.button("Clear").on_hover_text("Clear the console view").clicked() {
                self.clear();
            }
            if ui.button("Copy all").on_hover_text("Copy all (filtered) lines").clicked() {
                ui.ctx().copy_text(self.all_text());
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let shown = self.row_count();
                if self.filter.is_empty() {
                    ui.weak(format!("{shown} lines"));
                } else {
                    ui.weak(format!("{shown} / {} lines", self.lines.len()));
                }
                if !self.at_bottom && ui.button("⬇ Jump to bottom").clicked() {
                    self.jump_to_bottom = true;
                }
            });
        });
    }

    pub fn body(&mut self, ui: &mut Ui) {
        let dark = ui.visuals().dark_mode;
        let pal = Palette::new(dark);
        let row_h = ui.text_style_height(&TextStyle::Monospace);
        ui.spacing_mut().item_spacing.y = 0.0;
        let n = self.row_count();

        let frame = egui::Frame::NONE.fill(pal.bg).inner_margin(egui::Margin::symmetric(6, 4)).corner_radius(4.0);
        frame.show(ui, |ui| {
            let mut sa = egui::ScrollArea::both()
                .id_salt("console_scroll")
                .auto_shrink([false, false])
                .stick_to_bottom(true)
                .animated(false);
            if self.jump_to_bottom {
                sa = sa.vertical_scroll_offset(row_h * n as f32 + 1e6);
                self.jump_to_bottom = false;
            } else if self.dropped_rows > 0 && !self.at_bottom {
                // Keep the view anchored while old lines fall off the front.
                if let Some(st) = self.scroll_id.and_then(|id| egui::scroll_area::State::load(ui.ctx(), id)) {
                    let off = (st.offset.y - self.dropped_rows as f32 * row_h).max(0.0);
                    sa = sa.vertical_scroll_offset(off);
                }
            }
            self.dropped_rows = 0;
            let out = sa.show_rows(ui, row_h, n, |ui, range| {
                for r in range {
                    let Some(l) = self.row(r) else { continue };
                    let mut rt = RichText::new(&l.text).monospace();
                    rt = match l.kind {
                        LineKind::Plain | LineKind::Info => rt.color(pal.text),
                        LineKind::Sim => rt.color(pal.sim).italics(),
                        LineKind::Error => rt.color(pal.error),
                        LineKind::Warn => rt.color(pal.warn),
                        LineKind::Debug => rt.color(pal.dim),
                    };
                    ui.add(egui::Label::new(rt).extend());
                }
            });
            self.scroll_id = Some(out.id);
            let max_off = (out.content_size.y - out.inner_rect.height()).max(0.0);
            self.at_bottom = out.state.offset.y >= max_off - row_h * 0.5;
        });
    }
}

struct Palette {
    bg: Color32,
    text: Color32,
    sim: Color32,
    error: Color32,
    warn: Color32,
    dim: Color32,
}

impl Palette {
    fn new(dark: bool) -> Self {
        if dark {
            Palette {
                bg: Color32::from_rgb(0x16, 0x17, 0x19),
                text: Color32::from_rgb(0xd4, 0xd4, 0xd0),
                sim: Color32::from_rgb(0x5f, 0xb8, 0xc9),
                error: Color32::from_rgb(0xf2, 0x6d, 0x6d),
                warn: Color32::from_rgb(0xe8, 0xc0, 0x5a),
                dim: Color32::from_rgb(0x88, 0x8a, 0x8e),
            }
        } else {
            Palette {
                bg: Color32::from_rgb(0xfa, 0xfa, 0xf8),
                text: Color32::from_rgb(0x22, 0x22, 0x22),
                sim: Color32::from_rgb(0x0b, 0x6e, 0x85),
                error: Color32::from_rgb(0xc0, 0x1c, 0x1c),
                warn: Color32::from_rgb(0x9a, 0x67, 0x00),
                dim: Color32::from_rgb(0x80, 0x80, 0x80),
            }
        }
    }
}

fn matches(text: &str, filter_lower: &str) -> bool {
    if text.is_ascii() && filter_lower.is_ascii() {
        // Fast path: ASCII case-insensitive substring search without allocating.
        let (h, n) = (text.as_bytes(), filter_lower.as_bytes());
        n.len() <= h.len() && h.windows(n.len()).any(|w| w.eq_ignore_ascii_case(n))
    } else {
        text.to_lowercase().contains(filter_lower)
    }
}

pub fn classify(line: &str) -> LineKind {
    if line.starts_with("[sim]") {
        return LineKind::Sim;
    }
    let b = line.as_bytes();
    // ESP-IDF: "E (1234) tag: msg"
    if b.len() >= 3 && b[1] == b' ' && b[2] == b'(' {
        match b[0] {
            b'E' => return LineKind::Error,
            b'W' => return LineKind::Warn,
            b'I' => return LineKind::Info,
            b'D' | b'V' => return LineKind::Debug,
            _ => {}
        }
    }
    // Arduino-ESP32: "[  1234][E][file.cpp:12] func(): msg"
    if line.starts_with('[') {
        let head = &line[..line.len().min(24)];
        if head.contains("][E]") || head.starts_with("[E]") {
            return LineKind::Error;
        }
        if head.contains("][W]") || head.starts_with("[W]") {
            return LineKind::Warn;
        }
        if head.contains("][D]") || head.contains("][V]") {
            return LineKind::Debug;
        }
    }
    LineKind::Plain
}

/// Remove ANSI escape sequences and other control characters (tabs become spaces).
pub fn strip_ansi(s: &str) -> String {
    if !s.bytes().any(|b| b < 0x20 || b == 0x7f) {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        match c {
            '\x1b' => match it.peek() {
                Some('[') => {
                    it.next();
                    // CSI: parameters/intermediates, then a final byte in @..=~
                    for c in it.by_ref() {
                        if ('@'..='~').contains(&c) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    it.next();
                    // OSC: until BEL or ST (ESC \)
                    while let Some(c) = it.next() {
                        if c == '\x07' {
                            break;
                        }
                        if c == '\x1b' {
                            if it.peek() == Some(&'\\') {
                                it.next();
                            }
                            break;
                        }
                    }
                }
                Some(_) => {
                    it.next();
                }
                None => {}
            },
            '\t' => out.push_str("    "),
            c if (c as u32) < 0x20 || c == '\x7f' => {}
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ansi() {
        assert_eq!(strip_ansi("\x1b[0;31mE (12) x: y\x1b[0m"), "E (12) x: y");
        assert_eq!(strip_ansi("a\tb"), "a    b");
        assert_eq!(strip_ansi("plain"), "plain");
        assert_eq!(strip_ansi("\x1b]0;title\x07ok"), "ok");
    }

    #[test]
    fn kinds() {
        assert_eq!(classify("E (12) wifi: fail"), LineKind::Error);
        assert_eq!(classify("W (12) wifi: meh"), LineKind::Warn);
        assert_eq!(classify("I (12) wifi: ok"), LineKind::Info);
        assert_eq!(classify("[sim] hello"), LineKind::Sim);
        assert_eq!(classify("[   123][E][x.cpp:1] f(): e"), LineKind::Error);
        assert_eq!(classify("Every day"), LineKind::Plain);
        assert!(matches("Hello World", "world"));
        assert!(!matches("Hello", "xyz"));
    }
}
