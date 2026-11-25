use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::models::Album;

use super::SAF_PREFIX;

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
            } else if common == best_len
                && common >= min_required
                && best_id != Some(cand.id.as_str())
            {
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

pub(super) fn is_http_url(raw: &str) -> bool {
    raw.as_bytes()
        .get(..7)
        .is_some_and(|b| b.eq_ignore_ascii_case(b"http://"))
        || raw
            .as_bytes()
            .get(..8)
            .is_some_and(|b| b.eq_ignore_ascii_case(b"https://"))
}

pub(super) fn is_absolute_or_drive(raw: &str) -> bool {
    let bytes = raw.as_bytes();
    raw.starts_with('/')
        || raw.starts_with('\\')
        || (bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':')
        || raw
            .as_bytes()
            .get(..7)
            .is_some_and(|b| b.eq_ignore_ascii_case(b"file://"))
}

pub(super) fn resolve_relative_lexically(base_dir: &Path, rel: &str) -> PathBuf {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::{CoverPaths, decode_file_uri};

    #[test]
    fn non_ascii_relative_paths_do_not_panic() {
        for raw in [
            "Кино/01.flac",
            "音楽/01.flac",
            "音楽カバー",
            "ééééééé",
            "ééé/x",
        ] {
            assert!(!is_http_url(raw));
            assert!(decode_file_uri(raw).is_none());
            assert!(!is_absolute_or_drive(raw));
            assert!(CoverPaths::to_local_path(Some(raw)).is_some());
        }
        assert_eq!(
            decode_file_uri("file:///a/%E9%9F%B3.flac").as_deref(),
            Some("/a/音.flac")
        );
    }
}
