use std::sync::atomic::{AtomicBool, Ordering};

use serde_json::Value;
use tracing::debug;

use super::normalize::{
    clean_album_title, normalize_https, primary_artist, upgrade_itunes_artwork_url,
    validate_artwork_url, validate_download_url,
};
use super::scoring::score_candidate;
use super::{
    ArtworkQuery, CoverCandidate, FETCH_COVER_EDGE, FetchCoverError, MAX_CANDIDATES,
    MAX_HTTP_BODY_BYTES, MAX_SEARCH_BODY_BYTES, OnlineArtworkResolver,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ItunesEntity {
    Album,
    Song,
}

impl ItunesEntity {
    fn as_str(self) -> &'static str {
        match self {
            Self::Album => "album",
            Self::Song => "song",
        }
    }
}

/// An iTunes artwork URL at `edge` px.
fn itunes_sized(raw100: &str, edge: u32) -> Option<String> {
    let norm = normalize_https(raw100);
    let up = if norm.contains("/100x100bb") {
        norm.replacen("/100x100bb", &format!("/{edge}x{edge}bb"), 1)
    } else if norm.contains("/60x60bb") {
        norm.replacen("/60x60bb", &format!("/{edge}x{edge}bb"), 1)
    } else {
        return None;
    };
    validate_download_url(&up)
}

fn pick_best_itunes_artwork(
    results: &[Value],
    query: &ArtworkQuery,
    entity: ItunesEntity,
) -> Option<String> {
    let cleaned_album = clean_album_title(&query.album);
    let song_mode = entity == ItunesEntity::Song;
    let mut best: Option<(i32, String)> = None;

    for (idx, item) in results.iter().enumerate() {
        let Some(raw_art) = item.get("artworkUrl100").and_then(Value::as_str) else {
            continue;
        };
        let Some(art_url) = upgrade_itunes_artwork_url(raw_art) else {
            continue;
        };
        let cand_artist = item.get("artistName").and_then(Value::as_str).unwrap_or("");
        let cand_album = item
            .get("collectionName")
            .and_then(Value::as_str)
            .unwrap_or("");
        let cand_track = item.get("trackName").and_then(Value::as_str).unwrap_or("");

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
    /// iTunes Search candidates for the picker, the one [`Self::resolve`] would choose first.
    /// Also searches cleaned album title and the Japanese store for CJK names. Blocking.
    pub fn itunes_candidates(
        &self,
        query: &ArtworkQuery,
        cancel: &AtomicBool,
    ) -> Result<Vec<CoverCandidate>, FetchCoverError> {
        let entity = if query.use_album {
            ItunesEntity::Album
        } else {
            ItunesEntity::Song
        };
        let subject = if query.use_album {
            &query.album
        } else {
            &query.title
        };
        let artist_prefix = query
            .has_searchable_artist()
            .then(|| primary_artist(&query.artist));
        let term = match artist_prefix {
            Some(a) => format!("{a} {subject}"),
            None => subject.clone(),
        };
        let mut results =
            self.itunes_results(&term, entity, None, 12, MAX_SEARCH_BODY_BYTES, cancel)?;
        if results.is_empty() && query.use_album {
            let cleaned = clean_album_title(&query.album);
            if cleaned != query.album {
                let clean_term = match artist_prefix {
                    Some(a) => format!("{a} {cleaned}"),
                    None => cleaned,
                };
                results = self.itunes_results(
                    &clean_term,
                    entity,
                    None,
                    12,
                    MAX_SEARCH_BODY_BYTES,
                    cancel,
                )?;
            }
        }
        if query.has_cjk() {
            results.extend(
                self.itunes_results(&term, entity, Some("jp"), 12, MAX_SEARCH_BODY_BYTES, cancel)
                    .unwrap_or_default(),
            );
        }

        let best = pick_best_itunes_artwork(&results, query, entity);
        let mut out: Vec<CoverCandidate> = Vec::new();
        for item in &results {
            let Some(raw) = item.get("artworkUrl100").and_then(Value::as_str) else {
                continue;
            };
            let (Some(thumb_url), Some(full)) =
                (itunes_sized(raw, 250), itunes_sized(raw, FETCH_COVER_EDGE))
            else {
                continue;
            };
            if out.iter().any(|c| c.thumb_url == thumb_url) {
                continue;
            }
            let str_of = |k: &str| item.get(k).and_then(Value::as_str).unwrap_or("").to_owned();
            let title = if query.use_album {
                str_of("collectionName")
            } else {
                str_of("trackName")
            };
            let fallback = itunes_sized(raw, 512);
            let is_best =
                best.is_some() && best == fallback.as_deref().and_then(validate_artwork_url);
            out.push(CoverCandidate {
                title,
                artist: str_of("artistName"),
                year: item
                    .get("releaseDate")
                    .and_then(Value::as_str)
                    .and_then(|d| d.get(..4))
                    .map(str::to_owned),
                best: is_best,
                full_urls: std::iter::once(full).chain(fallback).collect(),
                thumb_url,
                source: "ITUNES",
            });
        }
        out.sort_by_key(|c| !c.best);
        out.truncate(MAX_CANDIDATES);
        Ok(out)
    }

    fn itunes_results(
        &self,
        term: &str,
        entity: ItunesEntity,
        country: Option<&str>,
        limit: usize,
        body_limit: u64,
        cancel: &AtomicBool,
    ) -> Result<Vec<Value>, FetchCoverError> {
        if cancel.load(Ordering::Relaxed) {
            return Err(FetchCoverError::Cancelled);
        }
        let limit_str = limit.to_string();
        let mut req = self
            .agent
            .get("https://itunes.apple.com/search")
            .query("term", term)
            .query("entity", entity.as_str())
            .query("limit", &limit_str);
        if let Some(c) = country {
            req = req.query("country", c);
        }
        let mut resp = req.call().map_err(|err| {
            debug!(error = %err, "iTunes artwork lookup transport error");
            FetchCoverError::Network
        })?;
        match resp.status().as_u16() {
            200 => {}
            403 | 429 | 503 => return Err(FetchCoverError::Busy),
            status if status >= 500 => return Err(FetchCoverError::Busy),
            _ => return Ok(Vec::new()),
        }
        let val = Self::read_json_body(&mut resp, body_limit)?;
        Ok(val
            .get("results")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default())
    }

    fn search_itunes(
        &self,
        term: &str,
        entity: ItunesEntity,
        country: Option<&str>,
        query: &ArtworkQuery,
        cancel: &AtomicBool,
        had_transient_error: &mut bool,
    ) -> Option<String> {
        match self.itunes_results(term, entity, country, 8, MAX_HTTP_BODY_BYTES, cancel) {
            Ok(results) => pick_best_itunes_artwork(&results, query, entity),
            Err(_) => {
                *had_transient_error = true;
                None
            }
        }
    }

    pub(super) fn try_itunes(
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
            if let Some(url) = self.search_itunes(
                &term,
                ItunesEntity::Album,
                None,
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
                let cleaned_term = match artist_q {
                    Some(a) => format!("{a} {cleaned}"),
                    None => cleaned,
                };
                if let Some(url) = self.search_itunes(
                    &cleaned_term,
                    ItunesEntity::Album,
                    None,
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

            if query.has_cjk()
                && let Some(url) = self.search_itunes(
                    &term,
                    ItunesEntity::Album,
                    Some("jp"),
                    query,
                    cancel,
                    had_transient_error,
                )
            {
                return Some(url);
            }
            if *had_transient_error {
                return None;
            }
        }

        if !query.title.is_empty()
            && let Some(a) = artist_q
        {
            let song_term = format!("{a} {}", query.title);
            if let Some(url) = self.search_itunes(
                &song_term,
                ItunesEntity::Song,
                None,
                query,
                cancel,
                had_transient_error,
            ) {
                return Some(url);
            }
            if !*had_transient_error
                && query.has_cjk()
                && let Some(url) = self.search_itunes(
                    &song_term,
                    ItunesEntity::Song,
                    Some("jp"),
                    query,
                    cancel,
                    had_transient_error,
                )
            {
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
    fn itunes_sized_rewrites_and_normalizes_https() {
        assert_eq!(
            itunes_sized(
                "https://is1-ssl.mzstatic.com/image/thumb/a/b/100x100bb.jpg",
                250
            )
            .as_deref(),
            Some("https://is1-ssl.mzstatic.com/image/thumb/a/b/250x250bb.jpg")
        );
        assert_eq!(
            itunes_sized(
                "http://is1-ssl.mzstatic.com/image/thumb/a/b/60x60bb.jpg",
                1000
            )
            .as_deref(),
            Some("https://is1-ssl.mzstatic.com/image/thumb/a/b/1000x1000bb.jpg")
        );
        assert_eq!(
            itunes_sized("https://is1-ssl.mzstatic.com/image/a.jpg", 250),
            None
        );
        assert_eq!(
            itunes_sized("https://evil.example/100x100bb.jpg", 250),
            None
        );
    }

    #[test]
    fn itunes_scoring_handles_exact_cjk_features_cross_script_and_rejects_wrong_artist() {
        let q_exact = ArtworkQuery::from_metadata("Ado", "Kyougen", "Usseewa").unwrap();
        let results = vec![
            json!({
                "artistName": "Other Artist",
                "collectionName": "Random",
                "artworkUrl100": "https://is1-ssl.mzstatic.com/a/100x100bb.jpg",
            }),
            json!({
                "artistName": "ADO",
                "collectionName": "Kyougen",
                "artworkUrl100": "https://is1-ssl.mzstatic.com/b/100x100bb.jpg",
            }),
            json!({
                "artistName": "Ado feat. Miku",
                "collectionName": "Kyougen",
                "artworkUrl100": "https://is1-ssl.mzstatic.com/feat/100x100bb.jpg",
            }),
        ];
        assert_eq!(
            pick_best_itunes_artwork(&results, &q_exact, ItunesEntity::Album).as_deref(),
            Some("https://is1-ssl.mzstatic.com/b/512x512bb.jpg")
        );
        assert_eq!(
            pick_best_itunes_artwork(&results[2..], &q_exact, ItunesEntity::Album).as_deref(),
            Some("https://is1-ssl.mzstatic.com/feat/512x512bb.jpg")
        );

        let q_short = ArtworkQuery::from_metadata("Ado", "Shadow", "Shadow").unwrap();
        assert!(
            pick_best_itunes_artwork(
                &[json!({
                    "artistName": "Shadow",
                    "collectionName": "Shadow",
                    "artworkUrl100": "https://is1-ssl.mzstatic.com/trap/100x100bb.jpg",
                })],
                &q_short,
                ItunesEntity::Album
            )
            .is_none()
        );

        let q_cross =
            ArtworkQuery::from_metadata("ヨルシカ", "Elma", "Rain with Cappuccino").unwrap();
        assert_eq!(
            pick_best_itunes_artwork(
                &[json!({
                    "artistName": "Yorushika",
                    "collectionName": "Elma",
                    "artworkUrl100": "https://is1-ssl.mzstatic.com/elma/100x100bb.jpg",
                })],
                &q_cross,
                ItunesEntity::Album
            )
            .as_deref(),
            Some("https://is1-ssl.mzstatic.com/elma/512x512bb.jpg")
        );

        let q_same_script = ArtworkQuery::from_metadata("Taylor Swift", "Red", "Red").unwrap();
        assert!(
            pick_best_itunes_artwork(
                &[json!({
                    "artistName": "King Crimson",
                    "collectionName": "Red",
                    "artworkUrl100": "https://is1-ssl.mzstatic.com/kc/100x100bb.jpg",
                })],
                &q_same_script,
                ItunesEntity::Album
            )
            .is_none()
        );
    }
}
