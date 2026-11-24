use std::borrow::Cow;
use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use fast_image_resize::images::Image as FirImage;
use fast_image_resize::{FilterType, IntoImageView, ResizeAlg, ResizeOptions, Resizer};
use image::codecs::jpeg::JpegEncoder;
use image::codecs::png::PngEncoder;
use image::metadata::Orientation;
use image::{ColorType, DynamicImage, ImageDecoder, ImageEncoder, ImageFormat, ImageReader};
use thiserror::Error;

use crate::collections::{CollectionsData, CollectionsStore};
use crate::library::{LibraryError, LibraryMap, LibraryStore, try_delete};
use crate::paths::CoverPaths;

pub const MAX_COVER_FILE_BYTES: u64 = 30 * 1024 * 1024;
pub const MAX_PIXELS: u64 = 64_000_000;
pub const MAX_DIMENSION: u32 = 16_384;

#[derive(Debug, Error)]
pub enum CoverError {
    #[error("Unsupported image format. Please use JPG, PNG, or WEBP.")]
    UnsupportedFormat,
    #[error("Image could not be read, or is too large.")]
    InsaneImage,
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Image decode/encode error: {0}")]
    Image(String),
}

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

    fn fits(dims: Option<(u32, u32)>) -> bool {
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

/// Longest edge allowed for stored full-size covers. iced uploads images above ~2 MB
/// RGBA into a dedicated texture (a 4096px cover cost ~117 MB of GPU memory). 720px keeps
/// every aspect ratio under that 2 MB (724x724) limit, so full covers share the image atlas
/// with the thumbnails; it still covers the 510px full-screen cover at scale 1.
pub const MAX_FULL_COVER_EDGE: u32 = 720;

/// Size of `w` x `h` fitted within `max_edge` x `max_edge`, preserving aspect ratio.
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

/// Encodes a full-size cover fitted within `max_edge`. Opaque images become JPEG; images with
/// an alpha channel become PNG so transparency survives. Returns `(bytes, extension)`.
pub(crate) fn encode_capped_full_with_ext(
    img: &DynamicImage,
    max_edge: u32,
    quality: u8,
) -> Result<(Vec<u8>, &'static str), CoverError> {
    if img.color().has_alpha() {
        Ok((encode_capped_png(img, max_edge)?, "png"))
    } else {
        Ok((encode_capped_jpeg(img, max_edge, quality)?, "jpg"))
    }
}

#[cfg(test)]
pub(crate) fn encode_capped_full(
    img: &DynamicImage,
    max_edge: u32,
    quality: u8,
) -> Result<Vec<u8>, CoverError> {
    encode_capped_full_with_ext(img, max_edge, quality).map(|(bytes, _)| bytes)
}

/// Encode `img` as JPEG fitted within `max_edge` x `max_edge`, preserving aspect ratio.
/// Images already within the cap are encoded without resizing.
pub(crate) fn encode_capped_jpeg(
    img: &DynamicImage,
    max_edge: u32,
    quality: u8,
) -> Result<Vec<u8>, CoverError> {
    let (dw, dh) = capped_dims(img.width(), img.height(), max_edge);
    encode_to_size(img, dw, dh, quality)
}

fn encode_capped_png(img: &DynamicImage, max_edge: u32) -> Result<Vec<u8>, CoverError> {
    let rgba = img.to_rgba8();
    let (dw, dh) = capped_dims(rgba.width(), rgba.height(), max_edge);
    let pixels = if (dw, dh) != (rgba.width(), rgba.height()) {
        let pixel_type = rgba
            .pixel_type()
            .ok_or_else(|| CoverError::Image("Unsupported pixel type".to_owned()))?;
        let mut dst = FirImage::new(dw, dh, pixel_type);
        let opts = ResizeOptions::new().resize_alg(ResizeAlg::Convolution(FilterType::Mitchell));
        Resizer::new()
            .resize(&rgba, &mut dst, &opts)
            .map_err(|e| CoverError::Image(e.to_string()))?;
        dst.into_vec()
    } else {
        rgba.into_raw()
    };
    let mut out = Vec::new();
    PngEncoder::new(&mut out)
        .write_image(&pixels, dw, dh, ColorType::Rgba8.into())
        .map_err(|e| CoverError::Image(e.to_string()))?;
    Ok(out)
}

fn encode_to_size(
    img: &DynamicImage,
    dst_w: u32,
    dst_h: u32,
    quality: u8,
) -> Result<Vec<u8>, CoverError> {
    let rgb = img.to_rgb8();
    let pixels = if (dst_w, dst_h) != (rgb.width(), rgb.height()) {
        let pixel_type = rgb
            .pixel_type()
            .ok_or_else(|| CoverError::Image("Unsupported pixel type".to_owned()))?;
        let mut dst = FirImage::new(dst_w, dst_h, pixel_type);
        let mut resizer = Resizer::new();
        let opts = ResizeOptions::new()
            .resize_alg(ResizeAlg::Convolution(FilterType::Mitchell));
        resizer
            .resize(&rgb, &mut dst, &opts)
            .map_err(|e| CoverError::Image(e.to_string()))?;
        dst.into_vec()
    } else {
        rgb.into_raw()
    };

    let mut out = Vec::new();
    let mut enc = JpegEncoder::new_with_quality(&mut out, quality);
    enc.encode(&pixels, dst_w, dst_h, ColorType::Rgb8.into())
        .map_err(|e| CoverError::Image(e.to_string()))?;
    Ok(out)
}

static COVER_TMP_SEQ: AtomicU64 = AtomicU64::new(0);

fn compress_sha256_block(h: &mut [u32; 8], block: &[u8; 64]) {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
        0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
        0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
        0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
        0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
        0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
        0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
    ];

    let mut w = [0u32; 64];
    for (i, word) in block.as_chunks::<4>().0.iter().enumerate() {
        w[i] = u32::from_be_bytes(*word);
    }
    for i in 16..64 {
        let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
        let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
        w[i] = w[i - 16]
            .wrapping_add(s0)
            .wrapping_add(w[i - 7])
            .wrapping_add(s1);
    }

    let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = *h;
    for i in 0..64 {
        let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
        let ch = (e & f) ^ ((!e) & g);
        let temp1 = hh
            .wrapping_add(s1)
            .wrapping_add(ch)
            .wrapping_add(K[i])
            .wrapping_add(w[i]);
        let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
        let maj = (a & b) ^ (a & c) ^ (b & c);
        let temp2 = s0.wrapping_add(maj);

        hh = g;
        g = f;
        f = e;
        e = d.wrapping_add(temp1);
        d = c;
        c = b;
        b = a;
        a = temp1.wrapping_add(temp2);
    }

    h[0] = h[0].wrapping_add(a);
    h[1] = h[1].wrapping_add(b);
    h[2] = h[2].wrapping_add(c);
    h[3] = h[3].wrapping_add(d);
    h[4] = h[4].wrapping_add(e);
    h[5] = h[5].wrapping_add(f);
    h[6] = h[6].wrapping_add(g);
    h[7] = h[7].wrapping_add(hh);
}

/// FIPS 180-4 SHA-256 digest of `data`.
pub fn sha256(data: &[u8]) -> [u8; 32] {
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a,
        0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
    ];

    let (blocks, rem) = data.as_chunks::<64>();
    for block in blocks {
        compress_sha256_block(&mut h, block);
    }

    let bit_len = (data.len() as u64).wrapping_mul(8);
    let tail_len = if rem.len() < 56 { 64 } else { 128 };
    let mut tail = [0u8; 128];
    tail[..rem.len()].copy_from_slice(rem);
    tail[rem.len()] = 0x80;
    tail[tail_len - 8..tail_len].copy_from_slice(&bit_len.to_be_bytes());
    for block in tail[..tail_len].as_chunks::<64>().0 {
        compress_sha256_block(&mut h, block);
    }

    let mut out = [0u8; 32];
    for (i, word) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    out
}

/// First 12 bytes (24 lowercase hex characters) of `sha256(data)`.
pub fn sha256_hex_prefix(data: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let digest = sha256(data);
    let mut out = String::with_capacity(24);
    for &b in &digest[..12] {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

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
    if !ImageLimits::is_sane_bytes(data) {
        return Err(CoverError::InsaneImage);
    }
    let reader = ImageReader::new(Cursor::new(data))
        .with_guessed_format()
        .map_err(|e| CoverError::Image(e.to_string()))?;
    let fmt = reader.format();
    let mut decoder = reader
        .into_decoder()
        .map_err(|e| CoverError::Image(e.to_string()))?;
    let (w, h) = decoder.dimensions();
    let orient = decoder.orientation().unwrap_or(Orientation::NoTransforms);

    let (final_bytes, ext): (Cow<'_, [u8]>, &'static str) = if w.max(h) <= MAX_FULL_COVER_EDGE
        && orient == Orientation::NoTransforms
        && matches!(fmt, Some(ImageFormat::Jpeg | ImageFormat::Png | ImageFormat::WebP))
    {
        let ext = match fmt {
            Some(ImageFormat::Png) => "png",
            Some(ImageFormat::WebP) => "webp",
            _ => "jpg",
        };
        (Cow::Borrowed(data), ext)
    } else {
        let mut img =
            DynamicImage::from_decoder(decoder).map_err(|e| CoverError::Image(e.to_string()))?;
        img.apply_orientation(orient);
        let (encoded, ext) = encode_capped_full_with_ext(&img, MAX_FULL_COVER_EDGE, 90)?;
        (Cow::Owned(encoded), ext)
    };

    let hash = sha256_hex_prefix(&final_bytes);
    Ok((final_bytes, hash, ext))
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
        if let Err(e) = fs::rename(&tmp, &dest) {
            let _ = fs::remove_file(&tmp);
            if !dest.exists() {
                return Err(e.into());
            }
        } else {
            created = true;
        }
    } else if let Ok(file) = OpenOptions::new().write(true).open(&dest) {
        // Keep mtime current so a concurrent GC grace window treats reused covers as active.
        let _ = file.set_modified(SystemTime::now());
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
        if !ImageLimits::is_sane_path(source_path) {
            return Err(CoverError::InsaneImage);
        }
        let bytes = fs::read(source_path)?;
        let path = save_cover_bytes(&bytes)?;
        Ok(CoverPaths::to_file_url(&path))
    }

    pub fn thumb_for(full_path: &Path) -> std::path::PathBuf {
        let s = full_path.to_string_lossy();
        if s.len() >= "_full.jpg".len()
            && s[s.len() - "_full.jpg".len()..].eq_ignore_ascii_case("_full.jpg")
        {
            let sibling =
                std::path::PathBuf::from(format!("{}_thumb.jpg", &s[..s.len() - "_full.jpg".len()]));
            if sibling.exists() {
                return sibling;
            }
        }
        full_path.to_path_buf()
    }
}

/// Collects every playlist cover path (and any legacy `_thumb.jpg` sibling) from `collections`.
pub fn collect_playlist_cover_paths(collections: &CollectionsData, out: &mut HashSet<PathBuf>) {
    for pl in &collections.playlists {
        if let Some(p) = CoverPaths::to_local_path(pl.cover_art.as_deref()) {
            let thumb = PlaylistCovers::thumb_for(&p);
            if thumb != p {
                out.insert(thumb);
            }
            out.insert(p);
        }
    }
}

/// Every cover file path referenced by albums in `library` or playlists in `collections`.
pub fn referenced_cover_paths(
    library: &LibraryMap,
    collections: &CollectionsData,
) -> HashSet<PathBuf> {
    let mut set = HashSet::new();
    for album in library.values() {
        if let Some(p) = CoverPaths::to_local_path(album.cover_art.as_deref()) {
            set.insert(p);
        }
        if let Some(p) = CoverPaths::to_local_path(album.cover_art_full.as_deref()) {
            set.insert(p);
        }
    }
    collect_playlist_cover_paths(collections, &mut set);
    set
}

/// Deletes any managed cover file in `candidates` that is no longer referenced in `library`,
/// `collections`, or `extra_refs`.
pub fn delete_unreferenced_covers_with_extra(
    candidates: &[PathBuf],
    library: &LibraryMap,
    collections: &CollectionsData,
    extra_refs: &HashSet<PathBuf>,
) {
    if candidates.is_empty() {
        return;
    }
    let mut referenced = referenced_cover_paths(library, collections);
    referenced.extend(extra_refs.iter().cloned());
    for p in candidates {
        if CoverPaths::is_managed_cover_file(p) && !referenced.contains(p) {
            try_delete(p);
        }
    }
}

/// Deletes any managed cover file in `candidates` that is no longer referenced in `library` or `collections`.
pub fn delete_unreferenced_covers(
    candidates: &[PathBuf],
    library: &LibraryMap,
    collections: &CollectionsData,
) {
    delete_unreferenced_covers_with_extra(candidates, library, collections, &HashSet::new());
}

/// Migrates legacy playlist covers stored under `CoverPaths::cover_dir()` to single-file
/// content-addressed covers (`c_<hash>.<ext>`). Returns `true` if `collections` changed.
pub fn migrate_playlist_covers(collections: &mut CollectionsData) -> bool {
    let cover_dir = CoverPaths::cover_dir();
    let mut changed = false;
    for pl in &mut collections.playlists {
        let Some(local) = CoverPaths::to_local_path(pl.cover_art.as_deref()) else {
            continue;
        };
        if local.parent() != Some(cover_dir.as_path()) {
            continue;
        }
        if CoverPaths::is_content_cover_file(&local) && local.is_file() {
            continue;
        }
        if !local.is_file() {
            continue;
        }
        if let Ok(bytes) = fs::read(&local)
            && let Ok(new_path) = save_cover_bytes(&bytes)
        {
            let url = CoverPaths::to_file_url(&new_path);
            if pl.cover_art.as_deref() != Some(url.as_str()) {
                pl.cover_art = Some(url);
                changed = true;
            }
        }
    }
    changed
}

#[derive(Debug, Clone, Default)]
pub struct CoverDedupResult {
    pub migrated_albums: usize,
    pub removed_files: usize,
    pub changed_paths: Vec<PathBuf>,
}

/// Migrates legacy dual-file (`_thumb.jpg` / `_full.jpg`) or oversized album covers to deduplicated
/// single-file content-addressed covers, then deletes unreferenced managed files in `cover_dir`.
pub fn deduplicate_and_sweep_covers(
    extra_collections: Option<&CollectionsData>,
    extra_refs: Option<&Mutex<HashSet<PathBuf>>>,
) -> Result<CoverDedupResult, LibraryError> {
    let _guard = LibraryStore::write_lock()
        .lock()
        .expect("library write lock poisoned");
    let sweep_started = SystemTime::now();
    let pre_collections = CollectionsStore::load();
    let mut library = LibraryStore::load_raw()?;
    let mut explicitly_replaced: HashSet<PathBuf> = HashSet::new();
    let mut checked_ok: HashSet<PathBuf> = HashSet::new();
    let mut result = CoverDedupResult::default();

    for album in library.values_mut() {
        let thumb = CoverPaths::to_local_path(album.cover_art.as_deref());
        let full = CoverPaths::to_local_path(album.cover_art_full.as_deref());

        if thumb.is_some()
            && thumb == full
            && let Some(p) = thumb.as_deref()
        {
            if checked_ok.contains(p) {
                continue;
            }
            if CoverPaths::is_content_cover_file(p) && p.is_file() {
                let within_cap = ImageReader::open(p)
                    .and_then(|r| r.with_guessed_format())
                    .ok()
                    .and_then(|r| r.into_dimensions().ok())
                    .is_some_and(|(w, h)| w.max(h) <= MAX_FULL_COVER_EDGE);
                if within_cap {
                    checked_ok.insert(p.to_path_buf());
                    continue;
                }
            }
        }

        let src = full
            .as_deref()
            .filter(|p| p.is_file())
            .or_else(|| thumb.as_deref().filter(|p| p.is_file()));
        if let Some(src_path) = src {
            if let Ok(bytes) = fs::read(src_path)
                && let Ok(new_path) = save_cover_bytes(&bytes)
            {
                checked_ok.insert(new_path.clone());
                let url = CoverPaths::to_file_url(&new_path);
                for old in [thumb.as_ref(), full.as_ref()].into_iter().flatten() {
                    if old != &new_path {
                        explicitly_replaced.insert(old.clone());
                        if !result.changed_paths.contains(old) {
                            result.changed_paths.push(old.clone());
                        }
                    }
                }
                if !result.changed_paths.contains(&new_path) {
                    result.changed_paths.push(new_path.clone());
                }
                album.cover_art = Some(url.clone());
                album.cover_art_full = Some(url);
                result.migrated_albums += 1;
            }
        } else if album.cover_art.is_some() || album.cover_art_full.is_some() {
            album.cover_art = None;
            album.cover_art_full = None;
            result.migrated_albums += 1;
        }
    }

    if result.migrated_albums > 0 {
        LibraryStore::save_raw(&library)?;
    }

    let post_collections = CollectionsStore::load();
    let mut referenced = referenced_cover_paths(&library, &pre_collections);
    collect_playlist_cover_paths(&post_collections, &mut referenced);
    if let Some(c) = extra_collections {
        collect_playlist_cover_paths(c, &mut referenced);
    }
    if let Some(refs) = extra_refs
        && let Ok(guard) = refs.lock()
    {
        referenced.extend(guard.iter().cloned());
    }

    let cutoff = sweep_started.checked_sub(Duration::from_secs(5));
    if let Ok(entries) = fs::read_dir(CoverPaths::cover_dir()) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_file() && CoverPaths::is_managed_cover_file(&p) && !referenced.contains(&p) {
                let old_enough = fs::metadata(&p)
                    .and_then(|m| m.modified())
                    .ok()
                    .is_none_or(|mtime| cutoff.is_none_or(|c| mtime < c));
                if (explicitly_replaced.contains(&p) || old_enough) && fs::remove_file(&p).is_ok() {
                    result.removed_files += 1;
                }
            }
        }
    }

    Ok(result)
}

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
        let mut x = 0u32;
        while x < width {
            let idx = ((y as usize) * (width as usize) + (x as usize)) * 3;
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
            x += step_x;
        }
        y += step_y;
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
    // Keep the accent readable (about 6:1) against the `#121212` panel: lift value,
    // then shed saturation, until relative luminance reaches the floor.
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
        // Make ~3% of pixels vivid red.
        for i in 0..70 {
            let base = i * 3;
            pixels[base] = 220;
            pixels[base + 1] = 28;
            pixels[base + 2] = 28;
        }
        let pal = extract_cover_palette_from_rgb(&pixels, 48, 48);
        assert!(pal.vivid);
        assert!(pal.accent[0] > pal.accent[1] + 50);
        assert!(pal.accent[0] > pal.accent[2] + 50);
    }

    #[test]
    fn pure_blue_cover_accent_meets_contrast_floor() {
        let mut pixels = Vec::with_capacity(32 * 32 * 3);
        for _ in 0..(32 * 32) {
            pixels.extend_from_slice(&[0, 0, 255]);
        }
        let pal = extract_cover_palette_from_rgb(&pixels, 32, 32);
        assert!(pal.vivid);
        assert!(
            relative_luminance(pal.accent) >= MIN_ACCENT_LUMINANCE,
            "accent {:?} luminance {}",
            pal.accent,
            relative_luminance(pal.accent)
        );
        // Still recognisably blue.
        assert!(pal.accent[2] > pal.accent[0] && pal.accent[2] > pal.accent[1]);
    }

    #[test]
    fn large_dimensions_do_not_overflow_index_math() {
        // 70_000 * 70_000 overflows u32; a short buffer must simply stop early.
        let pixels = vec![200u8; 3 * 64];
        let pal = extract_cover_palette_from_rgb(&pixels, 70_000, 70_000);
        assert!(relative_luminance(pal.accent) >= MIN_ACCENT_LUMINANCE);
    }

    #[test]
    fn grayscale_cover_falls_back_to_silver() {
        let mut pixels = Vec::with_capacity(24 * 24 * 3);
        for _ in 0..(24 * 24) {
            pixels.extend_from_slice(&[90, 90, 90]);
        }
        let pal = extract_cover_palette_from_rgb(&pixels, 24, 24);
        assert!(!pal.vivid);
        assert!((pal.accent[0] as i16 - pal.accent[1] as i16).abs() <= 2);
        assert!((pal.accent[1] as i16 - pal.accent[2] as i16).abs() <= 2);
        assert!(pal.accent[0] >= 200 && pal.accent[0] <= 230);
    }

    fn dims_of(bytes: &[u8]) -> (u32, u32) {
        let img = ImageReader::new(Cursor::new(bytes))
            .with_guessed_format()
            .unwrap()
            .decode()
            .unwrap();
        (img.width(), img.height())
    }

    fn blank(w: u32, h: u32) -> DynamicImage {
        DynamicImage::ImageRgb8(image::RgbImage::new(w, h))
    }

    #[test]
    fn capped_jpeg_landscape() {
        let out = encode_capped_jpeg(&blank(3000, 2000), MAX_FULL_COVER_EDGE, 90).unwrap();
        assert_eq!(dims_of(&out), (720, 480));
    }

    #[test]
    fn capped_jpeg_portrait() {
        let out = encode_capped_jpeg(&blank(2000, 3000), MAX_FULL_COVER_EDGE, 90).unwrap();
        assert_eq!(dims_of(&out), (480, 720));
    }

    #[test]
    fn capped_full_keeps_alpha_as_png() {
        let mut rgba = image::RgbaImage::new(2048, 1024);
        rgba.put_pixel(0, 0, image::Rgba([255, 0, 0, 0]));
        let out = encode_capped_full(&DynamicImage::ImageRgba8(rgba), MAX_FULL_COVER_EDGE, 90).unwrap();
        assert!(out.starts_with(&[0x89, b'P', b'N', b'G']));
        let img = ImageReader::new(Cursor::new(&out)).with_guessed_format().unwrap().decode().unwrap();
        assert_eq!((img.width(), img.height()), (720, 360));
        assert!(img.color().has_alpha());
    }

    #[test]
    fn capped_full_opaque_is_jpeg() {
        let out = encode_capped_full(&blank(3000, 3000), MAX_FULL_COVER_EDGE, 90).unwrap();
        assert!(out.starts_with(&[0xFF, 0xD8]));
        assert_eq!(dims_of(&out), (720, 720));
    }

    #[test]
    fn capped_jpeg_small_unchanged() {
        let out = encode_capped_jpeg(&blank(700, 500), MAX_FULL_COVER_EDGE, 90).unwrap();
        assert_eq!(dims_of(&out), (700, 500));
        let out = encode_capped_jpeg(&blank(500, 700), MAX_FULL_COVER_EDGE, 90).unwrap();
        assert_eq!(dims_of(&out), (500, 700));
    }

    #[test]
    fn sha256_matches_nist_vectors() {
        fn hex(bytes: &[u8]) -> String {
            bytes.iter().map(|b| format!("{b:02x}")).collect()
        }
        assert_eq!(
            hex(&sha256(b"")),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            hex(&sha256(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            hex(&sha256(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq")),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
        let boundary_vectors: &[(usize, &str)] = &[
            (55, "9f4390f8d30c2dd92ec9f095b65e2b9ae9b0a925a5258e241c9f1e910f734318"),
            (56, "b35439a4ac6f0948b6d6f9e3c6af0f5f590ce20f1bde7090ef7970686ec6738a"),
            (63, "7d3e74a05d7db15bce4ad9ec0658ea98e3f06eeecf16b4c6fff2da457ddc2f34"),
            (64, "ffe054fe7ae0cb6dc65c3af9b61d5209f439851db43d0ba5997337df154668eb"),
            (65, "635361c48bb9eab14198e76ea8ab7f1a41685d6ad62aa9146d301d4f17eb0ae0"),
            (128, "6836cf13bac400e9105071cd6af47084dfacad4e5e302c94bfed24e013afb73e"),
        ];
        for &(len, expected) in boundary_vectors {
            assert_eq!(hex(&sha256(&vec![b'a'; len])), expected, "len={len}");
        }
    }

    #[test]
    fn save_cover_bytes_deduplicates_caps_and_preserves_formats() {
        let dir = std::env::temp_dir().join(format!("retro-cover-save-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        crate::paths::with_test_app_dir(&dir, || {
            let small_jpg = encode_capped_jpeg(&blank(500, 400), u32::MAX, 88).unwrap();
            let p1 = save_cover_bytes(&small_jpg).unwrap();
            let p2 = save_cover_bytes(&small_jpg).unwrap();
            assert_eq!(p1, p2);
            assert!(CoverPaths::is_content_cover_file(&p1));
            assert_eq!(p1.extension().and_then(|e| e.to_str()), Some("jpg"));
            assert_eq!(fs::read(&p1).unwrap(), small_jpg);

            let mut small_png = Cursor::new(Vec::new());
            DynamicImage::ImageRgba8(image::RgbaImage::new(320, 240))
                .write_to(&mut small_png, ImageFormat::Png)
                .unwrap();
            let png_bytes = small_png.into_inner();
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

            let big_jpg = encode_capped_jpeg(&blank(2400, 1200), u32::MAX, 90).unwrap();
            let p_big = save_cover_bytes(&big_jpg).unwrap();
            assert_eq!(dims_of(&fs::read(&p_big).unwrap()), (720, 360));

            let count = fs::read_dir(CoverPaths::cover_dir()).unwrap().count();
            assert_eq!(count, 4);
        });
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn delete_unreferenced_covers_respects_album_and_playlist_refs() {
        let dir = std::env::temp_dir().join(format!("retro-cover-refcount-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        crate::paths::with_test_app_dir(&dir, || {
            let img_a = encode_capped_jpeg(&blank(400, 400), u32::MAX, 88).unwrap();
            let mut img_b_buf = image::RgbImage::new(400, 400);
            img_b_buf.put_pixel(0, 0, image::Rgb([255, 0, 0]));
            let img_b = encode_capped_jpeg(&DynamicImage::ImageRgb8(img_b_buf), u32::MAX, 88).unwrap();
            let shared_path = save_cover_bytes(&img_a).unwrap();
            let orphan_path = save_cover_bytes(&img_b).unwrap();
            let shared_url = CoverPaths::to_file_url(&shared_path);

            let mut lib = LibraryMap::new();
            lib.insert(
                "Album 1".to_owned(),
                crate::library::AlbumDto {
                    cover_art: Some(shared_url.clone()),
                    cover_art_full: Some(shared_url.clone()),
                    ..Default::default()
                },
            );
            let mut cols = CollectionsData::default();
            cols.playlists.push(crate::collections::PlaylistData {
                id: 1,
                name: "Favs".to_owned(),
                cover_art: Some(shared_url.clone()),
                ..Default::default()
            });

            delete_unreferenced_covers(&[shared_path.clone(), orphan_path.clone()], &lib, &cols);
            assert!(shared_path.exists());
            assert!(!orphan_path.exists());

            lib.shift_remove("Album 1");
            delete_unreferenced_covers(std::slice::from_ref(&shared_path), &lib, &cols);
            assert!(shared_path.exists(), "playlist reference must keep shared cover alive");

            cols.playlists.clear();
            delete_unreferenced_covers(std::slice::from_ref(&shared_path), &lib, &cols);
            assert!(!shared_path.exists(), "cover must be deleted once all refs are gone");
        });
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn deduplicate_and_sweep_migrates_legacy_pairs_and_keeps_playlists() {
        let dir = std::env::temp_dir().join(format!("retro-cover-migrate-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        crate::paths::with_test_app_dir(&dir, || {
            let cover_dir = CoverPaths::cover_dir();
            fs::create_dir_all(&cover_dir).unwrap();

            let shared_img = encode_capped_jpeg(&blank(1000, 1000), u32::MAX, 90).unwrap();
            let legacy_a_thumb = CoverPaths::thumb_path("Album A");
            let legacy_a_full = CoverPaths::full_path("Album A");
            let legacy_b_thumb = CoverPaths::thumb_path("Album B");
            let legacy_b_full = CoverPaths::full_path("Album B");
            fs::write(&legacy_a_thumb, &shared_img).unwrap();
            fs::write(&legacy_a_full, &shared_img).unwrap();
            fs::write(&legacy_b_thumb, &shared_img).unwrap();
            fs::write(&legacy_b_full, &shared_img).unwrap();

            let mut pl_buf = image::RgbImage::new(300, 300);
            pl_buf.put_pixel(10, 10, image::Rgb([10, 200, 30]));
            let pl_img = encode_capped_jpeg(&DynamicImage::ImageRgb8(pl_buf), u32::MAX, 90).unwrap();
            let pl_legacy_full = cover_dir.join("native_pl_1_999_full.jpg");
            let pl_legacy_thumb = cover_dir.join("native_pl_1_999_thumb.jpg");
            fs::write(&pl_legacy_full, &pl_img).unwrap();
            fs::write(&pl_legacy_thumb, &pl_img).unwrap();

            let orphan = cover_dir.join("deadbeef_full.jpg");
            fs::write(&orphan, &shared_img).unwrap();
            let old_file = OpenOptions::new().write(true).open(&orphan).unwrap();
            old_file
                .set_modified(SystemTime::now() - Duration::from_secs(60))
                .unwrap();

            let fresh_orphan = cover_dir.join("freshbeef_full.jpg");
            fs::write(&fresh_orphan, &shared_img).unwrap();

            let mut lib = LibraryMap::new();
            lib.insert(
                "Album A".to_owned(),
                crate::library::AlbumDto {
                    name: Some("Album A".to_owned()),
                    cover_art: Some(CoverPaths::to_file_url(&legacy_a_thumb)),
                    cover_art_full: Some(CoverPaths::to_file_url(&legacy_a_full)),
                    ..Default::default()
                },
            );
            lib.insert(
                "Album B".to_owned(),
                crate::library::AlbumDto {
                    name: Some("Album B".to_owned()),
                    cover_art: Some(CoverPaths::to_file_url(&legacy_b_thumb)),
                    cover_art_full: Some(CoverPaths::to_file_url(&legacy_b_full)),
                    ..Default::default()
                },
            );
            LibraryStore::save_raw(&lib).unwrap();

            let mut cols = CollectionsData::default();
            cols.playlists.push(crate::collections::PlaylistData {
                id: 1,
                name: "PL".to_owned(),
                cover_art: Some(CoverPaths::to_file_url(&pl_legacy_full)),
                ..Default::default()
            });
            assert!(migrate_playlist_covers(&mut cols));
            CollectionsStore::save(&cols);

            let res = deduplicate_and_sweep_covers(Some(&cols), None).unwrap();
            assert_eq!(res.migrated_albums, 2);
            assert!(!legacy_a_thumb.exists());
            assert!(!legacy_a_full.exists());
            assert!(!legacy_b_thumb.exists());
            assert!(!legacy_b_full.exists());
            assert!(!orphan.exists(), "60s-old orphan must be swept");
            assert!(fresh_orphan.exists(), "<5s orphan must be spared during grace window");

            let updated_lib = LibraryStore::load_raw().unwrap();
            let url_a = updated_lib["Album A"].cover_art.as_ref().unwrap();
            let url_b = updated_lib["Album B"].cover_art.as_ref().unwrap();
            assert_eq!(url_a, url_b, "identical covers must deduplicate to one URL");
            assert_eq!(updated_lib["Album A"].cover_art_full.as_ref(), Some(url_a));
            let dedup_path = CoverPaths::to_local_path(Some(url_a)).unwrap();
            assert!(dedup_path.exists());
            assert_eq!(dims_of(&fs::read(&dedup_path).unwrap()), (720, 720));

            let pl_path = CoverPaths::to_local_path(cols.playlists[0].cover_art.as_deref()).unwrap();
            assert!(pl_path.exists());
            assert!(CoverPaths::is_content_cover_file(&pl_path));
        });
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_cover_bytes_handles_exif_rotation_oversized_alpha_png_and_bmp() {
        let dir = std::env::temp_dir().join(format!("retro-cover-formats-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        crate::paths::with_test_app_dir(&dir, || {
            // 1. Small JPEG with EXIF Orientation = 6 (Rotate90): 200x100 -> 100x200 with EXIF stripped.
            let raw_jpg = encode_capped_jpeg(&blank(200, 100), u32::MAX, 88).unwrap();
            assert_eq!(&raw_jpg[..2], &[0xFF, 0xD8]);
            // Minimal TIFF IFD0 in APP1 with 1 SHORT entry: tag 0x0112 (Orientation) = 6.
            let app1_payload: Vec<u8> = vec![
                b'E', b'x', b'i', b'f', 0, 0, // Exif header
                b'I', b'I', 0x2A, 0x00, 0x08, 0x00, 0x00, 0x00, // TIFF little-endian, IFD0 @ 8
                0x01, 0x00, // 1 entry
                0x12, 0x01, 0x03, 0x00, 0x01, 0x00, 0x00, 0x00, 0x06, 0x00, 0x00, 0x00, // Orientation=6
                0x00, 0x00, 0x00, 0x00, // next IFD = 0
            ];
            let seg_len = (app1_payload.len() + 2) as u16;
            let mut exif_jpg = Vec::with_capacity(raw_jpg.len() + app1_payload.len() + 4);
            exif_jpg.extend_from_slice(&[0xFF, 0xD8, 0xFF, 0xE1]);
            exif_jpg.extend_from_slice(&seg_len.to_be_bytes());
            exif_jpg.extend_from_slice(&app1_payload);
            exif_jpg.extend_from_slice(&raw_jpg[2..]);

            let exif_out = save_cover_bytes(&exif_jpg).unwrap();
            let exif_bytes = fs::read(&exif_out).unwrap();
            assert_eq!(dims_of(&exif_bytes), (100, 200));
            let orient = ImageReader::new(Cursor::new(&exif_bytes))
                .with_guessed_format()
                .unwrap()
                .into_decoder()
                .unwrap()
                .orientation()
                .unwrap_or(Orientation::NoTransforms);
            assert_eq!(orient, Orientation::NoTransforms);

            // 2. Oversized RGBA PNG stays PNG with alpha and is capped at 720px.
            let mut rgba = image::RgbaImage::new(1600, 800);
            rgba.put_pixel(10, 10, image::Rgba([20, 40, 60, 128]));
            let mut png_cur = Cursor::new(Vec::new());
            DynamicImage::ImageRgba8(rgba)
                .write_to(&mut png_cur, ImageFormat::Png)
                .unwrap();
            let png_out = save_cover_bytes(&png_cur.into_inner()).unwrap();
            assert_eq!(png_out.extension().and_then(|e| e.to_str()), Some("png"));
            let decoded_png = ImageReader::new(Cursor::new(fs::read(&png_out).unwrap()))
                .with_guessed_format()
                .unwrap()
                .decode()
                .unwrap();
            assert_eq!((decoded_png.width(), decoded_png.height()), (720, 360));
            assert!(decoded_png.color().has_alpha());

            // 3. BMP input converts cleanly to normalized JPEG.
            let mut bmp_cur = Cursor::new(Vec::new());
            blank(300, 200)
                .write_to(&mut bmp_cur, ImageFormat::Bmp)
                .unwrap();
            let bmp_out = save_cover_bytes(&bmp_cur.into_inner()).unwrap();
            assert_eq!(bmp_out.extension().and_then(|e| e.to_str()), Some("jpg"));
            assert_eq!(dims_of(&fs::read(&bmp_out).unwrap()), (300, 200));
        });
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn migrate_playlist_covers_ignores_paths_outside_cover_dir() {
        let dir = std::env::temp_dir().join(format!("retro-pl-outside-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        crate::paths::with_test_app_dir(&dir, || {
            let outside_dir = dir.join("external");
            fs::create_dir_all(&outside_dir).unwrap();
            let outside_img = outside_dir.join("custom.jpg");
            let jpg = encode_capped_jpeg(&blank(240, 240), u32::MAX, 88).unwrap();
            fs::write(&outside_img, &jpg).unwrap();
            let outside_url = CoverPaths::to_file_url(&outside_img);

            let mut cols = CollectionsData::default();
            cols.playlists.push(crate::collections::PlaylistData {
                id: 7,
                name: "External".to_owned(),
                cover_art: Some(outside_url.clone()),
                ..Default::default()
            });

            assert!(!migrate_playlist_covers(&mut cols));
            assert_eq!(cols.playlists[0].cover_art.as_deref(), Some(outside_url.as_str()));
            assert!(outside_img.exists());
        });
        let _ = fs::remove_dir_all(&dir);
    }
}
