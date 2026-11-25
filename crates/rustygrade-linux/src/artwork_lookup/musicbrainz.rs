use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde_json::Value;
use tracing::debug;

use super::normalize::{
    clean_album_title, escape_lucene, is_valid_mbid, normalize_for_match, normalize_https,
    primary_artist, validate_artwork_url,
};
use super::{
    ArtworkQuery, CoverCandidate, FetchCoverError, MAX_CANDIDATES, MAX_HTTP_BODY_BYTES,
    MAX_SEARCH_BODY_BYTES, OnlineArtworkResolver,
};

const MUSICBRAINZ_MIN_GAP: Duration = Duration::from_millis(1100);

impl OnlineArtworkResolver {
    /// MusicBrainz release groups with a Cover Art Archive front cover. Which of them have one is
    /// only known once their thumbnail downloads, so the caller drops those that fail. Blocking
    /// (MusicBrainz allows a request a second).
    pub fn musicbrainz_candidates(
        &self,
        query: &ArtworkQuery,
        cancel: &AtomicBool,
    ) -> Result<Vec<CoverCandidate>, FetchCoverError> {
        if !query.use_album {
            return Ok(Vec::new());
        }
        let cleaned = clean_album_title(&query.album);
        let album = escape_lucene(&cleaned);
        let mut groups = if query.has_searchable_artist() {
            let artist = escape_lucene(primary_artist(&query.artist));
            self.mb_release_groups(
                &format!("releasegroup:\"{album}\" AND artist:\"{artist}\""),
                cancel,
            )?
        } else {
            Vec::new()
        };
        if groups.is_empty() {
            let wanted = [
                normalize_for_match(&query.album),
                normalize_for_match(&cleaned),
            ];
            groups = self
                .mb_release_groups(&format!("releasegroup:\"{album}\""), cancel)?
                .into_iter()
                .filter(|g| {
                    wanted.contains(&normalize_for_match(
                        g.get("title").and_then(Value::as_str).unwrap_or(""),
                    ))
                })
                .collect();
        }
        let out = groups
            .iter()
            .filter(|g| g.get("score").and_then(Value::as_i64).unwrap_or(0) >= 60)
            .filter_map(|g| {
                let mbid = g
                    .get("id")
                    .and_then(Value::as_str)
                    .filter(|id| is_valid_mbid(id))?;
                let base = format!("https://coverartarchive.org/release-group/{mbid}");
                let artist = g
                    .get("artist-credit")
                    .and_then(Value::as_array)
                    .map(|credits| {
                        credits
                            .iter()
                            .map(|c| {
                                let name = c.get("name").and_then(Value::as_str).unwrap_or("");
                                let join =
                                    c.get("joinphrase").and_then(Value::as_str).unwrap_or("");
                                format!("{name}{join}")
                            })
                            .collect::<String>()
                    })
                    .unwrap_or_default();
                Some(CoverCandidate {
                    title: g
                        .get("title")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_owned(),
                    artist,
                    year: g
                        .get("first-release-date")
                        .and_then(Value::as_str)
                        .and_then(|d| d.get(..4))
                        .map(str::to_owned),
                    thumb_url: format!("{base}/front-250"),
                    full_urls: vec![format!("{base}/front-1200"), format!("{base}/front-500")],
                    source: "COVER ART ARCHIVE",
                    best: false,
                })
            })
            .take(MAX_CANDIDATES)
            .collect();
        Ok(out)
    }

    fn mb_release_groups(
        &self,
        lucene_q: &str,
        cancel: &AtomicBool,
    ) -> Result<Vec<Value>, FetchCoverError> {
        let mut attempt = 0u32;
        let mut resp = loop {
            if !Self::wait_rate_limit(&self.mb_last_call, MUSICBRAINZ_MIN_GAP, cancel) {
                return Err(FetchCoverError::Cancelled);
            }
            let resp = self
                .agent
                .get("https://musicbrainz.org/ws/2/release-group/")
                .query("query", lucene_q)
                .query("fmt", "json")
                .query("limit", "10")
                .call()
                .map_err(|_| FetchCoverError::Network)?;
            match resp.status().as_u16() {
                200 => break resp,
                503 if attempt < 2 => {
                    attempt += 1;
                    let pause =
                        Instant::now() + Duration::from_millis(if attempt == 2 { 2000 } else { 0 });
                    while Instant::now() < pause {
                        if cancel.load(Ordering::Relaxed) {
                            return Err(FetchCoverError::Cancelled);
                        }
                        std::thread::sleep(Duration::from_millis(50));
                    }
                }
                503 => return Err(FetchCoverError::Busy),
                _ => return Err(FetchCoverError::Network),
            }
        };
        let val = Self::read_json_body(&mut resp, MAX_SEARCH_BODY_BYTES)?;
        Ok(val
            .get("release-groups")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default())
    }

    pub(super) fn try_musicbrainz_caa(
        &self,
        query: &ArtworkQuery,
        cancel: &AtomicBool,
        had_transient_error: &mut bool,
    ) -> Option<String> {
        if !query.use_album {
            return None;
        }
        if !Self::wait_rate_limit(&self.mb_last_call, MUSICBRAINZ_MIN_GAP, cancel) {
            *had_transient_error = true;
            return None;
        }

        let cleaned_album = clean_album_title(&query.album);
        let escaped_album = escape_lucene(&cleaned_album);
        let has_artist = query.has_searchable_artist();
        let lucene_q = if has_artist {
            let escaped_artist = escape_lucene(primary_artist(&query.artist));
            format!("releasegroup:\"{escaped_album}\" AND artist:\"{escaped_artist}\"")
        } else {
            format!("releasegroup:\"{escaped_album}\"")
        };

        let mut resp = match self
            .agent
            .get("https://musicbrainz.org/ws/2/release-group/")
            .query("query", &lucene_q)
            .query("fmt", "json")
            .query("limit", "5")
            .call()
        {
            Ok(r) => r,
            Err(err) => {
                debug!(error = %err, "MusicBrainz lookup transport error");
                *had_transient_error = true;
                return None;
            }
        };

        let status = resp.status().as_u16();
        if matches!(status, 400 | 404) {
            debug!(status, "MusicBrainz lookup non-retryable status");
            return None;
        }
        if status != 200 {
            debug!(status, "MusicBrainz lookup transient status");
            *had_transient_error = true;
            return None;
        }

        let Ok(val) = Self::read_json_body(&mut resp, MAX_HTTP_BODY_BYTES) else {
            *had_transient_error = true;
            return None;
        };
        let groups = val.get("release-groups").and_then(Value::as_array)?;

        let min_score = if has_artist { 80 } else { 95 };
        let album_norms = [
            normalize_for_match(&query.album),
            normalize_for_match(&cleaned_album),
        ];

        for item in groups
            .iter()
            .filter(|g| g.get("score").and_then(Value::as_i64).unwrap_or(0) >= min_score)
            .take(3)
        {
            if !has_artist {
                let title_norm =
                    normalize_for_match(item.get("title").and_then(Value::as_str).unwrap_or(""));
                if title_norm.is_empty() || !album_norms.contains(&title_norm) {
                    continue;
                }
            }
            if cancel.load(Ordering::Relaxed) {
                *had_transient_error = true;
                return None;
            }
            let Some(mbid) = item.get("id").and_then(Value::as_str) else {
                continue;
            };
            if !is_valid_mbid(mbid) {
                continue;
            }
            let caa_url = format!("https://coverartarchive.org/release-group/{mbid}/front-500");
            let head_resp = match self
                .agent
                .head(&caa_url)
                .config()
                .max_redirects(0)
                .max_redirects_will_error(false)
                .build()
                .call()
            {
                Ok(r) => r,
                Err(err) => {
                    debug!(error = %err, "Cover Art Archive HEAD error");
                    *had_transient_error = true;
                    break;
                }
            };
            let head_status = head_resp.status().as_u16();
            if matches!(head_status, 301 | 302 | 303 | 307 | 308) {
                if let Some(loc) = head_resp
                    .headers()
                    .get("location")
                    .and_then(|v| v.to_str().ok())
                {
                    let normalized = normalize_https(loc);
                    if let Some(valid) = validate_artwork_url(&normalized) {
                        return Some(valid);
                    }
                }
                if let Some(valid) = validate_artwork_url(&caa_url) {
                    return Some(valid);
                }
            } else if head_status == 200 {
                if let Some(valid) = validate_artwork_url(&caa_url) {
                    return Some(valid);
                }
            } else if head_status == 404 {
                continue;
            } else {
                *had_transient_error = true;
                break;
            }
        }

        None
    }
}
