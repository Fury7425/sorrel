//! Background effects for the wallpaper, after Zeron's set. Dither, Halftone
//! and ASCII are baked into an image once and cached on disk, so showing
//! them costs nothing per frame. Scanlines are drawn live instead (see
//! `style::wallpaper`) so they stay one screen pixel thick; `apply` still
//! has them for completeness.

use std::{
    hash::{Hash, Hasher},
    path::{Path, PathBuf},
};

use image::{Rgba, RgbaImage, imageops};
use proto::Effect;

/// Pictures are baked at this width, so cells come out near screen size
/// however big or small the source is.
const BAKE_WIDTH: u32 = 1920;
/// The dark ground Halftone and ASCII draw on.
const GROUND: Rgba<u8> = Rgba([16, 15, 22, 255]);

/// Where the baked image for `source` with `effect` lives. The name changes
/// when the source file changes, so a stale bake is never shown.
pub fn cache_path(cache_dir: &Path, source: &str, effect: Effect) -> PathBuf {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    source.hash(&mut hasher);
    if let Ok(meta) = std::fs::metadata(source) {
        meta.len().hash(&mut hasher);
        meta.modified().ok().hash(&mut hasher);
    }
    cache_dir.join(format!("wallpaper-{effect:?}-{:016x}.png", hasher.finish()).to_lowercase())
}

/// Writes `source` with `effect` applied to `out`, unless it is already there.
pub fn bake(source: &Path, effect: Effect, out: &Path) -> Result<(), String> {
    if out.exists() {
        return Ok(());
    }
    let image = image::open(source).map_err(|e| format!("cannot read the wallpaper: {e}"))?;
    let mut image = image.to_rgba8();
    if image.width() != BAKE_WIDTH {
        let height = (image.height() * BAKE_WIDTH / image.width().max(1)).max(1);
        image = imageops::resize(&image, BAKE_WIDTH, height, imageops::FilterType::Triangle);
    }
    let baked = apply(&image, effect);
    if let Some(dir) = out.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    baked
        .save(out)
        .map_err(|e| format!("cannot save the wallpaper effect: {e}"))
}

pub fn apply(image: &RgbaImage, effect: Effect) -> RgbaImage {
    match effect {
        Effect::None => image.clone(),
        Effect::Scanlines => scanlines(image),
        Effect::Dither => dither(image),
        Effect::Halftone => halftone(image),
        Effect::Ascii => ascii(image),
    }
}

fn luma(p: &Rgba<u8>) -> f32 {
    (0.2126 * p[0] as f32 + 0.7152 * p[1] as f32 + 0.0722 * p[2] as f32) / 255.
}

/// Every third row darkened, like a display's lines.
fn scanlines(image: &RgbaImage) -> RgbaImage {
    let mut out = image.clone();
    for (_, y, p) in out.enumerate_pixels_mut() {
        if y % 3 == 0 {
            for c in 0..3 {
                p[c] = (p[c] as f32 * 0.55) as u8;
            }
        }
    }
    out
}

/// Ordered 4x4 Bayer dithering to four levels per channel, for a retro grain.
fn dither(image: &RgbaImage) -> RgbaImage {
    const BAYER: [[f32; 4]; 4] = [
        [0., 8., 2., 10.],
        [12., 4., 14., 6.],
        [3., 11., 1., 9.],
        [15., 7., 13., 5.],
    ];
    const LEVELS: f32 = 3.;
    let mut out = image.clone();
    for (x, y, p) in out.enumerate_pixels_mut() {
        let threshold = (BAYER[(y % 4) as usize][(x % 4) as usize] + 0.5) / 16. - 0.5;
        for c in 0..3 {
            let v = p[c] as f32 / 255. + threshold / LEVELS;
            p[c] = ((v * LEVELS).round().clamp(0., LEVELS) / LEVELS * 255.) as u8;
        }
    }
    out
}

/// The average color of the `cw` x `ch` cell at (`cx`, `cy`).
fn cell_color(image: &RgbaImage, cx: u32, cy: u32, cw: u32, ch: u32) -> Rgba<u8> {
    let (mut sum, mut n) = ([0u32; 3], 0u32);
    for y in cy..(cy + ch).min(image.height()) {
        for x in cx..(cx + cw).min(image.width()) {
            let p = image.get_pixel(x, y);
            for c in 0..3 {
                sum[c] += p[c] as u32;
            }
            n += 1;
        }
    }
    let n = n.max(1);
    Rgba([
        (sum[0] / n) as u8,
        (sum[1] / n) as u8,
        (sum[2] / n) as u8,
        255,
    ])
}

/// Brightens a color toward full value, so marks read on the dark ground.
fn lift(p: Rgba<u8>) -> Rgba<u8> {
    let max = p[0].max(p[1]).max(p[2]).max(1) as f32;
    let k = (255. / max).min(1.6);
    Rgba([
        (p[0] as f32 * k).min(255.) as u8,
        (p[1] as f32 * k).min(255.) as u8,
        (p[2] as f32 * k).min(255.) as u8,
        255,
    ])
}

/// Colored dots on a dark ground, bigger where the picture is brighter.
fn halftone(image: &RgbaImage) -> RgbaImage {
    const CELL: u32 = 9;
    let mut out = RgbaImage::from_pixel(image.width(), image.height(), GROUND);
    for cy in (0..image.height()).step_by(CELL as usize) {
        for cx in (0..image.width()).step_by(CELL as usize) {
            let color = cell_color(image, cx, cy, CELL, CELL);
            let radius = CELL as f32 / 2. * luma(&color).sqrt() * 1.15;
            let ink = lift(color);
            let (mx, my) = (cx as f32 + CELL as f32 / 2., cy as f32 + CELL as f32 / 2.);
            for y in cy..(cy + CELL).min(image.height()) {
                for x in cx..(cx + CELL).min(image.width()) {
                    let d = ((x as f32 + 0.5 - mx).powi(2) + (y as f32 + 0.5 - my).powi(2)).sqrt();
                    // One pixel of soft edge.
                    let cover = (radius - d + 0.5).clamp(0., 1.);
                    if cover > 0. {
                        out.put_pixel(x, y, mix(GROUND, ink, cover));
                    }
                }
            }
        }
    }
    out
}

fn mix(a: Rgba<u8>, b: Rgba<u8>, t: f32) -> Rgba<u8> {
    let m = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t) as u8;
    Rgba([m(a[0], b[0]), m(a[1], b[1]), m(a[2], b[2]), 255])
}

/// A 5x7 glyph for each step of the ramp ` .:-=+*#%@`, darkest first.
const GLYPHS: [[u8; 7]; 10] = [
    [0, 0, 0, 0, 0, 0, 0],
    [0, 0, 0, 0, 0, 0, 0b00100],
    [0, 0, 0b00100, 0, 0, 0b00100, 0],
    [0, 0, 0, 0b01110, 0, 0, 0],
    [0, 0, 0b11111, 0, 0b11111, 0, 0],
    [0, 0b00100, 0b00100, 0b11111, 0b00100, 0b00100, 0],
    [0, 0b10101, 0b01110, 0b11111, 0b01110, 0b10101, 0],
    [0b01010, 0b11111, 0b01010, 0b01010, 0b11111, 0b01010, 0],
    [0b11001, 0b11010, 0b00100, 0b01011, 0b10011, 0, 0],
    [
        0b01110, 0b10001, 0b10111, 0b10101, 0b10111, 0b10000, 0b01110,
    ],
];

/// The picture as characters: each 6x10 cell becomes a glyph whose density
/// follows the cell's brightness, drawn in the cell's color.
fn ascii(image: &RgbaImage) -> RgbaImage {
    const CW: u32 = 6;
    const CH: u32 = 10;
    let mut out = RgbaImage::from_pixel(image.width(), image.height(), GROUND);
    for cy in (0..image.height()).step_by(CH as usize) {
        for cx in (0..image.width()).step_by(CW as usize) {
            let color = cell_color(image, cx, cy, CW, CH);
            let step = ((luma(&color) * (GLYPHS.len() as f32 - 1.)).round() as usize)
                .min(GLYPHS.len() - 1);
            let ink = lift(color);
            for (row, bits) in GLYPHS[step].iter().enumerate() {
                for col in 0..5u32 {
                    if bits & (0b10000 >> col) != 0 {
                        let (x, y) = (cx + col, cy + 1 + row as u32);
                        if x < out.width() && y < out.height() {
                            out.put_pixel(x, y, ink);
                        }
                    }
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{Effect, GROUND, apply, luma};
    use image::{Rgba, RgbaImage};

    #[test]
    fn effects_keep_size_and_follow_brightness() {
        // Left half white, right half black.
        let image = RgbaImage::from_fn(60, 40, |x, _| {
            if x < 30 {
                Rgba([255, 255, 255, 255])
            } else {
                Rgba([0, 0, 0, 255])
            }
        });
        for effect in [
            Effect::Scanlines,
            Effect::Dither,
            Effect::Halftone,
            Effect::Ascii,
        ] {
            let out = apply(&image, effect);
            assert_eq!(out.dimensions(), image.dimensions(), "{effect:?}");
            let bright = |x0: u32, x1: u32| {
                let mut sum = 0.;
                for y in 0..40 {
                    for x in x0..x1 {
                        sum += luma(out.get_pixel(x, y));
                    }
                }
                sum
            };
            assert!(
                bright(0, 30) > bright(30, 60) + 1.,
                "{effect:?} lost the picture"
            );
        }
        // The dark half of Halftone and ASCII is bare ground.
        assert_eq!(*apply(&image, Effect::Ascii).get_pixel(50, 20), GROUND);
    }
}
