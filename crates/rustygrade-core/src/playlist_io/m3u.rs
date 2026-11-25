use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::Path;

use crate::models::{Album, Track};
use crate::paths::decode_file_uri;

use super::path_mapper::{
    TrackPathMapper, is_absolute_or_drive, is_http_url, resolve_relative_lexically,
};
use super::{MAX_M3U_BYTES, PlaylistIoError, write_file_direct_or_cleanup};

fn strip_newlines(s: &str) -> String {
    s.chars().filter(|&c| c != '\r' && c != '\n').collect()
}

/// Formats a playlist as Extended M3U8 (UTF-8, LF) compatible with Android & standard players.
pub fn format_m3u8(playlist_name: &str, tracks: &[Track]) -> String {
    let mut out = String::from("#EXTM3U\n");
    let clean_name = strip_newlines(playlist_name);
    if !clean_name.trim().is_empty() {
        out.push_str("#PLAYLIST:");
        out.push_str(clean_name.trim());
        out.push('\n');
    }
    for track in tracks {
        let path_src = if track.file_path.trim().is_empty() {
            &track.id
        } else {
            &track.file_path
        };
        let path_line = strip_newlines(path_src.trim());
        let path_line = path_line.trim();
        if path_line.is_empty() {
            continue;
        }
        let secs = if track.duration.is_finite() && track.duration > 0.0 {
            track.duration.round() as i64
        } else {
            -1
        };
        let artist = strip_newlines(track.artist.trim());
        let artist = artist.trim();
        let title = strip_newlines(track.title.trim());
        let title = title.trim();
        let album = strip_newlines(track.album.trim());
        let album = album.trim();

        out.push_str("#EXTINF:");
        out.push_str(&secs.to_string());
        out.push(',');
        if !artist.is_empty() && !title.is_empty() {
            out.push_str(artist);
            out.push_str(" - ");
            out.push_str(title);
        } else if !title.is_empty() {
            out.push_str(title);
        } else {
            out.push_str(artist);
        }
        out.push('\n');

        if !album.is_empty() {
            out.push_str("#EXTALB:");
            out.push_str(album);
            out.push('\n');
        }
        out.push_str(path_line);
        out.push('\n');
    }
    out
}

/// Writes an Extended M3U8 file directly to `dest` (portal SaveFile grants access only to `dest`).
pub fn export_m3u8_file(
    dest: &Path,
    playlist_name: &str,
    tracks: &[Track],
) -> Result<(), PlaylistIoError> {
    let text = format_m3u8(playlist_name, tracks);
    write_file_direct_or_cleanup(dest, |mut file| {
        file.write_all(text.as_bytes())?;
        file.flush()?;
        Ok(())
    })
}

#[derive(Debug, Clone, PartialEq)]
pub struct M3uEntry {
    pub raw_path: String,
    pub duration_secs: Option<f64>,
    pub artist: Option<String>,
    pub title: Option<String>,
    pub album: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ParsedM3u {
    pub playlist_name: Option<String>,
    pub entries: Vec<M3uEntry>,
}

pub fn parse_m3u(content: &str) -> ParsedM3u {
    let content = content.strip_prefix('\u{FEFF}').unwrap_or(content);
    let mut playlist_name: Option<String> = None;
    let mut entries = Vec::new();
    let mut pending_duration: Option<f64> = None;
    let mut pending_artist: Option<String> = None;
    let mut pending_title: Option<String> = None;
    let mut pending_album: Option<String> = None;

    for raw_line in content
        .split('\n')
        .flat_map(|s| s.strip_suffix('\r').unwrap_or(s).split('\r'))
    {
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('#') {
            if let Some(rest) = strip_ascii_prefix_ci(line, "#PLAYLIST:") {
                let name = rest.trim();
                if !name.is_empty() && playlist_name.is_none() {
                    playlist_name = Some(name.to_owned());
                }
            } else if let Some(rest) = strip_ascii_prefix_ci(line, "#EXTINF:") {
                pending_artist = None;
                pending_title = None;
                pending_album = None;

                let (dur_part, display_part) = rest.split_once(',').unwrap_or((rest, ""));
                pending_duration = dur_part
                    .split_whitespace()
                    .next()
                    .and_then(|tok| tok.parse::<f64>().ok())
                    .filter(|&d| d.is_finite() && d > 0.0);
                let display = display_part.trim();
                if !display.is_empty() {
                    if let Some((artist, title)) = display.split_once(" - ") {
                        let a = artist.trim();
                        let t = title.trim();
                        if !a.is_empty() && !t.is_empty() {
                            pending_artist = Some(a.to_owned());
                            pending_title = Some(t.to_owned());
                        } else if !t.is_empty() {
                            pending_title = Some(t.to_owned());
                        } else if !a.is_empty() {
                            pending_title = Some(a.to_owned());
                        }
                    } else {
                        pending_title = Some(display.to_owned());
                    }
                }
            } else if let Some(rest) = strip_ascii_prefix_ci(line, "#EXTALB:") {
                let alb = rest.trim();
                if !alb.is_empty() {
                    pending_album = Some(alb.to_owned());
                }
            }
            continue;
        }

        entries.push(M3uEntry {
            raw_path: line.to_owned(),
            duration_secs: pending_duration.take(),
            artist: pending_artist.take(),
            title: pending_title.take(),
            album: pending_album.take(),
        });
    }

    ParsedM3u {
        playlist_name,
        entries,
    }
}

fn strip_ascii_prefix_ci<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    if s.len() >= prefix.len()
        && s.is_char_boundary(prefix.len())
        && s[..prefix.len()].eq_ignore_ascii_case(prefix)
    {
        Some(&s[prefix.len()..])
    } else {
        None
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct M3uImportResult {
    pub name: String,
    pub track_ids: Vec<String>,
    pub exact: usize,
    pub remapped: usize,
    pub metadata: usize,
    pub unmatched: usize,
    pub total_entries: usize,
}

struct MetaTrackInfo {
    id: String,
    duration: f64,
}

struct MetadataMatcher {
    tracks: Vec<MetaTrackInfo>,
    by_artist_title_album: HashMap<(String, String, String), Vec<usize>>,
    by_artist_title: HashMap<(String, String), Vec<usize>>,
}

impl MetadataMatcher {
    fn from_albums(albums: &[Album]) -> Self {
        let mut tracks = Vec::new();
        let mut by_artist_title_album: HashMap<(String, String, String), Vec<usize>> =
            HashMap::new();
        let mut by_artist_title: HashMap<(String, String), Vec<usize>> = HashMap::new();
        let mut seen_ids = HashSet::new();

        for album in albums {
            for track in &album.tracks {
                if !seen_ids.insert(track.id.clone()) {
                    continue;
                }
                let norm_title = norm_meta(&track.title);
                if norm_title.is_empty() {
                    continue;
                }
                let norm_artist = norm_meta(&track.artist);
                let norm_album = if track.album.trim().is_empty() {
                    norm_meta(&album.name)
                } else {
                    norm_meta(&track.album)
                };
                let idx = tracks.len();
                tracks.push(MetaTrackInfo {
                    id: track.id.clone(),
                    duration: track.duration,
                });
                if !norm_album.is_empty() {
                    by_artist_title_album
                        .entry((norm_artist.clone(), norm_title.clone(), norm_album))
                        .or_default()
                        .push(idx);
                }
                if !norm_artist.is_empty() {
                    by_artist_title
                        .entry((norm_artist, norm_title))
                        .or_default()
                        .push(idx);
                }
            }
        }

        Self {
            tracks,
            by_artist_title_album,
            by_artist_title,
        }
    }

    fn resolve(&self, entry: &M3uEntry) -> Option<String> {
        let norm_title = entry.title.as_deref().map(norm_meta).unwrap_or_default();
        if norm_title.is_empty() {
            return None;
        }
        let norm_artist = entry.artist.as_deref().map(norm_meta).unwrap_or_default();
        let norm_album = entry
            .album
            .as_deref()
            .map(norm_meta)
            .filter(|a| !a.is_empty());

        let cands = if let Some(alb) = norm_album {
            self.by_artist_title_album
                .get(&(norm_artist, norm_title, alb))?
        } else if !norm_artist.is_empty() {
            self.by_artist_title.get(&(norm_artist, norm_title))?
        } else {
            return None;
        };

        if cands.len() == 1 {
            return Some(self.tracks[cands[0]].id.clone());
        }
        if cands.len() > 1
            && let Some(dur) = entry.duration_secs
            && dur.is_finite()
            && dur > 0.0
        {
            let dur_matches: Vec<&MetaTrackInfo> = cands
                .iter()
                .map(|&i| &self.tracks[i])
                .filter(|t| {
                    t.duration.is_finite() && t.duration > 0.0 && (t.duration - dur).abs() <= 3.0
                })
                .collect();
            if dur_matches.len() == 1 {
                return Some(dur_matches[0].id.clone());
            }
        }
        None
    }
}

fn norm_meta(s: &str) -> String {
    s.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

pub fn import_m3u_str(
    content: &str,
    playlist_path: Option<&Path>,
    albums: &[Album],
    source_roots: &[String],
) -> Result<M3uImportResult, PlaylistIoError> {
    if content.len() > MAX_M3U_BYTES {
        return Err(PlaylistIoError::TooLarge {
            what: "Playlist file",
            limit: "16 MiB".to_owned(),
        });
    }

    let parsed = parse_m3u(content);
    if parsed.entries.is_empty() {
        return Err(PlaylistIoError::EmptyPlaylist);
    }

    let fallback_stem = playlist_path
        .and_then(|p| p.file_stem())
        .and_then(|s| s.to_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    let name = parsed
        .playlist_name
        .or(fallback_stem)
        .unwrap_or_else(|| "Imported Playlist".to_owned());

    let mapper = TrackPathMapper::from_albums(albums, source_roots);
    let meta_matcher = MetadataMatcher::from_albums(albums);
    let parent_dir = playlist_path.and_then(Path::parent);

    let mut exact = 0usize;
    let mut remapped = 0usize;
    let mut metadata = 0usize;
    let mut unmatched = 0usize;
    let mut track_ids = Vec::new();
    let mut seen_track_ids = HashSet::new();

    for entry in &parsed.entries {
        let decoded_uri = decode_file_uri(&entry.raw_path);
        let resolved_rel = if decoded_uri.is_none()
            && !is_http_url(&entry.raw_path)
            && !is_absolute_or_drive(&entry.raw_path)
        {
            parent_dir.map(|dir| resolve_relative_lexically(dir, &entry.raw_path))
        } else {
            None
        };

        let mut exact_candidates: Vec<(String, bool)> = Vec::with_capacity(3);
        if let Some(rel) = &resolved_rel {
            exact_candidates.push((rel.to_string_lossy().into_owned(), true));
            exact_candidates.push((entry.raw_path.clone(), false));
        } else if let Some(u) = &decoded_uri {
            exact_candidates.push((u.clone(), true));
            exact_candidates.push((entry.raw_path.clone(), false));
        } else {
            let allow_probe =
                !is_http_url(&entry.raw_path) && is_absolute_or_drive(&entry.raw_path);
            exact_candidates.push((entry.raw_path.clone(), allow_probe));
        }
        let mut tier1_match: Option<String> = None;
        for (cand, allow_probe) in &exact_candidates {
            if let Some(id) = mapper.map_exact(cand) {
                tier1_match = Some(id);
                break;
            }
            if *allow_probe {
                let p = Path::new(cand);
                if p.is_file()
                    && let Ok(canon) = fs::canonicalize(p)
                    && let Some(id) = mapper.map_exact(&canon.to_string_lossy())
                {
                    tier1_match = Some(id);
                    break;
                }
            }
        }
        if let Some(id) = tier1_match {
            if seen_track_ids.insert(id.clone()) {
                exact += 1;
                track_ids.push(id);
            }
            continue;
        }

        if !is_http_url(&entry.raw_path) {
            let suffix_source = decoded_uri
                .as_deref()
                .or_else(|| resolved_rel.as_ref().and_then(|p| p.to_str()))
                .unwrap_or(&entry.raw_path);
            if let Some(id) = mapper.map_suffix(suffix_source) {
                if seen_track_ids.insert(id.clone()) {
                    remapped += 1;
                    track_ids.push(id);
                }
                continue;
            }
        }

        if let Some(id) = meta_matcher.resolve(entry) {
            if seen_track_ids.insert(id.clone()) {
                metadata += 1;
                track_ids.push(id);
            }
            continue;
        }

        unmatched += 1;
    }

    if track_ids.is_empty() {
        return Err(PlaylistIoError::NoMatchingTracks {
            total: parsed.entries.len(),
        });
    }

    Ok(M3uImportResult {
        name,
        track_ids,
        exact,
        remapped,
        metadata,
        unmatched,
        total_entries: parsed.entries.len(),
    })
}

const CP1252_80_9F: [char; 32] = [
    '\u{20AC}', '\u{0081}', '\u{201A}', '\u{0192}', '\u{201E}', '\u{2026}', '\u{2020}', '\u{2021}',
    '\u{02C6}', '\u{2030}', '\u{0160}', '\u{2039}', '\u{0152}', '\u{008D}', '\u{017D}', '\u{008F}',
    '\u{0090}', '\u{2018}', '\u{2019}', '\u{201C}', '\u{201D}', '\u{2022}', '\u{2013}', '\u{2014}',
    '\u{02DC}', '\u{2122}', '\u{0161}', '\u{203A}', '\u{0153}', '\u{009D}', '\u{017E}', '\u{0178}',
];

fn decode_cp1252_bytes(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len());
    for &b in bytes {
        if (0x80..=0x9F).contains(&b) {
            out.push(CP1252_80_9F[(b - 0x80) as usize]);
        } else {
            out.push(b as char);
        }
    }
    out
}

pub fn import_m3u_file(
    path: &Path,
    albums: &[Album],
    source_roots: &[String],
) -> Result<M3uImportResult, PlaylistIoError> {
    let meta = fs::metadata(path)?;
    if meta.len() > MAX_M3U_BYTES as u64 {
        return Err(PlaylistIoError::TooLarge {
            what: "Playlist file",
            limit: "16 MiB".to_owned(),
        });
    }
    let file = File::open(path)?;
    let mut buf = Vec::with_capacity((meta.len() as usize).min(MAX_M3U_BYTES));
    file.take(MAX_M3U_BYTES as u64 + 1).read_to_end(&mut buf)?;
    if buf.len() > MAX_M3U_BYTES {
        return Err(PlaylistIoError::TooLarge {
            what: "Playlist file",
            limit: "16 MiB".to_owned(),
        });
    }
    let is_legacy_m3u = path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("m3u"));
    let text = match String::from_utf8(buf) {
        Ok(utf8) => utf8,
        Err(err) if is_legacy_m3u => decode_cp1252_bytes(&err.into_bytes()),
        Err(_) => return Err(PlaylistIoError::InvalidUtf8),
    };
    import_m3u_str(&text, Some(path), albums, source_roots)
}

#[cfg(test)]
#[path = "m3u_tests.rs"]
mod tests;
