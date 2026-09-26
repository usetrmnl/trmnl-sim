//! The e-paper texture and the TRMNL OG-style device drawing.

use egui::{Color32, CornerRadius, Pos2, Rect, Stroke, StrokeKind, TextureFilter, TextureHandle, TextureOptions, Vec2};
use sim_api::SharedFrame;

/// Paper and ink colours used to render darkness 0..=255.
pub const PAPER: [u8; 3] = [0xe8, 0xe6, 0xdf];
pub const INK: [u8; 3] = [0x1d, 0x1d, 0x1b];

/// Bezel border as a fraction of the screen width.
pub const BEZEL_RATIO: f32 = 0.03;
/// Bezel below the screen as a fraction of the screen height (TRMNL OG / BWRY).
pub const CHIN_RATIO: f32 = 0.13;
/// The same for the TRMNL X.
pub const CHIN_RATIO_X: f32 = 0.12;

pub struct Screen {
    frame: SharedFrame,
    tex: Option<TextureHandle>,
    generation: Option<u64>,
    nearest: bool,
    size: [usize; 2],
    lut: [Color32; 256],
}

impl Screen {
    pub fn new(frame: SharedFrame) -> Self {
        let lut = std::array::from_fn(|d| {
            let t = d as f32 / 255.0;
            let ch = |i: usize| (PAPER[i] as f32 + (INK[i] as f32 - PAPER[i] as f32) * t).round() as u8;
            Color32::from_rgb(ch(0), ch(1), ch(2))
        });
        let size = {
            let f = frame.lock();
            [f.width, f.height]
        };
        Self { frame, tex: None, generation: None, nearest: true, size, lut }
    }

    /// Panel size in pixels (falls back to 800x480 while the frame is empty).
    pub fn size(&self) -> Vec2 {
        if self.size[0] == 0 || self.size[1] == 0 {
            Vec2::new(800.0, 480.0)
        } else {
            Vec2::new(self.size[0] as f32, self.size[1] as f32)
        }
    }

    fn options(nearest: bool) -> TextureOptions {
        if nearest {
            TextureOptions::NEAREST
        } else {
            TextureOptions { mipmap_mode: Some(TextureFilter::Linear), ..TextureOptions::LINEAR }
        }
    }

    /// Upload the frame if its generation changed (or the filter mode must change).
    /// Returns true if the texture was updated.
    pub fn update(&mut self, ctx: &egui::Context, nearest: bool) -> bool {
        let (width, height, pixels, rgb) = {
            let f = self.frame.lock();
            if Some(f.generation) == self.generation && nearest == self.nearest && self.tex.is_some() {
                return false;
            }
            self.generation = Some(f.generation);
            (f.width, f.height, f.pixels.clone(), f.rgb.clone())
        };
        self.nearest = nearest;
        self.size = [width, height];
        if width == 0 || height == 0 || pixels.len() < width * height {
            return false;
        }
        let px: Vec<Color32> = match rgb.filter(|c| c.len() >= width * height * 3) {
            // Color panels: each channel spans ink..paper, so white reads as paper and
            // black as ink, like the gray LUT.
            Some(c) => c[..width * height * 3]
                .chunks(3)
                .map(|p| {
                    let ch = |i: usize| {
                        (INK[i] as f32 + (PAPER[i] as f32 - INK[i] as f32) * p[i] as f32 / 255.0).round() as u8
                    };
                    Color32::from_rgb(ch(0), ch(1), ch(2))
                })
                .collect(),
            None => pixels[..width * height].iter().map(|&d| self.lut[d as usize]).collect(),
        };
        let img = egui::ColorImage::new([width, height], px);
        let opts = Self::options(nearest);
        match &mut self.tex {
            Some(t) => t.set(img, opts),
            None => self.tex = Some(ctx.load_texture("epaper", img, opts)),
        }
        true
    }

    /// Paint the device. `body` is the device body (bezel incl. chin; excludes the dock).
    pub fn paint(&self, painter: &egui::Painter, body: Rect, zoom: f32, geom: &Geometry, look: &Look) {
        let ppp = painter.ctx().pixels_per_point();
        let snap = |p: Pos2| Pos2::new((p.x * ppp).round() / ppp, (p.y * ppp).round() / ppp);
        let border = geom.border * zoom;
        let radius = (border * if geom.touchbar { 0.8 } else { 0.55 }).clamp(3.0, 40.0);
        let white = look.white_bezel;

        if look.docked && geom.dock > 0.0 {
            paint_dock(painter, body, geom.dock * zoom, white);
        }

        // Soft drop shadow.
        let shadow = egui::epaint::Shadow {
            offset: [0, (border * 0.25).clamp(2.0, 12.0) as i8],
            blur: (border * 0.9).clamp(6.0, 40.0) as u8,
            spread: 0,
            color: Color32::from_black_alpha(if white { 50 } else { 80 }),
        };
        painter.add(shadow.as_shape(body, radius));

        let (fill, edge, lip) = if white {
            (
                Color32::from_rgb(0xf3, 0xf2, 0xee),
                Color32::from_rgb(0xc9, 0xc7, 0xc0),
                Color32::from_rgb(0xd6, 0xd4, 0xcd),
            )
        } else {
            (
                Color32::from_rgb(0x23, 0x23, 0x24),
                Color32::from_rgb(0x3c, 0x3c, 0x3e),
                Color32::from_rgb(0x12, 0x12, 0x13),
            )
        };
        painter.rect_filled(body, radius, fill);
        painter.rect_stroke(body, radius, Stroke::new(1.0, edge), StrokeKind::Inside);

        let screen = geom.screen_rect(body, zoom, self.size());
        let screen = Rect::from_min_max(snap(screen.min), snap(screen.max));
        // A thin recessed lip around the glass.
        let lip_w = (border * 0.12).clamp(1.0, 3.0);
        painter.rect_filled(screen.expand(lip_w), CornerRadius::same(1), lip);
        match &self.tex {
            Some(t) => {
                painter.image(t.id(), screen, Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), Color32::WHITE);
            }
            None => {
                painter.rect_filled(screen, 0.0, Color32::from_rgb(PAPER[0], PAPER[1], PAPER[2]));
            }
        }

        if geom.touchbar {
            let zones = geom.zone_rects(body, zoom, self.size());
            paint_touchbar(painter, &zones, &look.zones, white, look.accent);
        }
    }
}

/// Visual state of a touch bar zone.
#[derive(Clone, Copy, Default, PartialEq)]
pub enum ZoneVis {
    #[default]
    Idle,
    Hover,
    /// Pressed locally but not yet reported down (tap window) or reported by the emulator.
    Active,
    Latched,
}

pub struct Look {
    pub white_bezel: bool,
    pub docked: bool,
    pub zones: [ZoneVis; 3],
    pub accent: Color32,
}

/// Device proportions in panel pixels (multiply by zoom for points).
#[derive(Clone, Copy, Debug)]
pub struct Geometry {
    pub border: f32,
    /// Extra bezel below the screen (on the TRMNL X it holds the touch bar).
    pub chin: f32,
    /// Space reserved below the body for the dock.
    pub dock: f32,
    pub touchbar: bool,
}

impl Geometry {
    pub fn new(scr: Vec2, touchbar: bool, has_dock: bool) -> Self {
        let border = if touchbar { scr.x * 0.018 } else { scr.x * BEZEL_RATIO };
        // Measured on product photos: below the screen, the body is about 13% of the screen
        // height on the OG/BWRY and 12% on the X, against a thinner bezel elsewhere.
        let bottom = scr.y * if touchbar { CHIN_RATIO_X } else { CHIN_RATIO };
        let chin = (bottom - border).max(0.0);
        let dock = if has_dock { scr.y * 0.05 } else { 0.0 };
        Geometry { border, chin, dock, touchbar }
    }

    /// Body (bezel + chin) size at zoom 1.
    pub fn body(&self, scr: Vec2) -> Vec2 {
        Vec2::new(scr.x + 2.0 * self.border, scr.y + 2.0 * self.border + self.chin)
    }

    /// Body plus the dock allowance, at zoom 1.
    pub fn total(&self, scr: Vec2) -> Vec2 {
        self.body(scr) + Vec2::new(0.0, self.dock)
    }

    pub fn screen_rect(&self, body: Rect, zoom: f32, scr: Vec2) -> Rect {
        Rect::from_min_size(body.min + Vec2::splat(self.border * zoom), scr * zoom)
    }

    /// Left / center / right touch zones inside the chin.
    pub fn zone_rects(&self, body: Rect, zoom: f32, scr: Vec2) -> [Rect; 3] {
        let screen = self.screen_rect(body, zoom, scr);
        let chin_top = screen.max.y;
        let chin_bottom = body.max.y - self.border * zoom * 0.5;
        let chin_h = (chin_bottom - chin_top).max(1.0);
        let w = screen.width() * 0.62;
        let h = (chin_h * 0.42).max(6.0);
        let strip = Rect::from_center_size(Pos2::new(screen.center().x, chin_top + chin_h * 0.5), Vec2::new(w, h));
        std::array::from_fn(|i| {
            let x0 = strip.min.x + strip.width() * i as f32 / 3.0;
            Rect::from_min_size(Pos2::new(x0, strip.min.y), Vec2::new(strip.width() / 3.0, h))
        })
    }
}

fn paint_dock(painter: &egui::Painter, body: Rect, dock_h: f32, white: bool) {
    let (fill, edge, top) = if white {
        (Color32::from_rgb(0xdc, 0xda, 0xd3), Color32::from_rgb(0xb9, 0xb7, 0xb0), Color32::from_rgb(0xee, 0xed, 0xe8))
    } else {
        (Color32::from_rgb(0x30, 0x30, 0x33), Color32::from_rgb(0x48, 0x48, 0x4c), Color32::from_rgb(0x3e, 0x3e, 0x42))
    };
    let w = body.width() * 0.78;
    let base = Rect::from_min_max(
        Pos2::new(body.center().x - w / 2.0, body.max.y - dock_h * 0.9),
        Pos2::new(body.center().x + w / 2.0, body.max.y + dock_h),
    );
    let r = (dock_h * 0.35).clamp(2.0, 24.0);
    painter.add(
        egui::epaint::Shadow {
            offset: [0, (dock_h * 0.2).clamp(1.0, 8.0) as i8],
            blur: (dock_h * 0.8).clamp(4.0, 30.0) as u8,
            spread: 0,
            color: Color32::from_black_alpha(60),
        }
        .as_shape(base, r),
    );
    painter.rect_filled(base, r, fill);
    painter.rect_stroke(base, r, Stroke::new(1.0, edge), StrokeKind::Inside);
    // Top highlight where the device rests.
    painter.line_segment(
        [Pos2::new(base.min.x + r, body.max.y + 1.0), Pos2::new(base.max.x - r, body.max.y + 1.0)],
        Stroke::new(1.0, top),
    );
    // Charging LED on the front of the dock.
    let led = Pos2::new(base.max.x - r - dock_h * 0.4, body.max.y + dock_h * 0.5);
    let lr = (dock_h * 0.09).clamp(1.5, 4.0);
    painter.circle_filled(led, lr * 2.2, Color32::from_rgba_unmultiplied(0x3c, 0xd0, 0x6a, 40));
    painter.circle_filled(led, lr, Color32::from_rgb(0x3c, 0xd0, 0x6a));
}

fn paint_touchbar(painter: &egui::Painter, zones: &[Rect; 3], vis: &[ZoneVis; 3], white: bool, accent: Color32) {
    let strip = zones[0].union(zones[2]);
    let r = strip.height() / 2.0;
    let (base, edge, mark) = if white {
        (Color32::from_rgb(0xe7, 0xe6, 0xe1), Color32::from_rgb(0xcc, 0xca, 0xc3), Color32::from_rgb(0x9a, 0x98, 0x92))
    } else {
        (Color32::from_rgb(0x2c, 0x2c, 0x2e), Color32::from_rgb(0x3a, 0x3a, 0x3d), Color32::from_rgb(0x7a, 0x7a, 0x80))
    };
    painter.rect_filled(strip, r, base);
    for (i, (z, v)) in zones.iter().zip(vis).enumerate() {
        let cr = match i {
            0 => CornerRadius { nw: r as u8, sw: r as u8, ne: 0, se: 0 },
            2 => CornerRadius { ne: r as u8, se: r as u8, nw: 0, sw: 0 },
            _ => CornerRadius::ZERO,
        };
        let overlay = match v {
            ZoneVis::Idle => None,
            ZoneVis::Hover => Some(accent.gamma_multiply(0.25)),
            ZoneVis::Active => Some(accent),
            ZoneVis::Latched => Some(accent.gamma_multiply(0.75)),
        };
        if let Some(c) = overlay {
            painter.rect_filled(z.shrink(0.5), cr, c);
        }
        let on = matches!(v, ZoneVis::Active | ZoneVis::Latched);
        let col = if on { Color32::WHITE } else { mark };
        // Zone glyph drawn with lines (no font dependency): ‹  •  ›
        let c = z.center();
        let s = (z.height() * 0.22).clamp(2.0, 9.0);
        let stroke = Stroke::new((s * 0.35).clamp(1.0, 2.5), col);
        match i {
            0 => {
                painter.line_segment([c + Vec2::new(s * 0.4, -s), c + Vec2::new(-s * 0.4, 0.0)], stroke);
                painter.line_segment([c + Vec2::new(-s * 0.4, 0.0), c + Vec2::new(s * 0.4, s)], stroke);
            }
            1 => {
                painter.circle_filled(c, s * 0.45, col);
            }
            _ => {
                painter.line_segment([c + Vec2::new(-s * 0.4, -s), c + Vec2::new(s * 0.4, 0.0)], stroke);
                painter.line_segment([c + Vec2::new(s * 0.4, 0.0), c + Vec2::new(-s * 0.4, s)], stroke);
            }
        }
        if i > 0 {
            let x = z.min.x;
            painter.line_segment(
                [Pos2::new(x, z.min.y + z.height() * 0.25), Pos2::new(x, z.max.y - z.height() * 0.25)],
                Stroke::new(1.0, edge),
            );
        }
    }
    painter.rect_stroke(strip, r, Stroke::new(1.0, edge), StrokeKind::Inside);
}
