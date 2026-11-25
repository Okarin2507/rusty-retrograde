use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use image::ImageReader;

use crate::collections::{CollectionsData, CollectionsStore};
use crate::library::{LibraryError, LibraryMap, LibraryStore, try_delete};
use crate::paths::CoverPaths;

use super::encode::MAX_FULL_COVER_EDGE;
use super::store::{PlaylistCovers, save_cover_bytes};

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
        .unwrap_or_else(|e| e.into_inner());
    let sweep_started = SystemTime::now();
    let pre_collections = CollectionsStore::load();
    let mut library = LibraryStore::load_raw()?;
    let mut explicitly_replaced: HashSet<PathBuf> = HashSet::new();
    let mut changed_seen: HashSet<PathBuf> = HashSet::new();
    let mut checked_ok: HashSet<PathBuf> = HashSet::new();
    let mut result = CoverDedupResult::default();

    let mut record_changed = |p: PathBuf| {
        if changed_seen.insert(p.clone()) {
            result.changed_paths.push(p);
        }
    };

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
                        record_changed(old.clone());
                    }
                }
                record_changed(new_path);
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
                    .is_some_and(|mtime| cutoff.is_some_and(|c| mtime < c));
                if (explicitly_replaced.contains(&p) || old_enough) && fs::remove_file(&p).is_ok() {
                    result.removed_files += 1;
                }
            }
        }
    }

    Ok(result)
}

#[cfg(test)]
mod tests {
    use std::fs::OpenOptions;

    use super::*;
    use crate::collections::PlaylistData;
    use crate::library::AlbumDto;
    use crate::test_util::{TempDir, solid_png};

    #[test]
    fn delete_unreferenced_covers_respects_album_and_playlist_refs() {
        let dir = TempDir::new("cover-refcount");
        crate::paths::with_test_app_dir(&dir, || {
            let shared_path = save_cover_bytes(&solid_png(64, 64, [10, 20, 30])).unwrap();
            let orphan_path = save_cover_bytes(&solid_png(64, 64, [255, 0, 0])).unwrap();
            let shared_url = CoverPaths::to_file_url(&shared_path);

            let mut lib = LibraryMap::new();
            lib.insert(
                "Album 1".to_owned(),
                AlbumDto {
                    cover_art: Some(shared_url.clone()),
                    cover_art_full: Some(shared_url.clone()),
                    ..Default::default()
                },
            );
            let mut cols = CollectionsData {
                playlists: vec![PlaylistData {
                    id: 1,
                    name: "Favs".to_owned(),
                    cover_art: Some(shared_url),
                    ..Default::default()
                }],
                ..Default::default()
            };

            delete_unreferenced_covers(&[shared_path.clone(), orphan_path.clone()], &lib, &cols);
            assert!(shared_path.exists() && !orphan_path.exists());

            lib.shift_remove("Album 1");
            delete_unreferenced_covers(std::slice::from_ref(&shared_path), &lib, &cols);
            assert!(shared_path.exists(), "playlist ref keeps cover alive");

            cols.playlists.clear();
            delete_unreferenced_covers(std::slice::from_ref(&shared_path), &lib, &cols);
            assert!(!shared_path.exists());
        });
    }

    #[test]
    fn deduplicate_and_sweep_migrates_legacy_pairs_and_keeps_playlists() {
        let dir = TempDir::new("cover-migrate");
        crate::paths::with_test_app_dir(&dir, || {
            let cover_dir = CoverPaths::cover_dir();
            fs::create_dir_all(&cover_dir).unwrap();

            let shared_img = solid_png(1000, 1000, [40, 50, 60]);
            let leg_a_t = CoverPaths::thumb_path("Album A");
            let leg_a_f = CoverPaths::full_path("Album A");
            let leg_b_t = CoverPaths::thumb_path("Album B");
            let leg_b_f = CoverPaths::full_path("Album B");
            for p in [&leg_a_t, &leg_a_f, &leg_b_t, &leg_b_f] {
                fs::write(p, &shared_img).unwrap();
            }

            let pl_legacy_full = cover_dir.join("native_pl_1_999_full.jpg");
            fs::write(&pl_legacy_full, solid_png(300, 300, [10, 200, 30])).unwrap();

            let orphan = cover_dir.join("deadbeef_full.jpg");
            fs::write(&orphan, &shared_img).unwrap();
            OpenOptions::new()
                .write(true)
                .open(&orphan)
                .unwrap()
                .set_modified(SystemTime::now() - Duration::from_secs(60))
                .unwrap();
            let fresh_orphan = cover_dir.join("freshbeef_full.jpg");
            fs::write(&fresh_orphan, &shared_img).unwrap();

            let mut lib = LibraryMap::new();
            for (name, t, f) in [
                ("Album A", &leg_a_t, &leg_a_f),
                ("Album B", &leg_b_t, &leg_b_f),
            ] {
                lib.insert(
                    name.to_owned(),
                    AlbumDto {
                        name: Some(name.to_owned()),
                        cover_art: Some(CoverPaths::to_file_url(t)),
                        cover_art_full: Some(CoverPaths::to_file_url(f)),
                        ..Default::default()
                    },
                );
            }
            LibraryStore::save_raw(&lib).unwrap();

            let mut cols = CollectionsData {
                playlists: vec![PlaylistData {
                    id: 1,
                    name: "PL".to_owned(),
                    cover_art: Some(CoverPaths::to_file_url(&pl_legacy_full)),
                    ..Default::default()
                }],
                ..Default::default()
            };
            assert!(migrate_playlist_covers(&mut cols));
            CollectionsStore::save(&cols);

            let res = deduplicate_and_sweep_covers(Some(&cols), None).unwrap();
            assert_eq!(res.migrated_albums, 2);
            for p in [&leg_a_t, &leg_a_f, &leg_b_t, &leg_b_f, &orphan] {
                assert!(!p.exists());
            }
            assert!(fresh_orphan.exists());

            let updated_lib = LibraryStore::load_raw().unwrap();
            let url_a = updated_lib["Album A"].cover_art.as_ref().unwrap();
            assert_eq!(url_a, updated_lib["Album B"].cover_art.as_ref().unwrap());
            let pl_path =
                CoverPaths::to_local_path(cols.playlists[0].cover_art.as_deref()).unwrap();
            assert!(pl_path.exists() && CoverPaths::is_content_cover_file(&pl_path));
        });
    }

    #[test]
    fn migrate_playlist_covers_ignores_paths_outside_cover_dir() {
        let dir = TempDir::new("pl-outside");
        crate::paths::with_test_app_dir(&dir, || {
            let outside_dir = dir.join("external");
            fs::create_dir_all(&outside_dir).unwrap();
            let outside_img = outside_dir.join("custom.jpg");
            fs::write(&outside_img, solid_png(64, 64, [1, 2, 3])).unwrap();
            let outside_url = CoverPaths::to_file_url(&outside_img);

            let mut cols = CollectionsData {
                playlists: vec![PlaylistData {
                    id: 7,
                    name: "External".to_owned(),
                    cover_art: Some(outside_url.clone()),
                    ..Default::default()
                }],
                ..Default::default()
            };

            assert!(!migrate_playlist_covers(&mut cols));
            assert_eq!(
                cols.playlists[0].cover_art.as_deref(),
                Some(outside_url.as_str())
            );
            assert!(outside_img.exists());
        });
    }
}
