use std::sync::atomic::AtomicBool;
use std::time::Duration;

use serde_json::Value;

use super::normalize::{
    clean_album_title, normalize_https, primary_artist, validate_artwork_url, validate_download_url,
};
use super::scoring::{score_candidate, string_similarity_score};
use super::{
    ArtworkQuery, CoverCandidate, FetchCoverError, MAX_CANDIDATES, MAX_SEARCH_BODY_BYTES,
    OnlineArtworkResolver,
};

const DISCOGS_MIN_GAP: Duration = Duration::from_millis(1050);

/// Strips Discogs' trailing `" (N)"` numeric artist disambiguation (e.g. `"Ado (3)"` -> `"Ado"`).
pub fn strip_discogs_disambig(artist: &str) -> &str {
    let trimmed = artist.trim();
    if let Some(head) = trimmed.strip_suffix(')')
        && let Some(idx) = head.rfind('(')
    {
        let num = head[idx + 1..].trim();
        if !num.is_empty() && num.bytes().all(|b| b.is_ascii_digit()) {
            let prefix = head[..idx].trim_end();
            if !prefix.is_empty() {
                return prefix;
            }
        }
    }
    trimmed
}

fn split_discogs_title(raw: &str) -> (&str, &str) {
    match raw.split_once(" - ") {
        Some((artist, title)) => (strip_discogs_disambig(artist), title.trim()),
        None => ("", raw.trim()),
    }
}

fn discogs_url_with(
    item: &Value,
    keys: &[&str],
    validate: fn(&str) -> Option<String>,
) -> Option<String> {
    for key in keys {
        let Some(raw) = item.get(*key).and_then(Value::as_str) else {
            continue;
        };
        let norm = normalize_https(raw);
        if norm.to_ascii_lowercase().contains("spacer.gif") {
            continue;
        }
        if let Some(valid) = validate(&norm) {
            return Some(valid);
        }
    }
    None
}

fn discogs_year(item: &Value) -> Option<String> {
    match item.get("year") {
        Some(Value::String(s)) => {
            let y = s.trim();
            (!y.is_empty()).then(|| y.chars().take(4).collect())
        }
        Some(Value::Number(n)) => n.as_u64().map(|y| y.to_string()),
        _ => None,
    }
}

fn pick_best_discogs_artwork(results: &[Value], query: &ArtworkQuery) -> Option<String> {
    let cleaned_album = clean_album_title(&query.album);
    let artist_targets = [query.artist.as_str(), primary_artist(&query.artist)];
    let mut best: Option<(i32, String)> = None;

    for (idx, item) in results.iter().enumerate() {
        let Some(art_url) = discogs_url_with(item, &["cover_image", "thumb"], validate_artwork_url)
        else {
            continue;
        };
        let raw_title = item.get("title").and_then(Value::as_str).unwrap_or("");
        let (cand_artist, cand_album) = split_discogs_title(raw_title);
        let total = if query.use_album {
            match score_candidate(
                query,
                &cleaned_album,
                cand_artist,
                cand_album,
                "",
                false,
                idx,
            ) {
                Some(s) => s,
                None => continue,
            }
        } else {
            if !query.has_searchable_artist() {
                continue;
            }
            let (artist_pts, _) = string_similarity_score(cand_artist, &artist_targets, 60, 35);
            if artist_pts < 35 {
                continue;
            }
            artist_pts + (40 - (idx.min(4) as i32) * 5)
        };
        if best.as_ref().is_none_or(|(s, _)| total > *s) {
            best = Some((total, art_url));
        }
    }

    best.map(|(_, url)| url)
}

impl OnlineArtworkResolver {
    /// Discogs Database Search candidates for the cover picker. Requires an active token. Blocking.
    pub fn discogs_candidates(
        &self,
        query: &ArtworkQuery,
        cancel: &AtomicBool,
    ) -> Result<Vec<CoverCandidate>, FetchCoverError> {
        let providers = self.providers();
        let Some(token) = providers.effective_discogs_token() else {
            return Ok(Vec::new());
        };
        let results = self.discogs_search_with_fallback(query, token, 12, cancel)?;
        let best = pick_best_discogs_artwork(&results, query);
        let mut out: Vec<CoverCandidate> = Vec::new();
        for item in &results {
            let Some(thumb_url) =
                discogs_url_with(item, &["thumb", "cover_image"], validate_download_url)
            else {
                continue;
            };
            if out.iter().any(|c| c.thumb_url == thumb_url) {
                continue;
            }
            let mut full_urls = Vec::new();
            for key in ["cover_image", "thumb"] {
                if let Some(u) = discogs_url_with(item, &[key], validate_download_url)
                    && !full_urls.contains(&u)
                {
                    full_urls.push(u);
                }
            }
            if full_urls.is_empty() {
                full_urls.push(thumb_url.clone());
            }
            let raw_title = item.get("title").and_then(Value::as_str).unwrap_or("");
            let (artist, title) = split_discogs_title(raw_title);
            let best_url = discogs_url_with(item, &["cover_image", "thumb"], validate_artwork_url);
            out.push(CoverCandidate {
                title: title.to_owned(),
                artist: artist.to_owned(),
                year: discogs_year(item),
                thumb_url,
                full_urls,
                source: "DISCOGS",
                best: best.is_some() && best == best_url,
            });
        }
        out.sort_by_key(|c| !c.best);
        out.truncate(MAX_CANDIDATES);
        Ok(out)
    }

    fn discogs_search_with_fallback(
        &self,
        query: &ArtworkQuery,
        token: &str,
        per_page: usize,
        cancel: &AtomicBool,
    ) -> Result<Vec<Value>, FetchCoverError> {
        let mut results = self.discogs_search(query, token, per_page, false, cancel)?;
        if results.is_empty() && query.use_album {
            results = self.discogs_search(query, token, per_page, true, cancel)?;
        }
        Ok(results)
    }

    fn discogs_search(
        &self,
        query: &ArtworkQuery,
        token: &str,
        per_page: usize,
        use_q_fallback: bool,
        cancel: &AtomicBool,
    ) -> Result<Vec<Value>, FetchCoverError> {
        if !Self::wait_rate_limit(&self.discogs_last_call, DISCOGS_MIN_GAP, cancel) {
            return Err(FetchCoverError::Cancelled);
        }
        let per_page_str = per_page.to_string();
        let mut req = self
            .agent
            .get("https://api.discogs.com/database/search")
            .header("Authorization", &format!("Discogs token={token}"))
            .query("type", "release")
            .query("per_page", &per_page_str);
        if query.use_album {
            let cleaned = clean_album_title(&query.album);
            if use_q_fallback {
                let q_term = if query.has_searchable_artist() {
                    format!("{} {cleaned}", primary_artist(&query.artist))
                } else {
                    cleaned
                };
                req = req.query("q", &q_term);
            } else {
                req = req.query("release_title", &cleaned);
                if query.has_searchable_artist() {
                    req = req.query("artist", primary_artist(&query.artist));
                }
            }
        } else {
            req = req.query("track", &query.title);
            if query.has_searchable_artist() {
                req = req.query("artist", primary_artist(&query.artist));
            }
        }
        let mut resp = req.call().map_err(|_| FetchCoverError::Network)?;
        match resp.status().as_u16() {
            200 => {}
            401 | 403 => return Err(FetchCoverError::Unauthorized),
            429 | 503 => return Err(FetchCoverError::Busy),
            _ => return Err(FetchCoverError::Network),
        }
        let val = Self::read_json_body(&mut resp, MAX_SEARCH_BODY_BYTES)?;
        Ok(val
            .get("results")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default())
    }

    pub(super) fn try_discogs(
        &self,
        query: &ArtworkQuery,
        token: &str,
        cancel: &AtomicBool,
        had_transient_error: &mut bool,
    ) -> Option<String> {
        match self.discogs_search_with_fallback(query, token, 8, cancel) {
            Ok(results) => pick_best_discogs_artwork(&results, query),
            Err(FetchCoverError::Unauthorized) => None,
            Err(_) => {
                *had_transient_error = true;
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn discogs_scoring_parses_disambig_and_ignores_spacer_gif() {
        assert_eq!(strip_discogs_disambig("Ado (3)"), "Ado");
        assert_eq!(strip_discogs_disambig("Orbital (UK)"), "Orbital (UK)");

        let q = ArtworkQuery::from_metadata("Ado", "Kyougen", "Usseewa").unwrap();
        let results = vec![
            json!({
                "title": "Ado (3) - Kyougen",
                "cover_image": "https://st.discogs.com/assets/spacer.gif",
                "thumb": "https://st.discogs.com/assets/spacer.gif",
            }),
            json!({
                "title": "Other - Kyougen",
                "cover_image": "https://i.discogs.com/wrong/cover.jpg",
            }),
            json!({
                "title": "Ado (3) - Kyougen",
                "cover_image": "https://i.discogs.com/good/cover.jpg",
                "thumb": "https://i.discogs.com/good/thumb.jpg",
            }),
        ];
        assert_eq!(
            pick_best_discogs_artwork(&results, &q).as_deref(),
            Some("https://i.discogs.com/good/cover.jpg")
        );
    }
}
