use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant, UNIX_EPOCH};

use rayon::prelude::*;

use crate::collections::CollectionsStore;
use crate::covers::delete_unreferenced_covers;
use crate::library::{AlbumDto, LibraryError, LibraryStore, TrackDto};
use crate::paths::CoverPaths;

mod cover_extract;
mod discovery;
mod grouping;
mod metadata;
mod prune;

use cover_extract::find_and_save_cover;
use discovery::{Discovery, walk_paths};
use grouping::{
    AlbumFolders, common_artist, merge_split_albums, normalize_artist, resolve_album, same_artist,
};
use metadata::{Parsed, parse_file};
use prune::{collect_missing, prune_tracks};

pub const AUDIO_EXTENSIONS: &[&str] = &[
    "flac", "mp3", "wav", "aac", "ogg", "m4a", "wma", "aiff", "alac", "opus",
];

#[derive(Debug, Clone)]
pub struct ScanProgress {
    pub phase: &'static str,
    pub scanned: usize,
    pub total: usize,
    pub current_file: String,
}

#[derive(Debug, Clone, Default)]
pub struct ScanResult {
    pub added_tracks: usize,
    pub new_albums: usize,
    pub covers: usize,
    pub updated_tracks: usize,
    pub removed_tracks: usize,
    pub removed_albums: usize,
    pub missing_tracks: usize,
    pub merged_albums: usize,
}

impl ScanResult {
    pub fn changed(&self) -> bool {
        self.added_tracks > 0
            || self.covers > 0
            || self.updated_tracks > 0
            || self.removed_tracks > 0
            || self.removed_albums > 0
            || self.merged_albums > 0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PrunePolicy {
    #[default]
    Never,
    Safe,
    ForceAll,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct ScanOptions {
    pub prune: PrunePolicy,
    pub max_threads: Option<usize>,
    pub force: bool,
    /// Re-extracts album covers during the scan without disturbing covers when extraction fails.
    pub resync_covers: bool,
}

pub struct Scanner;

impl Scanner {
    pub fn is_audio_file(path: &Path) -> bool {
        path.extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| AUDIO_EXTENSIONS.iter().any(|x| x.eq_ignore_ascii_case(e)))
    }

    pub fn discover(paths: &[PathBuf]) -> Vec<PathBuf> {
        walk_paths(paths).files
    }

    pub fn scan(
        input_paths: &[PathBuf],
        options: ScanOptions,
        cancel: Option<&AtomicBool>,
        mut progress: Option<&mut (dyn FnMut(ScanProgress) + Send)>,
    ) -> Result<ScanResult, LibraryError> {
        if let Some(cb) = progress.as_deref_mut() {
            cb(ScanProgress {
                phase: "discovering",
                scanned: 0,
                total: 0,
                current_file: "Discovering files...".to_owned(),
            });
        }

        let found = walk_paths(input_paths);
        let _guard = LibraryStore::write_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        scan_locked(&found, options, cancel, progress)
    }
}

/// On case-sensitive POSIX systems paths are keyed verbatim so distinct files differing only in
/// case do not collide and Unicode/ASCII folding never drifts.
#[inline]
pub(super) fn path_key(s: &str) -> String {
    #[cfg(unix)]
    {
        s.to_owned()
    }
    #[cfg(not(unix))]
    {
        s.to_lowercase()
    }
}

fn scan_locked(
    found: &Discovery,
    options: ScanOptions,
    cancel: Option<&AtomicBool>,
    mut progress: Option<&mut (dyn FnMut(ScanProgress) + Send)>,
) -> Result<ScanResult, LibraryError> {
    let mut library = LibraryStore::load_raw()?;
    let mut dirty = false;
    let mut touched: HashSet<String> = HashSet::new();
    let mut mixed: HashSet<String> = HashSet::new();
    let mut detached_covers: Vec<PathBuf> = Vec::new();
    let mut removed_albums = 0usize;

    let merged = merge_split_albums(&mut library, &mut touched, &mut mixed, &mut detached_covers);

    let mut by_path: HashMap<String, (String, usize)> = HashMap::new();
    for (key, album) in &library {
        if let Some(tracks) = &album.tracks {
            for (idx, t) in tracks.iter().enumerate() {
                if let Some(fp) = &t.file_path {
                    by_path.entry(path_key(fp)).or_insert((key.clone(), idx));
                }
            }
        }
    }

    // Map canonical symlinked paths back to any pre-existing symlinked path stored in `library`.
    let mut canon_to_existing: HashMap<PathBuf, String> = HashMap::new();
    if !found.root_aliases.is_empty() {
        for (canon_root, raw_root) in &found.root_aliases {
            for fp in by_path.keys() {
                if let Ok(rel) = Path::new(fp).strip_prefix(raw_root) {
                    canon_to_existing.insert(canon_root.join(rel), fp.clone());
                }
            }
        }
    }

    let mut to_parse: Vec<(PathBuf, String)> = Vec::new();
    let mut changed: HashSet<String> = HashSet::new();
    let mut stamps: HashMap<String, (i64, i64)> = HashMap::new();
    let mut in_scan: HashSet<String> = HashSet::new();
    let mut seen_keys: HashSet<String> = HashSet::new();

    for f in &found.files {
        let f_str = canon_to_existing
            .get(f)
            .cloned()
            .unwrap_or_else(|| f.to_string_lossy().into_owned());
        let key_str = path_key(&f_str);
        seen_keys.insert(key_str.clone());
        let Some(stamp) = file_stamp(f) else {
            continue;
        };
        stamps.insert(key_str.clone(), stamp);

        if let Some((album_key, track_idx)) = by_path.get(&key_str) {
            in_scan.insert(album_key.clone());
            if let Some(album) = library.get_mut(album_key)
                && let Some(tracks) = &mut album.tracks
                && let Some(track) = tracks.get_mut(*track_idx)
            {
                if track.mtime.is_none() || track.size.is_none() {
                    track.mtime = Some(stamp.0);
                    track.size = Some(stamp.1);
                    dirty = true;
                } else if options.force
                    || track.mtime != Some(stamp.0)
                    || track.size != Some(stamp.1)
                {
                    to_parse.push((f.clone(), f_str));
                    changed.insert(key_str);
                }
            }
        } else {
            to_parse.push((f.clone(), f_str));
        }
    }

    let missing_tracks = collect_missing(&library, &by_path, found, &seen_keys);
    let (removed, missing) = match options.prune {
        PrunePolicy::Never => (0, missing_tracks.safe.len() + missing_tracks.suspect.len()),
        PrunePolicy::Safe => (
            prune_tracks(&mut library, &missing_tracks.safe, &mut touched),
            missing_tracks.suspect.len(),
        ),
        PrunePolicy::ForceAll => {
            let mut all_gone = missing_tracks.safe;
            all_gone.extend(missing_tracks.suspect);
            (prune_tracks(&mut library, &all_gone, &mut touched), 0)
        }
    };

    let mut parsed_results: Vec<Parsed> = Vec::new();
    if !to_parse.is_empty() {
        let total = to_parse.len();
        if let Some(cb) = progress.as_deref_mut() {
            cb(ScanProgress {
                phase: "scanning",
                scanned: 0,
                total,
                current_file: String::new(),
            });
        }
        let threads = options
            .max_threads
            .unwrap_or_else(|| {
                std::thread::available_parallelism()
                    .map(|n| n.get())
                    .unwrap_or(4)
                    .min(4)
            })
            .max(1);
        let done = AtomicUsize::new(0);
        let last_emit = Mutex::new(Instant::now());
        let cb_lock = progress.as_deref_mut().map(Mutex::new);
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .build()
            .map_err(std::io::Error::other)?;
        parsed_results = pool.install(|| {
            to_parse
                .par_iter()
                .filter_map(|(disk_path, stored_path)| {
                    if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
                        return None;
                    }
                    let res = parse_file(disk_path, stored_path);
                    let n = done.fetch_add(1, Ordering::Relaxed) + 1;
                    if let Some(ref lock) = cb_lock
                        && (n == total || n.is_multiple_of(16))
                        && let Ok(mut t) = last_emit.try_lock()
                        && (n == total || t.elapsed() >= Duration::from_millis(80))
                    {
                        *t = Instant::now();
                        if let Ok(mut cb) = lock.lock() {
                            cb(ScanProgress {
                                phase: "scanning",
                                scanned: n,
                                total,
                                current_file: stored_path.clone(),
                            });
                        }
                    }
                    res
                })
                .collect()
        });
    }

    if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
        return Ok(ScanResult::default());
    }

    parsed_results.sort_by_key(|a| a.file_path.to_lowercase());

    let mut folder_cache: HashMap<String, AlbumFolders> = HashMap::new();
    let mut added = 0usize;
    let mut updated = 0usize;
    let mut new_albums = 0usize;
    let now_ms = crate::now_ms();

    let mut known_paths: HashSet<String> = HashSet::new();
    for album in library.values() {
        for t in album.tracks.iter().flatten() {
            if let Some(fp) = &t.file_path {
                known_paths.insert(path_key(fp));
            }
        }
    }

    // Track per-album metadata observed during this scan so tag edits refresh album year/artist/album_artist.
    let mut edited_albums: HashSet<String> = HashSet::new();
    let mut parsed_years: HashMap<String, Vec<i32>> = HashMap::new();
    let mut parsed_album_artists: HashMap<String, Vec<Option<String>>> = HashMap::new();

    for r in parsed_results {
        let low = path_key(&r.file_path);
        let mut is_update = false;
        let mut old_slot: Option<(String, usize)> = None;
        if changed.contains(&low)
            && let Some((old_key, _)) = by_path.get(&low).cloned()
            && let Some(old_album) = library.get_mut(&old_key)
            && let Some(tracks) = &mut old_album.tracks
        {
            if let Some(pos) = tracks
                .iter()
                .position(|t| t.file_path.as_deref().is_some_and(|fp| path_key(fp) == low))
            {
                old_slot = Some((old_key.clone(), pos));
            }
            tracks.retain(|t| t.file_path.as_deref().is_none_or(|fp| path_key(fp) != low));
            folder_cache.remove(&old_key);
            touched.insert(old_key.clone());
            edited_albums.insert(old_key);
            known_paths.remove(&low);
            is_update = true;
        }

        let (key, created) = resolve_album(&mut library, &mut folder_cache, &r, now_ms);
        if created {
            new_albums += 1;
        }
        if is_update {
            edited_albums.insert(key.clone());
        }
        parsed_years.entry(key.clone()).or_default().push(r.year);
        parsed_album_artists
            .entry(key.clone())
            .or_default()
            .push(r.album_artist.clone());

        let album = library.get_mut(&key).expect("album just resolved");
        if known_paths.contains(&low) {
            continue;
        }

        let stamp = stamps.get(&low).copied();
        let track = TrackDto {
            id: Some(r.file_path.clone()),
            title: Some(r.title),
            artist: Some(r.artist),
            album: Some(key.clone()),
            duration: r.duration,
            file_path: Some(r.file_path.clone()),
            src: Some(CoverPaths::to_file_url(Path::new(&r.file_path))),
            track_number: Some(r.track_number),
            disc_number: if r.disc_number <= 0 { 1 } else { r.disc_number },
            codec: Some(r.codec),
            sample_rate: r.sample_rate,
            bits_per_sample: r.bits_per_sample,
            channels: r.channels,
            mtime: stamp.map(|s| s.0),
            size: stamp.map(|s| s.1),
            extra: Default::default(),
        };
        let tracks = album.tracks.get_or_insert_with(Vec::new);
        match old_slot {
            Some((old_key, pos)) if old_key == key => tracks.insert(pos.min(tracks.len()), track),
            _ => tracks.push(track),
        }
        folder_cache
            .entry(key.clone())
            .or_insert_with(|| AlbumFolders::from_album(album))
            .add(&r.file_path);
        known_paths.insert(low);
        touched.insert(key.clone());
        in_scan.insert(key);
        if is_update {
            updated += 1;
        } else {
            added += 1;
        }
    }

    // Refresh album-level year, album_artist, and artist across newly populated or edited albums.
    for key in &touched {
        let Some(album) = library.get_mut(key) else {
            continue;
        };
        let track_count = album.tracks.as_ref().map_or(0, Vec::len);
        let all_reparsed = parsed_years
            .get(key)
            .is_some_and(|ys| ys.len() == track_count && track_count > 0);
        let was_edited = edited_albums.contains(key);

        if let Some(ys) = parsed_years.get(key) {
            let first_nonzero = ys.iter().copied().find(|&y| y > 0);
            if all_reparsed {
                // Follow a changed non-zero year from the tags, but never clear a stored year
                // just because the files no longer carry one.
                if let Some(y) = first_nonzero {
                    album.year = y;
                }
            } else if was_edited && let Some(y) = first_nonzero {
                album.year = y;
            } else if album.year == 0
                && let Some(y) = first_nonzero
            {
                album.year = y;
            }
        }

        if let Some(aas) = parsed_album_artists.get(key) {
            let non_empty: Vec<&str> = aas
                .iter()
                .filter_map(|aa| aa.as_deref().map(str::trim).filter(|s| !s.is_empty()))
                .collect();
            let all_same_aa =
                !non_empty.is_empty() && non_empty.iter().all(|a| same_artist(non_empty[0], a));

            if all_reparsed {
                if non_empty.is_empty() || !all_same_aa {
                    album.album_artist = None;
                } else {
                    album.album_artist = Some(non_empty[0].to_owned());
                }
            } else if !mixed.contains(key)
                && let Some(&first_aa) = non_empty.first()
            {
                if !all_same_aa {
                    mixed.insert(key.clone());
                } else if let Some(existing_aa) = album
                    .album_artist
                    .as_deref()
                    .filter(|s| !s.trim().is_empty())
                {
                    if !same_artist(existing_aa, first_aa) {
                        mixed.insert(key.clone());
                    }
                } else {
                    album.album_artist = Some(first_aa.to_owned());
                }
            }
        }

        if let Some(tracks) = &album.tracks
            && !tracks.is_empty()
        {
            if let Some(aa) = album
                .album_artist
                .as_deref()
                .filter(|s| !s.trim().is_empty())
            {
                album.artist = Some(aa.to_owned());
            } else if was_edited || mixed.contains(key) {
                let distinct: HashSet<String> = tracks
                    .iter()
                    .map(|t| normalize_artist(t.artist.as_deref().unwrap_or("")))
                    .collect();
                if distinct.len() > 1 {
                    album.artist = Some(common_artist(tracks.iter().map(|t| t.artist.as_deref())));
                } else if let Some(only) = tracks
                    .first()
                    .and_then(|t| t.artist.as_deref())
                    .filter(|s| !s.trim().is_empty())
                {
                    album.artist = Some(only.to_owned());
                }
            } else if tracks.len() > 1 {
                let distinct: HashSet<String> = tracks
                    .iter()
                    .map(|t| normalize_artist(t.artist.as_deref().unwrap_or("")))
                    .collect();
                if distinct.len() > 1 {
                    mixed.insert(key.clone());
                    album.album_artist = None;
                    album.artist = Some(common_artist(tracks.iter().map(|t| t.artist.as_deref())));
                }
            }
        }

        // A partially re-read album whose edited AlbumArtist disagrees with the stored one is a
        // mixed album: drop the album artist and fall back to the common track artist.
        if mixed.contains(key)
            && !all_reparsed
            && let Some(tracks) = &album.tracks
            && !tracks.is_empty()
        {
            let artist = common_artist(tracks.iter().map(|t| t.artist.as_deref()));
            album.album_artist = None;
            album.artist = Some(artist);
        }
    }
    for key in &mixed {
        if touched.contains(key) {
            continue;
        }
        if let Some(album) = library.get_mut(key)
            && let Some(tracks) = &album.tracks
            && !tracks.is_empty()
        {
            album.album_artist = None;
            album.artist = Some(common_artist(tracks.iter().map(|t| t.artist.as_deref())));
        }
    }

    if options.prune != PrunePolicy::Never {
        for (key, album) in &library {
            if album.tracks.as_ref().is_none_or(|t| t.is_empty()) {
                touched.insert(key.clone());
            }
        }
    }

    for key in &touched {
        let empty = library
            .get(key)
            .is_some_and(|a| a.tracks.as_ref().is_none_or(|t| t.is_empty()));
        if empty {
            if let Some(removed_album) = library.shift_remove(key) {
                removed_albums += 1;
                collect_album_cover_candidates(key, &removed_album, &mut detached_covers);
            }
            continue;
        }
        if let Some(album) = library.get_mut(key)
            && let Some(tracks) = &mut album.tracks
        {
            tracks.sort_by(|a, b| {
                let da = if a.disc_number <= 0 { 1 } else { a.disc_number };
                let db = if b.disc_number <= 0 { 1 } else { b.disc_number };
                da.cmp(&db).then_with(|| {
                    a.track_number
                        .unwrap_or(0)
                        .cmp(&b.track_number.unwrap_or(0))
                })
            });
        }
    }

    let needing: Vec<String> = in_scan
        .into_iter()
        .filter(|k| {
            library
                .get(k)
                .is_some_and(|a| options.resync_covers || album_cover_missing(a))
        })
        .collect();
    let mut covers = 0usize;
    let mut cover_dedup_cache: HashMap<[u8; 32], String> = HashMap::new();
    let total_covers = needing.len();
    for (i, key) in needing.into_iter().enumerate() {
        if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
            break;
        }
        if let Some(cb) = progress.as_deref_mut() {
            cb(ScanProgress {
                phase: "covers",
                scanned: i,
                total: total_covers,
                current_file: key.clone(),
            });
        }
        if let Some(album) = library.get(&key)
            && let Some((thumb, full)) = find_and_save_cover(album, &mut cover_dedup_cache)
            && let Some(album_mut) = library.get_mut(&key)
        {
            let new_thumb_local = CoverPaths::to_local_path(Some(&thumb));
            let new_full_local = CoverPaths::to_local_path(Some(&full));
            if let Some(old) = CoverPaths::to_local_path(album_mut.cover_art.as_deref())
                && new_thumb_local.as_ref() != Some(&old)
            {
                detached_covers.push(old);
            }
            if let Some(old) = CoverPaths::to_local_path(album_mut.cover_art_full.as_deref())
                && new_full_local.as_ref() != Some(&old)
            {
                detached_covers.push(old);
            }
            if album_mut.cover_art.as_deref() != Some(thumb.as_str())
                || album_mut.cover_art_full.as_deref() != Some(full.as_str())
            {
                dirty = true;
            }
            album_mut.cover_art = Some(thumb);
            album_mut.cover_art_full = Some(full);
            covers += 1;
        }
    }

    let result = ScanResult {
        added_tracks: added,
        new_albums,
        covers,
        updated_tracks: updated,
        removed_tracks: removed,
        removed_albums,
        missing_tracks: missing,
        merged_albums: merged,
    };
    if !mixed.is_empty() || !edited_albums.is_empty() {
        dirty = true;
    }
    if result.changed() || dirty {
        LibraryStore::save_raw(&library)?;
        if !detached_covers.is_empty() {
            let collections = CollectionsStore::load();
            delete_unreferenced_covers(&detached_covers, &library, &collections);
        }
    }
    Ok(result)
}

pub(crate) fn collect_album_cover_candidates(key: &str, album: &AlbumDto, out: &mut Vec<PathBuf>) {
    if let Some(p) = CoverPaths::to_local_path(album.cover_art.as_deref()) {
        out.push(p);
    }
    if let Some(p) = CoverPaths::to_local_path(album.cover_art_full.as_deref()) {
        out.push(p);
    }
    out.push(CoverPaths::thumb_path(key));
    out.push(CoverPaths::full_path(key));
}

fn album_cover_missing(album: &AlbumDto) -> bool {
    let Some(thumb) = CoverPaths::to_local_path(album.cover_art.as_deref()) else {
        return true;
    };
    if !thumb.is_file() {
        return true;
    }
    if let Some(full) = CoverPaths::to_local_path(album.cover_art_full.as_deref())
        && !full.is_file()
    {
        return true;
    }
    false
}

fn file_stamp(path: &Path) -> Option<(i64, i64)> {
    let meta = fs::metadata(path).ok()?;
    let modified = meta.modified().ok()?;
    let ms = modified
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    Some((ms, meta.len() as i64))
}

#[cfg(test)]
mod tests;
