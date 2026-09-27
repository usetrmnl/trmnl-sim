//! Built-in pictures: the default screen (so the server works out of the box) and the
//! "identify" screen. Drawn with a 5×7 pixel font, then converted like any upload.

use image::{Rgb, RgbImage};

use crate::convert::Panel;

const BLACK: Rgb<u8> = Rgb([0, 0, 0]);
const WHITE: Rgb<u8> = Rgb([255, 255, 255]);

/// 5×7 glyphs, one row per byte (bit 4 = leftmost column).
fn glyph(c: char) -> [u8; 7] {
    match c.to_ascii_uppercase() {
        'A' => [0b01110, 0b10001, 0b10001, 0b11111, 0b10001, 0b10001, 0b10001],
        'B' => [0b11110, 0b10001, 0b10001, 0b11110, 0b10001, 0b10001, 0b11110],
        'C' => [0b01110, 0b10001, 0b10000, 0b10000, 0b10000, 0b10001, 0b01110],
        'D' => [0b11100, 0b10010, 0b10001, 0b10001, 0b10001, 0b10010, 0b11100],
        'E' => [0b11111, 0b10000, 0b10000, 0b11110, 0b10000, 0b10000, 0b11111],
        'F' => [0b11111, 0b10000, 0b10000, 0b11110, 0b10000, 0b10000, 0b10000],
        'G' => [0b01110, 0b10001, 0b10000, 0b10111, 0b10001, 0b10001, 0b01111],
        'H' => [0b10001, 0b10001, 0b10001, 0b11111, 0b10001, 0b10001, 0b10001],
        'I' => [0b01110, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110],
        'J' => [0b00111, 0b00010, 0b00010, 0b00010, 0b00010, 0b10010, 0b01100],
        'K' => [0b10001, 0b10010, 0b10100, 0b11000, 0b10100, 0b10010, 0b10001],
        'L' => [0b10000, 0b10000, 0b10000, 0b10000, 0b10000, 0b10000, 0b11111],
        'M' => [0b10001, 0b11011, 0b10101, 0b10101, 0b10001, 0b10001, 0b10001],
        'N' => [0b10001, 0b10001, 0b11001, 0b10101, 0b10011, 0b10001, 0b10001],
        'O' => [0b01110, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110],
        'P' => [0b11110, 0b10001, 0b10001, 0b11110, 0b10000, 0b10000, 0b10000],
        'Q' => [0b01110, 0b10001, 0b10001, 0b10001, 0b10101, 0b10010, 0b01101],
        'R' => [0b11110, 0b10001, 0b10001, 0b11110, 0b10100, 0b10010, 0b10001],
        'S' => [0b01111, 0b10000, 0b10000, 0b01110, 0b00001, 0b00001, 0b11110],
        'T' => [0b11111, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100],
        'U' => [0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110],
        'V' => [0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01010, 0b00100],
        'W' => [0b10001, 0b10001, 0b10001, 0b10101, 0b10101, 0b10101, 0b01010],
        'X' => [0b10001, 0b10001, 0b01010, 0b00100, 0b01010, 0b10001, 0b10001],
        'Y' => [0b10001, 0b10001, 0b10001, 0b01010, 0b00100, 0b00100, 0b00100],
        'Z' => [0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b10000, 0b11111],
        '0' => [0b01110, 0b10001, 0b10011, 0b10101, 0b11001, 0b10001, 0b01110],
        '1' => [0b00100, 0b01100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110],
        '2' => [0b01110, 0b10001, 0b00001, 0b00010, 0b00100, 0b01000, 0b11111],
        '3' => [0b11111, 0b00010, 0b00100, 0b00010, 0b00001, 0b10001, 0b01110],
        '4' => [0b00010, 0b00110, 0b01010, 0b10010, 0b11111, 0b00010, 0b00010],
        '5' => [0b11111, 0b10000, 0b11110, 0b00001, 0b00001, 0b10001, 0b01110],
        '6' => [0b00110, 0b01000, 0b10000, 0b11110, 0b10001, 0b10001, 0b01110],
        '7' => [0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b01000, 0b01000],
        '8' => [0b01110, 0b10001, 0b10001, 0b01110, 0b10001, 0b10001, 0b01110],
        '9' => [0b01110, 0b10001, 0b10001, 0b01111, 0b00001, 0b00010, 0b01100],
        ' ' => [0; 7],
        '-' => [0, 0, 0, 0b11111, 0, 0, 0],
        '.' => [0, 0, 0, 0, 0, 0b01100, 0b01100],
        ':' => [0, 0b01100, 0b01100, 0, 0b01100, 0b01100, 0],
        '/' => [0b00001, 0b00001, 0b00010, 0b00100, 0b01000, 0b10000, 0b10000],
        '#' => [0b01010, 0b01010, 0b11111, 0b01010, 0b11111, 0b01010, 0b01010],
        '!' => [0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0, 0b00100],
        '_' => [0, 0, 0, 0, 0, 0, 0b11111],
        '+' => [0, 0b00100, 0b00100, 0b11111, 0b00100, 0b00100, 0],
        '(' => [0b00010, 0b00100, 0b01000, 0b01000, 0b01000, 0b00100, 0b00010],
        ')' => [0b01000, 0b00100, 0b00010, 0b00010, 0b00010, 0b00100, 0b01000],
        _ => [0b01110, 0b10001, 0b00001, 0b00010, 0b00100, 0, 0b00100], // '?'
    }
}

/// Width in pixels of `text` at `scale` (6 columns per glyph, no trailing gap).
fn text_width(text: &str, scale: u32) -> u32 {
    (text.chars().count() as u32 * 6).saturating_sub(1) * scale
}

fn fill(img: &mut RgbImage, x0: u32, y0: u32, w: u32, h: u32, c: Rgb<u8>) {
    for y in y0..(y0 + h).min(img.height()) {
        for x in x0..(x0 + w).min(img.width()) {
            img.put_pixel(x, y, c);
        }
    }
}

/// Draw `text` horizontally centered with its top at `y`.
fn text_centered(img: &mut RgbImage, text: &str, y: u32, scale: u32, c: Rgb<u8>) {
    let mut x = img.width().saturating_sub(text_width(text, scale)) / 2;
    for ch in text.chars() {
        for (row, bits) in glyph(ch).iter().enumerate() {
            for col in 0..5 {
                if bits & (0x10 >> col) != 0 {
                    fill(img, x + col * scale, y + row as u32 * scale, scale, scale, c);
                }
            }
        }
        x += 6 * scale;
    }
}

/// The largest scale at which `text` spans at most `frac` of the width.
fn scale_for(img: &RgbImage, text: &str, frac: f32) -> u32 {
    ((img.width() as f32 * frac) as u32 / text_width(text, 1).max(1)).max(1)
}

/// The screen served until you add your own images: a title, a gray ramp and the BWRY
/// inks as swatches, which show off the conversion on every panel.
pub fn default_screen(panel: Panel) -> RgbImage {
    let (w, h) = panel.size();
    let mut img = RgbImage::from_pixel(w, h, WHITE);
    let s = scale_for(&img, "TRMNL SIM", 0.66);
    let title_y = h * 18 / 100;
    text_centered(&mut img, "TRMNL SIM", title_y, s, BLACK);
    let sub = (s / 3).max(1);
    let sub_y = title_y + 7 * s + 2 * s;
    text_centered(&mut img, "BUILT-IN SERVER", sub_y, sub, BLACK);

    // Swatches: black, red, yellow, white (framed).
    let sw = w / 10;
    let sh = h / 9;
    let x0 = (w - 4 * sw - 3 * sw / 4) / 2;
    let y0 = sub_y + 7 * sub + 3 * sub;
    for (i, c) in [[0, 0, 0], [255, 0, 0], [255, 255, 0], [255, 255, 255]].into_iter().enumerate() {
        let x = x0 + i as u32 * (sw + sw / 4);
        fill(&mut img, x, y0, sw, sh, BLACK);
        let b = (w / 400).max(2);
        fill(&mut img, x + b, y0 + b, sw - 2 * b, sh - 2 * b, Rgb(c));
    }

    // A black-to-white ramp across the bottom.
    let ry = y0 + sh + sh / 2;
    let rh = h / 12;
    let (rx, rw) = (w / 10, w * 8 / 10);
    for x in 0..rw {
        let v = (x * 255 / (rw - 1)) as u8;
        fill(&mut img, rx + x, ry, 1, rh, Rgb([v; 3]));
    }
    let hint = "DROP IMAGES ON THE SIMULATOR WINDOW";
    let hs = scale_for(&img, hint, 0.6).min(sub);
    text_centered(&mut img, hint, ry + rh + rh / 2, hs, BLACK);
    img
}

/// The screen for the `identify` special function: the device's friendly ID, large.
pub fn identify_screen(panel: Panel, friendly_id: &str) -> RgbImage {
    let (w, h) = panel.size();
    let mut img = RgbImage::from_pixel(w, h, WHITE);
    let s = scale_for(&img, friendly_id, 0.7).min(h / 12);
    let y = (h - 7 * s) / 2;
    text_centered(&mut img, friendly_id, y, s, BLACK);
    let small = (s / 3).max(1);
    text_centered(&mut img, "IDENTIFY", y.saturating_sub(12 * small), small, BLACK);
    img
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn screens_fit_their_panels() {
        for panel in [Panel::Og, Panel::Bwry, Panel::X, Panel::Spectra6] {
            let img = default_screen(panel);
            assert_eq!(img.dimensions(), panel.size());
            // Some ink, mostly paper.
            let dark = img.pixels().filter(|p| p[0] < 128).count() as f64 / (img.width() * img.height()) as f64;
            assert!(dark > 0.02 && dark < 0.5, "{panel:?} {dark}");
            let id = identify_screen(panel, "SIMTST");
            assert!(id.pixels().any(|p| p[0] == 0));
        }
    }
}
