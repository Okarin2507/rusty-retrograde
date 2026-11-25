use std::fs;
use std::io::Cursor;
use std::path::Path;

use fast_image_resize::images::Image as FirImage;
use fast_image_resize::{FilterType, IntoImageView, ResizeAlg, ResizeOptions, Resizer};
use image::codecs::jpeg::JpegEncoder;
use image::codecs::png::PngEncoder;
use image::{ColorType, DynamicImage, ImageBuffer, ImageEncoder, ImageReader, Pixel};

use super::CoverError;

pub const MAX_COVER_FILE_BYTES: u64 = 30 * 1024 * 1024;
pub const MAX_PIXELS: u64 = 64_000_000;
pub const MAX_DIMENSION: u32 = 16_384;

/// Longest edge allowed for stored full-size covers. iced uploads images above ~2 MB
/// RGBA into a dedicated texture (a 4096px cover cost ~117 MB of GPU memory). 720px keeps
/// every aspect ratio under that 2 MB (724x724) limit, so full covers share the image atlas
/// with the thumbnails; it still covers the 510px full-screen cover at scale 1.
pub const MAX_FULL_COVER_EDGE: u32 = 720;

pub struct ImageLimits;

impl ImageLimits {
    pub fn is_sane_bytes(data: &[u8]) -> bool {
        if data.len() as u64 > MAX_COVER_FILE_BYTES {
            return false;
        }
        let Ok(reader) = ImageReader::new(Cursor::new(data)).with_guessed_format() else {
            return false;
        };
        Self::fits(reader.into_dimensions().ok())
    }

    pub fn is_sane_path(path: &Path) -> bool {
        let Ok(meta) = fs::metadata(path) else {
            return false;
        };
        if meta.len() > MAX_COVER_FILE_BYTES {
            return false;
        }
        let Ok(reader) = ImageReader::open(path).and_then(|r| r.with_guessed_format()) else {
            return false;
        };
        Self::fits(reader.into_dimensions().ok())
    }

    pub(crate) fn fits(dims: Option<(u32, u32)>) -> bool {
        let Some((w, h)) = dims else {
            return false;
        };
        w > 0
            && h > 0
            && w <= MAX_DIMENSION
            && h <= MAX_DIMENSION
            && (w as u64) * (h as u64) <= MAX_PIXELS
    }
}

fn capped_dims(w: u32, h: u32, max_edge: u32) -> (u32, u32) {
    if max_edge == 0 || w.max(h) <= max_edge {
        return (w, h);
    }
    let scale = |side: u32, long: u32| -> u32 {
        (((side as u64) * (max_edge as u64) + (long as u64) / 2) / (long as u64)).max(1) as u32
    };
    if h > w {
        (scale(w, h), max_edge)
    } else {
        (max_edge, scale(h, w))
    }
}

fn resize_pixels<P>(
    buf: ImageBuffer<P, Vec<u8>>,
    dst_w: u32,
    dst_h: u32,
) -> Result<Vec<u8>, CoverError>
where
    P: Pixel<Subpixel = u8>,
    ImageBuffer<P, Vec<u8>>: IntoImageView,
{
    if (dst_w, dst_h) == (buf.width(), buf.height()) {
        return Ok(buf.into_raw());
    }
    let pixel_type = buf
        .pixel_type()
        .ok_or_else(|| CoverError::Image("Unsupported pixel type".to_owned()))?;
    let mut dst = FirImage::new(dst_w, dst_h, pixel_type);
    let opts = ResizeOptions::new().resize_alg(ResizeAlg::Convolution(FilterType::Mitchell));
    Resizer::new()
        .resize(&buf, &mut dst, &opts)
        .map_err(|e| CoverError::Image(e.to_string()))?;
    Ok(dst.into_vec())
}

/// Encodes a full-size cover fitted within `max_edge`. Opaque images become JPEG; images with
/// an alpha channel become PNG so transparency survives. Returns `(bytes, extension)`.
pub(crate) fn encode_capped_full_with_ext(
    img: &DynamicImage,
    max_edge: u32,
    quality: u8,
) -> Result<(Vec<u8>, &'static str), CoverError> {
    if img.color().has_alpha() {
        let rgba = img.to_rgba8();
        let (dw, dh) = capped_dims(rgba.width(), rgba.height(), max_edge);
        let pixels = resize_pixels(rgba, dw, dh)?;
        let mut out = Vec::new();
        PngEncoder::new(&mut out)
            .write_image(&pixels, dw, dh, ColorType::Rgba8.into())
            .map_err(|e| CoverError::Image(e.to_string()))?;
        Ok((out, "png"))
    } else {
        Ok((encode_capped_jpeg(img, max_edge, quality)?, "jpg"))
    }
}

/// Encode `img` as JPEG fitted within `max_edge` x `max_edge`, preserving aspect ratio.
pub(crate) fn encode_capped_jpeg(
    img: &DynamicImage,
    max_edge: u32,
    quality: u8,
) -> Result<Vec<u8>, CoverError> {
    let rgb = img.to_rgb8();
    let (dw, dh) = capped_dims(rgb.width(), rgb.height(), max_edge);
    let pixels = resize_pixels(rgb, dw, dh)?;
    let mut out = Vec::new();
    JpegEncoder::new_with_quality(&mut out, quality)
        .encode(&pixels, dw, dh, ColorType::Rgb8.into())
        .map_err(|e| CoverError::Image(e.to_string()))?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dims_of(bytes: &[u8]) -> (u32, u32) {
        let img = ImageReader::new(Cursor::new(bytes))
            .with_guessed_format()
            .unwrap()
            .decode()
            .unwrap();
        (img.width(), img.height())
    }

    #[test]
    fn capped_encoding_preserves_aspect_ratio_and_alpha() {
        let blank = |w, h| DynamicImage::ImageRgb8(image::RgbImage::new(w, h));
        for ((w, h), exp) in [
            ((3000, 2000), (720, 480)),
            ((2000, 3000), (480, 720)),
            ((700, 500), (700, 500)),
            ((500, 700), (500, 700)),
        ] {
            let (out, ext) =
                encode_capped_full_with_ext(&blank(w, h), MAX_FULL_COVER_EDGE, 90).unwrap();
            assert_eq!(ext, "jpg");
            assert_eq!(dims_of(&out), exp);
        }

        let mut rgba = image::RgbaImage::new(2048, 1024);
        rgba.put_pixel(0, 0, image::Rgba([255, 0, 0, 0]));
        let (out, ext) =
            encode_capped_full_with_ext(&DynamicImage::ImageRgba8(rgba), MAX_FULL_COVER_EDGE, 90)
                .unwrap();
        assert_eq!(ext, "png");
        assert!(out.starts_with(&[0x89, b'P', b'N', b'G']));
        assert_eq!(dims_of(&out), (720, 360));
    }
}
