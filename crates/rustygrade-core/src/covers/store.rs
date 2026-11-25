use std::borrow::Cow;
use std::fs::{self, OpenOptions};
use std::io::{Cursor, ErrorKind};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

use image::metadata::Orientation;
use image::{DynamicImage, ImageDecoder, ImageFormat, ImageReader};

use crate::paths::CoverPaths;

use super::CoverError;
use super::encode::{
    ImageLimits, MAX_COVER_FILE_BYTES, MAX_FULL_COVER_EDGE, encode_capped_full_with_ext,
};
use super::hash::sha256_hex_prefix;

static COVER_TMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Normalized, hashed cover image ready to be persisted into `CoverPaths::cover_dir()`.
#[derive(Debug, Clone)]
pub struct PreparedCover {
    bytes: Vec<u8>,
    hash: String,
    ext: &'static str,
}

/// Result of writing a content-addressed cover file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedCover {
    pub path: PathBuf,
    /// `true` when this call newly created `path` on disk; `false` when identical content already existed.
    pub created: bool,
}

fn prepare_cover_cow(data: &[u8]) -> Result<(Cow<'_, [u8]>, String, &'static str), CoverError> {
    if data.len() as u64 > MAX_COVER_FILE_BYTES {
        return Err(CoverError::InsaneImage);
    }
    let reader = ImageReader::new(Cursor::new(data))
        .with_guessed_format()
        .map_err(|_| CoverError::InsaneImage)?;
    let fmt = reader.format();
    let mut decoder = reader.into_decoder().map_err(|_| CoverError::InsaneImage)?;
    let (w, h) = decoder.dimensions();
    if !ImageLimits::fits(Some((w, h))) {
        return Err(CoverError::InsaneImage);
    }
    let orient = decoder.orientation().unwrap_or(Orientation::NoTransforms);
    // Decode pixels even when keeping original bytes so truncated/corrupt payloads are rejected.
    let mut img =
        DynamicImage::from_decoder(decoder).map_err(|e| CoverError::Image(e.to_string()))?;

    let (final_bytes, ext): (Cow<'_, [u8]>, &'static str) = if w.max(h) <= MAX_FULL_COVER_EDGE
        && orient == Orientation::NoTransforms
        && matches!(
            fmt,
            Some(ImageFormat::Jpeg | ImageFormat::Png | ImageFormat::WebP)
        ) {
        let ext = match fmt {
            Some(ImageFormat::Png) => "png",
            Some(ImageFormat::WebP) => "webp",
            _ => "jpg",
        };
        (Cow::Borrowed(data), ext)
    } else {
        img.apply_orientation(orient);
        let (encoded, ext) = encode_capped_full_with_ext(&img, MAX_FULL_COVER_EDGE, 90)?;
        (Cow::Owned(encoded), ext)
    };

    let hash = sha256_hex_prefix(&final_bytes);
    Ok((final_bytes, hash, ext))
}

fn touch_mtime(path: &Path) {
    if let Ok(file) = OpenOptions::new().write(true).open(path) {
        let _ = file.set_modified(SystemTime::now());
    }
}

fn write_cover_bytes(
    final_bytes: &[u8],
    hash: &str,
    ext: &'static str,
) -> Result<SavedCover, CoverError> {
    let dest = CoverPaths::content_cover_path(hash, ext);
    let cover_dir = CoverPaths::cover_dir();
    fs::create_dir_all(&cover_dir)?;
    let mut created = false;
    if !dest.exists() {
        let seq = COVER_TMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let tmp = cover_dir.join(format!("c_{hash}.{ext}.tmp.{}.{seq}", std::process::id()));
        if let Err(e) = fs::write(&tmp, final_bytes) {
            let _ = fs::remove_file(&tmp);
            return Err(e.into());
        }
        // `hard_link` atomically ensures only the winning writer gets `created = true`.
        match fs::hard_link(&tmp, &dest) {
            Ok(()) => {
                let _ = fs::remove_file(&tmp);
                created = true;
            }
            Err(e) if e.kind() == ErrorKind::AlreadyExists => {
                let _ = fs::remove_file(&tmp);
                touch_mtime(&dest);
            }
            Err(e)
                if matches!(
                    e.kind(),
                    ErrorKind::Unsupported
                        | ErrorKind::PermissionDenied
                        | ErrorKind::CrossesDevices
                ) =>
            {
                if let Err(re) = fs::rename(&tmp, &dest) {
                    let _ = fs::remove_file(&tmp);
                    if !dest.exists() {
                        return Err(re.into());
                    }
                } else {
                    created = true;
                }
            }
            Err(e) => {
                let _ = fs::remove_file(&tmp);
                return Err(e.into());
            }
        }
    } else {
        touch_mtime(&dest);
    }
    Ok(SavedCover {
        path: dest,
        created,
    })
}

/// Decodes, EXIF-orients, downscales if needed, and hashes cover image bytes in memory without
/// holding any store lock or writing to disk.
pub fn prepare_cover_bytes(data: &[u8]) -> Result<PreparedCover, CoverError> {
    let (bytes, hash, ext) = prepare_cover_cow(data)?;
    Ok(PreparedCover {
        bytes: bytes.into_owned(),
        hash,
        ext,
    })
}

/// Writes a previously prepared cover into `CoverPaths::cover_dir()`, returning whether a new
/// file was created on disk.
pub fn write_prepared_cover(prepared: &PreparedCover) -> Result<SavedCover, CoverError> {
    write_cover_bytes(&prepared.bytes, &prepared.hash, prepared.ext)
}

/// Normalizes cover bytes to <= `MAX_FULL_COVER_EDGE` (applying EXIF orientation) and writes a
/// single content-addressed file `c_<hash>.<ext>` under `CoverPaths::cover_dir()`, skipping the write
/// if identical bytes already exist on disk.
pub fn save_cover_bytes(data: &[u8]) -> Result<PathBuf, CoverError> {
    let (final_bytes, hash, ext) = prepare_cover_cow(data)?;
    Ok(write_cover_bytes(&final_bytes, &hash, ext)?.path)
}

pub struct PlaylistCovers;

impl PlaylistCovers {
    pub const EXTENSIONS: &'static [&'static str] = &["jpg", "jpeg", "png", "webp"];

    pub fn is_supported_ext(path: &Path) -> bool {
        path.extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| Self::EXTENSIONS.iter().any(|x| x.eq_ignore_ascii_case(e)))
    }

    pub fn save(source_path: &Path) -> Result<String, CoverError> {
        if !Self::is_supported_ext(source_path) {
            return Err(CoverError::UnsupportedFormat);
        }
        let meta = fs::metadata(source_path).map_err(|_| CoverError::InsaneImage)?;
        if meta.len() > MAX_COVER_FILE_BYTES {
            return Err(CoverError::InsaneImage);
        }
        let bytes = fs::read(source_path)?;
        let path = save_cover_bytes(&bytes)?;
        Ok(CoverPaths::to_file_url(&path))
    }

    pub fn thumb_for(full_path: &Path) -> PathBuf {
        const SUFFIX: &[u8] = b"_full.jpg";
        let s = full_path.to_string_lossy();
        if s.len() >= SUFFIX.len()
            && s.as_bytes()[s.len() - SUFFIX.len()..].eq_ignore_ascii_case(SUFFIX)
        {
            let sibling = PathBuf::from(format!("{}_thumb.jpg", &s[..s.len() - SUFFIX.len()]));
            if sibling.exists() {
                return sibling;
            }
        }
        full_path.to_path_buf()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::{TempDir, solid_png, solid_rgba_png};

    fn dims_of(bytes: &[u8]) -> (u32, u32) {
        let img = ImageReader::new(Cursor::new(bytes))
            .with_guessed_format()
            .unwrap()
            .decode()
            .unwrap();
        (img.width(), img.height())
    }

    #[test]
    fn save_cover_bytes_deduplicates_caps_and_preserves_formats() {
        let dir = TempDir::new("cover-save");
        crate::paths::with_test_app_dir(&dir, || {
            let blank = |w, h| DynamicImage::ImageRgb8(image::RgbImage::new(w, h));
            let small_jpg =
                super::super::encode::encode_capped_jpeg(&blank(500, 400), u32::MAX, 88).unwrap();
            let p1 = save_cover_bytes(&small_jpg).unwrap();
            assert_eq!(save_cover_bytes(&small_jpg).unwrap(), p1);
            assert!(CoverPaths::is_content_cover_file(&p1));
            assert_eq!(p1.extension().and_then(|e| e.to_str()), Some("jpg"));
            assert_eq!(fs::read(&p1).unwrap(), small_jpg);

            let png_bytes = solid_rgba_png(320, 240, [0, 0, 0, 0]);
            let p_png = save_cover_bytes(&png_bytes).unwrap();
            assert_eq!(p_png.extension().and_then(|e| e.to_str()), Some("png"));
            assert_eq!(fs::read(&p_png).unwrap(), png_bytes);

            let mut small_webp = Cursor::new(Vec::new());
            blank(200, 200)
                .write_to(&mut small_webp, ImageFormat::WebP)
                .unwrap();
            let webp_bytes = small_webp.into_inner();
            let p_webp = save_cover_bytes(&webp_bytes).unwrap();
            assert_eq!(p_webp.extension().and_then(|e| e.to_str()), Some("webp"));
            assert_eq!(fs::read(&p_webp).unwrap(), webp_bytes);

            let big_jpg =
                super::super::encode::encode_capped_jpeg(&blank(2400, 1200), u32::MAX, 90).unwrap();
            let p_big = save_cover_bytes(&big_jpg).unwrap();
            assert_eq!(dims_of(&fs::read(&p_big).unwrap()), (720, 360));
            assert_eq!(fs::read_dir(CoverPaths::cover_dir()).unwrap().count(), 4);

            // Truncated JPEG header without valid pixel stream must fail decode.
            assert!(
                save_cover_bytes(b"\xFF\xD8\xFF\xC0\x00\x0B\x08\x00\x10\x00\x10\x03\x01").is_err()
            );
        });
    }

    #[test]
    fn save_cover_bytes_rotates_exif_and_handles_non_ascii_thumb_for() {
        let dir = TempDir::new("cover-exif");
        crate::paths::with_test_app_dir(&dir, || {
            let blank = DynamicImage::ImageRgb8(image::RgbImage::new(200, 100));
            let raw_jpg = super::super::encode::encode_capped_jpeg(&blank, u32::MAX, 88).unwrap();
            let app1: Vec<u8> = vec![
                b'E', b'x', b'i', b'f', 0, 0, b'I', b'I', 0x2A, 0x00, 0x08, 0x00, 0x00, 0x00, 0x01,
                0x00, 0x12, 0x01, 0x03, 0x00, 0x01, 0x00, 0x00, 0x00, 0x06, 0x00, 0x00, 0x00, 0x00,
                0x00, 0x00, 0x00,
            ];
            let seg_len = (app1.len() + 2) as u16;
            let mut exif_jpg = Vec::with_capacity(raw_jpg.len() + app1.len() + 4);
            exif_jpg.extend_from_slice(&[0xFF, 0xD8, 0xFF, 0xE1]);
            exif_jpg.extend_from_slice(&seg_len.to_be_bytes());
            exif_jpg.extend_from_slice(&app1);
            exif_jpg.extend_from_slice(&raw_jpg[2..]);

            let exif_out = save_cover_bytes(&exif_jpg).unwrap();
            let exif_bytes = fs::read(&exif_out).unwrap();
            assert_eq!(dims_of(&exif_bytes), (100, 200));

            // Non-ASCII short path must not panic in thumb_for.
            let non_ascii = Path::new("/tmp/あいうx");
            assert_eq!(PlaylistCovers::thumb_for(non_ascii), non_ascii);
            let _ = solid_png(16, 16, [1, 2, 3]);
        });
    }
}
