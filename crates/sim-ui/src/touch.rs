//! Touch bar input (TRMNL X): turns mouse/keyboard holds into touch commands.
//!
//! A short click (released within [`TAP_WINDOW`]) becomes `Command::Touch { ms: TAP_MS }`, which
//! the emulator times in virtual time (so taps stay taps in turbo mode). Anything held longer
//! becomes `TouchDown` once the window expires and `TouchUp` on release. A latched zone
//! (shift-click) goes down immediately and stays down until clicked again, so two zones can be
//! held together (left + right is the WiFi-reset gesture).

use std::time::{Duration, Instant};

use sim_api::{Command, TouchZone};

pub const ZONES: [TouchZone; 3] = [TouchZone::Left, TouchZone::Center, TouchZone::Right];
pub const TAP_WINDOW: Duration = Duration::from_millis(180);
pub const TAP_MS: u64 = 120;

#[derive(Default, Clone, Copy)]
struct Zone {
    /// When the user started pressing (mouse/key/latch).
    active_since: Option<Instant>,
    /// When `TouchDown` was sent (None while still inside the tap window).
    down_at: Option<Instant>,
    latched: bool,
}

#[derive(Default)]
pub struct TouchInput {
    zones: [Zone; 3],
    /// Sources holding each zone this frame (reset by `begin_frame`).
    src: [bool; 3],
    /// Duration of the most recent completed touch (from touch-down, or the tap length).
    pub last_hold: Option<Duration>,
    /// Zones that were down together in the most recent multi-zone hold.
    pub last_combo: Option<[bool; 3]>,
}

impl TouchInput {
    pub fn begin_frame(&mut self) {
        self.src = [false; 3];
    }

    /// Something (mouse, key, panel button) holds zone `i` this frame.
    pub fn hold(&mut self, i: usize) {
        self.src[i] = true;
    }

    pub fn toggle_latch(&mut self, i: usize) {
        self.zones[i].latched = !self.zones[i].latched;
    }

    pub fn unlatch(&mut self, i: usize) {
        self.zones[i].latched = false;
    }

    pub fn is_latched(&self, i: usize) -> bool {
        self.zones[i].latched
    }

    pub fn is_active(&self, i: usize) -> bool {
        self.zones[i].active_since.is_some()
    }

    pub fn any_active(&self) -> bool {
        (0..3).any(|i| self.is_active(i))
    }

    /// Seconds since touch-down of the longest-held zone currently down.
    pub fn longest_hold(&self) -> Option<f32> {
        self.zones.iter().filter_map(|z| z.down_at).map(|t| t.elapsed().as_secs_f32()).reduce(f32::max)
    }

    pub fn down_mask(&self) -> [bool; 3] {
        std::array::from_fn(|i| self.zones[i].down_at.is_some())
    }

    /// Release everything (e.g. when the window loses focus).
    pub fn release_all(&mut self, send: impl FnMut(Command)) {
        self.src = [false; 3];
        for z in &mut self.zones {
            z.latched = false;
        }
        self.update(send);
    }

    /// Apply this frame's sources, sending commands for any transitions.
    pub fn update(&mut self, mut send: impl FnMut(Command)) {
        let now = Instant::now();
        let mask_before = self.down_mask();
        for (i, &zone) in ZONES.iter().enumerate() {
            let want = self.src[i] || self.zones[i].latched;
            let z = &mut self.zones[i];
            match (want, z.active_since) {
                (true, None) => {
                    z.active_since = Some(now);
                    if z.latched {
                        send(Command::TouchDown(zone));
                        z.down_at = Some(now);
                    }
                }
                (true, Some(since)) => {
                    if z.down_at.is_none() && (z.latched || now - since >= TAP_WINDOW) {
                        send(Command::TouchDown(zone));
                        z.down_at = Some(now);
                    }
                }
                (false, Some(since)) => {
                    if let Some(d) = z.down_at {
                        send(Command::TouchUp(zone));
                        self.last_hold = Some(now - d);
                    } else {
                        send(Command::Touch { zone, ms: TAP_MS });
                        self.last_hold = Some(Duration::from_millis(TAP_MS).max(now - since));
                    }
                    z.active_since = None;
                    z.down_at = None;
                }
                (false, None) => {}
            }
        }
        if mask_before.iter().filter(|&&b| b).count() >= 2 {
            self.last_combo = Some(mask_before);
        } else if self.down_mask().iter().any(|&b| b) {
            self.last_combo = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tap_then_hold() {
        let mut t = TouchInput::default();
        let mut sent = vec![];
        t.begin_frame();
        t.hold(0);
        t.update(|c| sent.push(format!("{c:?}")));
        assert!(sent.is_empty());
        t.begin_frame();
        t.update(|c| sent.push(format!("{c:?}")));
        assert_eq!(sent, vec!["Touch { zone: Left, ms: 120 }"]);

        sent.clear();
        t.toggle_latch(2);
        t.begin_frame();
        t.update(|c| sent.push(format!("{c:?}")));
        assert_eq!(sent, vec!["TouchDown(Right)"]);
        t.toggle_latch(2);
        t.begin_frame();
        t.update(|c| sent.push(format!("{c:?}")));
        assert_eq!(sent.last().unwrap(), "TouchUp(Right)");
    }
}
