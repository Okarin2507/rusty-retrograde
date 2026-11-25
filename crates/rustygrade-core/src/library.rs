use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use indexmap::IndexMap;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};
use thiserror::Error;

use crate::collections::CollectionsStore;
use crate::covers::delete_unreferenced_covers_with_extra;
use crate::json_store::{LoadStatus, load_with_backup, save_atomic_bytes};
use crate::models::{Album, Track};
use crate::paths::CoverPaths;
use crate::scanner::collect_album_cover_candidates;

#[derive(Debug, Error)]
pub enum LibraryError {
    #[error("I/O error accessing library.json: {0}")]
    Io(#[from] std::io::Error),
    #[error(
        "library.json is damaged and there is no usable backup. It was copied to library.json.corrupt; remove library.json to start a new library."
    )]
    Corrupt,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AlbumDto {
    pub name: Option<String>,
    pub artist: Option<String>,
    pub cover_art: Option<String>,
    pub cover_art_full: Option<String>,
    #[serde(deserialize_with = "de_i32_lenient")]
    pub year: i32,
    #[serde(deserialize_with = "de_i64_lenient")]
    pub added_at: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub album_artist: Option<String>,
    pub tracks: Option<Vec<TrackDto>>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct TrackDto {
    pub id: Option<String>,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    #[serde(deserialize_with = "de_f64_lenient")]
    pub duration: f64,
    pub file_path: Option<String>,
    pub src: Option<String>,
    #[serde(
        skip_serializing_if = "Option::is_none",
        deserialize_with = "de_opt_i32_lenient"
    )]
    pub track_number: Option<i32>,
    #[serde(deserialize_with = "de_i32_lenient")]
    pub disc_number: i32,
    pub codec: Option<String>,
    #[serde(deserialize_with = "de_i32_lenient")]
    pub sample_rate: i32,
    #[serde(deserialize_with = "de_i32_lenient")]
    pub bits_per_sample: i32,
    #[serde(deserialize_with = "de_i32_lenient")]
    pub channels: i32,
    #[serde(
        skip_serializing_if = "Option::is_none",
        deserialize_with = "de_opt_i64_lenient"
    )]
    pub mtime: Option<i64>,
    #[serde(
        skip_serializing_if = "Option::is_none",
        deserialize_with = "de_opt_i64_lenient"
    )]
    pub size: Option<i64>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Keeps the order of the keys in library.json, like the C# dictionary (ties in "Recently Added" depend on it).
pub type LibraryMap = IndexMap<String, AlbumDto>;

static WRITE_LOCK: Mutex<()> = Mutex::new(());

pub struct LibraryStore;

fn resolve_library_key(library: &LibraryMap, name: &str, artist: &str) -> Option<String> {
    if let Some(a) = library.get(name) {
        let dto_artist = a.artist.as_deref().unwrap_or("Unknown Artist");
        let dto_aa = a.album_artist.as_deref().unwrap_or("");
        if artist.is_empty() || dto_artist == artist || dto_aa == artist {
            return Some(name.to_owned());
        }
    }
    library
        .iter()
        .find(|(k, a)| {
            let dto_name = a.name.as_deref().unwrap_or(k.as_str());
            let dto_artist = a.artist.as_deref().unwrap_or("Unknown Artist");
            dto_name == name && (artist.is_empty() || dto_artist == artist)
        })
        .map(|(k, _)| k.clone())
}

impl LibraryStore {
    pub fn library_path() -> PathBuf {
        CoverPaths::app_dir().join("library.json")
    }

    pub fn backup_path() -> PathBuf {
        CoverPaths::app_dir().join("library.json.bak")
    }

    pub fn damaged_path() -> PathBuf {
        CoverPaths::app_dir().join("library.json.corrupt")
    }

    pub fn write_lock() -> &'static Mutex<()> {
        &WRITE_LOCK
    }

    pub fn load() -> Result<Vec<Album>, LibraryError> {
        let raw = Self::load_raw()?;
        Ok(raw.into_values().map(map_album).collect())
    }

    pub fn load_raw() -> Result<LibraryMap, LibraryError> {
        match load_with_backup(
            &Self::library_path(),
            &Self::backup_path(),
            &Self::damaged_path(),
            |bytes| serde_json::from_slice::<LibraryMap>(bytes).ok(),
        ) {
            LoadStatus::Ok(parsed) => Ok(parsed),
            LoadStatus::Missing => Ok(LibraryMap::new()),
            LoadStatus::Corrupt => Err(LibraryError::Corrupt),
        }
    }

    pub fn save_raw(library: &LibraryMap) -> Result<(), LibraryError> {
        let bytes = serde_json::to_vec(library).map_err(std::io::Error::other)?;
        save_atomic_bytes(&Self::library_path(), Some(&Self::backup_path()), &bytes)?;
        Ok(())
    }

    pub fn delete_albums(
        targets: &[(String, String)],
        extra_pinned_covers: &HashSet<PathBuf>,
    ) -> Result<usize, LibraryError> {
        let _guard = WRITE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut library = Self::load_raw()?;
        let mut removed = 0;
        let mut detached = Vec::new();
        for (name, artist) in targets {
            let Some(key) = resolve_library_key(&library, name, artist) else {
                continue;
            };
            if let Some(album) = library.shift_remove(&key) {
                removed += 1;
                collect_album_cover_candidates(&key, &album, &mut detached);
            }
        }
        if removed > 0 {
            Self::save_raw(&library)?;
            let collections = CollectionsStore::load();
            delete_unreferenced_covers_with_extra(
                &detached,
                &library,
                &collections,
                extra_pinned_covers,
            );
        }
        Ok(removed)
    }

    /// Stores a hand-made track order for `(name, artist)`. Returns whether the album was found.
    pub fn set_album_track_order(
        name: &str,
        artist: &str,
        ids: &[String],
    ) -> Result<bool, LibraryError> {
        let _guard = WRITE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut library = Self::load_raw()?;
        let Some(key) = resolve_library_key(&library, name, artist) else {
            return Ok(false);
        };
        let Some(tracks) = library.get_mut(&key).and_then(|a| a.tracks.as_mut()) else {
            return Ok(false);
        };
        reorder_by_ids(tracks, ids, |t| t.id.as_deref().or(t.file_path.as_deref()));
        Self::save_raw(&library)?;
        Ok(true)
    }

    pub fn clear_covers(
        targets: &[(String, String)],
        extra_pinned_covers: &HashSet<PathBuf>,
    ) -> Result<(), LibraryError> {
        let _guard = WRITE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut library = Self::load_raw()?;
        let mut changed = false;
        let mut detached = Vec::new();
        for (name, artist) in targets {
            let Some(key) = resolve_library_key(&library, name, artist) else {
                continue;
            };
            if let Some(album) = library.get_mut(&key) {
                collect_album_cover_candidates(&key, album, &mut detached);
                album.cover_art = None;
                album.cover_art_full = None;
                changed = true;
            }
        }
        if changed {
            Self::save_raw(&library)?;
            let collections = CollectionsStore::load();
            delete_unreferenced_covers_with_extra(
                &detached,
                &library,
                &collections,
                extra_pinned_covers,
            );
        }
        Ok(())
    }

    /// Undoes a resync from older builds that created `*.resync-old` cover backups. Run at startup.
    pub fn recover_stashed_covers() -> Result<usize, LibraryError> {
        let Ok(entries) = fs::read_dir(CoverPaths::cover_dir()) else {
            return Ok(0);
        };
        let stale: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "resync-old"))
            .collect();
        if stale.is_empty() {
            return Ok(0);
        }
        let _guard = WRITE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        for old in &stale {
            let target = old.with_extension("");
            if target.exists() {
                try_delete(old);
            } else {
                let _ = fs::rename(old, &target);
            }
        }
        let mut library = Self::load_raw()?;
        let mut fixed = 0;
        for (name, album) in library.iter_mut() {
            if !album.cover_art.as_deref().unwrap_or("").is_empty() {
                continue;
            }
            let thumb = CoverPaths::thumb_path(name);
            let full = CoverPaths::full_path(name);
            if thumb.exists() && full.exists() {
                album.cover_art = Some(CoverPaths::to_file_url(&thumb));
                album.cover_art_full = Some(CoverPaths::to_file_url(&full));
                fixed += 1;
            }
        }
        if fixed > 0 {
            Self::save_raw(&library)?;
        }
        Ok(fixed)
    }
}

pub(crate) fn try_delete(path: &Path) {
    if path.exists() {
        let _ = fs::remove_file(path);
    }
}

pub fn map_album(a: AlbumDto) -> Album {
    let name = a
        .name
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "Unknown Album".to_owned());
    let album_artist = a
        .artist
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "Unknown Artist".to_owned());
    let thumb = CoverPaths::to_local_path(a.cover_art.as_deref());
    let full = CoverPaths::to_local_path(a.cover_art_full.as_deref()).or_else(|| thumb.clone());

    let tracks = a
        .tracks
        .unwrap_or_default()
        .into_iter()
        .filter_map(|t| {
            let file_path = t.file_path?;
            if file_path.is_empty() {
                return None;
            }
            let title = match t.title {
                Some(s) if !s.is_empty() => s,
                _ => Path::new(&file_path)
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("Unknown")
                    .to_owned(),
            };
            let id = match t.id {
                Some(s) if !s.is_empty() => s,
                _ => file_path.clone(),
            };
            let artist = match t.artist {
                Some(s) if !s.is_empty() => s,
                _ => album_artist.clone(),
            };
            Some(Track {
                id,
                title,
                artist,
                album: name.clone(),
                duration: t.duration,
                file_path,
                track_number: t.track_number.unwrap_or(0),
                disc_number: if t.disc_number <= 0 { 1 } else { t.disc_number },
                codec: t.codec.unwrap_or_default(),
                sample_rate: t.sample_rate,
                bits_per_sample: t.bits_per_sample,
                channels: t.channels,
                cover_thumb_path: thumb.clone(),
                cover_full_path: full.clone(),
            })
        })
        .collect();

    Album {
        name,
        artist: album_artist,
        cover_thumb_path: thumb,
        cover_full_path: full,
        year: a.year,
        added_at: a.added_at,
        tracks,
    }
}

/// Puts `items` in the order of `ids` (matched through `id_of`); items whose id is not listed keep
/// their relative order after the listed ones, and listed ids with no item are skipped.
pub fn reorder_by_ids<T>(items: &mut Vec<T>, ids: &[String], id_of: impl Fn(&T) -> Option<&str>) {
    let rank: HashMap<&str, usize> = ids
        .iter()
        .enumerate()
        .map(|(i, id)| (id.as_str(), i))
        .collect();
    let mut keyed: Vec<(usize, usize, T)> = std::mem::take(items)
        .into_iter()
        .enumerate()
        .map(|(pos, item)| {
            let r = id_of(&item)
                .and_then(|id| rank.get(id).copied())
                .unwrap_or(usize::MAX);
            (r, pos, item)
        })
        .collect();
    keyed.sort_by_key(|(r, pos, _)| (*r, *pos));
    items.extend(keyed.into_iter().map(|(_, _, item)| item));
}

/// `albums` with `(name, artist)`'s tracks put in the order of `ids` (see [`reorder_by_ids`]).
pub fn with_album_track_order(
    albums: &[Album],
    name: &str,
    artist: &str,
    ids: &[String],
) -> Vec<Album> {
    albums
        .iter()
        .map(|a| {
            let mut a = a.clone();
            if a.name == name && (artist.is_empty() || a.artist == artist) {
                reorder_by_ids(&mut a.tracks, ids, |t| Some(t.id.as_str()));
            }
            a
        })
        .collect()
}

pub fn build_track_map(albums: &[Album]) -> HashMap<&str, &Track> {
    let total = albums.iter().map(|a| a.tracks.len()).sum();
    let mut map = HashMap::with_capacity(total);
    for album in albums {
        for track in &album.tracks {
            map.insert(track.id.as_str(), track);
        }
    }
    map
}

fn de_i32_lenient<'de, D: Deserializer<'de>>(d: D) -> Result<i32, D::Error> {
    Ok(de_opt_i32_lenient(d)?.unwrap_or(0))
}

fn de_opt_i32_lenient<'de, D: Deserializer<'de>>(d: D) -> Result<Option<i32>, D::Error> {
    let v = Option::<Value>::deserialize(d)?;
    Ok(match v {
        Some(Value::Number(n)) => n
            .as_i64()
            .map(|x| x as i32)
            .or_else(|| n.as_f64().map(|x| x as i32)),
        Some(Value::String(s)) => s.trim().parse::<i32>().ok(),
        _ => None,
    })
}

fn de_i64_lenient<'de, D: Deserializer<'de>>(d: D) -> Result<i64, D::Error> {
    Ok(de_opt_i64_lenient(d)?.unwrap_or(0))
}

fn de_opt_i64_lenient<'de, D: Deserializer<'de>>(d: D) -> Result<Option<i64>, D::Error> {
    let v = Option::<Value>::deserialize(d)?;
    Ok(match v {
        Some(Value::Number(n)) => n.as_i64().or_else(|| n.as_f64().map(|x| x as i64)),
        Some(Value::String(s)) => s.trim().parse::<i64>().ok(),
        _ => None,
    })
}

fn de_f64_lenient<'de, D: Deserializer<'de>>(d: D) -> Result<f64, D::Error> {
    let v = Option::<Value>::deserialize(d)?;
    Ok(match v {
        Some(Value::Number(n)) => n.as_f64().unwrap_or(0.0),
        Some(Value::String(s)) => s.trim().parse::<f64>().unwrap_or(0.0),
        _ => 0.0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::covers::save_cover_bytes;
    use crate::test_util::{TempDir, solid_png};

    #[test]
    fn reorder_by_ids_puts_listed_first_and_keeps_the_rest() {
        let mut v = vec!["a", "b", "c", "d", "e"];
        let ids: Vec<String> = ["d", "x", "b", "a"].iter().map(|s| s.to_string()).collect();
        reorder_by_ids(&mut v, &ids, |s| Some(*s));
        assert_eq!(v, ["d", "b", "a", "c", "e"]);
    }

    #[test]
    fn delete_albums_preserves_shared_cover_until_last_reference_is_removed() {
        let dir = TempDir::new("lib-del");
        crate::paths::with_test_app_dir(&dir, || {
            let cover_path = save_cover_bytes(&solid_png(64, 64, [0, 0, 0])).unwrap();
            let cover_url = CoverPaths::to_file_url(&cover_path);

            let mut lib = LibraryMap::new();
            for (name, artist) in [("Album 1", "Art A"), ("Album 2", "Art B")] {
                lib.insert(
                    name.to_owned(),
                    AlbumDto {
                        name: Some(name.to_owned()),
                        artist: Some(artist.to_owned()),
                        cover_art: Some(cover_url.clone()),
                        cover_art_full: Some(cover_url.clone()),
                        ..Default::default()
                    },
                );
            }
            LibraryStore::save_raw(&lib).unwrap();

            assert_eq!(
                LibraryStore::delete_albums(
                    &[("Album 1".to_owned(), "Art A".to_owned())],
                    &HashSet::new(),
                )
                .unwrap(),
                1
            );
            assert!(
                cover_path.exists(),
                "Album 2 still references the shared cover"
            );

            assert_eq!(
                LibraryStore::delete_albums(
                    &[("Album 2".to_owned(), "Art B".to_owned())],
                    &HashSet::new(),
                )
                .unwrap(),
                1
            );
            assert!(
                !cover_path.exists(),
                "shared cover must be removed when last album is deleted"
            );
        });
    }
}
