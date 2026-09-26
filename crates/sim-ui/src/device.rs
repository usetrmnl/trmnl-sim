//! The e-paper texture and the TRMNL OG-style device drawing.

use egui::{Color32, CornerRadius, Pos2, Rect, Stroke, StrokeKind, TextureFilter, TextureHandle, TextureOptions, Vec2};
use sim_api::SharedFrame;

/// Paper and ink colours used to render darkness 0..=255.
pub const PAPER: [u8; 3] = [0xe8, 0xe6, 0xdf];
pub const INK: [u8; 3] = [0x1d, 0x1d, 0x1b];

/// Bezel border as a fraction of the screen width.
pub const BEZEL_RATIO: f32 = 0.03;

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
        let (width, height, pixels) = {
            let f = self.frame.lock();
            if Some(f.generation) == self.generation && nearest == self.nearest && self.tex.is_some() {
                return false;
            }
            self.generation = Some(f.generation);
            (f.width, f.height, f.pixels.clone())
        };
        self.nearest = nearest;
        self.size = [width, height];
        if width == 0 || height == 0 || pixels.len() < width * height {
            return false;
        }
        let px: Vec<Color32> = pixels[..width * height].iter().map(|&d| self.lut[d as usize]).collect();
        let img = egui::ColorImage::new([width, height], px);
        let opts = Self::options(nearest);
        match &mut self.tex {
            Some(t) => t.set(img, opts),
            None => self.tex = Some(ctx.load_texture("epaper", img, opts)),
        }
        true
    }

    /// Paint the device (bezel + screen) filling `rect` exactly.
    pub fn paint(&self, painter: &egui::Painter, rect: Rect, zoom: f32, white_bezel: bool) {
        let ppp = painter.ctx().pixels_per_point();
        let snap = |p: Pos2| Pos2::new((p.x * ppp).round() / ppp, (p.y * ppp).round() / ppp);
        let border = self.size().x * BEZEL_RATIO * zoom;
        let radius = (border * 0.55).clamp(3.0, 40.0);

        // Soft drop shadow.
        let shadow = egui::epaint::Shadow {
            offset: [0, (border * 0.25).clamp(2.0, 12.0) as i8],
            blur: (border * 0.9).clamp(6.0, 40.0) as u8,
            spread: 0,
            color: Color32::from_black_alpha(if white_bezel { 50 } else { 80 }),
        };
        painter.add(shadow.as_shape(rect, radius));

        let (body, edge, lip) = if white_bezel {
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
        painter.rect_filled(rect, radius, body);
        painter.rect_stroke(rect, radius, Stroke::new(1.0, edge), StrokeKind::Inside);

        let screen = Rect::from_min_max(snap(rect.min + Vec2::splat(border)), snap(rect.max - Vec2::splat(border)));
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
    }
}
