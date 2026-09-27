//! Turning ordinary pictures into what a TRMNL panel's firmware downloads: resized to the
//! panel, dithered to its inks, and encoded the way the TRMNL server serves them.
//!
//! | panel | served as |
//! |---|---|
//! | OG (800×480, black/white) | 1-bit BMP, palette index 1 = white, bottom-up rows |
//! | BWRY (800×480, black/white/yellow/red) | 2-bit palette PNG (the OG-family PNG decoder can't take truecolor rows that wide) |
//! | X (1872×1404, 16 grays) | 4-bit grayscale PNG |
//! | Spectra 6 (800×480, black/white/yellow/red/blue/green; reTerminal E1002) | 4-bit palette PNG |
//! | other black and white sizes (BYOD boards) | 1-bit grayscale PNG |
//! | other 16-gray panels (BYOD parallel panels) | 4-bit grayscale PNG |
//! | black/white/red | 2-bit palette PNG |

use image::{Rgb, RgbImage, imageops::FilterType};

/// What a panel can show.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Inks {
    /// Black and white.
    Mono,
    /// 16 grays (parallel panels driven by FastEPD).
    Gray16,
    /// Black, white and red.
    Bwr,
    /// Black, white, yellow and red.
    Bwry,
    /// E Ink Spectra 6: black, white, yellow, red, blue and green.
    Spectra6,
}

impl Inks {
    pub fn name(self) -> &'static str {
        match self {
            Inks::Mono => "mono",
            Inks::Gray16 => "gray16",
            Inks::Bwr => "bwr",
            Inks::Bwry => "bwry",
            Inks::Spectra6 => "spectra6",
        }
    }

    fn is_color(self) -> bool {
        matches!(self, Inks::Bwr | Inks::Bwry | Inks::Spectra6)
    }
}

/// The panel the images are for: its inks and size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Panel {
    pub inks: Inks,
    pub width: u32,
    pub height: u32,
}

// The TRMNL panels keep the names they had as enum variants.
#[allow(non_upper_case_globals)]
impl Panel {
    /// TRMNL OG: 800×480, black and white.
    pub const Og: Panel = Panel::new(Inks::Mono, 800, 480);
    /// TRMNL BWRY: 800×480, black, white, yellow and red.
    pub const Bwry: Panel = Panel::new(Inks::Bwry, 800, 480);
    /// TRMNL X: 1872×1404, 16 grays.
    pub const X: Panel = Panel::new(Inks::Gray16, 1872, 1404);
    /// E Ink Spectra 6 (Seeed reTerminal E1002): 800×480.
    pub const Spectra6: Panel = Panel::new(Inks::Spectra6, 800, 480);

    pub const fn new(inks: Inks, width: u32, height: u32) -> Self {
        Panel { inks, width, height }
    }

    pub fn size(self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Short name: og / bwry / x / spectra6 for the TRMNL panels, else inks and size
    /// (e.g. mono-400x300).
    pub fn name(self) -> String {
        match self {
            Panel::Og => "og".into(),
            Panel::Bwry => "bwry".into(),
            Panel::X => "x".into(),
            Panel::Spectra6 => "spectra6".into(),
            _ => format!("{}-{}x{}", self.inks.name(), self.width, self.height),
        }
    }

    /// What a converted image is served as, for display.
    pub fn format(self) -> &'static str {
        match self.inks {
            Inks::Mono if self == Panel::Og => "1-bit BMP",
            Inks::Mono => "1-bit PNG",
            Inks::Gray16 => "4-bit gray PNG",
            Inks::Bwr => "2-bit 3-color PNG",
            Inks::Bwry => "2-bit 4-color PNG",
            Inks::Spectra6 => "4-bit 6-color PNG",
        }
    }
}

/// The TRMNL BWRY inks, in palette order.
pub const BWRY_PALETTE: [[u8; 3]; 4] = [[0, 0, 0], [255, 255, 255], [255, 255, 0], [255, 0, 0]];

/// The black/white/red inks, in palette order.
pub const BWR_PALETTE: [[u8; 3]; 3] = [[0, 0, 0], [255, 255, 255], [255, 0, 0]];

/// The Spectra 6 inks, in the firmware's color order (bb_epaper's color indices).
pub const SPECTRA6_PALETTE: [[u8; 3]; 6] =
    [[0, 0, 0], [255, 255, 255], [255, 255, 0], [255, 0, 0], [0, 0, 255], [0, 255, 0]];

/// How a picture that isn't the panel's aspect ratio is fitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Fit {
    /// Scale to fit inside, pad with white.
    #[default]
    Contain,
    /// Scale to fill, crop the overflow (centered).
    Cover,
    /// Scale each axis independently.
    Stretch,
}

impl Fit {
    pub fn parse(s: &str) -> Option<Fit> {
        match s {
            "contain" => Some(Fit::Contain),
            "cover" => Some(Fit::Cover),
            "stretch" => Some(Fit::Stretch),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConvertOptions {
    /// Floyd–Steinberg error diffusion (otherwise nearest ink/gray per pixel).
    pub dither: bool,
    pub fit: Fit,
}

impl Default for ConvertOptions {
    fn default() -> Self {
        ConvertOptions { dither: true, fit: Fit::Contain }
    }
}

/// An image as a viewer of the panel sees it: 8-bit gray (255 = paper) or RGB. Directly
/// comparable with the simulator's screenshots.
#[derive(Debug, Clone)]
pub struct Preview {
    pub width: u32,
    pub height: u32,
    /// 1 = gray, 3 = RGB.
    pub channels: u8,
    pub pixels: Vec<u8>,
}

impl Preview {
    /// The pixel at (x, y) as RGB.
    pub fn rgb(&self, x: u32, y: u32) -> [u8; 3] {
        let i = (y * self.width + x) as usize * self.channels as usize;
        match self.channels {
            3 => [self.pixels[i], self.pixels[i + 1], self.pixels[i + 2]],
            _ => [self.pixels[i]; 3],
        }
    }

    /// 8-bit gray or RGB PNG.
    pub fn to_png(&self) -> Vec<u8> {
        let color = if self.channels == 3 { png::ColorType::Rgb } else { png::ColorType::Grayscale };
        encode_png(self.width, self.height, color, png::BitDepth::Eight, None, &self.pixels)
    }
}

/// An image ready to serve to the device.
#[derive(Debug, Clone)]
pub struct Converted {
    /// The file the device downloads.
    pub data: Vec<u8>,
    /// `bmp` or `png`.
    pub ext: &'static str,
    /// What the panel should show once the firmware has drawn it.
    pub preview: Preview,
}

impl Converted {
    pub fn content_type(&self) -> &'static str {
        if self.ext == "png" { "image/png" } else { "image/bmp" }
    }
}

/// Decode a PNG, JPEG, BMP or GIF and convert it for `panel`.
pub fn convert(bytes: &[u8], panel: Panel, opts: ConvertOptions) -> Result<Converted, String> {
    let img = image::load_from_memory(bytes).map_err(|e| format!("can't read image: {e}"))?;
    Ok(convert_image(&flatten(img), panel, opts))
}

/// Serve `bytes` exactly as given (a PNG or BMP the caller prepared for the firmware).
/// The preview is decoded from it, so it is only as faithful as the firmware's own
/// decoding of that file.
pub fn passthrough(bytes: &[u8], panel: Panel) -> Result<Converted, String> {
    let ext = if bytes.starts_with(b"\x89PNG") {
        "png"
    } else if bytes.starts_with(b"BM") {
        "bmp"
    } else {
        return Err("raw images must be PNG or BMP".into());
    };
    let img = image::load_from_memory(bytes).map_err(|e| format!("can't read image: {e}"))?;
    let (width, height) = (img.width(), img.height());
    let preview = if panel.inks.is_color() {
        Preview { width, height, channels: 3, pixels: img.to_rgb8().into_raw() }
    } else {
        Preview { width, height, channels: 1, pixels: img.to_luma8().into_raw() }
    };
    Ok(Converted { data: bytes.to_vec(), ext, preview })
}

/// Composite transparency onto white paper.
fn flatten(img: image::DynamicImage) -> RgbImage {
    if !img.color().has_alpha() {
        return img.to_rgb8();
    }
    let rgba = img.to_rgba8();
    let mut out = RgbImage::new(rgba.width(), rgba.height());
    for (o, p) in out.pixels_mut().zip(rgba.pixels()) {
        let a = p[3] as u32;
        *o = Rgb(std::array::from_fn(|c| ((p[c] as u32 * a + 255 * (255 - a)) / 255) as u8));
    }
    out
}

/// Resize, dither and encode an RGB image for `panel`.
pub fn convert_image(img: &RgbImage, panel: Panel, opts: ConvertOptions) -> Converted {
    let (w, h) = panel.size();
    let fitted = fit(img, w, h, opts.fit);
    match panel.inks {
        Inks::Mono => {
            let gray = dither_gray(&fitted, 2, opts.dither);
            let pixels: Vec<u8> = gray.iter().map(|&v| if v == 0 { 0 } else { 255 }).collect();
            // The TRMNL server's BMP for the 7.5" panel; 1-bit PNGs for other sizes (the
            // firmware's BMP path expects 800x480).
            let (data, ext) = if panel == Panel::Og {
                (bmp_1bit(w, h, |x, y| gray[(y * w + x) as usize] == 0), "bmp")
            } else {
                (png_gray(w, h, 1, &gray), "png")
            };
            Converted { data, ext, preview: Preview { width: w, height: h, channels: 1, pixels } }
        }
        Inks::Gray16 => {
            let levels = dither_gray(&fitted, 16, opts.dither);
            let data = png_gray(w, h, 4, &levels);
            let pixels = levels.iter().map(|&v| v * 17).collect();
            Converted { data, ext: "png", preview: Preview { width: w, height: h, channels: 1, pixels } }
        }
        Inks::Bwr | Inks::Bwry | Inks::Spectra6 => {
            let (palette, quantize) = match panel.inks {
                Inks::Bwr => (&BWR_PALETTE[..], bwr_quantize as fn(u8, u8, u8) -> u8),
                Inks::Bwry => (&BWRY_PALETTE[..], bwry_quantize as fn(u8, u8, u8) -> u8),
                _ => (&SPECTRA6_PALETTE[..], spectra6_quantize as fn(u8, u8, u8) -> u8),
            };
            let idx = dither_palette(&fitted, palette, quantize, opts.dither);
            let data = png_palette(w, h, palette, &idx);
            let pixels = idx.iter().flat_map(|&i| palette[i as usize]).collect();
            Converted { data, ext: "png", preview: Preview { width: w, height: h, channels: 3, pixels } }
        }
    }
}

/// Scale `img` onto a white `w`×`h` canvas.
fn fit(img: &RgbImage, w: u32, h: u32, fit: Fit) -> RgbImage {
    let (sw, sh) = img.dimensions();
    if (sw, sh) == (w, h) || sw == 0 || sh == 0 {
        return if sw == 0 || sh == 0 { RgbImage::from_pixel(w, h, Rgb([255; 3])) } else { img.clone() };
    }
    let filter = FilterType::CatmullRom;
    match fit {
        Fit::Stretch => image::imageops::resize(img, w, h, filter),
        Fit::Contain | Fit::Cover => {
            let (kx, ky) = (w as f64 / sw as f64, h as f64 / sh as f64);
            let k = if fit == Fit::Contain { kx.min(ky) } else { kx.max(ky) };
            let (nw, nh) = (((sw as f64 * k).round() as u32).max(1), ((sh as f64 * k).round() as u32).max(1));
            let scaled = image::imageops::resize(img, nw, nh, filter);
            let mut out = RgbImage::from_pixel(w, h, Rgb([255; 3]));
            image::imageops::overlay(&mut out, &scaled, (w as i64 - nw as i64) / 2, (h as i64 - nh as i64) / 2);
            out
        }
    }
}

fn luma(p: &Rgb<u8>) -> f32 {
    0.299 * p[0] as f32 + 0.587 * p[1] as f32 + 0.114 * p[2] as f32
}

/// Floyd–Steinberg: push `err` (per channel) to the unvisited neighbours of pixel `i`.
fn diffuse<const N: usize>(buf: &mut [[f32; N]], w: usize, i: usize, err: [f32; N]) {
    let (x, h) = (i % w, buf.len() / w);
    let y = i / w;
    let mut add = |j: usize, k: f32| {
        for c in 0..N {
            buf[j][c] += err[c] * k;
        }
    };
    if x + 1 < w {
        add(i + 1, 7.0 / 16.0);
    }
    if y + 1 < h {
        if x > 0 {
            add(i + w - 1, 3.0 / 16.0);
        }
        add(i + w, 5.0 / 16.0);
        if x + 1 < w {
            add(i + w + 1, 1.0 / 16.0);
        }
    }
}

/// Reduce to `levels` evenly spaced grays; returns the level per pixel (0 = black).
fn dither_gray(img: &RgbImage, levels: u32, dither: bool) -> Vec<u8> {
    let w = img.width() as usize;
    let mut buf: Vec<[f32; 1]> = img.pixels().map(|p| [luma(p)]).collect();
    let step = 255.0 / (levels - 1) as f32;
    let mut out = vec![0u8; buf.len()];
    for i in 0..buf.len() {
        let v = buf[i][0];
        let q = (v / step).round().clamp(0.0, (levels - 1) as f32);
        out[i] = q as u8;
        if dither {
            diffuse(&mut buf, w, i, [v - q * step]);
        }
    }
    out
}

/// Reduce to a color panel's inks; returns palette indices. Without dithering, colors are
/// classified like that panel's firmware does (`quantize`).
fn dither_palette(img: &RgbImage, palette: &[[u8; 3]], quantize: fn(u8, u8, u8) -> u8, dither: bool) -> Vec<u8> {
    if !dither {
        return img.pixels().map(|p| quantize(p[0], p[1], p[2])).collect();
    }
    let w = img.width() as usize;
    let mut buf: Vec<[f32; 3]> = img.pixels().map(|p| [p[0] as f32, p[1] as f32, p[2] as f32]).collect();
    let mut out = vec![0u8; buf.len()];
    for i in 0..buf.len() {
        let v = buf[i];
        let dist = |c: &[u8; 3]| (0..3).map(|k| (v[k] - c[k] as f32).powi(2)).sum::<f32>();
        let best = (0..palette.len()).min_by(|&a, &b| dist(&palette[a]).total_cmp(&dist(&palette[b]))).unwrap();
        out[i] = best as u8;
        let c = palette[best];
        diffuse(&mut buf, w, i, std::array::from_fn(|k| v[k] - c[k] as f32));
    }
    out
}

/// The TRMNL BWRY firmware's color reduction (`GetBWYRPixel` in display.cpp), as a
/// [`BWRY_PALETTE`] index.
pub fn bwry_quantize(r: u8, g: u8, b: u8) -> u8 {
    let (r, g, b) = (r as i32, g as i32, b as i32);
    let gr = (b + r + g * 2) >> 2;
    if r > b || g > b {
        if gr < 90 && r < 80 && g < 80 {
            0
        } else if r - b > 32 && r - g > r / 2 {
            3
        } else if r - b > 32 && g - b > 32 {
            2
        } else {
            1
        }
    } else if gr >= 100 {
        1
    } else {
        0
    }
}

/// Black/white/red: the BWRY reduction with yellow as white, as a [`BWR_PALETTE`] index.
pub fn bwr_quantize(r: u8, g: u8, b: u8) -> u8 {
    match bwry_quantize(r, g, b) {
        0 => 0,
        3 => 2,
        _ => 1,
    }
}

/// The Spectra 6 firmware's color reduction (`GetSpectraPixel` in display.cpp): the
/// nearest of its reference inks to the color's RGB333 value, as a [`SPECTRA6_PALETTE`]
/// index.
pub fn spectra6_quantize(r: u8, g: u8, b: u8) -> u8 {
    const INKS: [[i32; 3]; 6] = [[0, 0, 0], [192, 192, 192], [192, 192, 0], [192, 0, 0], [0, 0, 192], [0, 192, 0]];
    let q = |v: u8| (v >> 5) as i32 * 36;
    let (r, g, b) = (q(r), q(g), q(b));
    let dist = |c: &[i32; 3]| (r - c[0]).pow(2) + (g - c[1]).pow(2) + (b - c[2]).pow(2);
    // The first of equally near inks wins, as in the firmware's loop.
    (0..6).fold(0, |best, j| if dist(&INKS[j]) < dist(&INKS[best]) { j } else { best }) as u8
}

// ---- encoders ----------------------------------------------------------------------------------

/// A 1-bit BMP in the format TRMNL serves: palette 0 = black, 1 = white, bottom-up rows.
/// `black(x, y)` says which pixels are ink.
pub fn bmp_1bit(w: u32, h: u32, black: impl Fn(u32, u32) -> bool) -> Vec<u8> {
    let row = (w as usize).div_ceil(32) * 4;
    let mut data = vec![0u8; row * h as usize];
    for y in 0..h {
        let base = (h - 1 - y) as usize * row;
        for x in 0..w {
            if !black(x, y) {
                data[base + x as usize / 8] |= 0x80 >> (x % 8);
            }
        }
    }
    let palette = [0u8, 0, 0, 0, 255, 255, 255, 0];
    let offset = 14 + 40 + palette.len() as u32;
    let mut out = Vec::with_capacity(offset as usize + data.len());
    out.extend_from_slice(b"BM");
    for v in [offset + data.len() as u32, 0, offset] {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out.extend_from_slice(&40u32.to_le_bytes());
    out.extend_from_slice(&(w as i32).to_le_bytes());
    out.extend_from_slice(&(h as i32).to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // planes
    out.extend_from_slice(&1u16.to_le_bytes()); // bits per pixel
    for v in [0u32, data.len() as u32, 2835, 2835, 2, 2] {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out.extend_from_slice(&palette);
    out.extend_from_slice(&data);
    out
}

/// Pack `values` (one per pixel, `bits` wide) MSB first into rows.
fn pack(w: u32, h: u32, bits: u32, values: &[u8]) -> Vec<u8> {
    let per_byte = 8 / bits;
    let row = (w * bits).div_ceil(8) as usize;
    let mut out = vec![0u8; row * h as usize];
    for y in 0..h {
        for x in 0..w {
            let v = values[(y * w + x) as usize] & ((1 << bits) - 1) as u8;
            out[y as usize * row + (x / per_byte) as usize] |= v << (8 - bits * (x % per_byte + 1));
        }
    }
    out
}

/// A grayscale PNG of `bits` depth (1, 2, 4 or 8); `levels` are 0 (black) ..= 2^bits - 1.
pub fn png_gray(w: u32, h: u32, bits: u32, levels: &[u8]) -> Vec<u8> {
    let depth = match bits {
        1 => png::BitDepth::One,
        2 => png::BitDepth::Two,
        4 => png::BitDepth::Four,
        _ => png::BitDepth::Eight,
    };
    encode_png(w, h, png::ColorType::Grayscale, depth, None, &pack(w, h, bits.min(8), levels))
}

/// An indexed PNG: 2 bits per pixel for up to four colors, else 4.
pub fn png_palette(w: u32, h: u32, palette: &[[u8; 3]], indices: &[u8]) -> Vec<u8> {
    let plte: Vec<u8> = palette.iter().flatten().copied().collect();
    let (depth, bits) = if palette.len() <= 4 { (png::BitDepth::Two, 2) } else { (png::BitDepth::Four, 4) };
    encode_png(w, h, png::ColorType::Indexed, depth, Some(plte), &pack(w, h, bits, indices))
}

fn encode_png(
    w: u32,
    h: u32,
    color: png::ColorType,
    depth: png::BitDepth,
    palette: Option<Vec<u8>>,
    data: &[u8],
) -> Vec<u8> {
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, w, h);
        enc.set_color(color);
        enc.set_depth(depth);
        if let Some(p) = palette {
            enc.set_palette(p);
        }
        let mut wr = enc.write_header().expect("PNG header");
        wr.write_image_data(data).expect("PNG data");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode(bytes: &[u8]) -> image::DynamicImage {
        image::load_from_memory(bytes).unwrap()
    }

    #[test]
    fn og_is_a_1_bit_bmp_like_trmnl_serves() {
        let mut img = RgbImage::from_pixel(800, 480, Rgb([255; 3]));
        for y in 0..10 {
            for x in 0..20 {
                img.put_pixel(x, y, Rgb([0; 3]));
            }
        }
        let c = convert_image(&img, Panel::Og, ConvertOptions::default());
        assert_eq!(c.ext, "bmp");
        assert_eq!(&c.data[..2], b"BM");
        assert_eq!(c.data.len(), 14 + 40 + 8 + 100 * 480);
        assert_eq!(u16::from_le_bytes([c.data[28], c.data[29]]), 1, "bits per pixel");
        // The last row in the file is the top row of the picture: 20 black pixels, then white.
        let top = &c.data[c.data.len() - 100..];
        assert_eq!(&top[..3], &[0x00, 0x00, 0x0f]);
        let back = decode(&c.data).to_luma8();
        assert_eq!(back.get_pixel(0, 0)[0], 0);
        assert_eq!(back.get_pixel(20, 0)[0], 255);
        assert_eq!(c.preview.pixels, back.into_raw());
    }

    #[test]
    fn contain_letterboxes_with_white_and_cover_crops() {
        // A black square on an 800x480 panel: contain = centered 480x480 black, white sides.
        let img = RgbImage::from_pixel(100, 100, Rgb([0; 3]));
        let c = convert_image(&img, Panel::Og, ConvertOptions { dither: false, fit: Fit::Contain });
        let at = |x: u32, y: u32| c.preview.pixels[(y * 800 + x) as usize];
        assert_eq!((at(10, 240), at(400, 240), at(790, 240)), (255, 0, 255));
        let c = convert_image(&img, Panel::Og, ConvertOptions { dither: false, fit: Fit::Cover });
        assert!(c.preview.pixels.iter().all(|&v| v == 0));
    }

    #[test]
    fn dithering_keeps_the_average_gray() {
        let img = RgbImage::from_pixel(64, 64, Rgb([128; 3]));
        let c = convert_image(&img, Panel::Og, ConvertOptions { dither: true, fit: Fit::Stretch });
        let black = c.preview.pixels.iter().filter(|&&v| v == 0).count() as f64 / c.preview.pixels.len() as f64;
        assert!((black - 0.5).abs() < 0.05, "{black}");
        let c = convert_image(&img, Panel::Og, ConvertOptions { dither: false, fit: Fit::Stretch });
        assert!(c.preview.pixels.iter().all(|&v| v == 255));
    }

    #[test]
    fn x_is_a_4_bit_gray_png_at_panel_size() {
        let img = RgbImage::from_fn(16, 1, |x, _| Rgb([(x * 17) as u8; 3]));
        let c = convert_image(&img, Panel::X, ConvertOptions { dither: false, fit: Fit::Stretch });
        assert_eq!(c.ext, "png");
        assert_eq!((c.data[24], c.data[25]), (4, 0), "IHDR: bit depth 4, grayscale");
        let back = decode(&c.data).to_luma8();
        assert_eq!(back.dimensions(), (1872, 1404));
        assert_eq!(back.get_pixel(0, 0)[0], 0);
        assert_eq!(back.get_pixel(1871, 700)[0], 255);
        assert_eq!(c.preview.pixels, back.into_raw());
    }

    #[test]
    fn bwry_is_a_2_bit_palette_png_of_the_four_inks() {
        let img = RgbImage::from_fn(800, 480, |x, _| {
            let c = BWRY_PALETTE[(x / 200) as usize];
            Rgb(c)
        });
        for dither in [false, true] {
            let c = convert_image(&img, Panel::Bwry, ConvertOptions { dither, fit: Fit::Contain });
            assert_eq!((c.data[24], c.data[25]), (2, 3), "IHDR: bit depth 2, indexed");
            let back = decode(&c.data).to_rgb8();
            for (i, ink) in BWRY_PALETTE.iter().enumerate() {
                assert_eq!(back.get_pixel(i as u32 * 200 + 100, 240).0, *ink);
            }
            assert_eq!(c.preview.pixels, back.into_raw());
        }
    }

    #[test]
    fn spectra6_is_a_4_bit_palette_png_of_the_six_inks() {
        let img = RgbImage::from_fn(800, 480, |x, _| Rgb(SPECTRA6_PALETTE[(x / 134).min(5) as usize]));
        for dither in [false, true] {
            let c = convert_image(&img, Panel::Spectra6, ConvertOptions { dither, fit: Fit::Contain });
            assert_eq!((c.data[24], c.data[25]), (4, 3), "IHDR: bit depth 4, indexed");
            let back = decode(&c.data).to_rgb8();
            for (i, ink) in SPECTRA6_PALETTE.iter().enumerate() {
                assert_eq!(back.get_pixel(i as u32 * 134 + 60, 240).0, *ink);
            }
            assert_eq!(c.preview.pixels, back.into_raw());
        }
    }

    #[test]
    fn spectra6_quantize_matches_the_firmware() {
        for (i, ink) in SPECTRA6_PALETTE.iter().enumerate() {
            assert_eq!(spectra6_quantize(ink[0], ink[1], ink[2]) as usize, i, "{ink:?}");
        }
        // Mid gray (RGB333 4,4,4 = 144): white (192) is nearer than black.
        assert_eq!(spectra6_quantize(128, 128, 128), 1);
        assert_eq!(spectra6_quantize(64, 64, 64), 0);
        // Orange lands on red or yellow, never on white.
        assert!(matches!(spectra6_quantize(255, 128, 0), 2 | 3));
    }

    #[test]
    fn bwry_quantize_matches_the_firmware() {
        assert_eq!(bwry_quantize(0, 0, 0), 0);
        assert_eq!(bwry_quantize(255, 255, 255), 1);
        assert_eq!(bwry_quantize(255, 255, 0), 2);
        assert_eq!(bwry_quantize(255, 0, 0), 3);
        assert_eq!(bwry_quantize(200, 60, 40), 3);
        assert_eq!(bwry_quantize(40, 40, 200), 0);
    }

    #[test]
    fn transparency_is_composited_on_white() {
        let img = image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(800, 480, image::Rgba([0, 0, 0, 0])));
        let mut png = Vec::new();
        img.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png).unwrap();
        let c = convert(&png, Panel::Og, ConvertOptions::default()).unwrap();
        assert!(c.preview.pixels.iter().all(|&v| v == 255));
    }

    #[test]
    fn passthrough_serves_the_bytes_unchanged() {
        let bmp = bmp_1bit(800, 480, |x, _| x < 400);
        let c = passthrough(&bmp, Panel::Og).unwrap();
        assert_eq!((c.ext, c.data == bmp), ("bmp", true));
        assert_eq!((c.preview.pixels[0], c.preview.pixels[799]), (0, 255));
        assert!(passthrough(b"GIF89a", Panel::Og).is_err());
    }

    #[test]
    fn other_panel_sizes_get_pngs_at_their_size() {
        let img = RgbImage::from_fn(64, 48, |x, _| if x < 32 { Rgb([0, 0, 0]) } else { Rgb([255, 0, 0]) });
        let mono = convert_image(&img, Panel::new(Inks::Mono, 400, 300), ConvertOptions::default());
        assert_eq!((mono.ext, mono.preview.width, mono.preview.height), ("png", 400, 300));
        let decoded = image::load_from_memory(&mono.data).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (400, 300));
        let bwr =
            convert_image(&img, Panel::new(Inks::Bwr, 648, 480), ConvertOptions { dither: false, fit: Fit::Stretch });
        assert_eq!(bwr.preview.rgb(10, 10), [0, 0, 0]);
        assert_eq!(bwr.preview.rgb(600, 10), [255, 0, 0]);
        assert_eq!(Panel::new(Inks::Bwr, 648, 480).name(), "bwr-648x480");
        assert_eq!(Panel::Og.name(), "og");
    }
}
