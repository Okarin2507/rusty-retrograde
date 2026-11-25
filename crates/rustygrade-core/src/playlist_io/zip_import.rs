use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Mutex;

use serde::Deserialize;
use serde_json::{Map, Value};

use crate::collections::{CollectionsData, PlaylistData, SmartPlaylistData};
use crate::covers::{ImageLimits, prepare_cover_bytes, write_prepared_cover};
use crate::library::LibraryStore;
use crate::models::Album;
use crate::paths::CoverPaths;
use crate::stats::{PlayStat, PlayStats};

use super::cover_names::{
    BackupBundle, BackupCoverFile, extract_cover_basename, find_existing_managed_cover_on_disk,
    validate_import_cover_filename,
};
use super::path_mapper::{MapOutcome, TrackPathMapper};
use super::{
    FILE_COLLECTIONS, FILE_STATS, MAX_BACKUP_COVER_BYTES, MAX_BACKUP_COVER_COUNT,
    MAX_BACKUP_COVERS_TOTAL_BYTES, MAX_BACKUP_JSON_BYTES, PlaylistIoError, claim_next_id,
    dedupe_preserving_order,
};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ZipImportReport {
    pub collections_imported: bool,
    pub liked_added: usize,
    pub liked_total: usize,
    pub playlists_imported: usize,
    pub playlists_total: usize,
    pub smart_playlists_imported: usize,
    pub stats_imported: usize,
    pub settings_skipped: bool,
    pub tracks_exact: usize,
    pub tracks_remapped: usize,
    pub tracks_unmatched: usize,
    pub covers_restored: usize,
}

struct PreparedZipPlaylist {
    data: PlaylistData,
    raw_cover_basename: Option<String>,
}

struct PreparedZipCollections {
    liked: Vec<String>,
    playlists: Vec<PreparedZipPlaylist>,
    smart: Option<Vec<SmartPlaylistData>>,
    imported_from: Option<String>,
    extra: Map<String, Value>,
}

/// Phase 1 output: validated JSON/images and pre-remapped track IDs built without store locks.
pub struct PreparedZipImport {
    collections: Option<PreparedZipCollections>,
    stats: Option<HashMap<String, PlayStat>>,
    has_settings: bool,
    referenced_covers: Vec<BackupCoverFile>,
    tracks_exact: usize,
    tracks_remapped: usize,
    tracks_unmatched: usize,
}

/// Phase 2 output: covers ingested into `c_<24-hex>.<ext>` under `CoverPaths::cover_dir()`, ready
/// to merge/replace into `CollectionsData` and `PlayStats` on the coordinator thread.
pub struct IngestedZipImport {
    collections: Option<PreparedZipCollections>,
    stats: Option<HashMap<String, PlayStat>>,
    has_settings: bool,
    cover_urls: HashMap<String, String>,
    pub restored_cover_paths: Vec<PathBuf>,
    covers_restored: usize,
    tracks_exact: usize,
    tracks_remapped: usize,
    tracks_unmatched: usize,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
struct BackupSettingsHint {
    library_folders: Vec<String>,
    #[serde(rename = "AndroidBackupSourceRoots")]
    android_backup_source_roots: Vec<String>,
}

fn push_unique_roots(out: &mut Vec<String>, roots: &[String]) {
    for folder in roots {
        let trimmed = folder.trim();
        if !trimmed.is_empty() && !out.iter().any(|r| r == trimmed) {
            out.push(trimmed.to_owned());
        }
    }
}

fn build_stats_ci_index(map: &HashMap<String, PlayStat>) -> HashMap<String, String> {
    let mut index = HashMap::with_capacity(map.len());
    for k in map.keys() {
        index.entry(k.to_lowercase()).or_insert_with(|| k.clone());
    }
    index
}

fn merge_stat_entry_indexed(
    map: &mut HashMap<String, PlayStat>,
    ci_index: &mut HashMap<String, String>,
    dst_id: String,
    incoming: PlayStat,
) {
    if let Some(entry) = map.get_mut(&dst_id) {
        entry.count = entry.count.max(incoming.count);
        entry.last = entry.last.max(incoming.last);
        return;
    }
    let low = dst_id.to_lowercase();
    if let Some(existing_key) = ci_index.get(&low)
        && let Some(entry) = map.get_mut(existing_key)
    {
        entry.count = entry.count.max(incoming.count);
        entry.last = entry.last.max(incoming.last);
        return;
    }
    ci_index.insert(low, dst_id.clone());
    map.insert(dst_id, incoming);
}

/// Validates JSON payloads and cover bounds in `bundle`, and remaps collection and stats track IDs
/// onto the local library without taking store locks or touching disk.
pub fn prepare_zip_import(
    bundle: BackupBundle,
    albums: &[Album],
    local_source_roots: &[String],
) -> Result<PreparedZipImport, PlaylistIoError> {
    if bundle.collections_json.is_none()
        && bundle.stats_json.is_none()
        && bundle.settings_json.is_none()
    {
        return Err(PlaylistIoError::EmptyBackup);
    }
    if bundle.collections_json.is_none() && bundle.stats_json.is_none() {
        return Err(PlaylistIoError::MissingCollections);
    }

    let parsed_collections = match &bundle.collections_json {
        Some(raw) => {
            if raw.len() > MAX_BACKUP_JSON_BYTES {
                return Err(PlaylistIoError::TooLarge {
                    what: FILE_COLLECTIONS,
                    limit: "16 MiB".to_owned(),
                });
            }
            Some(serde_json::from_str::<CollectionsData>(raw).map_err(|e| {
                PlaylistIoError::InvalidJson {
                    file: FILE_COLLECTIONS,
                    message: e.to_string(),
                }
            })?)
        }
        None => None,
    };

    let parsed_stats = match &bundle.stats_json {
        Some(raw) => {
            if raw.len() > MAX_BACKUP_JSON_BYTES {
                return Err(PlaylistIoError::TooLarge {
                    what: FILE_STATS,
                    limit: "16 MiB".to_owned(),
                });
            }
            Some(serde_json::from_str::<PlayStats>(raw).map_err(|e| {
                PlaylistIoError::InvalidJson {
                    file: FILE_STATS,
                    message: e.to_string(),
                }
            })?)
        }
        None => None,
    };

    if bundle.playlist_covers.len() > MAX_BACKUP_COVER_COUNT {
        return Err(PlaylistIoError::TooLarge {
            what: "The playlist cover set",
            limit: format!("{MAX_BACKUP_COVER_COUNT} files"),
        });
    }

    let has_settings = bundle.settings_json.is_some();
    let mut source_roots = Vec::new();
    if let Some(raw_settings) = &bundle.settings_json
        && raw_settings.len() <= MAX_BACKUP_JSON_BYTES
        && let Ok(hint) = serde_json::from_str::<BackupSettingsHint>(raw_settings)
    {
        push_unique_roots(&mut source_roots, &hint.library_folders);
        push_unique_roots(&mut source_roots, &hint.android_backup_source_roots);
    }
    push_unique_roots(&mut source_roots, local_source_roots);

    let mut total_cover_bytes = 0usize;
    let mut validated_covers: HashMap<String, BackupCoverFile> = HashMap::new();
    for cover in bundle.playlist_covers {
        validate_import_cover_filename(&cover.name)?;
        if cover.bytes.len() > MAX_BACKUP_COVER_BYTES {
            return Err(PlaylistIoError::InvalidCover {
                name: cover.name,
                reason: "cover exceeds 10 MiB limit",
            });
        }
        total_cover_bytes = total_cover_bytes.saturating_add(cover.bytes.len());
        if total_cover_bytes > MAX_BACKUP_COVERS_TOTAL_BYTES {
            return Err(PlaylistIoError::InvalidCover {
                name: cover.name,
                reason: "total playlist covers exceed 50 MiB limit",
            });
        }
        if !ImageLimits::is_sane_bytes(&cover.bytes) {
            return Err(PlaylistIoError::InvalidCover {
                name: cover.name,
                reason: "corrupt or unsupported image",
            });
        }
        validated_covers
            .entry(cover.name.to_ascii_lowercase())
            .or_insert(cover);
    }

    let mapper = TrackPathMapper::from_albums(albums, &source_roots);
    let mut track_outcomes: HashMap<String, MapOutcome> = HashMap::new();
    let mut resolve_track = |raw_id: &str| -> String {
        track_outcomes
            .entry(raw_id.to_owned())
            .or_insert_with(|| mapper.map_id(raw_id))
            .target_id()
            .to_owned()
    };

    let mut needed_cover_keys = HashSet::new();
    let collections = parsed_collections.map(|imp_col| {
        let liked = dedupe_preserving_order(imp_col.liked.iter().map(|id| resolve_track(id)));
        let playlists = imp_col
            .playlists
            .into_iter()
            .map(|mut p| {
                p.track_ids =
                    dedupe_preserving_order(p.track_ids.iter().map(|id| resolve_track(id)));
                let raw_cover_basename = extract_cover_basename(p.cover_art.as_deref())
                    .filter(|b| validate_import_cover_filename(b).is_ok());
                if let Some(base) = &raw_cover_basename {
                    needed_cover_keys.insert(base.to_ascii_lowercase());
                }
                p.cover_art = None;
                PreparedZipPlaylist {
                    data: p,
                    raw_cover_basename,
                }
            })
            .collect();
        PreparedZipCollections {
            liked,
            playlists,
            smart: imp_col.smart,
            imported_from: imp_col.imported_from,
            extra: imp_col.extra,
        }
    });

    let stats = parsed_stats.map(|imp_stats| {
        let mut mapped_tracks = HashMap::with_capacity(imp_stats.tracks.len());
        let mut ci_index = HashMap::with_capacity(imp_stats.tracks.len());
        for (raw_id, stat) in imp_stats.tracks {
            let dst_id = resolve_track(&raw_id);
            merge_stat_entry_indexed(&mut mapped_tracks, &mut ci_index, dst_id, stat);
        }
        mapped_tracks
    });

    let referenced_covers: Vec<BackupCoverFile> = needed_cover_keys
        .into_iter()
        .filter_map(|key| validated_covers.remove(&key))
        .collect();

    let (mut tracks_exact, mut tracks_remapped, mut tracks_unmatched) = (0, 0, 0);
    for outcome in track_outcomes.values() {
        match outcome {
            MapOutcome::Exact(_) => tracks_exact += 1,
            MapOutcome::Remapped(_) => tracks_remapped += 1,
            MapOutcome::Unmatched(_) => tracks_unmatched += 1,
        }
    }

    Ok(PreparedZipImport {
        collections,
        stats,
        has_settings,
        referenced_covers,
        tracks_exact,
        tracks_remapped,
        tracks_unmatched,
    })
}

impl PreparedZipImport {
    /// Decodes/normalizes referenced covers in memory first, then writes them under
    /// `LibraryStore::write_lock()` and pins them into `live_refs` and `pending_refs`.
    pub fn ingest_covers(
        self,
        live_refs: Option<&Mutex<HashSet<PathBuf>>>,
        pending_refs: Option<&Mutex<HashSet<PathBuf>>>,
    ) -> Result<IngestedZipImport, (PlaylistIoError, Vec<PathBuf>)> {
        let mut prepared_covers = Vec::with_capacity(self.referenced_covers.len());
        for cover in self.referenced_covers {
            // One undecodable cover must not sink the whole import; that playlist drops its cover.
            if let Ok(prepared) = prepare_cover_bytes(&cover.bytes) {
                prepared_covers.push((cover.name, prepared));
            }
        }

        let needed_basenames: Vec<String> = self
            .collections
            .as_ref()
            .map(|col| {
                col.playlists
                    .iter()
                    .filter_map(|p| p.raw_cover_basename.clone())
                    .collect()
            })
            .unwrap_or_default();

        let mut cover_urls = HashMap::with_capacity(prepared_covers.len() + needed_basenames.len());
        let mut restored_cover_paths = Vec::with_capacity(prepared_covers.len());
        let mut seen_saved_paths = HashSet::new();
        let mut newly_written_paths = Vec::new();
        let mut covers_restored = 0usize;

        let pin = |path: &PathBuf| {
            if let Some(pending) = pending_refs
                && let Ok(mut guard) = pending.lock()
            {
                guard.insert(path.clone());
            }
            if let Some(refs) = live_refs
                && let Ok(mut guard) = refs.lock()
            {
                guard.insert(path.clone());
            }
        };

        if !prepared_covers.is_empty() || !needed_basenames.is_empty() {
            let _guard = LibraryStore::write_lock().lock().map_err(|_| {
                (
                    PlaylistIoError::Io(std::io::Error::other("library write lock poisoned")),
                    Vec::new(),
                )
            })?;

            for (name, prepared) in prepared_covers {
                let saved = match write_prepared_cover(&prepared) {
                    Ok(s) => s,
                    Err(e) => {
                        return Err((
                            PlaylistIoError::CoverImage {
                                name,
                                message: e.to_string(),
                            },
                            newly_written_paths,
                        ));
                    }
                };
                if saved.created && !newly_written_paths.contains(&saved.path) {
                    newly_written_paths.push(saved.path.clone());
                }
                pin(&saved.path);
                if seen_saved_paths.insert(saved.path.clone()) {
                    restored_cover_paths.push(saved.path.clone());
                }
                cover_urls.insert(
                    name.to_ascii_lowercase(),
                    CoverPaths::to_file_url(&saved.path),
                );
                covers_restored += 1;
            }

            for base in needed_basenames {
                let key = base.to_ascii_lowercase();
                if cover_urls.contains_key(&key) {
                    continue;
                }
                if let Some(existing_path) = find_existing_managed_cover_on_disk(&base) {
                    pin(&existing_path);
                    if seen_saved_paths.insert(existing_path.clone()) {
                        restored_cover_paths.push(existing_path.clone());
                    }
                    cover_urls.insert(key, CoverPaths::to_file_url(&existing_path));
                }
            }
        }

        Ok(IngestedZipImport {
            collections: self.collections,
            stats: self.stats,
            has_settings: self.has_settings,
            cover_urls,
            restored_cover_paths,
            covers_restored,
            tracks_exact: self.tracks_exact,
            tracks_remapped: self.tracks_remapped,
            tracks_unmatched: self.tracks_unmatched,
        })
    }
}

impl IngestedZipImport {
    /// Applies pre-ingested collections and play stats onto `current` and `current_stats` in
    /// Merge or Replace mode, returning `(report, old_cover_urls_to_gc, stats_changed)`.
    pub fn apply(
        self,
        current: &mut CollectionsData,
        current_stats: &mut PlayStats,
        replace: bool,
    ) -> (ZipImportReport, Vec<String>, bool) {
        let mut old_replaced_covers = Vec::new();
        let mut collections_imported = false;
        let mut liked_added = 0usize;
        let mut playlists_imported = 0usize;
        let mut smart_playlists_imported = 0usize;

        if let Some(col) = self.collections {
            collections_imported = true;
            playlists_imported = col.playlists.len();
            smart_playlists_imported = col.smart.as_ref().map_or(0, Vec::len);
            let mapped_playlists: Vec<PlaylistData> =
                col.playlists
                    .into_iter()
                    .map(|mut p| {
                        p.data.cover_art = p.raw_cover_basename.as_deref().and_then(|base| {
                            self.cover_urls.get(&base.to_ascii_lowercase()).cloned()
                        });
                        p.data
                    })
                    .collect();

            if replace {
                for p in &current.playlists {
                    if let Some(url) = &p.cover_art {
                        old_replaced_covers.push(url.clone());
                    }
                }
                liked_added = col.liked.len();
                current.liked = col.liked;
                current.playlists = mapped_playlists;
                current.smart = col.smart;
                if col.imported_from.is_some() {
                    current.imported_from = col.imported_from;
                }
                for (k, v) in col.extra {
                    current.extra.insert(k, v);
                }
            } else {
                let before_liked: HashSet<String> = current.liked.iter().cloned().collect();
                liked_added = col
                    .liked
                    .iter()
                    .filter(|id| !before_liked.contains(*id))
                    .count();
                current.liked =
                    dedupe_preserving_order(current.liked.iter().cloned().chain(col.liked));

                merge_playlists(
                    &mut current.playlists,
                    mapped_playlists,
                    &mut old_replaced_covers,
                );
                if let Some(imp_smart) = col.smart {
                    let existing = current.smart.get_or_insert_with(Vec::new);
                    merge_smart_playlists(existing, imp_smart);
                }
                if current.imported_from.is_none() {
                    current.imported_from = col.imported_from;
                }
                for (k, v) in col.extra {
                    current.extra.insert(k, v);
                }
            }
        }

        let mut stats_imported = 0usize;
        let mut stats_changed = false;
        if let Some(mapped_stats) = self.stats {
            stats_imported = mapped_stats.len();
            stats_changed = true;
            if replace {
                current_stats.tracks = mapped_stats;
            } else {
                let mut ci_index = build_stats_ci_index(&current_stats.tracks);
                for (dst_id, stat) in mapped_stats {
                    merge_stat_entry_indexed(
                        &mut current_stats.tracks,
                        &mut ci_index,
                        dst_id,
                        stat,
                    );
                }
            }
            current_stats.rebuild_index();
        }

        let report = ZipImportReport {
            collections_imported,
            liked_added,
            liked_total: current.liked.len(),
            playlists_imported,
            playlists_total: current.playlists.len(),
            smart_playlists_imported,
            stats_imported,
            settings_skipped: self.has_settings,
            tracks_exact: self.tracks_exact,
            tracks_remapped: self.tracks_remapped,
            tracks_unmatched: self.tracks_unmatched,
            covers_restored: self.covers_restored,
        };
        (report, old_replaced_covers, stats_changed)
    }
}

fn playlist_names_compatible(existing_name: &str, imported_name: &str) -> bool {
    let ex = existing_name.trim();
    let imp = imported_name.trim();
    ex.is_empty() || ex.to_lowercase() == imp.to_lowercase()
}

fn merge_playlists(
    existing: &mut Vec<PlaylistData>,
    imported: Vec<PlaylistData>,
    old_replaced_covers: &mut Vec<String>,
) {
    let mut used_ids: HashSet<i64> = existing.iter().map(|p| p.id).collect();
    for mut imp in imported {
        if imp.id != 0
            && let Some(ex) = existing
                .iter_mut()
                .find(|p| p.id == imp.id && playlist_names_compatible(&p.name, &imp.name))
        {
            ex.track_ids =
                dedupe_preserving_order(ex.track_ids.iter().cloned().chain(imp.track_ids));
            if ex.name.trim().is_empty() && !imp.name.trim().is_empty() {
                ex.name = imp.name;
            }
            if let Some(new_cover) = imp.cover_art
                && let Some(prev) = ex.cover_art.replace(new_cover.clone())
                && prev != new_cover
            {
                old_replaced_covers.push(prev);
            }
            for (k, v) in imp.extra {
                ex.extra.insert(k, v);
            }
            continue;
        }
        imp.id = claim_next_id(&mut used_ids, imp.id);
        existing.push(imp);
    }
}

fn merge_smart_playlists(existing: &mut Vec<SmartPlaylistData>, imported: Vec<SmartPlaylistData>) {
    let mut used_ids: HashSet<i64> = existing.iter().map(|s| s.id).collect();
    for mut imp in imported {
        if imp.id != 0
            && let Some(ex) = existing
                .iter_mut()
                .find(|s| s.id == imp.id && playlist_names_compatible(&s.name, &imp.name))
        {
            let mut merged_extra = std::mem::take(&mut ex.extra);
            for (k, v) in imp.extra {
                merged_extra.insert(k, v);
            }
            imp.extra = merged_extra;
            *ex = imp;
            continue;
        }
        imp.id = claim_next_id(&mut used_ids, imp.id);
        existing.push(imp);
    }
}

#[cfg(test)]
#[path = "zip_import_tests.rs"]
mod tests;
