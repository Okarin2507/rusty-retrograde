use std::path::Path;

use image::ImageReader;

use super::encode::ImageLimits;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CoverPalette {
    /// Brightened accent color readable on `#121212` panels (Orbit Orange `#FF6B35` default).
    pub accent: [u8; 3],
    /// Rich ambient backdrop glow color.
    pub glow: [u8; 3],
    /// True when a dominant vivid hue was found (false for grayscale/fallback).
    pub vivid: bool,
}

impl CoverPalette {
    pub const ORBIT_DEFAULT: Self = Self {
        accent: [0xFF, 0x6B, 0x35],
        glow: [0xD9, 0x56, 0x26],
        vivid: true,
    };
}

impl Default for CoverPalette {
    fn default() -> Self {
        Self::ORBIT_DEFAULT
    }
}

pub fn extract_cover_palette(path: &Path) -> Option<CoverPalette> {
    if !ImageLimits::is_sane_path(path) {
        return None;
    }
    let img = ImageReader::open(path)
        .ok()?
        .with_guessed_format()
        .ok()?
        .decode()
        .ok()?;
    let small = img.thumbnail(48, 48).to_rgb8();
    Some(extract_cover_palette_from_rgb(
        small.as_raw(),
        small.width(),
        small.height(),
    ))
}

pub fn extract_cover_palette_from_rgb(pixels: &[u8], width: u32, height: u32) -> CoverPalette {
    const BINS: usize = 24;
    if width == 0 || height == 0 || pixels.len() < 3 {
        return CoverPalette::ORBIT_DEFAULT;
    }

    let step_x = (width / 48).max(1);
    let step_y = (height / 48).max(1);
    let mut weight = [0.0_f64; BINS];
    let mut bin_r = [0.0_f64; BINS];
    let mut bin_g = [0.0_f64; BINS];
    let mut bin_b = [0.0_f64; BINS];
    let mut avg_r = 0.0_f64;
    let mut avg_g = 0.0_f64;
    let mut avg_b = 0.0_f64;
    let mut count = 0usize;

    let mut y = 0u32;
    while y < height {
        let Some(row_base) = (y as usize)
            .checked_mul(width as usize)
            .and_then(|n| n.checked_mul(3))
        else {
            break;
        };
        if row_base + 2 >= pixels.len() {
            break;
        }
        let mut x = 0u32;
        while x < width {
            let Some(idx) = (x as usize)
                .checked_mul(3)
                .and_then(|dx| row_base.checked_add(dx))
            else {
                break;
            };
            if idx + 2 >= pixels.len() {
                break;
            }
            let r = pixels[idx] as f64;
            let g = pixels[idx + 1] as f64;
            let b = pixels[idx + 2] as f64;
            avg_r += r;
            avg_g += g;
            avg_b += b;
            count += 1;

            let (h, s, v) = rgb_to_hsv(r / 255.0, g / 255.0, b / 255.0);
            if v >= 0.12 && s >= 0.15 {
                let w = s * s * v;
                let bin = ((h / 360.0) * (BINS as f64)).floor() as usize % BINS;
                weight[bin] += w;
                bin_r[bin] += r * w;
                bin_g[bin] += g * w;
                bin_b[bin] += b * w;
            }
            x = match x.checked_add(step_x) {
                Some(next) => next,
                None => break,
            };
        }
        y = match y.checked_add(step_y) {
            Some(next) => next,
            None => break,
        };
    }

    if count == 0 {
        return CoverPalette::ORBIT_DEFAULT;
    }

    let mut best = 0usize;
    for i in 1..BINS {
        if weight[i] > weight[best] {
            best = i;
        }
    }

    let vivid = weight[best] > (count as f64) * 0.002;
    let base = if vivid {
        [
            bin_r[best] / weight[best],
            bin_g[best] / weight[best],
            bin_b[best] / weight[best],
        ]
    } else {
        let n = count as f64;
        [avg_r / n, avg_g / n, avg_b / n]
    };

    let (h, s, v) = rgb_to_hsv(base[0] / 255.0, base[1] / 255.0, base[2] / 255.0);
    let (mut acc_s, mut acc_v) = if vivid {
        (s.clamp(0.45, 0.85), v.clamp(0.88, 1.0))
    } else {
        (s.clamp(0.0, 0.18), v.clamp(0.80, 0.88))
    };
    let mut accent = hsv_to_rgb(h, acc_s, acc_v);
    let mut guard = 0;
    while relative_luminance(accent) < MIN_ACCENT_LUMINANCE && guard < 64 {
        if acc_v < 1.0 {
            acc_v = (acc_v + 0.04).min(1.0);
        } else {
            acc_s = (acc_s - 0.02).max(0.0);
        }
        accent = hsv_to_rgb(h, acc_s, acc_v);
        guard += 1;
    }
    let glow = if vivid {
        hsv_to_rgb(h, s.clamp(0.45, 0.85), v.clamp(0.60, 0.85))
    } else {
        hsv_to_rgb(h, s.clamp(0.0, 0.20), v.clamp(0.35, 0.55))
    };

    CoverPalette {
        accent,
        glow,
        vivid,
    }
}

/// Minimum WCAG relative luminance for an accent on the `#121212` panel (about 6:1 contrast).
pub const MIN_ACCENT_LUMINANCE: f64 = 0.30;

/// WCAG 2.x relative luminance of an sRGB color (0.0 = black, 1.0 = white).
pub fn relative_luminance(rgb: [u8; 3]) -> f64 {
    fn lin(c: u8) -> f64 {
        let c = c as f64 / 255.0;
        if c <= 0.04045 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    }
    0.2126 * lin(rgb[0]) + 0.7152 * lin(rgb[1]) + 0.0722 * lin(rgb[2])
}

fn rgb_to_hsv(r: f64, g: f64, b: f64) -> (f64, f64, f64) {
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let delta = max - min;
    let h = if delta <= 1e-6 {
        0.0
    } else if (max - r).abs() <= 1e-6 {
        60.0 * (((g - b) / delta).rem_euclid(6.0))
    } else if (max - g).abs() <= 1e-6 {
        60.0 * ((b - r) / delta + 2.0)
    } else {
        60.0 * ((r - g) / delta + 4.0)
    };
    let s = if max <= 1e-6 { 0.0 } else { delta / max };
    (h.rem_euclid(360.0), s, max)
}

fn hsv_to_rgb(h: f64, s: f64, v: f64) -> [u8; 3] {
    let h = h.rem_euclid(360.0);
    let c = v * s;
    let x = c * (1.0 - (((h / 60.0) % 2.0) - 1.0).abs());
    let m = v - c;
    let (r1, g1, b1) = match (h / 60.0).floor() as i32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    [
        ((r1 + m) * 255.0).round().clamp(0.0, 255.0) as u8,
        ((g1 + m) * 255.0).round().clamp(0.0, 255.0) as u8,
        ((b1 + m) * 255.0).round().clamp(0.0, 255.0) as u8,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dark_cover_with_small_red_accent_extracts_red() {
        let mut pixels = vec![10u8; 48 * 48 * 3];
        for i in 0..70 {
            let base = i * 3;
            pixels[base] = 220;
            pixels[base + 1] = 28;
            pixels[base + 2] = 28;
        }
        let pal = extract_cover_palette_from_rgb(&pixels, 48, 48);
        assert!(pal.vivid);
        assert!(pal.accent[0] > pal.accent[1] + 50 && pal.accent[0] > pal.accent[2] + 50);
    }

    #[test]
    fn pure_blue_cover_accent_meets_contrast_floor() {
        let pixels = [0u8, 0, 255].repeat(32 * 32);
        let pal = extract_cover_palette_from_rgb(&pixels, 32, 32);
        assert!(pal.vivid);
        assert!(relative_luminance(pal.accent) >= MIN_ACCENT_LUMINANCE);
        assert!(pal.accent[2] > pal.accent[0] && pal.accent[2] > pal.accent[1]);
    }

    #[test]
    fn large_dimensions_do_not_overflow_index_math() {
        let pixels = vec![200u8; 3 * 64];
        let pal = extract_cover_palette_from_rgb(&pixels, 70_000, 70_000);
        assert!(relative_luminance(pal.accent) >= MIN_ACCENT_LUMINANCE);
        let max_pal = extract_cover_palette_from_rgb(&pixels, u32::MAX, u32::MAX);
        assert!(relative_luminance(max_pal.accent) >= MIN_ACCENT_LUMINANCE);
    }

    #[test]
    fn grayscale_cover_falls_back_to_silver() {
        let pixels = [90u8, 90, 90].repeat(24 * 24);
        let pal = extract_cover_palette_from_rgb(&pixels, 24, 24);
        assert!(!pal.vivid);
        assert!((pal.accent[0] as i16 - pal.accent[1] as i16).abs() <= 2);
        assert!((pal.accent[1] as i16 - pal.accent[2] as i16).abs() <= 2);
        assert!(pal.accent[0] >= 200 && pal.accent[0] <= 230);
    }
}
