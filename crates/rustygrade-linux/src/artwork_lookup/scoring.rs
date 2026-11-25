use super::ArtworkQuery;
use super::normalize::{contains_cjk, normalize_for_match, primary_artist};

fn token_subsequence_match(haystack: &str, needle: &str) -> bool {
    let h: Vec<&str> = haystack.split_whitespace().collect();
    let n: Vec<&str> = needle.split_whitespace().collect();
    !n.is_empty() && h.len() >= n.len() && h.windows(n.len()).any(|w| w == n.as_slice())
}

pub(super) fn string_similarity_score(
    candidate_raw: &str,
    targets: &[&str],
    exact_pts: i32,
    partial_pts: i32,
) -> (i32, bool) {
    let cand = normalize_for_match(candidate_raw);
    if cand.is_empty() {
        return (0, false);
    }
    let mut best = 0;
    for target in targets {
        let t = normalize_for_match(target);
        if t.is_empty() {
            continue;
        }
        if cand == t {
            return (exact_pts, true);
        }
        if token_subsequence_match(&cand, &t) || token_subsequence_match(&t, &cand) {
            best = best.max(partial_pts);
        } else if contains_cjk(&cand) && contains_cjk(&t) {
            let (shorter, longer) = if cand.chars().count() <= t.chars().count() {
                (&cand, &t)
            } else {
                (&t, &cand)
            };
            if shorter.chars().count() >= 2 && longer.contains(shorter.as_str()) {
                best = best.max(partial_pts);
            }
        }
    }
    (best, false)
}

fn is_album_only_candidate_safe(cand_artist: &str, query_album: &str) -> bool {
    let a = normalize_for_match(cand_artist);
    if matches!(
        a.as_str(),
        "various artists" | "soundtrack" | "original soundtrack" | "va"
    ) {
        return true;
    }
    let q = normalize_for_match(query_album);
    q.chars().count() >= 10 || q.split_whitespace().count() >= 2
}

pub(super) fn score_candidate(
    query: &ArtworkQuery,
    cleaned_album: &str,
    cand_artist: &str,
    cand_album: &str,
    cand_track: &str,
    song_mode: bool,
    idx: usize,
) -> Option<i32> {
    let has_artist = query.has_searchable_artist();
    let artist_targets = [query.artist.as_str(), primary_artist(&query.artist)];
    let album_targets = [query.album.as_str(), cleaned_album];
    let title_targets = [query.title.as_str()];

    let (album_pts, album_exact) = if query.use_album {
        string_similarity_score(cand_album, &album_targets, 70, 45)
    } else {
        (0, false)
    };

    let (primary_match_pts, exact_primary, allow_cjk_fallback, matched_raw) = if !song_mode {
        (
            album_pts,
            album_exact,
            contains_cjk(&query.album),
            cand_album,
        )
    } else {
        let (track_pts, track_exact) = string_similarity_score(cand_track, &title_targets, 65, 40);
        let bonus = if query.use_album && album_pts > 0 {
            15
        } else {
            0
        };
        (
            track_pts + bonus,
            track_exact,
            contains_cjk(&query.title),
            cand_track,
        )
    };

    let artist_score = if has_artist {
        let (pts, _) = string_similarity_score(cand_artist, &artist_targets, 60, 35);
        if pts > 0 {
            pts
        } else {
            let cross_script = contains_cjk(&query.artist) != contains_cjk(cand_artist);
            let norm_matched = normalize_for_match(matched_raw);
            let min_len = if contains_cjk(&norm_matched) { 2 } else { 4 };
            if cross_script && exact_primary && norm_matched.chars().count() >= min_len {
                -15
            } else {
                return None;
            }
        }
    } else {
        if !album_exact || !is_album_only_candidate_safe(cand_artist, &query.album) {
            return None;
        }
        0
    };

    if primary_match_pts == 0 && !(allow_cjk_fallback && artist_score > 0 && idx < 2) {
        return None;
    }

    let cjk_rank_bonus = if query.has_cjk() && artist_score > 0 {
        match idx {
            0 => 20,
            1 => 10,
            _ => 0,
        }
    } else {
        0
    };

    let total = artist_score + primary_match_pts + cjk_rank_bonus;
    (total >= 50).then_some(total)
}
