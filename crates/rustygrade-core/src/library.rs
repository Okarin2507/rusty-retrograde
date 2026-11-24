use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};

use indexmap::IndexMap;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};
use thiserror::Error;

use crate::collections::CollectionsStore;
use crate::covers::delete_unreferenced_covers;
use crate::models::{Album, Track};
use crate::paths::CoverPaths;

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
    #[serde(skip_serializing_if = "Option::is_none", deserialize_with = "de_opt_i32_lenient")]
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
    #[serde(skip_serializing_if = "Option::is_none", deserialize_with = "de_opt_i64_lenient")]
    pub mtime: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none", deserialize_with = "de_opt_i64_lenient")]
    pub size: Option<i64>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Keeps the order of the keys in library.json, like the C# dictionary (ties in "Recently Added" depend on it).
pub type LibraryMap = IndexMap<String, AlbumDto>;

static WRITE_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));
static RESTORED_FROM_BACKUP: LazyLock<Mutex<bool>> = LazyLock::new(|| Mutex::new(false));

pub struct LibraryStore;

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

    pub fn restored_from_backup() -> bool {
        RESTORED_FROM_BACKUP.lock().map(|g| *g).unwrap_or(false)
    }

    pub fn write_lock() -> &'static Mutex<()> {
        &WRITE_LOCK
    }

    pub fn load() -> Result<Vec<Album>, LibraryError> {
        let raw = Self::load_raw()?;
        Ok(raw.into_values().map(map_album).collect())
    }

    pub fn load_raw() -> Result<LibraryMap, LibraryError> {
        let path = Self::library_path();
        if !path.exists() {
            return Ok(LibraryMap::new());
        }
        if let Ok(bytes) = fs::read(&path)
            && let Ok(parsed) = serde_json::from_slice::<LibraryMap>(&bytes)
        {
            return Ok(parsed);
        }

        let _ = fs::copy(&path, Self::damaged_path());
        let backup = Self::backup_path();
        if backup.exists()
            && let Ok(bytes) = fs::read(&backup)
            && let Ok(parsed) = serde_json::from_slice::<LibraryMap>(&bytes)
        {
            let _ = fs::copy(&backup, &path);
            if let Ok(mut flag) = RESTORED_FROM_BACKUP.lock() {
                *flag = true;
            }
            return Ok(parsed);
        }
        Err(LibraryError::Corrupt)
    }

    pub fn save_raw(library: &LibraryMap) -> Result<(), LibraryError> {
        let dir = CoverPaths::app_dir();
        fs::create_dir_all(&dir)?;
        let path = Self::library_path();
        let backup = Self::backup_path();
        let tmp = CoverPaths::app_dir().join("library.json.tmp");
        let bytes = serde_json::to_vec(library).map_err(std::io::Error::other)?;
        fs::write(&tmp, bytes)?;
        if path.exists() {
            let _ = fs::copy(&path, &backup);
        }
        fs::rename(&tmp, &path)?;
        Ok(())
    }

    pub fn delete_albums(names: &[String]) -> Result<usize, LibraryError> {
        let _guard = WRITE_LOCK.lock().expect("library lock poisoned");
        let mut library = Self::load_raw()?;
        let mut removed = 0;
        let mut detached = Vec::new();
        for name in names {
            if let Some(album) = library.shift_remove(name) {
                removed += 1;
                if let Some(p) = CoverPaths::to_local_path(album.cover_art.as_deref()) {
                    detached.push(p);
                }
                if let Some(p) = CoverPaths::to_local_path(album.cover_art_full.as_deref()) {
                    detached.push(p);
                }
                detached.push(CoverPaths::thumb_path(name));
                detached.push(CoverPaths::full_path(name));
            }
        }
        if removed > 0 {
            Self::save_raw(&library)?;
            let collections = CollectionsStore::load();
            delete_unreferenced_covers(&detached, &library, &collections);
        }
        Ok(removed)
    }

    /// Stores a hand-made track order for album `name` (an album without track numbers, whose
    /// tracks keep their order in library.json). Tracks are matched by their id (as [`map_album`]
    /// gives it); tracks missing from `ids` keep their relative order after the listed ones.
    /// Returns whether the album was found.
    pub fn set_album_track_order(name: &str, ids: &[String]) -> Result<bool, LibraryError> {
        let _guard = WRITE_LOCK.lock().expect("library lock poisoned");
        let mut library = Self::load_raw()?;
        let Some(tracks) = library.get_mut(name).and_then(|a| a.tracks.as_mut()) else {
            return Ok(false);
        };
        reorder_by_ids(tracks, ids, |t| t.id.as_deref().or(t.file_path.as_deref()));
        Self::save_raw(&library)?;
        Ok(true)
    }

    pub fn clear_covers(names: &[String]) -> Result<(), LibraryError> {
        let _guard = WRITE_LOCK.lock().expect("library lock poisoned");
        let mut library = Self::load_raw()?;
        let mut changed = false;
        let mut detached = Vec::new();
        for name in names {
            if let Some(album) = library.get_mut(name) {
                if let Some(p) = CoverPaths::to_local_path(album.cover_art.as_deref()) {
                    detached.push(p);
                }
                if let Some(p) = CoverPaths::to_local_path(album.cover_art_full.as_deref()) {
                    detached.push(p);
                }
                detached.push(CoverPaths::thumb_path(name));
                detached.push(CoverPaths::full_path(name));
                album.cover_art = None;
                album.cover_art_full = None;
                changed = true;
            }
        }
        if changed {
            Self::save_raw(&library)?;
            let collections = CollectionsStore::load();
            delete_unreferenced_covers(&detached, &library, &collections);
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
        let _guard = WRITE_LOCK.lock().expect("library lock poisoned");
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
    let name = a.name.clone().unwrap_or_else(|| "Unknown Album".to_owned());
    let album_artist = a
        .artist
        .clone()
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
            let id = t.id.unwrap_or_else(|| file_path.clone());
            Some(Track {
                id,
                title,
                artist: t.artist.unwrap_or_else(|| album_artist.clone()),
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
    let rank: HashMap<&str, usize> = ids.iter().enumerate().map(|(i, id)| (id.as_str(), i)).collect();
    let mut keyed: Vec<(usize, usize, T)> = std::mem::take(items)
        .into_iter()
        .enumerate()
        .map(|(pos, item)| {
            let r = id_of(&item).and_then(|id| rank.get(id).copied()).unwrap_or(usize::MAX);
            (r, pos, item)
        })
        .collect();
    keyed.sort_by_key(|(r, pos, _)| (*r, *pos));
    items.extend(keyed.into_iter().map(|(_, _, item)| item));
}

/// `albums` with album `name`'s tracks put in the order of `ids` (see [`reorder_by_ids`]).
pub fn with_album_track_order(albums: &[Album], name: &str, ids: &[String]) -> Vec<Album> {
    albums
        .iter()
        .map(|a| {
            let mut a = a.clone();
            if a.name == name {
                reorder_by_ids(&mut a.tracks, ids, |t| Some(t.id.as_str()));
            }
            a
        })
        .collect()
}

pub fn build_track_index(albums: &[Album]) -> HashMap<String, (Track, Album)> {
    let mut map = HashMap::new();
    for album in albums {
        for track in &album.tracks {
            map.insert(track.id.clone(), (track.clone(), album.clone()));
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
        Some(Value::Number(n)) => n.as_i64().map(|x| x as i32).or_else(|| n.as_f64().map(|x| x as i32)),
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
    use image::{DynamicImage, ImageFormat};
    use std::io::Cursor;

    #[test]
    fn reorder_by_ids_puts_listed_first_and_keeps_the_rest() {
        let mut v = vec!["a", "b", "c", "d", "e"];
        let ids: Vec<String> = ["d", "x", "b", "a"].iter().map(|s| s.to_string()).collect();
        reorder_by_ids(&mut v, &ids, |s| Some(*s));
        assert_eq!(v, ["d", "b", "a", "c", "e"]);
    }

    #[test]
    fn delete_albums_preserves_shared_cover_until_last_reference_is_removed() {
        let dir = std::env::temp_dir().join(format!("retro-lib-del-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        crate::paths::with_test_app_dir(&dir, || {
            let mut png = Cursor::new(Vec::new());
            DynamicImage::ImageRgb8(image::RgbImage::new(64, 64))
                .write_to(&mut png, ImageFormat::Png)
                .unwrap();
            let cover_path = save_cover_bytes(&png.into_inner()).unwrap();
            let cover_url = CoverPaths::to_file_url(&cover_path);

            let mut lib = LibraryMap::new();
            lib.insert(
                "Album 1".to_owned(),
                AlbumDto {
                    name: Some("Album 1".to_owned()),
                    cover_art: Some(cover_url.clone()),
                    cover_art_full: Some(cover_url.clone()),
                    ..Default::default()
                },
            );
            lib.insert(
                "Album 2".to_owned(),
                AlbumDto {
                    name: Some("Album 2".to_owned()),
                    cover_art: Some(cover_url.clone()),
                    cover_art_full: Some(cover_url),
                    ..Default::default()
                },
            );
            LibraryStore::save_raw(&lib).unwrap();

            assert_eq!(LibraryStore::delete_albums(&["Album 1".to_owned()]).unwrap(), 1);
            assert!(cover_path.exists(), "Album 2 still references the shared cover");

            assert_eq!(LibraryStore::delete_albums(&["Album 2".to_owned()]).unwrap(), 1);
            assert!(!cover_path.exists(), "shared cover must be removed when last album is deleted");
        });
        let _ = fs::remove_dir_all(&dir);
    }
}
