use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::Deserialize;
use serde_json::{Map, Value};
use thiserror::Error;

use crate::collections::{CollectionsData, PlaylistData, SmartPlaylistData};
#[cfg(test)]
use crate::covers::save_cover_bytes;
use crate::covers::{ImageLimits, prepare_cover_bytes, sha256_hex_prefix, write_prepared_cover};
use crate::library::LibraryStore;
use crate::models::{Album, Track};
use crate::paths::CoverPaths;
use crate::stats::{PlayStat, PlayStats};
use crate::zip_archive::{ZipError, ZipReader, ZipWriter};

pub const SAF_PREFIX: &str = "/saf/";
pub const MAX_M3U_BYTES: usize = 16 * 1024 * 1024;

pub const MAX_BACKUP_JSON_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_BACKUP_COVER_BYTES: usize = 10 * 1024 * 1024;
pub const MAX_BACKUP_COVERS_TOTAL_BYTES: usize = 50 * 1024 * 1024;
pub const MAX_ARCHIVE_TOTAL_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_BACKUP_COVER_COUNT: usize = MAX_ARCHIVE_ACCEPTED_ENTRIES - 1;
pub const MAX_ARCHIVE_ACCEPTED_ENTRIES: usize = 500;
pub const MAX_ARCHIVE_SCANNED_ENTRIES: usize = 50_000;
pub const MAX_ARCHIVE_IGNORED_BYTES: u64 = 1 << 30;
pub const ARCHIVE_IGNORED_RATIO_GRACE_BYTES: u64 = 8 * 1024 * 1024;
pub const MAX_ARCHIVE_IGNORED_RATIO: u64 = 100;

const FILE_COLLECTIONS: &str = "native-collections.json";
const FILE_STATS: &str = "native-stats.json";
const FILE_SETTINGS: &str = "native-settings.json";
const PLAYLIST_COVER_PREFIX: &str = "native_pl_";

#[derive(Debug, Error)]
pub enum PlaylistIoError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Zip(#[from] ZipError),
    #[error("Playlist file is not valid UTF-8.")]
    InvalidUtf8,
    #[error("{what} is too large to import (limit {limit}).")]
    TooLarge { what: &'static str, limit: String },
    #[error("Playlist file contains no track entries.")]
    EmptyPlaylist,
    #[error("No tracks in playlist ({total} entries) matched your library.")]
    NoMatchingTracks { total: usize },
    #[error("Backup archive contains no Retrograde data files.")]
    EmptyBackup,
    #[error("Backup archive contains neither native-collections.json nor native-stats.json.")]
    MissingCollections,
    #[error("Invalid JSON in {file}: {message}")]
    InvalidJson { file: &'static str, message: String },
    #[error("Invalid playlist cover '{name}': {reason}")]
    InvalidCover { name: String, reason: &'static str },
    #[error("Image error while importing cover '{name}': {message}")]
    CoverImage { name: String, message: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MapOutcome {
    Exact(String),
    Remapped(String),
    Unmatched(String),
}

impl MapOutcome {
    pub fn target_id(&self) -> &str {
        match self {
            Self::Exact(s) | Self::Remapped(s) | Self::Unmatched(s) => s,
        }
    }
}

struct CandidateTrack {
    id: String,
    segs: Vec<String>,
}

/// Cross-OS track path mapper ported from `retrograde-android/rust/retrograde-core/src/backup.rs`.
pub struct TrackPathMapper {
    exact: HashMap<String, String>,
    ci_exact: HashMap<String, Option<String>>,
    source_roots: Vec<Vec<String>>,
    all_tracks: Vec<CandidateTrack>,
    by_filename: HashMap<String, Vec<usize>>,
}

impl TrackPathMapper {
    pub fn from_albums(albums: &[Album], source_roots: &[String]) -> Self {
        let mut exact = HashMap::new();
        let mut ci_exact: HashMap<String, Option<String>> = HashMap::new();
        let mut all_tracks = Vec::new();
        let mut by_filename: HashMap<String, Vec<usize>> = HashMap::new();
        let mut seen_ids = HashSet::new();

        for album in albums {
            for track in &album.tracks {
                exact.insert(track.id.clone(), track.id.clone());
                if !track.file_path.is_empty() {
                    exact
                        .entry(track.file_path.clone())
                        .or_insert_with(|| track.id.clone());
                }
                for key in [&track.id, &track.file_path] {
                    if key.is_empty() {
                        continue;
                    }
                    let low = key.to_lowercase();
                    ci_exact
                        .entry(low)
                        .and_modify(|slot| {
                            if slot.as_deref() != Some(track.id.as_str()) {
                                *slot = None;
                            }
                        })
                        .or_insert_with(|| Some(track.id.clone()));
                }

                if seen_ids.insert(track.id.clone()) {
                    let seg_source = if !track.file_path.is_empty() {
                        &track.file_path
                    } else {
                        &track.id
                    };
                    let segs = normalize_path_segments(seg_source);
                    if let Some(filename) = segs.last().cloned() {
                        let idx = all_tracks.len();
                        all_tracks.push(CandidateTrack {
                            id: track.id.clone(),
                            segs,
                        });
                        by_filename.entry(filename).or_default().push(idx);
                    }
                }
            }
        }

        let mut norm_roots: Vec<Vec<String>> = source_roots
            .iter()
            .map(|r| normalize_path_segments(r))
            .filter(|s| !s.is_empty())
            .collect();
        // Longest prefix first so more specific roots are tried before parent roots.
        norm_roots.sort_by_key(|r| std::cmp::Reverse(r.len()));
        let mut dedup_roots = Vec::with_capacity(norm_roots.len());
        let mut seen_roots = HashSet::new();
        for r in norm_roots {
            if seen_roots.insert(r.clone()) {
                dedup_roots.push(r);
            }
        }

        Self {
            exact,
            ci_exact,
            source_roots: dedup_roots,
            all_tracks,
            by_filename,
        }
    }

    pub fn map_exact(&self, raw_id: &str) -> Option<String> {
        if let Some(local) = self.exact.get(raw_id) {
            return Some(local.clone());
        }
        if let Some(Some(local)) = self.ci_exact.get(&raw_id.to_lowercase()) {
            return Some(local.clone());
        }
        None
    }

    pub fn map_suffix(&self, raw_id: &str) -> Option<String> {
        let segs = normalize_path_segments(raw_id);
        let filename = segs.last()?;
        let candidates = self.by_filename.get(filename)?;

        // Explicit/settings source_root prefix match.
        for root in &self.source_roots {
            if segs.len() > root.len() && segs[..root.len()] == root[..] {
                let rel = &segs[root.len()..];
                let mut matched_id: Option<&str> = None;
                let mut ambiguous = false;
                for &idx in candidates {
                    let cand = &self.all_tracks[idx];
                    if cand.segs.ends_with(rel) {
                        match matched_id {
                            None => matched_id = Some(&cand.id),
                            Some(prev) if prev != cand.id => {
                                ambiguous = true;
                                break;
                            }
                            _ => {}
                        }
                    }
                }
                if !ambiguous && let Some(id) = matched_id {
                    return Some(id.to_owned());
                }
            }
        }

        // Automatic longest common trailing-segment match: require >= 3 segments when the parent
        // folder is a generic disc folder (`CD1`, `Disc 2`, `Disk 01`) so `AlbumA/CD1/01.flac`
        // cannot match `AlbumB/CD1/01.flac`; otherwise >= 2 segments.
        let min_required = if segs.len() >= 2 && is_generic_disc_folder(&segs[segs.len() - 2]) {
            3
        } else {
            2
        };

        let mut best_len = 0usize;
        let mut best_id: Option<&str> = None;
        let mut tied = false;

        for &idx in candidates {
            let cand = &self.all_tracks[idx];
            let common = common_trailing_segments(&segs, &cand.segs);
            if common > best_len {
                best_len = common;
                best_id = Some(&cand.id);
                tied = false;
            } else if common == best_len && common >= min_required && best_id != Some(cand.id.as_str()) {
                tied = true;
            }
        }

        if best_len >= min_required && !tied {
            best_id.map(str::to_owned)
        } else {
            None
        }
    }

    pub fn map_id(&self, raw_id: &str) -> MapOutcome {
        if let Some(local) = self.map_exact(raw_id) {
            return MapOutcome::Exact(local);
        }
        if let Some(local) = self.map_suffix(raw_id) {
            return MapOutcome::Remapped(local);
        }
        MapOutcome::Unmatched(raw_id.to_owned())
    }
}

fn is_generic_disc_folder(seg: &str) -> bool {
    let rest = if let Some(r) = seg.strip_prefix("cd") {
        r
    } else if let Some(r) = seg.strip_prefix("disc") {
        r
    } else if let Some(r) = seg.strip_prefix("disk") {
        r
    } else {
        return false;
    };
    let rest = rest.strip_prefix([' ', '_', '-']).unwrap_or(rest);
    !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit())
}

fn common_trailing_segments(a: &[String], b: &[String]) -> usize {
    a.iter()
        .rev()
        .zip(b.iter().rev())
        .take_while(|(x, y)| x == y)
        .count()
}

/// Normalizes a local, Windows, Linux, or Android SAF path into lowercase segments.
pub fn normalize_path_segments(raw: &str) -> Vec<String> {
    let replaced = raw.trim().replace('\\', "/");
    if replaced.is_empty() {
        return Vec::new();
    }
    let (was_saf, body) = if let Some(rest) = replaced.strip_prefix(SAF_PREFIX) {
        (true, rest)
    } else if replaced == "/saf" {
        (true, "")
    } else {
        (false, replaced.as_str())
    };
    let had_leading_slash = !was_saf && body.starts_with('/');

    let mut segs: Vec<String> = Vec::new();
    for part in body.split('/') {
        if part.is_empty() || part == "." {
            continue;
        }
        if part == ".." {
            if !segs.is_empty() {
                segs.pop();
            }
            continue;
        }
        segs.push(part.to_lowercase());
    }

    if let Some(first) = segs.first().cloned()
        && let Some(colon) = first.find(':')
    {
        let prefix = &first[..colon];
        let is_drive = colon == 1 && prefix.as_bytes()[0].is_ascii_alphabetic();
        let strip_prefix = was_saf || !had_leading_slash || is_drive;
        if strip_prefix {
            let after = &first[colon + 1..];
            if after.is_empty() {
                segs.remove(0);
            } else {
                segs[0] = after.to_owned();
            }
        }
    }

    segs
}

/// Suggests a filesystem-safe export filename (e.g. `my_mix.m3u8` or `my_mix.zip`).
pub fn suggest_export_filename(name: &str, ext: &str) -> String {
    let sanitized: String = name
        .trim()
        .chars()
        .map(|c| {
            if c.is_control() || matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') {
                '_'
            } else {
                c
            }
        })
        .collect();
    let trimmed = sanitized.trim_matches(|c: char| c == '.' || c == '_' || c.is_whitespace());
    let base: String = trimmed.chars().take(100).collect();
    let stem = base.trim_matches(|c: char| c == '.' || c == '_' || c.is_whitespace());
    let stem = if stem.is_empty() { "playlist" } else { stem };
    let clean_ext = ext.trim().trim_start_matches('.');
    if clean_ext.is_empty() {
        stem.to_owned()
    } else {
        format!("{stem}.{clean_ext}")
    }
}

fn strip_newlines(s: &str) -> String {
    s.chars().filter(|&c| c != '\r' && c != '\n').collect()
}

fn write_file_direct_or_cleanup(
    dest: &Path,
    write_fn: impl FnOnce(File) -> Result<(), PlaylistIoError>,
) -> Result<(), PlaylistIoError> {
    let file = File::create(dest)?;
    if let Err(err) = write_fn(file) {
        let _ = fs::remove_file(dest);
        return Err(err);
    }
    Ok(())
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
    if s.len() >= prefix.len() && s.is_char_boundary(prefix.len()) && s[..prefix.len()].eq_ignore_ascii_case(prefix) {
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
    s.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

fn is_http_url(raw: &str) -> bool {
    (raw.as_bytes().get(..7).is_some_and(|b| b.eq_ignore_ascii_case(b"http://")))
        || (raw.as_bytes().get(..8).is_some_and(|b| b.eq_ignore_ascii_case(b"https://")))
}

fn decode_file_uri(raw: &str) -> Option<String> {
    if !raw.as_bytes().get(..7).is_some_and(|b| b.eq_ignore_ascii_case(b"file://")) {
        return None;
    }
    let mut body = &raw[7..];
    if body.as_bytes().get(..10).is_some_and(|b| b.eq_ignore_ascii_case(b"localhost/")) {
        body = &body[9..];
    }
    let before_qf = match (body.find('?'), body.find('#')) {
        (Some(q), Some(h)) => &body[..q.min(h)],
        (Some(q), None) => &body[..q],
        (None, Some(h)) => &body[..h],
        (None, None) => body,
    };
    let bytes = before_qf.as_bytes();
    let p = if bytes.len() > 2 && bytes[0] == b'/' && bytes[2] == b':' && bytes[1].is_ascii_alphabetic() {
        &before_qf[1..]
    } else {
        before_qf
    };
    let decoded = urlencoding::decode(p)
        .map(|c| c.into_owned())
        .unwrap_or_else(|_| p.to_owned());
    Some(decoded)
}

fn is_absolute_or_drive(raw: &str) -> bool {
    let bytes = raw.as_bytes();
    raw.starts_with('/')
        || raw.starts_with('\\')
        || (bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':')
        || raw.as_bytes().get(..7).is_some_and(|b| b.eq_ignore_ascii_case(b"file://"))
}

fn resolve_relative_lexically(base_dir: &Path, rel: &str) -> PathBuf {
    let mut out = base_dir.to_path_buf();
    for seg in rel.replace('\\', "/").split('/') {
        if seg.is_empty() || seg == "." {
            continue;
        }
        if seg == ".." {
            out.pop();
        } else {
            out.push(seg);
        }
    }
    out
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

        // Tier 1: Exact / unambiguous case-insensitive match.
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

        // Tier 2: Cross-OS path suffix match (skipped for HTTP/HTTPS stream URLs).
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

        // Tier 3: Unambiguous metadata fallback.
        if let Some(id) = meta_matcher.resolve(entry) {
            if seen_track_ids.insert(id.clone()) {
                metadata += 1;
                track_ids.push(id);
            }
            continue;
        }

        // Tier 4: Unmatched.
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackupCoverFile {
    pub name: String,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone, Default)]
pub struct BackupBundle {
    pub collections_json: Option<String>,
    pub stats_json: Option<String>,
    pub settings_json: Option<String>,
    pub playlist_covers: Vec<BackupCoverFile>,
}

fn has_supported_cover_ext(name: &str) -> bool {
    Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| matches!(e.to_ascii_lowercase().as_str(), "jpg" | "jpeg" | "png" | "webp"))
}

/// Validates Android `native_pl_<1..=120 allowed chars>.(jpg|jpeg|png|webp)` filenames,
/// matching `BackupArchive.kt`'s `COVER_NAME` regex and path checks.
pub fn is_android_cover_filename(name: &str) -> bool {
    if name.is_empty()
        || name.contains('/')
        || name.contains('\\')
        || name.contains('\0')
        || name.contains("..")
        || !has_supported_cover_ext(name)
    {
        return false;
    }
    let Some(rest) = name.strip_prefix(PLAYLIST_COVER_PREFIX) else {
        return false;
    };
    let Some((stem, _ext)) = rest.rsplit_once('.') else {
        return false;
    };
    (1..=120).contains(&stem.len())
        && stem
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
}

/// Validates Desktop content-addressed `c_<24-hex>.(jpg|jpeg|png|webp)` filenames.
pub fn is_content_cover_filename(name: &str) -> bool {
    if name.is_empty()
        || name.contains('/')
        || name.contains('\\')
        || name.contains('\0')
        || name.contains("..")
        || !has_supported_cover_ext(name)
    {
        return false;
    }
    let Some(rest) = name.strip_prefix("c_") else {
        return false;
    };
    let Some((stem, _ext)) = rest.rsplit_once('.') else {
        return false;
    };
    stem.len() == 24 && stem.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

fn validate_import_cover_filename(name: &str) -> Result<(), PlaylistIoError> {
    if name.is_empty()
        || name.contains('/')
        || name.contains('\\')
        || name.contains('\0')
        || name.contains("..")
    {
        return Err(PlaylistIoError::InvalidCover {
            name: name.to_owned(),
            reason: "illegal path characters",
        });
    }
    if !(is_android_cover_filename(name) || is_content_cover_filename(name)) {
        return Err(PlaylistIoError::InvalidCover {
            name: name.to_owned(),
            reason: "unexpected cover filename format",
        });
    }
    Ok(())
}

fn extract_cover_basename(cover_art: Option<&str>) -> Option<String> {
    let local = CoverPaths::to_local_path(cover_art)?;
    let raw = local.to_string_lossy();
    let base = raw.rsplit(['/', '\\']).next()?.trim();
    if base.is_empty() {
        None
    } else {
        Some(base.to_owned())
    }
}

fn detect_safe_ext(path: &Path, bytes: &[u8]) -> &'static str {
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        match ext.to_ascii_lowercase().as_str() {
            "jpg" | "jpeg" => return "jpg",
            "png" => return "png",
            "webp" => return "webp",
            _ => {}
        }
    }
    match image::guess_format(bytes) {
        Ok(image::ImageFormat::Png) => "png",
        Ok(image::ImageFormat::WebP) => "webp",
        _ => "jpg",
    }
}

/// Computes an Android-compatible `native_pl_*` cover filename for ZIP export.
pub fn android_export_cover_name(local_path: &Path, bytes: &[u8]) -> String {
    if let Some(base) = local_path.file_name().and_then(|n| n.to_str()) {
        if is_android_cover_filename(base) {
            return base.to_owned();
        }
        if is_content_cover_filename(base) {
            let candidate = format!("{PLAYLIST_COVER_PREFIX}{base}");
            if is_android_cover_filename(&candidate) {
                return candidate;
            }
        }
    }
    let hash = sha256_hex_prefix(bytes);
    let ext = detect_safe_ext(local_path, bytes);
    format!("{PLAYLIST_COVER_PREFIX}c_{hash}.{ext}")
}

/// Builds a ZIP-ready `BackupBundle` containing `native-collections.json` (with rewritten
/// Android-compatible `covers/native_pl_*` paths) and deduplicated playlist cover bytes.
pub fn build_collections_backup_bundle(
    collections: &CollectionsData,
) -> Result<BackupBundle, PlaylistIoError> {
    let mut exported = collections.clone();
    let mut seen_names = HashSet::new();
    let mut playlist_covers = Vec::new();
    let mut total_cover_bytes = 0usize;
    let cover_dir = CoverPaths::cover_dir();
    let canon_cover_dir = fs::canonicalize(&cover_dir).ok();

    for playlist in &mut exported.playlists {
        let Some(local) = CoverPaths::to_local_path(playlist.cover_art.as_deref()) else {
            playlist.cover_art = None;
            continue;
        };
        if !CoverPaths::is_managed_cover_file(&local) {
            playlist.cover_art = None;
            continue;
        }
        if let Some(ref dir) = canon_cover_dir {
            let Ok(canon) = fs::canonicalize(&local) else {
                playlist.cover_art = None;
                continue;
            };
            if !canon.starts_with(dir) {
                playlist.cover_art = None;
                continue;
            }
        }
        let Ok(meta) = fs::metadata(&local) else {
            playlist.cover_art = None;
            continue;
        };
        if !meta.is_file() || meta.len() > MAX_BACKUP_COVER_BYTES as u64 {
            playlist.cover_art = None;
            continue;
        }
        let Ok(bytes) = fs::read(&local) else {
            playlist.cover_art = None;
            continue;
        };
        if bytes.len() > MAX_BACKUP_COVER_BYTES || !ImageLimits::is_sane_bytes(&bytes) {
            playlist.cover_art = None;
            continue;
        }
        let export_name = android_export_cover_name(&local, &bytes);
        if seen_names.contains(&export_name) {
            playlist.cover_art = Some(format!("covers/{export_name}"));
            continue;
        }
        if playlist_covers.len() >= MAX_BACKUP_COVER_COUNT
            || total_cover_bytes.saturating_add(bytes.len()) > MAX_BACKUP_COVERS_TOTAL_BYTES
        {
            playlist.cover_art = None;
            continue;
        }
        total_cover_bytes += bytes.len();
        seen_names.insert(export_name.clone());
        playlist.cover_art = Some(format!("covers/{export_name}"));
        playlist_covers.push(BackupCoverFile {
            name: export_name,
            bytes,
        });
    }

    let collections_json = serde_json::to_string_pretty(&exported).map_err(|e| {
        PlaylistIoError::InvalidJson {
            file: FILE_COLLECTIONS,
            message: e.to_string(),
        }
    })?;
    if collections_json.len() > MAX_BACKUP_JSON_BYTES {
        return Err(PlaylistIoError::TooLarge {
            what: FILE_COLLECTIONS,
            limit: "16 MiB".to_owned(),
        });
    }

    Ok(BackupBundle {
        collections_json: Some(collections_json),
        stats_json: None,
        settings_json: None,
        playlist_covers,
    })
}

pub fn build_single_playlist_backup_bundle(
    playlist: &PlaylistData,
) -> Result<BackupBundle, PlaylistIoError> {
    let wrapper = CollectionsData {
        liked: Vec::new(),
        playlists: vec![playlist.clone()],
        smart: None,
        imported_from: None,
        extra: Map::new(),
    };
    build_collections_backup_bundle(&wrapper)
}

pub fn write_backup_zip<W: Write>(
    writer: W,
    bundle: &BackupBundle,
) -> Result<W, PlaylistIoError> {
    let mut zip = ZipWriter::new(writer);
    if let Some(c) = &bundle.collections_json {
        zip.add_deflated(FILE_COLLECTIONS, c.as_bytes())?;
    }
    if let Some(s) = &bundle.stats_json {
        zip.add_deflated(FILE_STATS, s.as_bytes())?;
    }
    if let Some(st) = &bundle.settings_json {
        zip.add_deflated(FILE_SETTINGS, st.as_bytes())?;
    }
    let mut seen_covers = HashSet::new();
    for cover in &bundle.playlist_covers {
        if !is_android_cover_filename(&cover.name) {
            return Err(PlaylistIoError::InvalidCover {
                name: cover.name.clone(),
                reason: "cover name is not Android-compatible (expected native_pl_*)",
            });
        }
        if seen_covers.insert(cover.name.clone()) {
            zip.add_stored(&format!("covers/{}", cover.name), &cover.bytes)?;
        }
    }
    Ok(zip.finish()?)
}

/// Writes a full collections archive (`.zip`) directly to `dest`. Returns number of covers written.
pub fn export_collections_zip_file(
    dest: &Path,
    collections: &CollectionsData,
) -> Result<usize, PlaylistIoError> {
    let bundle = build_collections_backup_bundle(collections)?;
    let cover_count = bundle.playlist_covers.len();
    write_file_direct_or_cleanup(dest, |file| {
        let mut f = write_backup_zip(file, &bundle)?;
        f.flush()?;
        Ok(())
    })?;
    Ok(cover_count)
}

/// Writes a single custom playlist archive (`.zip`) directly to `dest`. Returns number of covers written.
pub fn export_playlist_zip_file(
    dest: &Path,
    playlist: &PlaylistData,
) -> Result<usize, PlaylistIoError> {
    let bundle = build_single_playlist_backup_bundle(playlist)?;
    let cover_count = bundle.playlist_covers.len();
    write_file_direct_or_cleanup(dest, |file| {
        let mut f = write_backup_zip(file, &bundle)?;
        f.flush()?;
        Ok(())
    })?;
    Ok(cover_count)
}

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

/// Reads and validates a Retrograde `.zip` archive using the same size/entry/zip-bomb rules
/// as Android's `BackupArchive.kt`, plus accepting referenced desktop `covers/c_<24-hex>.*` files.
pub fn read_backup_zip<R: Read + Seek>(reader: R) -> Result<BackupBundle, PlaylistIoError> {
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
        let remaining = MAX_ARCHIVE_TOTAL_BYTES.saturating_sub(total_bytes);
        let max_read = MAX_BACKUP_JSON_BYTES.min(remaining);
        let bytes = zip.read_entry_bounded(
            &entry,
            max_read,
            ARCHIVE_IGNORED_RATIO_GRACE_BYTES,
            MAX_ARCHIVE_IGNORED_RATIO,
        )?;
        total_bytes += bytes.len();
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

    // Only accept `c_<24-hex>.*` files that are referenced by `native-collections.json`, so an
    // archive containing a desktop `~/.rustygrade/covers` album cache does not blow past 500 entries.
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

    let mut playlist_covers = Vec::new();
    let mut seen_covers = HashSet::new();
    let mut ignored_bytes = 0u64;

    for (entry, name, is_android) in cover_entries {
        let lower_name = name.to_ascii_lowercase();
        if !is_android && !referenced_covers.contains(&lower_name) {
            zip.drain_ignored_entry(
                &entry,
                &mut ignored_bytes,
                MAX_ARCHIVE_IGNORED_BYTES,
                ARCHIVE_IGNORED_RATIO_GRACE_BYTES,
                MAX_ARCHIVE_IGNORED_RATIO,
            )?;
            continue;
        }
        if !seen_covers.insert(lower_name) {
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
        let remaining = MAX_ARCHIVE_TOTAL_BYTES.saturating_sub(total_bytes);
        let max_read = MAX_BACKUP_COVER_BYTES.min(remaining);
        let bytes = zip.read_entry_bounded(
            &entry,
            max_read,
            ARCHIVE_IGNORED_RATIO_GRACE_BYTES,
            MAX_ARCHIVE_IGNORED_RATIO,
        )?;
        total_bytes += bytes.len();
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

    Ok(BackupBundle {
        collections_json,
        stats_json,
        settings_json,
        playlist_covers,
    })
}

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

/// Reads and validates a `.zip` backup archive just enough to summarize its contents for UI
/// confirmation or single-playlist protection.
pub fn inspect_backup_zip<R: Read + Seek>(reader: R) -> Result<ZipArchiveSummary, PlaylistIoError> {
    let bundle = read_backup_zip(reader)?;
    let has_stats = bundle.stats_json.is_some();
    let has_settings = bundle.settings_json.is_some();

    let mut playlists = 0usize;
    let mut liked = 0usize;
    let mut smart = 0usize;
    let mut single_pl_shape = false;

    let has_collections = if let Some(raw) = &bundle.collections_json {
        if raw.len() > MAX_BACKUP_JSON_BYTES {
            return Err(PlaylistIoError::TooLarge {
                what: FILE_COLLECTIONS,
                limit: "16 MiB".to_owned(),
            });
        }
        let col: CollectionsData =
            serde_json::from_str(raw).map_err(|e| PlaylistIoError::InvalidJson {
                file: FILE_COLLECTIONS,
                message: e.to_string(),
            })?;
        playlists = col.playlists.len();
        liked = col.liked.len();
        smart = col.smart.as_ref().map_or(0, Vec::len);
        single_pl_shape =
            !has_stats && !has_settings && playlists == 1 && liked == 0 && col.smart.as_ref().is_none_or(Vec::is_empty);
        true
    } else {
        false
    };

    let stats_tracks = if let Some(raw) = &bundle.stats_json {
        if raw.len() > MAX_BACKUP_JSON_BYTES {
            return Err(PlaylistIoError::TooLarge {
                what: FILE_STATS,
                limit: "16 MiB".to_owned(),
            });
        }
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

fn find_existing_managed_cover_on_disk(base: &str) -> Option<PathBuf> {
    let cover_dir = CoverPaths::cover_dir();
    let direct = cover_dir.join(base);
    if direct.is_file() && CoverPaths::is_managed_cover_file(&direct) {
        return Some(direct);
    }
    if let Some(stripped) = base.strip_prefix(PLAYLIST_COVER_PREFIX)
        && is_content_cover_filename(stripped)
    {
        let content_path = cover_dir.join(stripped);
        if content_path.is_file() && CoverPaths::is_managed_cover_file(&content_path) {
            return Some(content_path);
        }
    }
    None
}

/// Validates JSON payloads and covers in `bundle`, and remaps collection and stats track IDs
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

    // Only ingest cover entries actually referenced by imported playlists (ignoring sibling `_thumb.jpg`
    // files exported by Android when only `_full.jpg` is referenced by `playlist.coverArt`).
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
        // Phase 2a: Decode/normalize/hash covers BEFORE acquiring LibraryStore::write_lock().
        let mut prepared_covers = Vec::with_capacity(self.referenced_covers.len());
        for cover in self.referenced_covers {
            // One undecodable cover must not sink the whole import; that playlist just
            // ends up without art.
            let Ok(prepared) = prepare_cover_bytes(&cover.bytes) else {
                continue;
            };
            prepared_covers.push((cover.name, prepared));
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
                // Pending first: a concurrent live-set rebuild unions pending, so the cover is
                // never momentarily unprotected.
                if let Some(pending) = pending_refs
                    && let Ok(mut guard) = pending.lock()
                {
                    guard.insert(saved.path.clone());
                }
                if let Some(refs) = live_refs
                    && let Ok(mut guard) = refs.lock()
                {
                    guard.insert(saved.path.clone());
                }
                if seen_saved_paths.insert(saved.path.clone()) {
                    restored_cover_paths.push(saved.path.clone());
                }
                let url = CoverPaths::to_file_url(&saved.path);
                cover_urls.insert(name.to_ascii_lowercase(), url);
                covers_restored += 1;
            }

            // Recover referenced covers already present on disk if omitted from the archive.
            for base in needed_basenames {
                let key = base.to_ascii_lowercase();
                if cover_urls.contains_key(&key) {
                    continue;
                }
                if let Some(existing_path) = find_existing_managed_cover_on_disk(&base) {
                    if let Some(pending) = pending_refs
                        && let Ok(mut guard) = pending.lock()
                    {
                        guard.insert(existing_path.clone());
                    }
                    if let Some(refs) = live_refs
                        && let Ok(mut guard) = refs.lock()
                    {
                        guard.insert(existing_path.clone());
                    }
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
            let mapped_playlists: Vec<PlaylistData> = col
                .playlists
                .into_iter()
                .map(|mut p| {
                    p.data.cover_art = p
                        .raw_cover_basename
                        .as_deref()
                        .and_then(|base| self.cover_urls.get(&base.to_ascii_lowercase()).cloned());
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
                    merge_stat_entry_indexed(&mut current_stats.tracks, &mut ci_index, dst_id, stat);
                }
            }
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

/// Allocates a free positive playlist ID starting at `wanted.max(1)` without colliding with `used`.
pub fn claim_next_id(used: &mut HashSet<i64>, wanted: i64) -> i64 {
    let mut id = wanted.max(1);
    while !used.insert(id) {
        id = id.checked_add(1).unwrap_or(1);
    }
    id
}

fn dedupe_preserving_order(iter: impl Iterator<Item = String>) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for item in iter {
        if seen.insert(item.clone()) {
            out.push(item);
        }
    }
    out
}

fn playlist_names_compatible(existing_name: &str, imported_name: &str) -> bool {
    let ex = existing_name.trim();
    let imp = imported_name.trim();
    ex.is_empty() || ex.eq_ignore_ascii_case(imp)
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
            *ex = imp;
            continue;
        }
        imp.id = claim_next_id(&mut used_ids, imp.id);
        existing.push(imp);
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use image::{DynamicImage, ImageFormat, Rgb, RgbImage};
    use regex::Regex;

    use super::*;
    use crate::collections::SmartRule;

    fn mk_track_full(
        id: &str,
        title: &str,
        artist: &str,
        album: &str,
        duration: f64,
        file_path: &str,
    ) -> Track {
        Track {
            id: id.to_owned(),
            title: title.to_owned(),
            artist: artist.to_owned(),
            album: album.to_owned(),
            duration,
            file_path: file_path.to_owned(),
            track_number: 1,
            disc_number: 1,
            codec: "FLAC".to_owned(),
            sample_rate: 44100,
            bits_per_sample: 16,
            channels: 2,
            cover_thumb_path: None,
            cover_full_path: None,
        }
    }

    fn mk_album(name: &str, artist: &str, tracks: Vec<Track>) -> Album {
        Album {
            name: name.to_owned(),
            artist: artist.to_owned(),
            cover_thumb_path: None,
            cover_full_path: None,
            year: 2024,
            added_at: 1,
            tracks,
        }
    }

    fn make_png_bytes(rgb: [u8; 3]) -> Vec<u8> {
        let mut img = RgbImage::new(64, 64);
        for px in img.pixels_mut() {
            *px = Rgb(rgb);
        }
        let mut cur = Cursor::new(Vec::new());
        DynamicImage::ImageRgb8(img)
            .write_to(&mut cur, ImageFormat::Png)
            .unwrap();
        cur.into_inner()
    }

    fn fresh_temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "retro-plio-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn m3u8_export_and_import_roundtrip_strips_newlines_and_handles_missing_artist() {
        let tracks = vec![
            mk_track_full(
                "/home/okarin/Music/Ado/Kyogen/01.flac",
                "Usseewa\n",
                "Ado\r\n",
                "Kyogen\n",
                206.4,
                "/home/okarin/Music/Ado/Kyogen/01.flac",
            ),
            mk_track_full(
                "/home/okarin/Music/NoArtist/02.flac",
                "Ambient Intro",
                "",
                "",
                0.0,
                "/home/okarin/Music/NoArtist/02.flac",
            ),
        ];

        let m3u = format_m3u8("My\r\nMix", &tracks);
        assert!(!m3u.contains('\r'));
        assert!(m3u.starts_with("#EXTM3U\n#PLAYLIST:MyMix\n"));
        assert!(m3u.contains("#EXTINF:206,Ado - Usseewa\n#EXTALB:Kyogen\n/home/okarin/Music/Ado/Kyogen/01.flac\n"));
        assert!(m3u.contains("#EXTINF:-1,Ambient Intro\n/home/okarin/Music/NoArtist/02.flac\n"));

        let clean_albums = vec![mk_album(
            "Kyogen",
            "Ado",
            vec![
                mk_track_full(
                    "/home/okarin/Music/Ado/Kyogen/01.flac",
                    "Usseewa",
                    "Ado",
                    "Kyogen",
                    206.4,
                    "/home/okarin/Music/Ado/Kyogen/01.flac",
                ),
                mk_track_full(
                    "/home/okarin/Music/NoArtist/02.flac",
                    "Ambient Intro",
                    "",
                    "",
                    0.0,
                    "/home/okarin/Music/NoArtist/02.flac",
                ),
            ],
        )];
        let res = import_m3u_str(&m3u, None, &clean_albums, &[]).unwrap();
        assert_eq!(res.name, "MyMix");
        assert_eq!(res.exact, 2);
        assert_eq!(res.remapped, 0);
        assert_eq!(res.metadata, 0);
        assert_eq!(res.unmatched, 0);
        assert_eq!(
            res.track_ids,
            vec![
                "/home/okarin/Music/Ado/Kyogen/01.flac".to_owned(),
                "/home/okarin/Music/NoArtist/02.flac".to_owned(),
            ]
        );
    }

    #[test]
    fn m3u_import_handles_bom_crlf_file_uri_relative_android_windows_disc_guard_and_metadata() {
        let dir = fresh_temp_dir("m3u-tiers");
        let albums = vec![
            mk_album(
                "Alpha",
                "Artist A",
                vec![
                    mk_track_full(
                        "/home/okarin/Music/Rock/Alpha/01 Track.flac",
                        "Track One",
                        "Artist A",
                        "Alpha",
                        200.0,
                        "/home/okarin/Music/Rock/Alpha/01 Track.flac",
                    ),
                    mk_track_full(
                        "/home/okarin/Music/Jazz/Beta/02.flac",
                        "Blue Note",
                        "Artist B",
                        "Beta",
                        180.0,
                        "/home/okarin/Music/Jazz/Beta/02.flac",
                    ),
                    mk_track_full(
                        "/home/okarin/Music/Re:Zero/01.flac",
                        "Styx Helix",
                        "MYTH & ROID",
                        "Re:Zero",
                        260.0,
                        "/home/okarin/Music/Re:Zero/01.flac",
                    ),
                    mk_track_full(
                        "/home/okarin/Music/AlbumW/CD1/01.flac",
                        "Disc Track",
                        "Artist W",
                        "AlbumW",
                        150.0,
                        "/home/okarin/Music/AlbumW/CD1/01.flac",
                    ),
                    mk_track_full(
                        "/home/okarin/Music/Singles/flat_unique.flac",
                        "Flat Unique",
                        "Solo",
                        "Singles",
                        140.0,
                        "/home/okarin/Music/Singles/flat_unique.flac",
                    ),
                    // Same artist + title in two albums and durations for metadata fallback tests:
                    mk_track_full(
                        "/home/okarin/Music/Live/Echo.flac",
                        "Echo",
                        "Band",
                        "Live 2024",
                        300.0,
                        "/home/okarin/Music/Live/Echo.flac",
                    ),
                    mk_track_full(
                        "/home/okarin/Music/Studio/Echo.flac",
                        "Echo",
                        "Band",
                        "Studio",
                        190.0,
                        "/home/okarin/Music/Studio/Echo.flac",
                    ),
                ],
            ),
        ];

        let playlist_path = dir.join("Sub").join("My Export.m3u");
        let m3u = concat!(
            "\u{FEFF}#EXTM3U\r\n",
            "#UNKNOWN_DIRECTIVE:foo\r\n",
            "\r\n",
            // Tier 1: file:// URI with percent-encoded space -> Exact
            "file:///home/okarin/Music/Rock/Alpha/01%20Track.flac\r\n",
            // Tier 2: Android SAF path -> Remapped
            "/saf/primary:Music/Jazz/Beta/02.flac\r\n",
            // Tier 2: Android /storage path -> Remapped
            "/storage/emulated/0/Music/Re:Zero/01.flac\r\n",
            // Tier 2: Windows backslash path under source_roots -> Remapped
            "D:\\PhoneMusic\\flat_unique.flac\r\n",
            // Tier 2: Generic disc folder guard (AlbumY/CD1/01.flac must NOT match AlbumW/CD1/01.flac)
            "/storage/emulated/0/Music/AlbumY/CD1/01.flac\r\n",
            // Tier 2: Generic disc folder guard also rejects Disc 1 when parent album differs
            "/storage/emulated/0/Music/AlbumY/Disc 1/01.flac\r\n",
            // Tier 2: Generic disc folder WITH matching album parent -> Remapped
            "D:\\Backup\\AlbumW\\CD1\\01.flac\r\n",
            // Tier 3: Metadata with #EXTALB -> unique match on (artist, title, album)
            "#EXTINF:190,  Band  -  echo \r\n",
            "#EXTALB:  studio \r\n",
            "/android/moved/somewhere/unknown1.flac\r\n",
            // Tier 3: Metadata without #EXTALB, tie-broken by duration (±3s)
            "#EXTINF:302,Band - Echo\r\n",
            "/android/moved/somewhere/unknown2.flac\r\n",
            // Tier 3: Ambiguous metadata (no album, no duration) -> Unmatched
            "#EXTINF:-1,Band - Echo\r\n",
            "/android/moved/somewhere/unknown3.flac\r\n",
            // Tier 3: #EXTALB mismatch must NOT fall back across albums -> Unmatched
            "#EXTINF:190,Band - Echo\r\n",
            "#EXTALB:Wrong Album\r\n",
            "/android/moved/somewhere/unknown4.flac\r\n",
            // Duplicate of first track -> deduplicated in output list
            "/home/okarin/Music/Rock/Alpha/01 Track.flac\r\n",
        );

        let res = import_m3u_str(
            m3u,
            Some(&playlist_path),
            &albums,
            &["D:\\PhoneMusic".to_owned()],
        )
        .unwrap();

        // Fallback to filename stem when #PLAYLIST is missing.
        assert_eq!(res.name, "My Export");
        assert_eq!(res.exact, 1); // file:// URI (duplicate counted once)
        assert_eq!(res.remapped, 4); // SAF, /storage/Re:Zero, source_root flat_unique, AlbumW/CD1
        assert_eq!(res.metadata, 2); // #EXTALB Studio + duration 302 -> Live 2024
        assert_eq!(res.unmatched, 4); // AlbumY/CD1, AlbumY/Disc 1, ambiguous no-dur Echo, wrong #EXTALB Echo
        assert_eq!(res.total_entries, 12);
        assert_eq!(
            res.exact + res.remapped + res.metadata,
            res.track_ids.len()
        );
        assert_eq!(
            res.track_ids,
            vec![
                "/home/okarin/Music/Rock/Alpha/01 Track.flac".to_owned(),
                "/home/okarin/Music/Jazz/Beta/02.flac".to_owned(),
                "/home/okarin/Music/Re:Zero/01.flac".to_owned(),
                "/home/okarin/Music/Singles/flat_unique.flac".to_owned(),
                "/home/okarin/Music/AlbumW/CD1/01.flac".to_owned(),
                "/home/okarin/Music/Studio/Echo.flac".to_owned(),
                "/home/okarin/Music/Live/Echo.flac".to_owned(),
            ]
        );

        // Relative path resolution against playlist parent directory.
        let rel_m3u_path = PathBuf::from("/home/okarin/Music/Playlists/fav.m3u8");
        let rel_res = import_m3u_str(
            "#EXTM3U\n../Rock/Alpha/01 Track.flac\n",
            Some(&rel_m3u_path),
            &albums,
            &[],
        )
        .unwrap();
        assert_eq!(rel_res.exact, 1);
        assert_eq!(
            rel_res.track_ids,
            vec!["/home/okarin/Music/Rock/Alpha/01 Track.flac".to_owned()]
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn m3u_import_rejects_empty_zero_matches_and_oversized_files() {
        assert!(matches!(
            import_m3u_str("#EXTM3U\n#PLAYLIST:Empty\n", None, &[], &[]),
            Err(PlaylistIoError::EmptyPlaylist)
        ));
        assert!(matches!(
            import_m3u_str("#EXTM3U\n/does/not/exist/01.flac\n", None, &[], &[]),
            Err(PlaylistIoError::NoMatchingTracks { total: 1 })
        ));
        let oversized = "a".repeat(MAX_M3U_BYTES + 1);
        assert!(matches!(
            import_m3u_str(&oversized, None, &[], &[]),
            Err(PlaylistIoError::TooLarge { .. })
        ));
    }

    #[test]
    fn zip_export_uses_android_compatible_cover_names_and_roundtrips_with_c_store() {
        let dir = fresh_temp_dir("zip-roundtrip");
        crate::paths::with_test_app_dir(&dir, || {
            let png = make_png_bytes([30, 190, 150]);
            let saved_cover = save_cover_bytes(&png).unwrap();
            let cover_name = saved_cover.file_name().unwrap().to_str().unwrap().to_owned();
            assert!(cover_name.starts_with("c_"));

            let albums = vec![mk_album(
                "Kyogen",
                "Ado",
                vec![mk_track_full(
                    "/home/okarin/Music/Ado/Kyogen/01.flac",
                    "Usseewa",
                    "Ado",
                    "Kyogen",
                    206.0,
                    "/home/okarin/Music/Ado/Kyogen/01.flac",
                )],
            )];

            let mut col = CollectionsData {
                liked: vec!["/home/okarin/Music/Ado/Kyogen/01.flac".to_owned()],
                playlists: vec![PlaylistData {
                    id: 1,
                    name: "Desktop Mix".to_owned(),
                    cover_art: Some(CoverPaths::to_file_url(&saved_cover)),
                    track_ids: vec!["/home/okarin/Music/Ado/Kyogen/01.flac".to_owned()],
                    extra: Map::new(),
                }],
                smart: Some(vec![SmartPlaylistData {
                    id: 1,
                    name: "High Plays".to_owned(),
                    match_all: true,
                    rules: vec![SmartRule {
                        field: "plays".to_owned(),
                        op: "gte".to_owned(),
                        value: "1".to_owned(),
                    }],
                    sort: "album".to_owned(),
                    limit: 0,
                    extra: Map::new(),
                }]),
                imported_from: None,
                extra: Map::new(),
            };

            let zip_path = dir.join("export.zip");
            let exported_covers = export_collections_zip_file(&zip_path, &col).unwrap();
            assert_eq!(exported_covers, 1);

            // Inspect raw ZIP entries to verify Android wire compatibility.
            let mut raw_zip = ZipReader::open(File::open(&zip_path).unwrap(), 100).unwrap();
            let entries = raw_zip.entries().to_vec();
            let android_re =
                Regex::new(r"(?i)^native_pl_[A-Za-z0-9_.-]{1,120}\.(jpg|jpeg|png|webp)$").unwrap();
            let expected_zip_cover_name = format!("native_pl_{cover_name}");
            assert!(android_re.is_match(&expected_zip_cover_name));
            assert!(entries.iter().any(|e| {
                e.raw_name == format!("covers/{expected_zip_cover_name}")
            }));

            let col_entry = entries
                .iter()
                .find(|e| e.raw_name == "native-collections.json")
                .unwrap()
                .clone();
            let col_bytes = raw_zip
                .read_entry_bounded(&col_entry, MAX_BACKUP_JSON_BYTES, u64::MAX, 100)
                .unwrap();
            let parsed_exported: CollectionsData = serde_json::from_slice(&col_bytes).unwrap();
            assert_eq!(
                parsed_exported.playlists[0].cover_art.as_deref(),
                Some(format!("covers/{expected_zip_cover_name}").as_str())
            );

            // Remove original cover on disk, import ZIP in Replace mode into clean collections, and verify
            // content-addressed `c_*` store restoration and `live_playlist_covers` update.
            fs::remove_file(&saved_cover).unwrap();
            assert!(!saved_cover.exists());

            let bundle = read_backup_zip(File::open(&zip_path).unwrap()).unwrap();
            let prepared = prepare_zip_import(bundle, &albums, &[]).unwrap();
            let live_refs = Mutex::new(HashSet::new());
            let ingested = prepared.ingest_covers(Some(&live_refs), None).unwrap();
            assert_eq!(ingested.restored_cover_paths, vec![saved_cover.clone()]);
            assert!(live_refs.lock().unwrap().contains(&saved_cover));

            let (report, old_covers, _) =
                ingested.apply(&mut col, &mut PlayStats::default(), true);
            assert_eq!(report.covers_restored, 1);
            assert_eq!(report.tracks_exact, 1);
            assert_eq!(old_covers.len(), 1);
            assert!(saved_cover.exists());
            assert_eq!(
                col.playlists[0].cover_art.as_deref(),
                Some(CoverPaths::to_file_url(&saved_cover).as_str())
            );
        });
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn zip_import_accepts_android_and_c_covers_skips_album_cache_and_guards_merge_ids() {
        let dir = fresh_temp_dir("zip-compat-merge");
        crate::paths::with_test_app_dir(&dir, || {
            let png_a = make_png_bytes([255, 80, 40]);
            let png_b = make_png_bytes([40, 120, 255]);
            let hash_b = sha256_hex_prefix(&png_b);
            let c_cover_name = format!("c_{hash_b}.png");

            let albums = vec![mk_album(
                "Kyogen",
                "Ado",
                vec![mk_track_full(
                    "/home/okarin/Music/Ado/Kyogen/01.flac",
                    "Usseewa",
                    "Ado",
                    "Kyogen",
                    206.0,
                    "/home/okarin/Music/Ado/Kyogen/01.flac",
                )],
            )];

            // Simulate an archive exported from Android / zipped `.retrograde` carrying 600 unreferenced
            // album covers, an Android `native_pl_*` cover + unused `_thumb`, and a referenced `c_*` cover.
            let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
            let json = format!(
                r#"{{
                    "liked": ["/saf/primary:Music/Ado/Kyogen/01.flac"],
                    "playlists": [
                        {{
                            "id": 1,
                            "name": "Android Playlist",
                            "coverArt": "file:///data/user/0/dev.okarin.retrograde/files/covers/native_pl_1_99_full.png",
                            "trackIds": ["/storage/emulated/0/Music/Ado/Kyogen/01.flac"]
                        }},
                        {{
                            "id": 2,
                            "name": "Desktop Match",
                            "coverArt": "covers/{c_cover_name}",
                            "trackIds": ["/home/okarin/Music/Ado/Kyogen/01.flac"]
                        }}
                    ]
                }}"#
            );
            writer
                .add_deflated("native-collections.json", json.as_bytes())
                .unwrap();
            writer
                .add_stored("covers/native_pl_1_99_full.png", &png_a)
                .unwrap();
            // Android sibling thumb not referenced directly by playlist.coverArt -> ignored during ingest.
            writer
                .add_stored("covers/native_pl_1_99_thumb.png", &png_b)
                .unwrap();
            writer
                .add_stored(&format!("covers/{c_cover_name}"), &png_b)
                .unwrap();
            // 600 unreferenced album-cache covers (exceeds 500 if not ignored as unreferenced).
            for i in 0..600 {
                writer
                    .add_stored(&format!("covers/c_{i:024x}.png"), b"not-even-read")
                    .unwrap();
            }
            let zip_bytes = writer.finish().unwrap().into_inner();

            let bundle = read_backup_zip(Cursor::new(zip_bytes)).unwrap();
            // 3 accepted cover files (`native_pl_..._full`, `native_pl_..._thumb`, referenced `c_*`).
            assert_eq!(bundle.playlist_covers.len(), 3);

            let prepared = prepare_zip_import(bundle, &albums, &[]).unwrap();
            let ingested = prepared.ingest_covers(None, None).unwrap();
            // Only the 2 referenced covers were actually written into Desktop's store.
            assert_eq!(ingested.covers_restored, 2);

            // Existing Desktop has id=1 with a DIFFERENT name ("Local Favorites") and id=2 with the SAME
            // name ("desktop match" case-insensitive). Merge must NOT overwrite id=1!
            let mut local = CollectionsData {
                playlists: vec![
                    PlaylistData {
                        id: 1,
                        name: "Local Favorites".to_owned(),
                        cover_art: None,
                        track_ids: vec!["/other/track.flac".to_owned()],
                        extra: Map::new(),
                    },
                    PlaylistData {
                        id: 2,
                        name: "desktop match".to_owned(),
                        cover_art: Some("file:///tmp/old_cover.png".to_owned()),
                        track_ids: vec![],
                        extra: Map::new(),
                    },
                ],
                ..Default::default()
            };

            let (report, old_covers, _) =
                ingested.apply(&mut local, &mut PlayStats::default(), false);
            assert_eq!(report.playlists_imported, 2);
            assert_eq!(report.playlists_total, 3);
            assert_eq!(old_covers, vec!["file:///tmp/old_cover.png".to_owned()]);
            assert_eq!(local.playlists[0].name, "Local Favorites");
            assert_eq!(local.playlists[0].track_ids, vec!["/other/track.flac".to_owned()]);
            assert_eq!(local.playlists[1].name, "desktop match");
            assert_eq!(
                local.playlists[1].track_ids,
                vec!["/home/okarin/Music/Ado/Kyogen/01.flac".to_owned()]
            );
            assert_eq!(local.playlists[2].name, "Android Playlist");
            assert_ne!(local.playlists[2].id, 1);
            assert_ne!(local.playlists[2].id, 2);
        });
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn zip_reader_rejects_traversal_oversized_covers_and_too_many_accepted_covers() {
        // Traversal
        let mut w_trav = ZipWriter::new(Cursor::new(Vec::new()));
        w_trav.add_stored("../../etc/passwd", b"x").unwrap();
        let trav_zip = w_trav.finish().unwrap().into_inner();
        assert!(matches!(
            read_backup_zip(Cursor::new(trav_zip)),
            Err(PlaylistIoError::Zip(ZipError::PathTraversal(_)))
        ));

        // Oversized cover (>10 MiB)
        let mut w_big = ZipWriter::new(Cursor::new(Vec::new()));
        w_big
            .add_deflated("native-collections.json", br#"{"liked":[]}"#)
            .unwrap();
        w_big
            .add_stored("covers/native_pl_1_big.jpg", &vec![0u8; MAX_BACKUP_COVER_BYTES + 1])
            .unwrap();
        let big_zip = w_big.finish().unwrap().into_inner();
        assert!(matches!(
            read_backup_zip(Cursor::new(big_zip)),
            Err(PlaylistIoError::Zip(ZipError::EntryTooLarge { .. }))
        ));

        // Exceeding MAX_ARCHIVE_ACCEPTED_ENTRIES with 500 `native_pl_*` covers + 1 JSON = 501
        let mut w_many = ZipWriter::new(Cursor::new(Vec::new()));
        w_many
            .add_deflated("native-collections.json", br#"{"liked":[]}"#)
            .unwrap();
        for i in 0..MAX_ARCHIVE_ACCEPTED_ENTRIES {
            w_many
                .add_stored(&format!("covers/native_pl_{i}.jpg"), b"1")
                .unwrap();
        }
        let many_zip = w_many.finish().unwrap().into_inner();
        assert!(matches!(
            read_backup_zip(Cursor::new(many_zip)),
            Err(PlaylistIoError::TooLarge { .. })
        ));
    }

    #[test]
    fn non_ascii_relative_paths_do_not_panic() {
        for raw in ["Кино/01.flac", "音楽/01.flac", "音楽カバー", "ééééééé", "ééé/x"] {
            assert!(!is_http_url(raw));
            assert!(decode_file_uri(raw).is_none());
            assert!(!is_absolute_or_drive(raw));
            assert!(CoverPaths::to_local_path(Some(raw)).is_some());
        }
        assert_eq!(decode_file_uri("file:///a/%E9%9F%B3.flac").as_deref(), Some("/a/音.flac"));
    }

    #[test]
    fn cover_cap_leaves_room_for_collections_json() {
        const { assert!(MAX_BACKUP_COVER_COUNT < MAX_ARCHIVE_ACCEPTED_ENTRIES) };
    }

    #[test]
    fn single_playlist_zip_export_leaves_liked_and_smart_empty_for_safe_merge() {
        let dir = fresh_temp_dir("single-pl-zip");
        crate::paths::with_test_app_dir(&dir, || {
            let png = make_png_bytes([10, 20, 30]);
            let saved = save_cover_bytes(&png).unwrap();
            let pl = PlaylistData {
                id: 5,
                name: "Night Drive".to_owned(),
                cover_art: Some(CoverPaths::to_file_url(&saved)),
                track_ids: vec!["/music/a.flac".to_owned()],
                extra: Map::new(),
            };
            let zip_path = dir.join("night_drive.zip");
            assert_eq!(export_playlist_zip_file(&zip_path, &pl).unwrap(), 1);

            let bundle = read_backup_zip(File::open(&zip_path).unwrap()).unwrap();
            let col: CollectionsData =
                serde_json::from_str(bundle.collections_json.as_deref().unwrap()).unwrap();
            assert!(col.liked.is_empty());
            assert!(col.smart.is_none());
            assert_eq!(col.playlists.len(), 1);
            assert_eq!(col.playlists[0].name, "Night Drive");
            assert!(
                col.playlists[0]
                    .cover_art
                    .as_deref()
                    .is_some_and(|c| c.starts_with("covers/native_pl_c_"))
            );
            let summary = inspect_backup_zip(File::open(&zip_path).unwrap()).unwrap();
            assert!(summary.is_single_playlist_export);
            assert_eq!(summary.playlists, 1);
            assert_eq!(summary.liked, 0);
            assert_eq!(summary.smart, 0);
        });
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn zip_archive_rules_macosx_shallowest_backup_prefix_dupes_and_exact_caps() {
        let png = make_png_bytes([12, 34, 56]);
        let mut w = ZipWriter::new(Cursor::new(Vec::new()));
        // Ignored AppleDouble / __MACOSX junk must not shadow real entries or blow limits.
        w.add_stored("__MACOSX/._native-collections.json", b"applejunk")
            .unwrap();
        // Deeper collections should lose to shallower root `native-collections.json`.
        w.add_deflated("Backup/native-collections.json", br#"{"liked":["/deep.flac"]}"#)
            .unwrap();
        w.add_deflated(
            "native-collections.json",
            br#"{"liked":["/shallow.flac"],"playlists":[{"id":1,"name":"Deep","coverArt":"covers/native_pl_1_deep.png","trackIds":[]}]}"#,
        )
        .unwrap();
        // Nested Backup/covers/ accepted, duplicate cover drained cleanly.
        w.add_stored("Backup/covers/native_pl_1_deep.png", &png)
            .unwrap();
        w.add_stored("covers/native_pl_1_deep.png", b"duplicate-ignored")
            .unwrap();
        let zip_bytes = w.finish().unwrap().into_inner();
        let bundle = read_backup_zip(Cursor::new(zip_bytes)).unwrap();
        let col: CollectionsData =
            serde_json::from_str(bundle.collections_json.as_deref().unwrap()).unwrap();
        assert_eq!(col.liked, vec!["/shallow.flac".to_owned()]);
        assert_eq!(bundle.playlist_covers.len(), 1);
        assert_eq!(bundle.playlist_covers[0].bytes, png);

        // Exact accepted entry limit: 499 covers + 1 JSON = 500 succeeds; 500 covers + 1 JSON fails.
        let mut w_ok = ZipWriter::new(Cursor::new(Vec::new()));
        w_ok.add_deflated("native-collections.json", br#"{"liked":[]}"#)
            .unwrap();
        for i in 0..(MAX_ARCHIVE_ACCEPTED_ENTRIES - 1) {
            w_ok.add_stored(&format!("covers/native_pl_{i}.jpg"), b"1")
                .unwrap();
        }
        let ok_zip = w_ok.finish().unwrap().into_inner();
        assert_eq!(
            read_backup_zip(Cursor::new(ok_zip)).unwrap().playlist_covers.len(),
            MAX_ARCHIVE_ACCEPTED_ENTRIES - 1
        );

        // Exact 50 MiB total cover limit via prepare_zip_import: valid PNG header padded to 10 MiB.
        let mut padded_10m = png;
        padded_10m.resize(MAX_BACKUP_COVER_BYTES, 0);
        assert!(ImageLimits::is_sane_bytes(&padded_10m));
        let mut bundle_50m = BackupBundle {
            collections_json: Some(r#"{"liked":[]}"#.to_owned()),
            stats_json: None,
            settings_json: None,
            playlist_covers: (0..5)
                .map(|i| BackupCoverFile {
                    name: format!("native_pl_{i}.png"),
                    bytes: padded_10m.clone(),
                })
                .collect(),
        };
        assert!(prepare_zip_import(bundle_50m.clone(), &[], &[]).is_ok());
        bundle_50m.playlist_covers.push(BackupCoverFile {
            name: "native_pl_overflow.png".to_owned(),
            bytes: make_png_bytes([1, 2, 3]),
        });
        assert!(matches!(
            prepare_zip_import(bundle_50m, &[], &[]),
            Err(PlaylistIoError::InvalidCover { .. })
        ));
    }

    #[test]
    fn java_zip_output_stream_fixture_roundtrips_collections_stats_source_roots_and_covers() {
        let dir = fresh_temp_dir("java-fixture");
        crate::paths::with_test_app_dir(&dir, || {
            let png = make_png_bytes([90, 140, 220]);
            let albums = vec![mk_album(
                "Kyogen",
                "Ado",
                vec![mk_track_full(
                    "/home/okarin/DesktopMusic/FlatMix/01_unique_ado.flac",
                    "Usseewa",
                    "Ado",
                    "Kyogen",
                    206.0,
                    "/home/okarin/DesktopMusic/FlatMix/01_unique_ado.flac",
                )],
            )];

            // Simulate Java ZipOutputStream bit-3 data-descriptor entries for JSONs + STORED cover.
            let col_json = br#"{
                "liked": ["/saf/primary:CustomSourceRoot/01_unique_ado.flac"],
                "playlists": [{
                    "id": 10,
                    "name": "From Android",
                    "coverArt": "file:///data/user/0/dev.okarin.retrograde/files/covers/native_pl_10_1_full.png",
                    "trackIds": ["/saf/primary:CustomSourceRoot/01_unique_ado.flac"]
                }],
                "smart": [],
                "androidTag": "ok"
            }"#;
            let stats_json = br#"{"Tracks":{"/saf/primary:CustomSourceRoot/01_unique_ado.flac":{"Count":7,"Last":1700000000000}}}"#;
            let settings_json =
                br#"{"AndroidBackupSourceRoots":["/saf/primary:CustomSourceRoot"]}"#;

            // Build bit-3 deflated archive with multiple entries manually matching ZipOutputStream.
            fn build_android_zip(
                entries: &[(&str, &[u8], u16)],
            ) -> Vec<u8> {
                use flate2::Compression;
                use flate2::write::DeflateEncoder;

                let mut out = Vec::new();
                struct CdMeta {
                    name: Vec<u8>,
                    method: u16,
                    flags: u16,
                    crc: u32,
                    comp: u32,
                    uncomp: u32,
                    off: u32,
                }
                let mut cds = Vec::new();
                for &(name, raw, method) in entries {
                    let crc = crc32fast::hash(raw);
                    let payload = if method == 8 {
                        let mut enc = DeflateEncoder::new(Vec::new(), Compression::default());
                        enc.write_all(raw).unwrap();
                        enc.finish().unwrap()
                    } else {
                        raw.to_vec()
                    };
                    let off = out.len() as u32;
                    let flags: u16 = if method == 8 { 0x0808 } else { 0x0800 };
                    let nb = name.as_bytes();
                    out.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
                    out.extend_from_slice(&20u16.to_le_bytes());
                    out.extend_from_slice(&flags.to_le_bytes());
                    out.extend_from_slice(&method.to_le_bytes());
                    out.extend_from_slice(&0u32.to_le_bytes());
                    out.extend_from_slice(&(if method == 8 { 0 } else { crc }).to_le_bytes());
                    out.extend_from_slice(&(if method == 8 { 0 } else { payload.len() as u32 }).to_le_bytes());
                    out.extend_from_slice(&(if method == 8 { 0 } else { raw.len() as u32 }).to_le_bytes());
                    out.extend_from_slice(&(nb.len() as u16).to_le_bytes());
                    out.extend_from_slice(&0u16.to_le_bytes());
                    out.extend_from_slice(nb);
                    out.extend_from_slice(&payload);
                    if method == 8 {
                        out.extend_from_slice(&0x0807_4b50u32.to_le_bytes());
                        out.extend_from_slice(&crc.to_le_bytes());
                        out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
                        out.extend_from_slice(&(raw.len() as u32).to_le_bytes());
                    }
                    cds.push(CdMeta {
                        name: nb.to_vec(),
                        method,
                        flags,
                        crc,
                        comp: payload.len() as u32,
                        uncomp: raw.len() as u32,
                        off,
                    });
                }
                let cd_off = out.len() as u32;
                for c in &cds {
                    out.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
                    out.extend_from_slice(&20u16.to_le_bytes());
                    out.extend_from_slice(&20u16.to_le_bytes());
                    out.extend_from_slice(&c.flags.to_le_bytes());
                    out.extend_from_slice(&c.method.to_le_bytes());
                    out.extend_from_slice(&0u32.to_le_bytes());
                    out.extend_from_slice(&c.crc.to_le_bytes());
                    out.extend_from_slice(&c.comp.to_le_bytes());
                    out.extend_from_slice(&c.uncomp.to_le_bytes());
                    out.extend_from_slice(&(c.name.len() as u16).to_le_bytes());
                    out.extend_from_slice(&[0u8; 12]);
                    out.extend_from_slice(&c.off.to_le_bytes());
                    out.extend_from_slice(&c.name);
                }
                let cd_size = (out.len() as u32) - cd_off;
                let cnt = cds.len() as u16;
                out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
                out.extend_from_slice(&0u32.to_le_bytes());
                out.extend_from_slice(&cnt.to_le_bytes());
                out.extend_from_slice(&cnt.to_le_bytes());
                out.extend_from_slice(&cd_size.to_le_bytes());
                out.extend_from_slice(&cd_off.to_le_bytes());
                out.extend_from_slice(&0u16.to_le_bytes());
                out
            }

            let fixture = build_android_zip(&[
                ("native-collections.json", col_json, 8),
                ("native-stats.json", stats_json, 8),
                ("native-settings.json", settings_json, 8),
                ("covers/native_pl_10_1_full.png", &png, 0),
            ]);

            let summary = inspect_backup_zip(Cursor::new(&fixture)).unwrap();
            assert!(!summary.is_single_playlist_export);
            assert!(summary.has_collections);
            assert!(summary.has_stats);
            assert!(summary.has_settings);
            assert_eq!(summary.stats_tracks, 1);

            let bundle = read_backup_zip(Cursor::new(&fixture)).unwrap();
            let prepared = prepare_zip_import(bundle, &albums, &[]).unwrap();
            let live_refs = Mutex::new(HashSet::new());
            let pending_refs = Mutex::new(HashSet::new());
            let ingested = prepared
                .ingest_covers(Some(&live_refs), Some(&pending_refs))
                .unwrap();

            let mut local_col = CollectionsData::default();
            let mut local_stats = PlayStats::default();
            let (report, _, stats_changed) =
                ingested.apply(&mut local_col, &mut local_stats, false);

            assert!(report.collections_imported);
            assert_eq!(report.stats_imported, 1);
            assert!(report.settings_skipped);
            assert!(stats_changed);
            assert_eq!(
                local_col.liked,
                vec!["/home/okarin/DesktopMusic/FlatMix/01_unique_ado.flac".to_owned()]
            );
            assert_eq!(
                local_stats
                    .get("/home/okarin/DesktopMusic/FlatMix/01_unique_ado.flac")
                    .map(|s| (s.count, s.last)),
                Some((7, 1700000000000))
            );
            assert_eq!(local_col.extra.get("androidTag").and_then(Value::as_str), Some("ok"));

            // Re-export and verify stored cover + cleared bit-3 LFH flags so Java ZipInputStream reads it.
            let reexport_path = dir.join("desktop_reexport.zip");
            export_collections_zip_file(&reexport_path, &local_col).unwrap();
            let raw_reexport = fs::read(&reexport_path).unwrap();
            let r = ZipReader::open(Cursor::new(&raw_reexport), 100).unwrap();
            let cover_e = r
                .entries()
                .iter()
                .find(|e| e.raw_name.starts_with("covers/native_pl_c_"))
                .unwrap();
            assert_eq!(cover_e.method, 0);
            assert_eq!(cover_e.flags & 0x0008, 0);
        });
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn cover_prepare_created_flags_rollback_disk_recovery_and_dir_guard() {
        let dir = fresh_temp_dir("cover-lifecycle");
        crate::paths::with_test_app_dir(&dir, || {
            let png = make_png_bytes([40, 200, 90]);
            // First write: created == true; second write: created == false.
            let prep = prepare_cover_bytes(&png).unwrap();
            let first = write_prepared_cover(&prep).unwrap();
            assert!(first.created);
            let second = write_prepared_cover(&prep).unwrap();
            assert!(!second.created);
            assert_eq!(first.path, second.path);

            // Extension-mismatched bytes (JPEG written into .png cover name) -> written with .jpg!
            let mut jpeg_cur = Cursor::new(Vec::new());
            DynamicImage::ImageRgb8(RgbImage::from_pixel(32, 32, Rgb([250, 10, 10])))
                .write_to(&mut jpeg_cur, ImageFormat::Jpeg)
                .unwrap();
            let jpeg_bytes = jpeg_cur.into_inner();
            let prep_jpg = prepare_cover_bytes(&jpeg_bytes).unwrap();
            let saved_jpg = write_prepared_cover(&prep_jpg).unwrap();
            assert!(saved_jpg.created);
            assert_eq!(saved_jpg.path.extension().and_then(|e| e.to_str()), Some("jpg"));
            assert!(!write_prepared_cover(&prep_jpg).unwrap().created);

            // Oversized image (>720px) re-encodes before write and returns created == true then false.
            let big = DynamicImage::ImageRgb8(RgbImage::from_pixel(900, 900, Rgb([10, 20, 240])));
            let mut big_cur = Cursor::new(Vec::new());
            big.write_to(&mut big_cur, ImageFormat::Png).unwrap();
            let prep_big = prepare_cover_bytes(&big_cur.into_inner()).unwrap();
            let saved_big = write_prepared_cover(&prep_big).unwrap();
            assert!(saved_big.created);
            assert_eq!(saved_big.path.extension().and_then(|e| e.to_str()), Some("jpg"));

            // ingest_covers rollback: batch containing pre-existing cover + corrupt cover leaves no new files.
            let prepared_err = PreparedZipImport {
                collections: Some(PreparedZipCollections {
                    liked: Vec::new(),
                    playlists: Vec::new(),
                    smart: None,
                    imported_from: None,
                    extra: Map::new(),
                }),
                stats: None,
                has_settings: false,
                referenced_covers: vec![
                    BackupCoverFile {
                        name: "native_pl_existing.png".to_owned(),
                        bytes: png.clone(),
                    },
                    BackupCoverFile {
                        name: "native_pl_corrupt.png".to_owned(),
                        bytes: b"not-a-valid-image".to_vec(),
                    },
                ],
                tracks_exact: 0,
                tracks_remapped: 0,
                tracks_unmatched: 0,
            };
            // An undecodable cover is skipped instead of failing the whole import.
            assert!(prepared_err.ingest_covers(None, None).is_ok());
            assert!(first.path.exists());

            // Disk cover recovery when archive omits cover entry, matching `c_<hash>.png` on disk.
            let first_name = first.path.file_name().unwrap().to_str().unwrap();
            let recover_bundle = BackupBundle {
                collections_json: Some(format!(
                    r#"{{"playlists":[{{"id":1,"name":"Recover","coverArt":"covers/native_pl_{first_name}","trackIds":[]}}]}}"#
                )),
                stats_json: None,
                settings_json: None,
                playlist_covers: Vec::new(),
            };
            let live_refs = Mutex::new(HashSet::new());
            let pending_refs = Mutex::new(HashSet::new());
            let ingested = prepare_zip_import(recover_bundle, &[], &[])
                .unwrap()
                .ingest_covers(Some(&live_refs), Some(&pending_refs))
                .unwrap();
            assert_eq!(ingested.covers_restored, 0);
            assert!(live_refs.lock().unwrap().contains(&first.path));
            assert!(pending_refs.lock().unwrap().contains(&first.path));
            let mut col = CollectionsData::default();
            ingested.apply(&mut col, &mut PlayStats::default(), false);
            assert_eq!(
                col.playlists[0].cover_art.as_deref(),
                Some(CoverPaths::to_file_url(&first.path).as_str())
            );

            // Export cover boundary guard: files outside `CoverPaths::cover_dir()` must NOT be exported.
            let outside_img = dir.join("outside_secret.png");
            fs::write(&outside_img, &png).unwrap();
            let col_outside = CollectionsData {
                playlists: vec![PlaylistData {
                    id: 1,
                    name: "Outside".to_owned(),
                    cover_art: Some(CoverPaths::to_file_url(&outside_img)),
                    track_ids: Vec::new(),
                    extra: Map::new(),
                }],
                ..Default::default()
            };
            let out_bundle = build_collections_backup_bundle(&col_outside).unwrap();
            assert!(out_bundle.playlist_covers.is_empty());
            let out_col: CollectionsData =
                serde_json::from_str(out_bundle.collections_json.as_deref().unwrap()).unwrap();
            assert_eq!(out_col.playlists[0].cover_art, None);
        });
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn zip_replace_preserves_extra_supports_stats_only_and_merges_stats() {
        let dir = fresh_temp_dir("stats-and-replace");
        crate::paths::with_test_app_dir(&dir, || {
            let albums = vec![mk_album(
                "Alb",
                "Art",
                vec![mk_track_full(
                    "/lib/t1.flac",
                    "One",
                    "Art",
                    "Alb",
                    120.0,
                    "/lib/t1.flac",
                )],
            )];

            // Stats-only archive succeeds cleanly.
            let stats_only_bundle = BackupBundle {
                collections_json: None,
                stats_json: Some(r#"{"Tracks":{"/lib/t1.flac":{"Count":4,"Last":200}}}"#.to_owned()),
                settings_json: None,
                playlist_covers: Vec::new(),
            };
            let mut w = ZipWriter::new(Cursor::new(Vec::new()));
            w.add_deflated(
                "native-stats.json",
                stats_only_bundle.stats_json.as_deref().unwrap().as_bytes(),
            )
            .unwrap();
            let stats_zip = w.finish().unwrap().into_inner();
            let summary = inspect_backup_zip(Cursor::new(&stats_zip)).unwrap();
            assert!(!summary.has_collections);
            assert!(summary.has_stats);
            assert_eq!(summary.stats_tracks, 1);

            let ingested_stats = prepare_zip_import(stats_only_bundle, &albums, &[])
                .unwrap()
                .ingest_covers(None, None)
                .unwrap();
            let mut col = CollectionsData::default();
            col.extra.insert("localKey".to_owned(), Value::Bool(true));
            let mut stats = PlayStats::default();
            stats.tracks.insert(
                "/lib/T1.FLAC".to_owned(),
                PlayStat { count: 9, last: 100 },
            );
            let (rep, _, stats_changed) = ingested_stats.apply(&mut col, &mut stats, false);
            assert!(!rep.collections_imported);
            assert!(stats_changed);
            // Merge keeps max count (9) and max timestamp (200) case-insensitively!
            assert_eq!(stats.tracks["/lib/T1.FLAC"].count, 9);
            assert_eq!(stats.tracks["/lib/T1.FLAC"].last, 200);

            // Replace mode merges `extra` instead of wiping unknown local fields and returns GC covers.
            col.playlists.push(PlaylistData {
                id: 1,
                name: "Old".to_owned(),
                cover_art: Some("file:///tmp/old.png".to_owned()),
                track_ids: Vec::new(),
                extra: Map::new(),
            });
            let replace_bundle = BackupBundle {
                collections_json: Some(r#"{"liked":[],"playlists":[],"remoteKey":42}"#.to_owned()),
                stats_json: Some(r#"{"Tracks":{"/lib/t1.flac":{"Count":2,"Last":50}}}"#.to_owned()),
                settings_json: None,
                playlist_covers: Vec::new(),
            };
            let ingested_rep = prepare_zip_import(replace_bundle, &albums, &[])
                .unwrap()
                .ingest_covers(None, None)
                .unwrap();
            let (rep2, old_gc, stats_changed2) = ingested_rep.apply(&mut col, &mut stats, true);
            assert!(rep2.collections_imported);
            assert!(stats_changed2);
            assert!(col.smart.is_none());
            assert_eq!(old_gc, vec!["file:///tmp/old.png".to_owned()]);
            assert_eq!(col.extra.get("localKey").and_then(Value::as_bool), Some(true));
            assert_eq!(col.extra.get("remoteKey").and_then(Value::as_i64), Some(42));
            assert_eq!(stats.tracks["/lib/t1.flac"].count, 2);
        });
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn m3u_edge_cases_uris_cwd_guard_empty_artist_extalb_lone_cr_and_latin1() {
        let dir = fresh_temp_dir("m3u-edge");
        let albums = vec![mk_album(
            "Alb",
            "Art",
            vec![
                mk_track_full(
                    "/home/okarin/Music/Track One.flac",
                    "Track One",
                    "Art",
                    "Alb",
                    200.0,
                    "/home/okarin/Music/Track One.flac",
                ),
                mk_track_full(
                    "C:\\Music\\Win Song.flac",
                    "Win Song",
                    "WinArt",
                    "WinAlb",
                    180.0,
                    "C:\\Music\\Win Song.flac",
                ),
                mk_track_full(
                    "/home/okarin/Music/Ambient/Intro.flac",
                    "Intro",
                    "",
                    "Ambient",
                    150.0,
                    "/home/okarin/Music/Ambient/Intro.flac",
                ),
                mk_track_full(
                    "/home/okarin/Music/Live/Stream.flac",
                    "Stream",
                    "Band",
                    "Live",
                    210.0,
                    "/home/okarin/Music/Live/Stream.flac",
                ),
                mk_track_full(
                    "/home/okarin/Music/Cafe.flac",
                    "Café – Noël",
                    "Chanson",
                    "Nights",
                    195.0,
                    "/home/okarin/Music/Cafe.flac",
                ),
            ],
        )];

        // Lone `\r`, `file://localhost/`, `file:///C:/` with `?query` & `#frag`, HTTPS skip Tier 2,
        // `#EXTALB` leak guard, and empty-artist metadata rules.
        let m3u = concat!(
            "#EXTM3U\r",
            "file://localhost/home/okarin/Music/Track%20One.flac?token=1#t=10\r",
            "file:///C:/Music/Win%20Song.flac?v=2#frag\r",
            // HTTP URL must skip Tier 2 (cannot suffix-match `/home/okarin/Music/Live/Stream.flac`)
            "https://example.com/Live/Stream.flac\r",
            // Empty artist without #EXTALB must NOT match title-only `Intro`
            "#EXTINF:150,Intro\r",
            "/missing/intro1.flac\r",
            // Empty artist WITH #EXTALB uniquely matches (empty artist, Intro, Ambient)
            "#EXTINF:150,Intro\r",
            "#EXTALB:Ambient\r",
            "/missing/intro2.flac\r",
            // Orphan #EXTALB followed by `#EXTINF` must NOT leak `WrongAlbum` to `Stream`!
            "#EXTALB:WrongAlbum\r",
            "#EXTINF:210,Band - Stream\r",
            "/missing/stream.flac\r",
        );
        let res = import_m3u_str(m3u, None, &albums, &[]).unwrap();
        assert_eq!(res.exact, 1); // file://localhost/... -> POSIX exact
        assert_eq!(res.remapped, 1); // file:///C:/Music/Win Song.flac -> remapped onto C:\Music\Win Song.flac
        assert_eq!(res.metadata, 2);
        assert_eq!(res.unmatched, 2);
        assert_eq!(
            res.track_ids,
            vec![
                "/home/okarin/Music/Track One.flac".to_owned(),
                "C:\\Music\\Win Song.flac".to_owned(),
                "/home/okarin/Music/Ambient/Intro.flac".to_owned(),
                "/home/okarin/Music/Live/Stream.flac".to_owned(),
            ]
        );

        // Relative entry must NOT probe process working directory before playlist parent directory:
        // `Cargo.toml` exists in process CWD (`retrograde` repo root), yet importing from a
        // synthetic playlist path must not treat `"Cargo.toml"` as hitting `canon_cwd/Cargo.toml`.
        let cwd_cargo = fs::canonicalize("Cargo.toml").unwrap();
        let cwd_album = vec![mk_album(
            "Cwd",
            "Dev",
            vec![mk_track_full(
                cwd_cargo.to_str().unwrap(),
                "Cargo",
                "Dev",
                "Cwd",
                100.0,
                cwd_cargo.to_str().unwrap(),
            )],
        )];
        let fake_pl = dir.join("other_dir").join("mix.m3u8");
        assert!(matches!(
            import_m3u_str("#EXTM3U\nCargo.toml\n", Some(&fake_pl), &cwd_album, &[]),
            Err(PlaylistIoError::NoMatchingTracks { total: 1 })
        ));

        // Legacy CP1252 / Latin-1 `.m3u` works via import_m3u_file; `.m3u8` rejects invalid UTF-8.
        let cp1252_m3u = dir.join("legacy.m3u");
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"#EXTM3U\n#EXTINF:195,Chanson - Caf");
        bytes.push(0xE9); // é
        bytes.extend_from_slice(b" ");
        bytes.push(0x96); // en-dash in CP1252
        bytes.extend_from_slice(b" No");
        bytes.push(0xEB); // ë
        bytes.extend_from_slice(b"l\n/other/missing.flac\n");
        fs::write(&cp1252_m3u, &bytes).unwrap();
        let latin_res = import_m3u_file(&cp1252_m3u, &albums, &[]).unwrap();
        assert_eq!(latin_res.track_ids, vec!["/home/okarin/Music/Cafe.flac".to_owned()]);

        let bad_m3u8 = dir.join("bad.m3u8");
        fs::write(&bad_m3u8, &bytes).unwrap();
        assert!(matches!(
            import_m3u_file(&bad_m3u8, &albums, &[]),
            Err(PlaylistIoError::InvalidUtf8)
        ));

        // Export filename & empty-path guard.
        assert_eq!(suggest_export_filename("///***???", "m3u8"), "playlist.m3u8");
        assert_eq!(suggest_export_filename("...", "zip"), "playlist.zip");
        assert_eq!(suggest_export_filename(".hidden.", "m3u8"), "hidden.m3u8");
        let long_name = format!("{} .", "a".repeat(98));
        assert_eq!(suggest_export_filename(&long_name, "m3u8"), format!("{}.m3u8", "a".repeat(98)));

        let empty_track = mk_track_full("", "Ghost", "Nobody", "Void", 100.0, "");
        let formatted = format_m3u8("Clean", &[empty_track]);
        assert_eq!(formatted, "#EXTM3U\n#PLAYLIST:Clean\n");

        let partial_dest = dir.join("partial_fail.m3u8");
        assert!(
            write_file_direct_or_cleanup(&partial_dest, |mut f| {
                f.write_all(b"partial")?;
                Err(PlaylistIoError::Io(std::io::Error::other("simulated disk full")))
            })
            .is_err()
        );
        assert!(!partial_dest.exists());

        let _ = fs::remove_dir_all(&dir);
    }
}
