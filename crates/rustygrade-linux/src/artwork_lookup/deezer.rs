use std::sync::atomic::{AtomicBool, Ordering};

use serde_json::Value;

use super::normalize::{
    clean_album_title, normalize_https, primary_artist, validate_artwork_url, validate_download_url,
};
use super::scoring::score_candidate;
use super::{
    ArtworkQuery, CoverCandidate, FetchCoverError, MAX_CANDIDATES, MAX_SEARCH_BODY_BYTES,
    OnlineArtworkResolver,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeezerEntity {
    Album,
    Track,
}

fn deezer_cover_obj(item: &Value, entity: DeezerEntity) -> Option<&Value> {
    match entity {
        DeezerEntity::Album => Some(item),
        DeezerEntity::Track => item.get("album"),
    }
}

fn is_deezer_placeholder(cover_obj: &Value, url: &str) -> bool {
    cover_obj
        .get("md5_image")
        .and_then(Value::as_str)
        .is_some_and(str::is_empty)
        || url.contains("/images/cover//")
}

fn deezer_url_with(
    cover_obj: &Value,
    keys: &[&str],
    validate: fn(&str) -> Option<String>,
) -> Option<String> {
    for key in keys {
        let Some(raw) = cover_obj.get(*key).and_then(Value::as_str) else {
            continue;
        };
        let norm = normalize_https(raw);
        if is_deezer_placeholder(cover_obj, &norm) {
            continue;
        }
        if let Some(valid) = validate(&norm) {
            return Some(valid);
        }
    }
    None
}

fn pick_best_deezer_artwork(
    results: &[Value],
    query: &ArtworkQuery,
    entity: DeezerEntity,
) -> Option<String> {
    let cleaned_album = clean_album_title(&query.album);
    let song_mode = entity == DeezerEntity::Track;
    let mut best: Option<(i32, String)> = None;

    for (idx, item) in results.iter().enumerate() {
        let Some(cover_obj) = deezer_cover_obj(item, entity) else {
            continue;
        };
        let Some(art_url) = deezer_url_with(
            cover_obj,
            &["cover_big", "cover_xl", "cover_medium"],
            validate_artwork_url,
        ) else {
            continue;
        };
        let cand_artist = item
            .get("artist")
            .and_then(|a| a.get("name"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let (cand_album, cand_track) = match entity {
            DeezerEntity::Album => (item.get("title").and_then(Value::as_str).unwrap_or(""), ""),
            DeezerEntity::Track => (
                cover_obj.get("title").and_then(Value::as_str).unwrap_or(""),
                item.get("title").and_then(Value::as_str).unwrap_or(""),
            ),
        };
        let Some(total) = score_candidate(
            query,
            &cleaned_album,
            cand_artist,
            cand_album,
            cand_track,
            song_mode,
            idx,
        ) else {
            continue;
        };
        if best.as_ref().is_none_or(|(s, _)| total > *s) {
            best = Some((total, art_url));
        }
    }

    best.map(|(_, url)| url)
}

impl OnlineArtworkResolver {
    /// Deezer Public API candidates for the cover picker. Blocking.
    pub fn deezer_candidates(
        &self,
        query: &ArtworkQuery,
        cancel: &AtomicBool,
    ) -> Result<Vec<CoverCandidate>, FetchCoverError> {
        let (entity, results) = if query.use_album {
            let term = if query.has_searchable_artist() {
                format!("{} {}", primary_artist(&query.artist), query.album)
            } else {
                query.album.clone()
            };
            let mut results = self.deezer_search(DeezerEntity::Album, &term, 12, cancel)?;
            if results.is_empty() {
                let cleaned = clean_album_title(&query.album);
                if cleaned != query.album {
                    let clean_term = if query.has_searchable_artist() {
                        format!("{} {cleaned}", primary_artist(&query.artist))
                    } else {
                        cleaned
                    };
                    results = self.deezer_search(DeezerEntity::Album, &clean_term, 12, cancel)?;
                }
            }
            (DeezerEntity::Album, results)
        } else {
            let term = if query.has_searchable_artist() {
                format!("{} {}", primary_artist(&query.artist), query.title)
            } else {
                query.title.clone()
            };
            (
                DeezerEntity::Track,
                self.deezer_search(DeezerEntity::Track, &term, 12, cancel)?,
            )
        };

        let best = pick_best_deezer_artwork(&results, query, entity);
        let mut out: Vec<CoverCandidate> = Vec::new();
        for item in &results {
            let Some(cover_obj) = deezer_cover_obj(item, entity) else {
                continue;
            };
            let Some(thumb_url) = deezer_url_with(
                cover_obj,
                &["cover_medium", "cover_big"],
                validate_download_url,
            ) else {
                continue;
            };
            if out.iter().any(|c| c.thumb_url == thumb_url) {
                continue;
            }
            let mut full_urls = Vec::new();
            for key in ["cover_xl", "cover_big"] {
                if let Some(u) = deezer_url_with(cover_obj, &[key], validate_download_url)
                    && !full_urls.contains(&u)
                {
                    full_urls.push(u);
                }
            }
            if full_urls.is_empty() {
                full_urls.push(thumb_url.clone());
            }
            let best_url = deezer_url_with(
                cover_obj,
                &["cover_big", "cover_xl", "cover_medium"],
                validate_artwork_url,
            );
            let artist = item
                .get("artist")
                .and_then(|a| a.get("name"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            let title = match entity {
                DeezerEntity::Album => item
                    .get("title")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned(),
                DeezerEntity::Track => cover_obj
                    .get("title")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .or_else(|| item.get("title").and_then(Value::as_str))
                    .unwrap_or("")
                    .to_owned(),
            };
            let year = cover_obj
                .get("release_date")
                .and_then(Value::as_str)
                .and_then(|d| d.get(..4))
                .map(str::to_owned);
            out.push(CoverCandidate {
                title,
                artist,
                year,
                thumb_url,
                full_urls,
                source: "DEEZER",
                best: best.is_some() && best == best_url,
            });
        }
        out.sort_by_key(|c| !c.best);
        out.truncate(MAX_CANDIDATES);
        Ok(out)
    }

    fn deezer_search(
        &self,
        entity: DeezerEntity,
        term: &str,
        limit: usize,
        cancel: &AtomicBool,
    ) -> Result<Vec<Value>, FetchCoverError> {
        if cancel.load(Ordering::Relaxed) {
            return Err(FetchCoverError::Cancelled);
        }
        let endpoint = match entity {
            DeezerEntity::Album => "https://api.deezer.com/search/album",
            DeezerEntity::Track => "https://api.deezer.com/search/track",
        };
        let limit_str = limit.to_string();
        let mut resp = self
            .agent
            .get(endpoint)
            .query("q", term)
            .query("limit", &limit_str)
            .call()
            .map_err(|_| FetchCoverError::Network)?;
        match resp.status().as_u16() {
            200 => {}
            403 | 429 | 503 => return Err(FetchCoverError::Busy),
            _ => return Err(FetchCoverError::Network),
        }
        let val = Self::read_json_body(&mut resp, MAX_SEARCH_BODY_BYTES)?;
        if let Some(err) = val.get("error") {
            if err.get("code").and_then(Value::as_i64) == Some(4) {
                return Err(FetchCoverError::Busy);
            }
            return Err(FetchCoverError::Network);
        }
        Ok(val
            .get("data")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default())
    }

    fn search_deezer(
        &self,
        entity: DeezerEntity,
        term: &str,
        query: &ArtworkQuery,
        cancel: &AtomicBool,
        had_transient_error: &mut bool,
    ) -> Option<String> {
        match self.deezer_search(entity, term, 8, cancel) {
            Ok(results) => pick_best_deezer_artwork(&results, query, entity),
            Err(FetchCoverError::NotFound) => None,
            Err(_) => {
                *had_transient_error = true;
                None
            }
        }
    }

    pub(super) fn try_deezer(
        &self,
        query: &ArtworkQuery,
        cancel: &AtomicBool,
        had_transient_error: &mut bool,
    ) -> Option<String> {
        let artist_q = query
            .has_searchable_artist()
            .then(|| primary_artist(&query.artist));

        if query.use_album {
            let term = match artist_q {
                Some(a) => format!("{a} {}", query.album),
                None => query.album.clone(),
            };
            if let Some(url) = self.search_deezer(
                DeezerEntity::Album,
                &term,
                query,
                cancel,
                had_transient_error,
            ) {
                return Some(url);
            }
            if *had_transient_error {
                return None;
            }
            let cleaned = clean_album_title(&query.album);
            if cleaned != query.album {
                let clean_term = match artist_q {
                    Some(a) => format!("{a} {cleaned}"),
                    None => cleaned,
                };
                if let Some(url) = self.search_deezer(
                    DeezerEntity::Album,
                    &clean_term,
                    query,
                    cancel,
                    had_transient_error,
                ) {
                    return Some(url);
                }
                if *had_transient_error {
                    return None;
                }
            }
        }

        if !query.title.is_empty()
            && let Some(a) = artist_q
        {
            let song_term = format!("{a} {}", query.title);
            if let Some(url) = self.search_deezer(
                DeezerEntity::Track,
                &song_term,
                query,
                cancel,
                had_transient_error,
            ) {
                return Some(url);
            }
        }

        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn deezer_scoring_matches_and_skips_placeholders() {
        let q = ArtworkQuery::from_metadata("Daft Punk", "Discovery", "One More Time").unwrap();
        let results = vec![
            json!({
                "title": "Discovery",
                "md5_image": "",
                "cover_big": "https://cdn-images.dzcdn.net/images/cover//500x500-000000-80-0-0.jpg",
                "artist": { "name": "Daft Punk" }
            }),
            json!({
                "title": "Discovery",
                "md5_image": "231c66c0ed260c708163eff3bb8458da",
                "cover_big": "https://cdn-images.dzcdn.net/images/cover/231c66c0ed260c708163eff3bb8458da/500x500-000000-80-0-0.jpg",
                "artist": { "name": "Daft Punk" }
            }),
        ];
        assert_eq!(
            pick_best_deezer_artwork(&results, &q, DeezerEntity::Album).as_deref(),
            Some(
                "https://cdn-images.dzcdn.net/images/cover/231c66c0ed260c708163eff3bb8458da/500x500-000000-80-0-0.jpg"
            )
        );

        let q_wrong = ArtworkQuery::from_metadata("Taylor Swift", "Red", "Red").unwrap();
        assert!(
            pick_best_deezer_artwork(
                &[json!({
                    "title": "Red",
                    "md5_image": "abcd",
                    "cover_big": "https://cdn-images.dzcdn.net/images/cover/abcd/500x500-000000-80-0-0.jpg",
                    "artist": { "name": "King Crimson" }
                })],
                &q_wrong,
                DeezerEntity::Album
            )
            .is_none()
        );
    }
}
