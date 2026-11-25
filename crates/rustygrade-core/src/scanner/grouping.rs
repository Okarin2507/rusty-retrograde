use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;

use crate::library::{AlbumDto, LibraryMap};

use super::collect_album_cover_candidates;
use super::metadata::Parsed;

static DISC_FOLDER_RX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^(cd|disc|disk|dvd|vol|volume)[\s._-]*\d+").unwrap());
static ARTIST_SEP_RX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\s*(?:/|&|,|;|、|，|＆|\s+feat\.?\s+|\s+ft\.?\s+|\s+x\s+|\s+with\s+)\s*")
        .unwrap()
});
static SPLIT_ALBUM_RX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(?P<base>.+) \((?P<who>[^()]+)\)(?: \d+)?$").unwrap());

pub(crate) fn is_disc_folder(name: &str) -> bool {
    DISC_FOLDER_RX.is_match(name.trim())
}

#[derive(Default)]
pub(super) struct AlbumFolders {
    own: HashSet<String>,
    parents: HashSet<String>,
}

impl AlbumFolders {
    pub(super) fn from_album(album: &AlbumDto) -> Self {
        let mut f = Self::default();
        if let Some(tracks) = &album.tracks {
            for t in tracks {
                if let Some(fp) = &t.file_path {
                    f.add(fp);
                }
            }
        }
        f
    }

    pub(super) fn add(&mut self, file_path: &str) {
        let folder = album_folder(file_path);
        let key = folder.to_lowercase();
        if self.own.insert(key)
            && let Some(parent) = Path::new(&folder).parent()
        {
            let p = parent.to_string_lossy().to_lowercase();
            if !p.is_empty() {
                self.parents.insert(p);
            }
        }
    }

    fn is_same_folder(&self, file_path: &str) -> bool {
        self.own.contains(&album_folder(file_path).to_lowercase())
    }

    fn is_near(&self, file_path: &str) -> bool {
        let folder = album_folder(file_path);
        let key = folder.to_lowercase();
        if self.own.contains(&key) || self.parents.contains(&key) {
            return true;
        }
        if let Some(parent) = Path::new(&folder).parent() {
            let pk = parent.to_string_lossy().to_lowercase();
            return !pk.is_empty() && (self.parents.contains(&pk) || self.own.contains(&pk));
        }
        false
    }
}

pub(super) fn album_folder(file_path: &str) -> String {
    let p = Path::new(file_path);
    let Some(dir) = p.parent() else {
        return String::new();
    };
    if let Some(name) = dir.file_name().and_then(|n| n.to_str())
        && is_disc_folder(name)
        && let Some(parent) = dir.parent()
    {
        let ps = parent.to_string_lossy();
        if !ps.is_empty() {
            return ps.into_owned();
        }
    }
    dir.to_string_lossy().into_owned()
}

pub(super) fn same_artist(a: &str, b: &str) -> bool {
    normalize_artist(a) == normalize_artist(b)
}

pub(super) fn normalize_artist(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_whitespace())
        .flat_map(|c| c.to_lowercase())
        .collect()
}

pub(super) fn common_artist<'a>(
    track_artists: impl IntoIterator<Item = Option<&'a str>>,
) -> String {
    let mut common: Option<Vec<String>> = None;
    for artist in track_artists {
        let names: Vec<String> = ARTIST_SEP_RX
            .split(artist.unwrap_or(""))
            .map(|n| n.trim().to_owned())
            .filter(|n| !n.is_empty())
            .collect();
        match &mut common {
            None => common = Some(names),
            Some(prev) => {
                let keys: HashSet<String> = names.iter().map(|n| normalize_artist(n)).collect();
                prev.retain(|n| keys.contains(&normalize_artist(n)));
                if prev.is_empty() {
                    break;
                }
            }
        }
    }
    common
        .and_then(|c| c.into_iter().next())
        .unwrap_or_else(|| "Various Artists".to_owned())
}

pub(super) fn merge_split_albums(
    library: &mut LibraryMap,
    touched: &mut HashSet<String>,
    mixed: &mut HashSet<String>,
    detached_covers: &mut Vec<PathBuf>,
) -> usize {
    let mut merged = 0usize;
    let keys: Vec<String> = library.keys().cloned().collect();
    for key in keys {
        let Some(cap) = SPLIT_ALBUM_RX.captures(&key) else {
            continue;
        };
        let base_key = cap["base"].to_owned();
        let who = cap["who"].to_owned();
        if base_key == key || !library.contains_key(&base_key) {
            continue;
        }
        let shares_and_matches = {
            let Some(split) = library.get(&key) else {
                continue;
            };
            let Some(split_tracks) = split.tracks.as_deref().filter(|t| !t.is_empty()) else {
                continue;
            };
            let matches_who = split
                .album_artist
                .as_deref()
                .is_some_and(|a| same_artist(&who, a))
                || split
                    .artist
                    .as_deref()
                    .is_some_and(|a| same_artist(&who, a));
            if !matches_who {
                false
            } else {
                let target_folders: HashSet<String> = library
                    .get(&base_key)
                    .and_then(|a| a.tracks.as_ref())
                    .into_iter()
                    .flatten()
                    .filter_map(|t| t.file_path.as_deref())
                    .map(|fp| album_folder(fp).to_lowercase())
                    .collect();
                !target_folders.is_empty()
                    && split_tracks
                        .iter()
                        .filter_map(|t| t.file_path.as_deref())
                        .any(|fp| target_folders.contains(&album_folder(fp).to_lowercase()))
            }
        };
        if !shares_and_matches {
            continue;
        }

        let Some(mut split_owned) = library.shift_remove(&key) else {
            continue;
        };
        let Some(target) = library.get_mut(&base_key) else {
            library.insert(key, split_owned);
            continue;
        };
        if let Some(tracks) = &mut split_owned.tracks {
            let target_tracks = target.tracks.get_or_insert_with(Vec::new);
            for mut t in tracks.drain(..) {
                t.album = Some(base_key.clone());
                target_tracks.push(t);
            }
        }
        if target.cover_art.as_deref().unwrap_or("").is_empty()
            && !split_owned.cover_art.as_deref().unwrap_or("").is_empty()
        {
            target.cover_art = split_owned.cover_art;
            target.cover_art_full = split_owned.cover_art_full;
        } else {
            collect_album_cover_candidates(&key, &split_owned, detached_covers);
        }
        if target.year == 0 {
            target.year = split_owned.year;
        }
        let t_at = if target.added_at == 0 {
            i64::MAX
        } else {
            target.added_at
        };
        let s_at = if split_owned.added_at == 0 {
            i64::MAX
        } else {
            split_owned.added_at
        };
        let min_at = t_at.min(s_at);
        target.added_at = if min_at == i64::MAX { 0 } else { min_at };

        let s_aa = split_owned
            .album_artist
            .as_deref()
            .or(split_owned.artist.as_deref())
            .unwrap_or("");
        let t_aa = target
            .album_artist
            .as_deref()
            .or(target.artist.as_deref())
            .unwrap_or("");
        if !same_artist(s_aa, t_aa) {
            mixed.insert(base_key.clone());
        }
        touched.insert(base_key);
        merged += 1;
    }
    merged
}

fn same_album(
    cache: &mut HashMap<String, AlbumFolders>,
    key: &str,
    album: &AlbumDto,
    r: &Parsed,
) -> bool {
    if album.tracks.as_ref().is_none_or(|t| t.is_empty()) {
        return true;
    }
    let folders = cache
        .entry(key.to_owned())
        .or_insert_with(|| AlbumFolders::from_album(album));
    if folders.is_same_folder(&r.file_path) {
        return true;
    }
    if let Some(aa) = album
        .album_artist
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        && let Some(ra) = r.album_artist.as_deref().filter(|s| !s.trim().is_empty())
    {
        return same_artist(aa, ra);
    }
    if folders.is_near(&r.file_path) {
        return true;
    }
    let candidate = r.album_artist.as_deref().unwrap_or(&r.artist);
    album
        .artist
        .as_deref()
        .is_some_and(|a| same_artist(a, candidate))
        || album
            .album_artist
            .as_deref()
            .is_some_and(|a| same_artist(a, &r.artist))
}

pub(super) fn resolve_album(
    library: &mut LibraryMap,
    cache: &mut HashMap<String, AlbumFolders>,
    r: &Parsed,
    now_ms: i64,
) -> (String, bool) {
    let artist = r
        .album_artist
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or(&r.artist)
        .to_owned();

    for attempt in 0..=20 {
        let key = match attempt {
            0 => r.album.clone(),
            1 => format!("{} ({})", r.album, artist),
            n => format!("{} ({}) {}", r.album, artist, n),
        };
        if !library.contains_key(&key) {
            library.insert(
                key.clone(),
                AlbumDto {
                    name: Some(key.clone()),
                    artist: Some(artist),
                    album_artist: r
                        .album_artist
                        .as_deref()
                        .filter(|s| !s.trim().is_empty())
                        .map(str::to_owned),
                    year: 0,
                    added_at: now_ms,
                    tracks: Some(Vec::new()),
                    ..AlbumDto::default()
                },
            );
            return (key, true);
        }
        if attempt >= 20 {
            return (key, false);
        }
        let existing = &library[&key];
        if same_album(cache, &key, existing, r) {
            return (key, false);
        }
    }
    unreachable!()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::TrackDto;

    #[test]
    fn common_artist_splits_separators() {
        assert_eq!(
            common_artist([
                Some("Cö shu Nie & Wuthering Waves"),
                Some("Wuthering Waves, SoYeon")
            ]),
            "Wuthering Waves"
        );
        assert_eq!(
            common_artist([Some("Alice"), Some("Bob")]),
            "Various Artists"
        );
    }

    #[test]
    fn disc_folders_match_with_suffixes() {
        for name in [
            "CD1",
            "CD1 (Bonus)",
            "Disc 2 - Remaster",
            "disk_03",
            "DVD2",
            "Volume 4",
        ] {
            assert!(is_disc_folder(name), "{name} should be a disc folder");
        }
        assert!(!is_disc_folder("CDBonus"));
        assert!(!is_disc_folder("Discography"));
        assert_eq!(
            album_folder("/music/Album/CD1 (Bonus)/01.flac"),
            "/music/Album"
        );
    }

    #[test]
    fn merges_split_albums_in_same_folder() {
        let mut lib = LibraryMap::new();
        for (key, artist, path) in [
            ("EP", "Artist A", "/music/ep/01.flac"),
            ("EP (Artist B)", "Artist B", "/music/ep/02.flac"),
        ] {
            lib.insert(
                key.to_owned(),
                AlbumDto {
                    name: Some(key.to_owned()),
                    artist: Some(artist.to_owned()),
                    album_artist: Some(artist.to_owned()),
                    tracks: Some(vec![TrackDto {
                        file_path: Some(path.to_owned()),
                        artist: Some(artist.to_owned()),
                        ..TrackDto::default()
                    }]),
                    ..AlbumDto::default()
                },
            );
        }

        let mut touched = HashSet::new();
        let mut mixed = HashSet::new();
        let mut detached = Vec::new();
        assert_eq!(
            merge_split_albums(&mut lib, &mut touched, &mut mixed, &mut detached),
            1
        );
        assert!(lib.contains_key("EP") && !lib.contains_key("EP (Artist B)"));
        assert_eq!(lib["EP"].tracks.as_ref().unwrap().len(), 2);
        assert!(mixed.contains("EP"));
    }
}
