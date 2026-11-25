use std::collections::HashSet;
use std::fs::{self, File};
use std::path::Path;

use thiserror::Error;

use crate::zip_archive::ZipError;

mod cover_names;
mod m3u;
mod path_mapper;
mod zip_export;
mod zip_import;
mod zip_read;

pub use cover_names::{
    BackupBundle, BackupCoverFile, android_export_cover_name, is_android_cover_filename,
    is_content_cover_filename,
};
pub use m3u::{
    M3uEntry, M3uImportResult, ParsedM3u, export_m3u8_file, format_m3u8, import_m3u_file,
    import_m3u_str, parse_m3u,
};
pub use path_mapper::{MapOutcome, TrackPathMapper, normalize_path_segments};
pub use zip_export::{
    build_collections_backup_bundle, build_single_playlist_backup_bundle,
    export_collections_zip_file, export_playlist_zip_file, write_backup_zip,
};
pub use zip_import::{IngestedZipImport, PreparedZipImport, ZipImportReport, prepare_zip_import};
pub use zip_read::{
    ZipArchiveSummary, inspect_backup_zip, inspect_backup_zip_file, read_backup_zip,
};

pub const SAF_PREFIX: &str = "/saf/";
pub const MAX_M3U_BYTES: usize = 16 * 1024 * 1024;

pub const MAX_BACKUP_JSON_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_BACKUP_COVER_BYTES: usize = 10 * 1024 * 1024;
pub const MAX_BACKUP_COVERS_TOTAL_BYTES: usize = 50 * 1024 * 1024;
pub const MAX_ARCHIVE_TOTAL_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_BACKUP_COVER_COUNT: usize = MAX_ARCHIVE_ACCEPTED_ENTRIES - 1;
pub const MAX_ARCHIVE_ACCEPTED_ENTRIES: usize = 500;
pub const MAX_ARCHIVE_SCANNED_ENTRIES: usize = 50_000;
pub const MAX_ARCHIVE_IGNORED_BYTES: u64 = 1 << 30;
pub const ARCHIVE_IGNORED_RATIO_GRACE_BYTES: u64 = 8 * 1024 * 1024;
pub const MAX_ARCHIVE_IGNORED_RATIO: u64 = 100;

pub(super) const FILE_COLLECTIONS: &str = "native-collections.json";
pub(super) const FILE_STATS: &str = "native-stats.json";
pub(super) const FILE_SETTINGS: &str = "native-settings.json";
pub(super) const PLAYLIST_COVER_PREFIX: &str = "native_pl_";

#[derive(Debug, Error)]
pub enum PlaylistIoError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Zip(#[from] ZipError),
    #[error("Playlist file is not valid UTF-8.")]
    InvalidUtf8,
    #[error("{what} is too large to import (limit {limit}).")]
    TooLarge { what: &'static str, limit: String },
    #[error("Playlist file contains no track entries.")]
    EmptyPlaylist,
    #[error("No tracks in playlist ({total} entries) matched your library.")]
    NoMatchingTracks { total: usize },
    #[error("Backup archive contains no Retrograde data files.")]
    EmptyBackup,
    #[error("Backup archive contains neither native-collections.json nor native-stats.json.")]
    MissingCollections,
    #[error("Invalid JSON in {file}: {message}")]
    InvalidJson { file: &'static str, message: String },
    #[error("Invalid playlist cover '{name}': {reason}")]
    InvalidCover { name: String, reason: &'static str },
    #[error("Image error while importing cover '{name}': {message}")]
    CoverImage { name: String, message: String },
}

/// Suggests a filesystem-safe export filename (e.g. `my_mix.m3u8` or `my_mix.zip`).
pub fn suggest_export_filename(name: &str, ext: &str) -> String {
    let sanitized: String = name
        .trim()
        .chars()
        .map(|c| {
            if c.is_control() || matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') {
                '_'
            } else {
                c
            }
        })
        .collect();
    let trimmed = sanitized.trim_matches(|c: char| c == '.' || c == '_' || c.is_whitespace());
    let base: String = trimmed.chars().take(100).collect();
    let stem = base.trim_matches(|c: char| c == '.' || c == '_' || c.is_whitespace());
    let stem = if stem.is_empty() { "playlist" } else { stem };
    let clean_ext = ext.trim().trim_start_matches('.');
    if clean_ext.is_empty() {
        stem.to_owned()
    } else {
        format!("{stem}.{clean_ext}")
    }
}

pub(super) fn write_file_direct_or_cleanup(
    dest: &Path,
    write_fn: impl FnOnce(File) -> Result<(), PlaylistIoError>,
) -> Result<(), PlaylistIoError> {
    let file = File::create(dest)?;
    if let Err(err) = write_fn(file) {
        let _ = fs::remove_file(dest);
        return Err(err);
    }
    Ok(())
}

/// Allocates a free positive playlist ID starting at `wanted.max(1)` without colliding with `used`.
pub fn claim_next_id(used: &mut HashSet<i64>, wanted: i64) -> i64 {
    let mut id = wanted.max(1);
    while !used.insert(id) {
        id = id.checked_add(1).unwrap_or(1);
    }
    id
}

pub(super) fn dedupe_preserving_order(iter: impl Iterator<Item = String>) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for item in iter {
        if seen.insert(item.clone()) {
            out.push(item);
        }
    }
    out
}

#[cfg(test)]
pub(super) mod tests_shared {
    use crate::models::{Album, Track};

    pub(in crate::playlist_io) fn mk_track(
        path: &str,
        title: &str,
        artist: &str,
        album: &str,
        duration: f64,
    ) -> Track {
        Track {
            id: path.to_owned(),
            title: title.to_owned(),
            artist: artist.to_owned(),
            album: album.to_owned(),
            duration,
            file_path: path.to_owned(),
            track_number: 1,
            disc_number: 1,
            codec: "FLAC".to_owned(),
            sample_rate: 44100,
            bits_per_sample: 16,
            channels: 2,
            cover_thumb_path: None,
            cover_full_path: None,
        }
    }

    pub(in crate::playlist_io) fn mk_album(name: &str, artist: &str, tracks: Vec<Track>) -> Album {
        Album {
            name: name.to_owned(),
            artist: artist.to_owned(),
            cover_thumb_path: None,
            cover_full_path: None,
            year: 2024,
            added_at: 1,
            tracks,
        }
    }
}
