use std::collections::HashSet;
use std::fs::File;
use std::io::{Read, Seek};
use std::path::Path;

use crate::collections::CollectionsData;
use crate::stats::PlayStats;
use crate::zip_archive::{ZipError, ZipReader};

use super::cover_names::{
    BackupBundle, BackupCoverFile, extract_cover_basename, is_android_cover_filename,
    is_content_cover_filename,
};
use super::{
    ARCHIVE_IGNORED_RATIO_GRACE_BYTES, FILE_COLLECTIONS, FILE_SETTINGS, FILE_STATS,
    MAX_ARCHIVE_ACCEPTED_ENTRIES, MAX_ARCHIVE_IGNORED_BYTES, MAX_ARCHIVE_IGNORED_RATIO,
    MAX_ARCHIVE_SCANNED_ENTRIES, MAX_ARCHIVE_TOTAL_BYTES, MAX_BACKUP_COVER_BYTES,
    MAX_BACKUP_JSON_BYTES, PlaylistIoError,
};

enum EntryRole {
    Json { file: &'static str, depth: usize },
    AndroidCover(String),
    ContentCover(String),
    Ignore,
}

fn validate_and_split_entry_path(raw_name: &str) -> Result<Option<Vec<String>>, PlaylistIoError> {
    if raw_name.contains('\0')
        || raw_name.starts_with('/')
        || raw_name.starts_with('\\')
        || (raw_name.len() >= 2
            && raw_name.as_bytes()[0].is_ascii_alphabetic()
            && raw_name.as_bytes()[1] == b':')
    {
        return Err(PlaylistIoError::Zip(ZipError::PathTraversal(
            raw_name.to_owned(),
        )));
    }
    let normalized = raw_name.replace('\\', "/");
    let raw_segs: Vec<&str> = normalized.split('/').collect();
    if raw_segs.contains(&"..") {
        return Err(PlaylistIoError::Zip(ZipError::PathTraversal(
            raw_name.to_owned(),
        )));
    }
    let segs: Vec<String> = raw_segs
        .into_iter()
        .filter(|s| !s.is_empty() && *s != ".")
        .map(str::to_owned)
        .collect();
    if segs.is_empty() || segs[0] == "__MACOSX" {
        return Ok(None);
    }
    Ok(Some(segs))
}

fn classify_entry(segs: &[String]) -> EntryRole {
    let json_candidate = match segs {
        [name] => Some((name.as_str(), 1usize)),
        [parent, name] if parent != "covers" => Some((name.as_str(), 2usize)),
        _ => None,
    };
    if let Some((name, depth)) = json_candidate {
        match name {
            FILE_COLLECTIONS => {
                return EntryRole::Json {
                    file: FILE_COLLECTIONS,
                    depth,
                };
            }
            FILE_STATS => {
                return EntryRole::Json {
                    file: FILE_STATS,
                    depth,
                };
            }
            FILE_SETTINGS => {
                return EntryRole::Json {
                    file: FILE_SETTINGS,
                    depth,
                };
            }
            _ => {}
        }
    }

    let cover_name = match segs {
        [dir, name] if dir == "covers" => Some(name.as_str()),
        [_, dir, name] if dir == "covers" => Some(name.as_str()),
        _ => None,
    };
    if let Some(name) = cover_name {
        if is_android_cover_filename(name) {
            return EntryRole::AndroidCover(name.to_owned());
        }
        if is_content_cover_filename(name) {
            return EntryRole::ContentCover(name.to_owned());
        }
    }
    EntryRole::Ignore
}

fn read_archive_entry<R: Read + Seek>(
    zip: &mut ZipReader<R>,
    entry: &crate::zip_archive::ZipEntryInfo,
    entry_limit: usize,
    total_bytes: &mut usize,
) -> Result<Vec<u8>, PlaylistIoError> {
    if entry.uncompressed_size > entry_limit as u64 {
        return Err(PlaylistIoError::Zip(ZipError::EntryTooLarge {
            name: entry.raw_name.clone(),
            limit: entry_limit,
        }));
    }
    let remaining = MAX_ARCHIVE_TOTAL_BYTES.saturating_sub(*total_bytes);
    if entry.uncompressed_size > remaining as u64 {
        return Err(PlaylistIoError::Zip(ZipError::TotalTooLarge {
            limit: MAX_ARCHIVE_TOTAL_BYTES,
        }));
    }
    let bytes = zip.read_entry_bounded(
        entry,
        entry_limit.min(remaining),
        ARCHIVE_IGNORED_RATIO_GRACE_BYTES,
        MAX_ARCHIVE_IGNORED_RATIO,
    )?;
    *total_bytes += bytes.len();
    Ok(bytes)
}

fn read_backup_entries<R: Read + Seek>(
    reader: R,
    read_covers_and_drain: bool,
) -> Result<BackupBundle, PlaylistIoError> {
    let mut zip = ZipReader::open(reader, MAX_ARCHIVE_SCANNED_ENTRIES)?;
    let entries = zip.entries().to_vec();

    let mut json_entries = Vec::new();
    let mut cover_entries = Vec::new();
    let mut ignore_entries = Vec::new();

    for entry in entries {
        let Some(segs) = validate_and_split_entry_path(&entry.raw_name)? else {
            ignore_entries.push(entry);
            continue;
        };
        if entry.is_dir {
            ignore_entries.push(entry);
            continue;
        }
        match classify_entry(&segs) {
            EntryRole::Json { file, depth } => json_entries.push((entry, file, depth)),
            EntryRole::AndroidCover(name) => cover_entries.push((entry, name, true)),
            EntryRole::ContentCover(name) => cover_entries.push((entry, name, false)),
            EntryRole::Ignore => ignore_entries.push(entry),
        }
    }

    let mut collections_json: Option<String> = None;
    let mut collections_depth = usize::MAX;
    let mut stats_json: Option<String> = None;
    let mut stats_depth = usize::MAX;
    let mut settings_json: Option<String> = None;
    let mut settings_depth = usize::MAX;

    let mut accepted_entries = 0usize;
    let mut total_bytes = 0usize;

    for (entry, file, depth) in json_entries {
        accepted_entries += 1;
        if accepted_entries > MAX_ARCHIVE_ACCEPTED_ENTRIES {
            return Err(PlaylistIoError::TooLarge {
                what: "Accepted backup entries",
                limit: format!("{MAX_ARCHIVE_ACCEPTED_ENTRIES} files"),
            });
        }
        let bytes = read_archive_entry(&mut zip, &entry, MAX_BACKUP_JSON_BYTES, &mut total_bytes)?;
        let text = String::from_utf8(bytes).map_err(|_| PlaylistIoError::InvalidUtf8)?;
        match file {
            FILE_COLLECTIONS if depth < collections_depth => {
                collections_json = Some(text);
                collections_depth = depth;
            }
            FILE_STATS if depth < stats_depth => {
                stats_json = Some(text);
                stats_depth = depth;
            }
            FILE_SETTINGS if depth < settings_depth => {
                settings_json = Some(text);
                settings_depth = depth;
            }
            _ => {}
        }
    }

    if collections_json.is_none() && stats_json.is_none() && settings_json.is_none() {
        return Err(PlaylistIoError::EmptyBackup);
    }

    let mut playlist_covers = Vec::new();
    if read_covers_and_drain {
        let referenced_covers: HashSet<String> = collections_json
            .as_deref()
            .and_then(|raw| serde_json::from_str::<CollectionsData>(raw).ok())
            .map(|col| {
                col.playlists
                    .iter()
                    .filter_map(|p| extract_cover_basename(p.cover_art.as_deref()))
                    .map(|b| b.to_ascii_lowercase())
                    .collect()
            })
            .unwrap_or_default();

        let mut seen_covers = HashSet::new();
        let mut ignored_bytes = 0u64;

        for (entry, name, is_android) in cover_entries {
            let lower_name = name.to_ascii_lowercase();
            if (!is_android && !referenced_covers.contains(&lower_name))
                || !seen_covers.insert(lower_name)
            {
                zip.drain_ignored_entry(
                    &entry,
                    &mut ignored_bytes,
                    MAX_ARCHIVE_IGNORED_BYTES,
                    ARCHIVE_IGNORED_RATIO_GRACE_BYTES,
                    MAX_ARCHIVE_IGNORED_RATIO,
                )?;
                continue;
            }
            accepted_entries += 1;
            if accepted_entries > MAX_ARCHIVE_ACCEPTED_ENTRIES {
                return Err(PlaylistIoError::TooLarge {
                    what: "Accepted backup entries",
                    limit: format!("{MAX_ARCHIVE_ACCEPTED_ENTRIES} files"),
                });
            }
            let bytes =
                read_archive_entry(&mut zip, &entry, MAX_BACKUP_COVER_BYTES, &mut total_bytes)?;
            playlist_covers.push(BackupCoverFile { name, bytes });
        }

        for entry in ignore_entries {
            zip.drain_ignored_entry(
                &entry,
                &mut ignored_bytes,
                MAX_ARCHIVE_IGNORED_BYTES,
                ARCHIVE_IGNORED_RATIO_GRACE_BYTES,
                MAX_ARCHIVE_IGNORED_RATIO,
            )?;
        }
    }

    Ok(BackupBundle {
        collections_json,
        stats_json,
        settings_json,
        playlist_covers,
    })
}

/// Reads and validates a Retrograde `.zip` archive using the same size/entry/zip-bomb rules
/// as Android's `BackupArchive.kt`, plus accepting referenced desktop `covers/c_<24-hex>.*` files.
pub fn read_backup_zip<R: Read + Seek>(reader: R) -> Result<BackupBundle, PlaylistIoError> {
    read_backup_entries(reader, true)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZipArchiveSummary {
    pub playlists: usize,
    pub liked: usize,
    pub smart: usize,
    pub stats_tracks: usize,
    pub has_collections: bool,
    pub has_stats: bool,
    pub has_settings: bool,
    pub is_single_playlist_export: bool,
}

/// Reads and validates a `.zip` backup archive's metadata JSON entries for UI confirmation
/// without decompressing cover images.
pub fn inspect_backup_zip<R: Read + Seek>(reader: R) -> Result<ZipArchiveSummary, PlaylistIoError> {
    let bundle = read_backup_entries(reader, false)?;
    let has_stats = bundle.stats_json.is_some();
    let has_settings = bundle.settings_json.is_some();

    let mut playlists = 0usize;
    let mut liked = 0usize;
    let mut smart = 0usize;
    let mut single_pl_shape = false;

    let has_collections = if let Some(raw) = &bundle.collections_json {
        let col: CollectionsData =
            serde_json::from_str(raw).map_err(|e| PlaylistIoError::InvalidJson {
                file: FILE_COLLECTIONS,
                message: e.to_string(),
            })?;
        playlists = col.playlists.len();
        liked = col.liked.len();
        smart = col.smart.as_ref().map_or(0, Vec::len);
        single_pl_shape = !has_stats
            && !has_settings
            && playlists == 1
            && liked == 0
            && col.smart.as_ref().is_none_or(Vec::is_empty);
        true
    } else {
        false
    };

    let stats_tracks = if let Some(raw) = &bundle.stats_json {
        let st: PlayStats =
            serde_json::from_str(raw).map_err(|e| PlaylistIoError::InvalidJson {
                file: FILE_STATS,
                message: e.to_string(),
            })?;
        st.tracks.len()
    } else {
        0
    };

    if !has_collections && !has_stats {
        return Err(PlaylistIoError::MissingCollections);
    }

    Ok(ZipArchiveSummary {
        playlists,
        liked,
        smart,
        stats_tracks,
        has_collections,
        has_stats,
        has_settings,
        is_single_playlist_export: single_pl_shape,
    })
}

/// Opens `path` and inspects a `.zip` backup archive via [`inspect_backup_zip`].
pub fn inspect_backup_zip_file(path: &Path) -> Result<ZipArchiveSummary, PlaylistIoError> {
    let file = File::open(path)?;
    inspect_backup_zip(file)
}
