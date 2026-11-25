use std::collections::HashSet;
use std::fs;
use std::io::Write;
use std::path::Path;

use serde_json::Map;

use crate::collections::{CollectionsData, PlaylistData};
use crate::covers::ImageLimits;
use crate::paths::CoverPaths;
use crate::zip_archive::ZipWriter;

use super::cover_names::{
    BackupBundle, BackupCoverFile, android_export_cover_name, is_android_cover_filename,
};
use super::{
    FILE_COLLECTIONS, FILE_SETTINGS, FILE_STATS, MAX_BACKUP_COVER_BYTES, MAX_BACKUP_COVER_COUNT,
    MAX_BACKUP_COVERS_TOTAL_BYTES, MAX_BACKUP_JSON_BYTES, PlaylistIoError,
    write_file_direct_or_cleanup,
};

/// Builds a ZIP-ready `BackupBundle` containing `native-collections.json` (with rewritten
/// Android-compatible `covers/native_pl_*` paths) and deduplicated playlist cover bytes.
pub fn build_collections_backup_bundle(
    collections: &CollectionsData,
) -> Result<BackupBundle, PlaylistIoError> {
    let mut exported = collections.clone();
    let mut seen_names = HashSet::new();
    let mut playlist_covers = Vec::new();
    let mut total_cover_bytes = 0usize;
    let cover_dir = CoverPaths::cover_dir();
    let canon_cover_dir = fs::canonicalize(&cover_dir).ok();

    for playlist in &mut exported.playlists {
        let Some(local) = CoverPaths::to_local_path(playlist.cover_art.as_deref()) else {
            playlist.cover_art = None;
            continue;
        };
        if !CoverPaths::is_managed_cover_file(&local) {
            playlist.cover_art = None;
            continue;
        }
        if let Some(ref dir) = canon_cover_dir {
            let Ok(canon) = fs::canonicalize(&local) else {
                playlist.cover_art = None;
                continue;
            };
            if !canon.starts_with(dir) {
                playlist.cover_art = None;
                continue;
            }
        }
        let Ok(meta) = fs::metadata(&local) else {
            playlist.cover_art = None;
            continue;
        };
        if !meta.is_file() || meta.len() > MAX_BACKUP_COVER_BYTES as u64 {
            playlist.cover_art = None;
            continue;
        }
        let Ok(bytes) = fs::read(&local) else {
            playlist.cover_art = None;
            continue;
        };
        if bytes.len() > MAX_BACKUP_COVER_BYTES || !ImageLimits::is_sane_bytes(&bytes) {
            playlist.cover_art = None;
            continue;
        }
        let export_name = android_export_cover_name(&local, &bytes);
        if seen_names.contains(&export_name) {
            playlist.cover_art = Some(format!("covers/{export_name}"));
            continue;
        }
        if playlist_covers.len() >= MAX_BACKUP_COVER_COUNT
            || total_cover_bytes.saturating_add(bytes.len()) > MAX_BACKUP_COVERS_TOTAL_BYTES
        {
            playlist.cover_art = None;
            continue;
        }
        total_cover_bytes += bytes.len();
        seen_names.insert(export_name.clone());
        playlist.cover_art = Some(format!("covers/{export_name}"));
        playlist_covers.push(BackupCoverFile {
            name: export_name,
            bytes,
        });
    }

    let collections_json =
        serde_json::to_string_pretty(&exported).map_err(|e| PlaylistIoError::InvalidJson {
            file: FILE_COLLECTIONS,
            message: e.to_string(),
        })?;
    if collections_json.len() > MAX_BACKUP_JSON_BYTES {
        return Err(PlaylistIoError::TooLarge {
            what: FILE_COLLECTIONS,
            limit: "16 MiB".to_owned(),
        });
    }

    Ok(BackupBundle {
        collections_json: Some(collections_json),
        stats_json: None,
        settings_json: None,
        playlist_covers,
    })
}

pub fn build_single_playlist_backup_bundle(
    playlist: &PlaylistData,
) -> Result<BackupBundle, PlaylistIoError> {
    let wrapper = CollectionsData {
        liked: Vec::new(),
        playlists: vec![playlist.clone()],
        smart: None,
        imported_from: None,
        extra: Map::new(),
    };
    build_collections_backup_bundle(&wrapper)
}

pub fn write_backup_zip<W: Write>(writer: W, bundle: &BackupBundle) -> Result<W, PlaylistIoError> {
    let mut zip = ZipWriter::new(writer);
    if let Some(c) = &bundle.collections_json {
        zip.add_deflated(FILE_COLLECTIONS, c.as_bytes())?;
    }
    if let Some(s) = &bundle.stats_json {
        zip.add_deflated(FILE_STATS, s.as_bytes())?;
    }
    if let Some(st) = &bundle.settings_json {
        zip.add_deflated(FILE_SETTINGS, st.as_bytes())?;
    }
    let mut seen_covers = HashSet::new();
    for cover in &bundle.playlist_covers {
        if !is_android_cover_filename(&cover.name) {
            return Err(PlaylistIoError::InvalidCover {
                name: cover.name.clone(),
                reason: "cover name is not Android-compatible (expected native_pl_*)",
            });
        }
        if seen_covers.insert(cover.name.clone()) {
            zip.add_stored(&format!("covers/{}", cover.name), &cover.bytes)?;
        }
    }
    Ok(zip.finish()?)
}

/// Writes a full collections archive (`.zip`) directly to `dest`. Returns number of covers written.
pub fn export_collections_zip_file(
    dest: &Path,
    collections: &CollectionsData,
) -> Result<usize, PlaylistIoError> {
    let bundle = build_collections_backup_bundle(collections)?;
    let cover_count = bundle.playlist_covers.len();
    write_file_direct_or_cleanup(dest, |file| {
        let mut f = write_backup_zip(file, &bundle)?;
        f.flush()?;
        Ok(())
    })?;
    Ok(cover_count)
}

/// Writes a single custom playlist archive (`.zip`) directly to `dest`. Returns number of covers written.
pub fn export_playlist_zip_file(
    dest: &Path,
    playlist: &PlaylistData,
) -> Result<usize, PlaylistIoError> {
    let bundle = build_single_playlist_backup_bundle(playlist)?;
    let cover_count = bundle.playlist_covers.len();
    write_file_direct_or_cleanup(dest, |file| {
        let mut f = write_backup_zip(file, &bundle)?;
        f.flush()?;
        Ok(())
    })?;
    Ok(cover_count)
}
