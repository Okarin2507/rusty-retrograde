use std::path::{Path, PathBuf};

use crate::covers::{PlaylistCovers, sha256_hex_prefix};
use crate::paths::CoverPaths;

use super::{PLAYLIST_COVER_PREFIX, PlaylistIoError};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackupCoverFile {
    pub name: String,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone, Default)]
pub struct BackupBundle {
    pub collections_json: Option<String>,
    pub stats_json: Option<String>,
    pub settings_json: Option<String>,
    pub playlist_covers: Vec<BackupCoverFile>,
}

/// Validates Android `native_pl_<1..=120 allowed chars>.(jpg|jpeg|png|webp)` filenames,
/// matching `BackupArchive.kt`'s `COVER_NAME` regex and path checks.
pub fn is_android_cover_filename(name: &str) -> bool {
    if name.is_empty()
        || name.contains('/')
        || name.contains('\\')
        || name.contains('\0')
        || name.contains("..")
        || !PlaylistCovers::is_supported_ext(Path::new(name))
    {
        return false;
    }
    let Some(rest) = name.strip_prefix(PLAYLIST_COVER_PREFIX) else {
        return false;
    };
    let Some((stem, _ext)) = rest.rsplit_once('.') else {
        return false;
    };
    (1..=120).contains(&stem.len())
        && stem
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
}

/// Validates Desktop content-addressed `c_<24-hex>.(jpg|jpeg|png|webp)` filenames.
pub fn is_content_cover_filename(name: &str) -> bool {
    if name.is_empty()
        || name.contains('/')
        || name.contains('\\')
        || name.contains('\0')
        || name.contains("..")
        || !PlaylistCovers::is_supported_ext(Path::new(name))
    {
        return false;
    }
    let Some(rest) = name.strip_prefix("c_") else {
        return false;
    };
    let Some((stem, _ext)) = rest.rsplit_once('.') else {
        return false;
    };
    stem.len() == 24 && stem.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

pub(super) fn validate_import_cover_filename(name: &str) -> Result<(), PlaylistIoError> {
    if name.is_empty()
        || name.contains('/')
        || name.contains('\\')
        || name.contains('\0')
        || name.contains("..")
    {
        return Err(PlaylistIoError::InvalidCover {
            name: name.to_owned(),
            reason: "illegal path characters",
        });
    }
    if !(is_android_cover_filename(name) || is_content_cover_filename(name)) {
        return Err(PlaylistIoError::InvalidCover {
            name: name.to_owned(),
            reason: "unexpected cover filename format",
        });
    }
    Ok(())
}

pub(super) fn extract_cover_basename(cover_art: Option<&str>) -> Option<String> {
    let local = CoverPaths::to_local_path(cover_art)?;
    let raw = local.to_string_lossy();
    let base = raw.rsplit(['/', '\\']).next()?.trim();
    (!base.is_empty()).then(|| base.to_owned())
}

fn detect_safe_ext(path: &Path, bytes: &[u8]) -> &'static str {
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        match ext.to_ascii_lowercase().as_str() {
            "jpg" | "jpeg" => return "jpg",
            "png" => return "png",
            "webp" => return "webp",
            _ => {}
        }
    }
    match image::guess_format(bytes) {
        Ok(image::ImageFormat::Png) => "png",
        Ok(image::ImageFormat::WebP) => "webp",
        _ => "jpg",
    }
}

/// Computes an Android-compatible `native_pl_*` cover filename for ZIP export.
pub fn android_export_cover_name(local_path: &Path, bytes: &[u8]) -> String {
    if let Some(base) = local_path.file_name().and_then(|n| n.to_str()) {
        if is_android_cover_filename(base) {
            return base.to_owned();
        }
        if is_content_cover_filename(base) {
            let candidate = format!("{PLAYLIST_COVER_PREFIX}{base}");
            if is_android_cover_filename(&candidate) {
                return candidate;
            }
        }
    }
    let hash = sha256_hex_prefix(bytes);
    let ext = detect_safe_ext(local_path, bytes);
    format!("{PLAYLIST_COVER_PREFIX}c_{hash}.{ext}")
}

pub(super) fn find_existing_managed_cover_on_disk(base: &str) -> Option<PathBuf> {
    let cover_dir = CoverPaths::cover_dir();
    let direct = cover_dir.join(base);
    if direct.is_file() && CoverPaths::is_managed_cover_file(&direct) {
        return Some(direct);
    }
    if let Some(stripped) = base.strip_prefix(PLAYLIST_COVER_PREFIX)
        && is_content_cover_filename(stripped)
    {
        let content_path = cover_dir.join(stripped);
        if content_path.is_file() && CoverPaths::is_managed_cover_file(&content_path) {
            return Some(content_path);
        }
    }
    None
}
