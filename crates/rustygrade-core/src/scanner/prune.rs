use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::library::LibraryMap;

use super::discovery::Discovery;
use super::path_key;

#[derive(Default)]
pub(super) struct MissingTracks {
    pub safe: Vec<(String, String)>,
    pub suspect: Vec<(String, String)>,
}

fn prep_dirs(dirs: &[PathBuf]) -> Vec<String> {
    dirs.iter()
        .map(|d| path_key(d.to_string_lossy().trim_end_matches(['/', '\\'])))
        .collect()
}

fn is_under_prepared(path_key_str: &str, dirs: &[String]) -> bool {
    for trimmed in dirs {
        if path_key_str.len() > trimmed.len() + 1
            && path_key_str.starts_with(trimmed.as_str())
            && matches!(
                path_key_str.as_bytes().get(trimmed.len()),
                Some(b'/') | Some(b'\\')
            )
        {
            return true;
        }
    }
    false
}

/// Library tracks under the scanned roots whose files are gone, split into `safe` (online folder)
/// and `suspect` (absent root or empty directory looking like an unmounted mount point).
/// Tracks under unreadable folders are skipped.
pub(super) fn collect_missing(
    library: &LibraryMap,
    by_path: &HashMap<String, (String, usize)>,
    found: &Discovery,
    seen_keys: &HashSet<String>,
) -> MissingTracks {
    if found.walked_dirs.is_empty() && found.gone_dirs.is_empty() {
        return MissingTracks::default();
    }
    let unreadable = prep_dirs(&found.unreadable_dirs);
    let walked = prep_dirs(&found.walked_dirs);
    let gone = prep_dirs(&found.gone_dirs);
    let suspect_empty = prep_dirs(&found.suspect_empty_dirs);

    let mut out = MissingTracks::default();
    for (p_key, (album_key, idx)) in by_path {
        if seen_keys.contains(p_key) {
            continue;
        }
        let Some(fp) = library
            .get(album_key)
            .and_then(|a| a.tracks.as_ref())
            .and_then(|ts| ts.get(*idx))
            .and_then(|t| t.file_path.as_deref())
        else {
            continue;
        };
        if is_under_prepared(p_key, &unreadable) {
            continue;
        }
        if !is_under_prepared(p_key, &walked) && !is_under_prepared(p_key, &gone) {
            continue;
        }
        if Path::new(fp).exists() {
            continue;
        }
        let item = (p_key.clone(), album_key.clone());
        if is_under_prepared(p_key, &gone) || is_under_prepared(p_key, &suspect_empty) {
            out.suspect.push(item);
        } else {
            out.safe.push(item);
        }
    }
    out
}

/// Drops the given missing tracks from the library; returns how many were removed.
pub(super) fn prune_tracks(
    library: &mut LibraryMap,
    gone: &[(String, String)],
    touched: &mut HashSet<String>,
) -> usize {
    let gone_paths: HashSet<&str> = gone.iter().map(|(p, _)| p.as_str()).collect();
    let mut removed = 0usize;
    for (_, album_key) in gone {
        if let Some(album) = library.get_mut(album_key)
            && let Some(tracks) = &mut album.tracks
        {
            let before = tracks.len();
            tracks.retain(|t| {
                t.file_path
                    .as_ref()
                    .is_none_or(|fp| !gone_paths.contains(path_key(fp).as_str()))
            });
            removed += before - tracks.len();
            touched.insert(album_key.clone());
        }
    }
    removed
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::super::discovery::walk_paths;
    use super::*;
    use crate::library::{AlbumDto, TrackDto};
    use crate::test_util::TempDir;

    fn album_with(paths: &[&str]) -> AlbumDto {
        AlbumDto {
            tracks: Some(
                paths
                    .iter()
                    .map(|p| TrackDto {
                        file_path: Some((*p).to_owned()),
                        ..TrackDto::default()
                    })
                    .collect(),
            ),
            ..AlbumDto::default()
        }
    }

    #[test]
    fn safe_prune_removes_deleted_online_tracks_while_protecting_offline_roots() {
        let base = TempDir::new("prune");
        let emptied_local = base.join("local-music");
        fs::create_dir_all(&emptied_local).unwrap();
        let emptied_local = fs::canonicalize(&emptied_local).unwrap();
        let absent = base.join("gone-parent/music");
        let healthy = base.join("ok");
        fs::create_dir_all(&healthy).unwrap();
        let healthy = fs::canonicalize(&healthy).unwrap();
        let present = healthy.join("01.flac");
        let deleted_ok = healthy.join("02-deleted.flac");
        fs::write(&present, b"").unwrap();

        let local_gone = emptied_local.join("Album/01.flac");
        let absent_track = absent.join("Album/02.flac");
        let elsewhere = base.join("not-scanned/03.flac");
        let s = |p: &Path| p.to_string_lossy().into_owned();
        let mut library = LibraryMap::new();
        library.insert("LocalEmptied".to_owned(), album_with(&[&s(&local_gone)]));
        library.insert("Absent".to_owned(), album_with(&[&s(&absent_track)]));
        library.insert(
            "Ok".to_owned(),
            album_with(&[&s(&present), &s(&deleted_ok)]),
        );
        library.insert("Elsewhere".to_owned(), album_with(&[&s(&elsewhere)]));

        let mut by_path = HashMap::new();
        for (key, album) in &library {
            for (idx, t) in album.tracks.iter().flatten().enumerate() {
                by_path.insert(path_key(t.file_path.as_ref().unwrap()), (key.clone(), idx));
            }
        }
        let mut found = walk_paths(&[emptied_local, absent, healthy]);
        let unmounted = base.join("unmounted-usb");
        let unmounted_track = unmounted.join("Album/04.flac");
        library.insert("USB".to_owned(), album_with(&[&s(&unmounted_track)]));
        by_path.insert(path_key(&s(&unmounted_track)), ("USB".to_owned(), 0));
        found.walked_dirs.push(unmounted.clone());
        found.suspect_empty_dirs.push(unmounted);

        let seen: HashSet<String> = found
            .files
            .iter()
            .map(|f| path_key(&f.to_string_lossy()))
            .collect();

        let missing_res = collect_missing(&library, &by_path, &found, &seen);
        let mut safe_gone = missing_res.safe;
        let mut suspect_gone = missing_res.suspect;
        safe_gone.sort();
        suspect_gone.sort();
        assert_eq!(
            safe_gone
                .iter()
                .map(|(_, k)| k.as_str())
                .collect::<Vec<_>>(),
            vec!["LocalEmptied", "Ok"]
        );
        assert_eq!(
            suspect_gone
                .iter()
                .map(|(_, k)| k.as_str())
                .collect::<Vec<_>>(),
            vec!["Absent", "USB"]
        );

        let mut touched = HashSet::new();
        assert_eq!(prune_tracks(&mut library, &safe_gone, &mut touched), 2);
        assert!(library["LocalEmptied"].tracks.as_ref().unwrap().is_empty());
        assert_eq!(library["Ok"].tracks.as_ref().unwrap().len(), 1);
        assert_eq!(library["Absent"].tracks.as_ref().unwrap().len(), 1);
        assert_eq!(library["USB"].tracks.as_ref().unwrap().len(), 1);
        assert_eq!(library["Elsewhere"].tracks.as_ref().unwrap().len(), 1);

        assert_eq!(prune_tracks(&mut library, &suspect_gone, &mut touched), 2);
        assert!(library["Absent"].tracks.as_ref().unwrap().is_empty());
        assert!(library["USB"].tracks.as_ref().unwrap().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_root_protects_nested_unreadable_subdirs() {
        use std::os::unix::fs::PermissionsExt;

        let base = TempDir::new("sym-unreadable");
        let real_root = base.join("real");
        let unreadable_sub = real_root.join("LockedAlbum");
        let link_root = base.join("link_music");
        fs::create_dir_all(&unreadable_sub).unwrap();
        std::os::unix::fs::symlink(&real_root, &link_root).unwrap();

        fs::set_permissions(&unreadable_sub, fs::Permissions::from_mode(0o000)).unwrap();
        if fs::read_dir(&unreadable_sub).is_err() {
            let found = walk_paths(std::slice::from_ref(&link_root));
            let raw_locked = link_root.join("LockedAlbum");
            let canon_locked = fs::canonicalize(&real_root).unwrap().join("LockedAlbum");
            assert!(found.unreadable_dirs.contains(&raw_locked));
            assert!(found.unreadable_dirs.contains(&canon_locked));

            let s_raw = raw_locked.join("01.flac").to_string_lossy().into_owned();
            let mut lib = LibraryMap::new();
            lib.insert("Locked".to_owned(), album_with(&[&s_raw]));
            let mut by_path = HashMap::new();
            by_path.insert(path_key(&s_raw), ("Locked".to_owned(), 0));
            let missing = collect_missing(&lib, &by_path, &found, &HashSet::new());
            assert!(missing.safe.is_empty() && missing.suspect.is_empty());
        }
        let _ = fs::set_permissions(&unreadable_sub, fs::Permissions::from_mode(0o755));
    }
}
