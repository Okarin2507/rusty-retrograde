use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use lofty::file::TaggedFileExt;
use lofty::picture::PictureType;
use lofty::probe::Probe;

use crate::covers::{MAX_COVER_FILE_BYTES, PlaylistCovers, save_cover_bytes, sha256};
use crate::library::AlbumDto;
use crate::paths::CoverPaths;

use super::grouping::album_folder;

const FOLDER_COVER_NAMES: &[&str] = &["cover", "folder", "front", "album", "albumart"];

pub(super) fn find_and_save_cover(
    album: &AlbumDto,
    cache: &mut HashMap<[u8; 32], String>,
) -> Option<(String, String)> {
    let tracks: Vec<&str> = album
        .tracks
        .as_deref()
        .unwrap_or(&[])
        .iter()
        .filter_map(|t| t.file_path.as_deref())
        .collect();

    let mut data: Option<Vec<u8>> = None;
    for &p in tracks.iter().take(3) {
        if let Some(bytes) = read_embedded_picture(Path::new(p)) {
            data = Some(bytes);
            break;
        }
    }

    if data.is_none() {
        let mut folders: Vec<String> = Vec::new();
        for &p in &tracks {
            if let Some(dir) = Path::new(p).parent() {
                let d = dir.to_string_lossy().into_owned();
                if !d.is_empty() && !folders.iter().any(|x| x.eq_ignore_ascii_case(&d)) {
                    folders.push(d);
                }
            }
            let af = album_folder(p);
            if !af.is_empty() && !folders.iter().any(|x| x.eq_ignore_ascii_case(&af)) {
                folders.push(af);
            }
            if folders.len() >= 8 {
                break;
            }
        }
        for folder in folders.into_iter().take(8) {
            if let Some(bytes) = read_folder_image(Path::new(&folder)) {
                data = Some(bytes);
                break;
            }
        }
    }

    save_cover(&data?, cache)
}

fn read_embedded_picture(source_file: &Path) -> Option<Vec<u8>> {
    let tagged = Probe::open(source_file).ok()?.read().ok()?;
    let tag = tagged.primary_tag().or_else(|| tagged.first_tag())?;
    let pic = tag
        .get_picture_type(PictureType::CoverFront)
        .or_else(|| tag.pictures().first())?;
    (!pic.data().is_empty()).then(|| pic.data().to_vec())
}

fn read_folder_image(folder: &Path) -> Option<Vec<u8>> {
    let entries = fs::read_dir(folder).ok()?;
    let images: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file() && PlaylistCovers::is_supported_ext(p))
        .collect();
    for &target_name in FOLDER_COVER_NAMES {
        if let Some(match_path) = images.iter().find(|p| {
            p.file_stem()
                .and_then(|s| s.to_str())
                .is_some_and(|stem| stem.eq_ignore_ascii_case(target_name))
        }) && let Ok(meta) = fs::metadata(match_path)
            && meta.len() <= MAX_COVER_FILE_BYTES
        {
            return fs::read(match_path).ok();
        }
    }
    None
}

fn save_cover(data: &[u8], cache: &mut HashMap<[u8; 32], String>) -> Option<(String, String)> {
    if data.len() as u64 > MAX_COVER_FILE_BYTES {
        return None;
    }
    let raw_hash = sha256(data);
    if let Some(url) = cache.get(&raw_hash) {
        return Some((url.clone(), url.clone()));
    }
    let path = save_cover_bytes(data).ok()?;
    let url = CoverPaths::to_file_url(&path);
    cache.insert(raw_hash, url.clone());
    Some((url.clone(), url))
}
