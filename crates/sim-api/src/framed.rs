//! The device body around the panel: proportions and colors shared by the window's drawing
//! and by screenshots that show the frame.

use crate::Frame;

/// Paper and ink colours used to render darkness 0..=255.
pub const PAPER: [u8; 3] = [0xe8, 0xe6, 0xdf];
pub const INK: [u8; 3] = [0x1d, 0x1d, 0x1b];

/// Bezel border as a fraction of the screen width.
pub const BEZEL_RATIO: f32 = 0.03;
/// Bezel below the screen as a fraction of the screen height (TRMNL OG / BWRY).
pub const CHIN_RATIO: f32 = 0.13;
/// The same for the TRMNL X.
pub const CHIN_RATIO_X: f32 = 0.12;

/// The color of the device body.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bezel {
    Black,
    White,
}

impl std::str::FromStr for Bezel {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "black" => Ok(Bezel::Black),
            "white" => Ok(Bezel::White),
            _ => Err(format!("frame must be black or white, not {s:?}")),
        }
    }
}

impl Bezel {
    /// Body fill, body edge and the lip around the glass.
    pub fn body_colors(self) -> [[u8; 3]; 3] {
        match self {
            Bezel::White => [[0xf3, 0xf2, 0xee], [0xc9, 0xc7, 0xc0], [0xd6, 0xd4, 0xcd]],
            Bezel::Black => [[0x23, 0x23, 0x24], [0x3c, 0x3c, 0x3e], [0x12, 0x12, 0x13]],
        }
    }

    /// Touch bar strip, its edge and its zone glyphs.
    pub fn touchbar_colors(self) -> [[u8; 3]; 3] {
        match self {
            Bezel::White => [[0xe7, 0xe6, 0xe1], [0xcc, 0xca, 0xc3], [0x9a, 0x98, 0x92]],
            Bezel::Black => [[0x2c, 0x2c, 0x2e], [0x3a, 0x3a, 0x3d], [0x7a, 0x7a, 0x80]],
        }
    }
}

/// Bezel border and the extra bezel below the screen, in panel pixels.
pub fn border_and_chin(width: f32, height: f32, touchbar: bool) -> (f32, f32) {
    let border = if touchbar { width * 0.018 } else { width * BEZEL_RATIO };
    // Measured on product photos: below the screen, the body is about 13% of the screen
    // height on the OG/BWRY and 12% on the X, against a thinner bezel elsewhere.
    let bottom = height * if touchbar { CHIN_RATIO_X } else { CHIN_RATIO };
    (border, (bottom - border).max(0.0))
}

/// Corner radius of a body whose bezel border is `border` wide.
pub fn corner_radius(border: f32, touchbar: bool) -> f32 {
    (border * if touchbar { 0.8 } else { 0.55 }).clamp(3.0, 40.0)
}

/// Width of the recessed lip around the glass.
pub fn lip_width(border: f32) -> f32 {
    (border * 0.12).clamp(1.0, 3.0)
}

impl Frame {
    /// The frame inside the device body: one image pixel per panel pixel, RGBA, transparent
    /// outside the body's rounded corners. Returns (width, height, data).
    pub fn framed(&self, touchbar: bool, bezel: Bezel) -> (usize, usize, Vec<u8>) {
        let (scr_w, scr_h) = (self.width as f32, self.height as f32);
        let (border, chin) = border_and_chin(scr_w, scr_h, touchbar);
        let border = border.round();
        let (body_w, body_h) = ((scr_w + 2.0 * border).round(), (scr_h + 2.0 * border + chin).round());
        let width = body_w as usize;
        let mut image = vec![0u8; width * body_h as usize * 4];
        let [fill, _, lip] = bezel.body_colors();
        fill_rounded_rect(&mut image, width, [0.0, 0.0, body_w, body_h], corner_radius(border, touchbar), fill);
        let l = lip_width(border);
        fill_rounded_rect(
            &mut image,
            width,
            [border - l, border - l, border + scr_w + l, border + scr_h + l],
            1.0,
            lip,
        );
        if touchbar {
            // The strip the window draws in the chin (sim-ui's `Geometry::zone_rects`).
            let chin_top = border + scr_h;
            let chin_h = (body_h - border * 0.5 - chin_top).max(1.0);
            let (w, h) = (scr_w * 0.62, (chin_h * 0.42).max(6.0));
            let (x, y) = (border + (scr_w - w) / 2.0, chin_top + (chin_h - h) / 2.0);
            fill_rounded_rect(&mut image, width, [x, y, x + w, y + h], h / 2.0, bezel.touchbar_colors()[0]);
        }
        // Paper and ink tones as in the window, so the screen stands out from a white bezel.
        let (_, _, channels, seen) = self.viewer(None);
        let tone =
            |i: usize, v: u8| (INK[i] as f32 + (PAPER[i] as f32 - INK[i] as f32) * v as f32 / 255.0).round() as u8;
        for (n, px) in seen.chunks(channels).enumerate() {
            let at = ((border as usize + n / self.width) * width + border as usize + n % self.width) * 4;
            for i in 0..3 {
                image[at + i] = tone(i, px[i % channels]);
            }
        }
        (width, body_h as usize, image)
    }
}

/// Blend an anti-aliased rounded rectangle `[x0, y0, x1, y1]` over `image` (RGBA, `width`
/// pixels per row).
fn fill_rounded_rect(image: &mut [u8], width: usize, [x0, y0, x1, y1]: [f32; 4], radius: f32, color: [u8; 3]) {
    let height = image.len() / 4 / width;
    for y in (y0.floor().max(0.0) as usize)..(y1.ceil() as usize).min(height) {
        for x in (x0.floor().max(0.0) as usize)..(x1.ceil() as usize).min(width) {
            let (cx, cy) = (x as f32 + 0.5, y as f32 + 0.5);
            let dx = (x0 + radius - cx).max(cx - (x1 - radius)).max(0.0);
            let dy = (y0 + radius - cy).max(cy - (y1 - radius)).max(0.0);
            let cover = (radius - dx.hypot(dy) + 0.5).clamp(0.0, 1.0);
            let px = &mut image[(y * width + x) * 4..][..4];
            let under = px[3] as f32 / 255.0 * (1.0 - cover);
            let alpha = cover + under;
            if alpha > 0.0 {
                for (i, c) in color.into_iter().enumerate() {
                    px[i] = ((c as f32 * cover + px[i] as f32 * under) / alpha).round() as u8;
                }
                px[3] = (alpha * 255.0).round() as u8;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn framed_puts_the_panel_inside_the_bezel() {
        let mut frame = Frame::new(800, 480);
        frame.pixels[0] = 255;
        let (width, height, image) = frame.framed(false, Bezel::Black);
        let px = |x: usize, y: usize| image[(y * width + x) * 4..][..4].to_vec();
        assert_eq!((width, height), (848, 566));
        assert_eq!(px(0, 0)[3], 0, "outside the rounded corner");
        assert_eq!(px(424, 2), [0x23, 0x23, 0x24, 255], "bezel");
        assert_eq!(px(24, 24), [INK[0], INK[1], INK[2], 255]);
        assert_eq!(px(823, 503), [PAPER[0], PAPER[1], PAPER[2], 255]);
        assert_eq!(px(424, 530), [0x23, 0x23, 0x24, 255], "chin");
    }
}
