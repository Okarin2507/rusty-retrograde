use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

static CACHE_TMP_SEQ: AtomicU64 = AtomicU64::new(1);

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::debug;

const USER_AGENT: &str = concat!(
    "Rustygrade/",
    env!("CARGO_PKG_VERSION"),
    " (https://github.com/Okarin2507/rusty-rustygrade)"
);

const CACHE_VERSION: u32 = 1;
const HIT_TTL_SECS: u64 = 30 * 86_400;
const MISS_TTL_SECS: u64 = 86_400;
const TRANSIENT_BACKOFF: Duration = Duration::from_secs(60);
const MAX_CACHE_ENTRIES: usize = 1000;
const MAX_HTTP_BODY_BYTES: u64 = 256 * 1024;
const HTTP_TIMEOUT: Duration = Duration::from_secs(4);
const MUSICBRAINZ_MIN_GAP: Duration = Duration::from_millis(1100);
const DISCOGS_MIN_GAP: Duration = Duration::from_millis(1050);
/// Edge of a cover fetched for the tag editor (iTunes renders any size; the Cover Art Archive
/// serves 250 / 500 / 1200, so it gets 1200). It is embedded in every file of an album edit,
/// so it stays modest; the app's own copy is capped at 720 px anyway.
pub const FETCH_COVER_EDGE: u32 = 1000;
const MAX_IMAGE_BYTES: u64 = 8 * 1024 * 1024;
/// A picker search lists every release of each group, far more than the resolver's lookups.
const MAX_SEARCH_BODY_BYTES: u64 = 1024 * 1024;
const IMAGE_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_DISCORD_IMAGE_LEN: usize = 256;
const MAX_DOWNLOAD_URL_LEN: usize = 2048;

const ALLOWED_ARTWORK_HOSTS: &[&str] = &[
    "mzstatic.com",
    "coverartarchive.org",
    "archive.org",
    "dzcdn.net",
    "discogs.com",
];

const SINGLE_WORD_EDITION_KEYWORDS: &[&str] = &[
    "disc",
    "cd",
    "deluxe",
    "edition",
    "remaster",
    "remastered",
    "version",
    "flac",
    "mp3",
    "aac",
    "wav",
    "alac",
    "bit",
    "khz",
    "explicit",
    "clean",
    "bonus",
    "complete",
    "expanded",
    "special",
    "limited",
    "anniversary",
    "standard",
];

const AMBIGUOUS_ALBUM_ONLY_TITLES: &[&str] = &[
    "greatest hits",
    "best of",
    "soundtrack",
    "original soundtrack",
    "ost",
    "anthology",
    "collection",
    "compilation",
    "live",
    "unplugged",
    "demos",
    "b sides",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtworkQuery {
    pub artist: String,
    pub album: String,
    pub title: String,
    pub use_album: bool,
    pub cache_key: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LookupOutcome {
    Found(String),
    NotFound,
    TransientError,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CacheLookup {
    Hit(Option<String>),
    Backoff,
    Miss,
}

pub trait ArtworkResolver: Send + Sync {
    fn resolve(&self, query: &ArtworkQuery, cancel: &AtomicBool) -> LookupOutcome;
}

/// Which online artwork providers are enabled for Discord RPC and cover fetching.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtworkProviders {
    pub itunes: bool,
    pub deezer: bool,
    pub musicbrainz: bool,
    pub discogs: bool,
    pub discogs_token: String,
}

impl Default for ArtworkProviders {
    fn default() -> Self {
        Self {
            itunes: true,
            deezer: true,
            musicbrainz: true,
            discogs: false,
            discogs_token: String::new(),
        }
    }
}

impl ArtworkProviders {
    pub fn from_settings(s: &rustygrade_core::AppSettings) -> Self {
        Self {
            itunes: s.artwork_itunes,
            deezer: s.artwork_deezer,
            musicbrainz: s.artwork_musicbrainz,
            discogs: s.artwork_discogs,
            discogs_token: s.discogs_token.trim().to_owned(),
        }
    }

    pub fn effective_discogs_token(&self) -> Option<&str> {
        self.discogs
            .then(|| self.discogs_token.trim())
            .filter(|t| !t.is_empty())
    }

    pub fn discogs_ready(&self) -> bool {
        self.effective_discogs_token().is_some()
    }

    pub fn any_enabled(&self) -> bool {
        self.itunes || self.deezer || self.musicbrainz || self.discogs_ready()
    }

    pub fn allows_url(&self, url: &str) -> bool {
        match provider_for_url(url) {
            Some("itunes") => self.itunes,
            Some("deezer") => self.deezer,
            Some("musicbrainz") => self.musicbrainz,
            Some("discogs") => self.discogs_ready(),
            _ => false,
        }
    }
}

impl ArtworkQuery {
    /// Builds a normalized lookup query and stable cache key from track tags.
    pub fn from_metadata(artist: &str, album: &str, title: &str) -> Option<Self> {
        let artist = collapse_spaces(artist);
        let album = collapse_spaces(album);
        let title = collapse_spaces(title);

        let has_album = !is_generic_album(&album);
        let has_artist = !is_generic_artist(&artist);
        let has_title = !title.is_empty();

        let (use_album, cache_key) = if has_album {
            let key = if has_artist {
                format!("album::{}::{}", norm_key(&artist), norm_key(&album))
            } else if is_ambiguous_album_only(&album) {
                return None;
            } else {
                format!("album_only::{}", norm_key(&album))
            };
            (true, key)
        } else if has_title && has_artist {
            (false, format!("track::{}::{}", norm_key(&artist), norm_key(&title)))
        } else {
            return None;
        };

        Some(Self {
            artist,
            album,
            title,
            use_album,
            cache_key,
        })
    }

    fn has_cjk(&self) -> bool {
        contains_cjk(&self.artist) || contains_cjk(&self.album) || contains_cjk(&self.title)
    }

    fn has_searchable_artist(&self) -> bool {
        !is_generic_artist(&self.artist)
    }
}

pub fn now_unix_u64() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

pub fn is_generic_album(s: &str) -> bool {
    let lower = collapse_spaces(s).to_lowercase();
    lower.is_empty()
        || lower.starts_with("unknown album")
        || matches!(
            lower.as_str(),
            "unknown"
                | "singles"
                | "single"
                | "misc"
                | "various"
                | "none"
                | "n/a"
                | "downloads"
                | "music"
        )
}

pub fn is_generic_artist(s: &str) -> bool {
    let lower = collapse_spaces(s).to_lowercase();
    lower.is_empty()
        || matches!(
            lower.as_str(),
            "unknown artist"
                | "unknown"
                | "various artists"
                | "various"
                | "v.a."
                | "va"
                | "none"
                | "n/a"
        )
}

fn is_ambiguous_album_only(album: &str) -> bool {
    let norm = normalize_for_match(album);
    norm.chars().count() < 4 || AMBIGUOUS_ALBUM_ONLY_TITLES.contains(&norm.as_str())
}

/// Extracts the leading artist name when tags list multiple collaborations or features.
pub fn primary_artist(artist: &str) -> &str {
    let trimmed = artist.trim();
    let lower = trimmed.to_ascii_lowercase();
    let seps = [";", " / ", " feat. ", " ft. ", " featuring ", " × "];
    let mut earliest: Option<usize> = None;
    for sep in seps {
        if let Some(idx) = lower.find(sep)
            && earliest.is_none_or(|cur| idx < cur)
        {
            earliest = Some(idx);
        }
    }
    match earliest {
        Some(idx) => {
            let head = trimmed[..idx].trim();
            if head.is_empty() { trimmed } else { head }
        }
        None => trimmed,
    }
}

/// Strips trailing edition/disc/format annotations for fallback album searches.
pub fn clean_album_title(album: &str) -> String {
    let mut current = collapse_spaces(album);
    loop {
        let trimmed = current.trim_end();
        if let Some(stripped) = strip_trailing_bracket_edition(trimmed, '(', ')')
            .or_else(|| strip_trailing_bracket_edition(trimmed, '[', ']'))
            .or_else(|| strip_trailing_disc_suffix(trimmed))
        {
            let next = stripped.trim().to_owned();
            if !next.is_empty() && next != current {
                current = next;
                continue;
            }
        }
        break;
    }
    current
}

fn strip_trailing_bracket_edition(s: &str, open: char, close: char) -> Option<&str> {
    if !s.ends_with(close) {
        return None;
    }
    let start = s.rfind(open)?;
    let inner = s[start + open.len_utf8()..s.len() - close.len_utf8()].to_lowercase();
    let words: Vec<&str> = inner
        .split(|ch: char| !ch.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect();
    let has_edition_word = words
        .iter()
        .any(|w| SINGLE_WORD_EDITION_KEYWORDS.contains(w))
        || words
            .windows(2)
            .any(|w| w == ["live", "at"] || w == ["hi", "res"]);
    if has_edition_word {
        Some(&s[..start])
    } else {
        None
    }
}

fn strip_trailing_disc_suffix(s: &str) -> Option<&str> {
    let lower = s.to_ascii_lowercase();
    for prefix in [" disc ", " cd "] {
        if let Some(idx) = lower.rfind(prefix) {
            let tail = lower[idx + prefix.len()..].trim();
            if !tail.is_empty() && tail.bytes().all(|b| b.is_ascii_digit()) {
                return Some(&s[..idx]);
            }
        }
    }
    None
}

pub fn contains_cjk(s: &str) -> bool {
    s.chars().any(|ch| {
        matches!(
            ch,
            '\u{1100}'..='\u{11ff}'
                | '\u{3040}'..='\u{30ff}'
                | '\u{3400}'..='\u{4dbf}'
                | '\u{4e00}'..='\u{9fff}'
                | '\u{ac00}'..='\u{d7af}'
                | '\u{ff65}'..='\u{ff9f}'
        )
    })
}

fn extract_https_host(url: &str) -> Option<&str> {
    let trimmed = url.trim();
    if !trimmed.starts_with("https://") || !trimmed.bytes().all(|b| matches!(b, 0x21..=0x7e)) {
        return None;
    }
    let after_scheme = &trimmed["https://".len()..];
    let authority_end = after_scheme
        .find(['/', '?', '#'])
        .unwrap_or(after_scheme.len());
    let authority = &after_scheme[..authority_end];
    if authority.is_empty() || authority.contains(['@', '\\', '%']) {
        return None;
    }
    authority.split(':').next().filter(|h| !h.is_empty())
}

fn host_matches(host: &str, domain: &str) -> bool {
    host == domain || host.ends_with(&format!(".{domain}"))
}

fn validate_https_cdn_url(url: &str, max_len: usize) -> Option<String> {
    let trimmed = url.trim();
    if trimmed.len() > max_len {
        return None;
    }
    let host = extract_https_host(trimmed)?.to_ascii_lowercase();
    let allowed = ALLOWED_ARTWORK_HOSTS
        .iter()
        .any(|domain| host_matches(&host, domain));
    if !allowed {
        return None;
    }
    Some(trimmed.to_owned())
}

/// Validates that a remote artwork URL is safe, HTTPS, within Discord's 256-char limit,
/// and hosted on one of the expected CDN domains.
pub fn validate_artwork_url(url: &str) -> Option<String> {
    validate_https_cdn_url(url, MAX_DISCORD_IMAGE_LEN)
}

fn validate_download_url(url: &str) -> Option<String> {
    validate_https_cdn_url(url, MAX_DOWNLOAD_URL_LEN)
}

fn normalize_https(raw: &str) -> String {
    let trimmed = raw.trim();
    if let Some(rest) = trimmed.strip_prefix("http://") {
        format!("https://{rest}")
    } else {
        trimmed.to_owned()
    }
}

pub fn provider_for_url(url: &str) -> Option<&'static str> {
    let host = extract_https_host(url)?.to_ascii_lowercase();
    if host_matches(&host, "mzstatic.com") {
        Some("itunes")
    } else if host_matches(&host, "dzcdn.net") {
        Some("deezer")
    } else if host_matches(&host, "coverartarchive.org") || host_matches(&host, "archive.org") {
        Some("musicbrainz")
    } else if host_matches(&host, "discogs.com") {
        Some("discogs")
    } else {
        None
    }
}

pub fn upgrade_itunes_artwork_url(raw: &str) -> Option<String> {
    let upgraded = if raw.contains("/100x100bb") {
        raw.replacen("/100x100bb", "/512x512bb", 1)
    } else if raw.contains("/60x60bb") {
        raw.replacen("/60x60bb", "/512x512bb", 1)
    } else {
        raw.to_owned()
    };
    validate_artwork_url(&upgraded)
}

pub fn is_valid_mbid(id: &str) -> bool {
    if id.len() != 36 {
        return false;
    }
    id.char_indices().all(|(idx, ch)| match idx {
        8 | 13 | 18 | 23 => ch == '-',
        _ => ch.is_ascii_hexdigit(),
    })
}

pub fn escape_lucene(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        if matches!(
            ch,
            '+' | '-'
                | '&'
                | '|'
                | '!'
                | '('
                | ')'
                | '{'
                | '}'
                | '['
                | ']'
                | '^'
                | '"'
                | '~'
                | '*'
                | '?'
                | ':'
                | '\\'
                | '/'
        ) {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

fn collapse_spaces(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn norm_key(s: &str) -> String {
    collapse_spaces(s).to_lowercase()
}

fn normalize_for_match(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_space = true;
    for ch in s.chars() {
        if matches!(ch, '\'' | '\u{2018}' | '\u{2019}') {
            continue;
        }
        for lc in ch.to_lowercase() {
            if lc.is_alphanumeric() {
                out.push(lc);
                last_space = false;
            } else if !last_space {
                out.push(' ');
                last_space = true;
            }
        }
    }
    out.trim().to_owned()
}

fn token_subsequence_match(haystack: &str, needle: &str) -> bool {
    let h: Vec<&str> = haystack.split_whitespace().collect();
    let n: Vec<&str> = needle.split_whitespace().collect();
    !n.is_empty() && h.len() >= n.len() && h.windows(n.len()).any(|w| w == n.as_slice())
}

fn string_similarity_score(
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

fn score_candidate(
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
        (album_pts, album_exact, contains_cjk(&query.album), cand_album)
    } else {
        let (track_pts, track_exact) =
            string_similarity_score(cand_track, &title_targets, 65, 40);
        let bonus = if query.use_album && album_pts > 0 { 15 } else { 0 };
        (track_pts + bonus, track_exact, contains_cjk(&query.title), cand_track)
    };

    let artist_score = if has_artist {
        let (pts, _) = string_similarity_score(cand_artist, &artist_targets, 60, 35);
        if pts > 0 {
            pts
        } else {
            let cross_script = contains_cjk(&query.artist) != contains_cjk(cand_artist);
            let norm_matched = normalize_for_match(matched_raw);
            let min_len = if contains_cjk(&norm_matched) { 2 } else { 4 };
            if cross_script
                && exact_primary
                && norm_matched.chars().count() >= min_len
            {
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
        let cand_album = item.get("collectionName").and_then(Value::as_str).unwrap_or("");
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
            let (artist_pts, _) =
                string_similarity_score(cand_artist, &artist_targets, 60, 35);
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

/// Online metadata-based album artwork resolver (iTunes -> Deezer -> MusicBrainz + CAA -> Discogs).
pub struct OnlineArtworkResolver {
    agent: ureq::Agent,
    image_agent: ureq::Agent,
    mb_last_call: Mutex<Option<Instant>>,
    discogs_last_call: Mutex<Option<Instant>>,
    providers: RwLock<ArtworkProviders>,
}

/// One cover the tag editor's picker offers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoverCandidate {
    pub title: String,
    pub artist: String,
    /// Release year, when the service gives one.
    pub year: Option<String>,
    /// A small picture for the picker (250 px).
    pub thumb_url: String,
    /// The picture to embed, largest first (each one tried in turn).
    pub full_urls: Vec<String>,
    /// `ITUNES`, `DEEZER`, `COVER ART ARCHIVE`, or `DISCOGS`.
    pub source: &'static str,
    /// The match [`OnlineArtworkResolver::resolve`] would pick on its own.
    pub best: bool,
}

/// Up to this many candidates per service.
const MAX_CANDIDATES: usize = 8;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchCoverError {
    NotFound,
    Network,
    /// The service is rate-limiting or overloaded (MusicBrainz answers 503).
    Busy,
    /// Discogs rejected the configured Personal Access Token (401/403).
    Unauthorized,
    Cancelled,
}

/// The process-wide resolver: Discord RPC and the tag editor share it, so the MusicBrainz
/// rate-limit gap and the connection pool are shared too.
pub fn shared_resolver() -> Arc<OnlineArtworkResolver> {
    static SHARED: OnceLock<Arc<OnlineArtworkResolver>> = OnceLock::new();
    Arc::clone(SHARED.get_or_init(|| Arc::new(OnlineArtworkResolver::new())))
}

/// An iTunes artwork URL at `edge` px.
fn itunes_sized(raw100: &str, edge: u32) -> Option<String> {
    let up = if raw100.contains("/100x100bb") {
        raw100.replacen("/100x100bb", &format!("/{edge}x{edge}bb"), 1)
    } else if raw100.contains("/60x60bb") {
        raw100.replacen("/60x60bb", &format!("/{edge}x{edge}bb"), 1)
    } else {
        return None;
    };
    validate_artwork_url(&up)
}

impl Default for OnlineArtworkResolver {
    fn default() -> Self {
        Self::new()
    }
}

impl OnlineArtworkResolver {
    pub fn new() -> Self {
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(HTTP_TIMEOUT))
            .https_only(true)
            .max_redirects(3)
            .http_status_as_error(false)
            .user_agent(USER_AGENT)
            .build();
        let image_config = ureq::Agent::config_builder()
            .timeout_global(Some(IMAGE_TIMEOUT))
            .https_only(true)
            .max_redirects(5)
            .http_status_as_error(false)
            .user_agent(USER_AGENT)
            .build();
        Self {
            agent: config.new_agent(),
            image_agent: image_config.new_agent(),
            mb_last_call: Mutex::new(None),
            discogs_last_call: Mutex::new(None),
            providers: RwLock::new(ArtworkProviders::default()),
        }
    }

    pub fn set_providers(&self, providers: ArtworkProviders) {
        let mut guard = self
            .providers
            .write()
            .unwrap_or_else(|p| p.into_inner());
        *guard = providers;
    }

    pub fn providers(&self) -> ArtworkProviders {
        self.providers
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    /// iTunes Search candidates for the picker, the one [`Self::resolve`] would choose first.
    /// Also searches the Japanese store for CJK names. Blocking.
    pub fn itunes_candidates(
        &self,
        query: &ArtworkQuery,
        cancel: &AtomicBool,
    ) -> Result<Vec<CoverCandidate>, FetchCoverError> {
        let entity = if query.use_album { ItunesEntity::Album } else { ItunesEntity::Song };
        let subject = if query.use_album { &query.album } else { &query.title };
        let term = if query.has_searchable_artist() {
            format!("{} {subject}", primary_artist(&query.artist))
        } else {
            subject.clone()
        };
        let mut results = self.itunes_results(&term, entity, None, cancel)?;
        if query.has_cjk() {
            results.extend(self.itunes_results(&term, entity, Some("jp"), cancel).unwrap_or_default());
        }
        let best = pick_best_itunes_artwork(&results, query, entity);
        let mut out: Vec<CoverCandidate> = Vec::new();
        for item in &results {
            let Some(raw) = item.get("artworkUrl100").and_then(Value::as_str) else { continue };
            let (Some(thumb_url), Some(full)) = (itunes_sized(raw, 250), itunes_sized(raw, FETCH_COVER_EDGE)) else {
                continue;
            };
            if out.iter().any(|c| c.thumb_url == thumb_url) {
                continue;
            }
            let str_of = |k: &str| item.get(k).and_then(Value::as_str).unwrap_or("").to_owned();
            let title = if query.use_album { str_of("collectionName") } else { str_of("trackName") };
            let fallback = itunes_sized(raw, 512);
            out.push(CoverCandidate {
                title,
                artist: str_of("artistName"),
                year: item.get("releaseDate").and_then(Value::as_str).and_then(|d| d.get(..4)).map(str::to_owned),
                best: best.is_some() && best == fallback,
                full_urls: std::iter::once(full).chain(fallback).collect(),
                thumb_url,
                source: "ITUNES",
            });
        }
        // The recommended one first, then iTunes' own order.
        out.sort_by_key(|c| !c.best);
        out.truncate(MAX_CANDIDATES);
        Ok(out)
    }

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
            self.mb_release_groups(&format!("releasegroup:\"{album}\" AND artist:\"{artist}\""), cancel)?
        } else {
            Vec::new()
        };
        if groups.is_empty() {
            // MusicBrainz matches the credited name only ("米津玄師", not "Kenshi Yonezu"): without
            // a hit, list the groups titled exactly like the album and let the user tell them apart.
            let wanted = [normalize_for_match(&query.album), normalize_for_match(&cleaned)];
            groups = self
                .mb_release_groups(&format!("releasegroup:\"{album}\""), cancel)?
                .into_iter()
                .filter(|g| wanted.contains(&normalize_for_match(g.get("title").and_then(Value::as_str).unwrap_or(""))))
                .collect();
        }
        let out = groups
            .iter()
            .filter(|g| g.get("score").and_then(Value::as_i64).unwrap_or(0) >= 60)
            .filter_map(|g| {
                let mbid = g.get("id").and_then(Value::as_str).filter(|id| is_valid_mbid(id))?;
                let base = format!("https://coverartarchive.org/release-group/{mbid}");
                let artist = g
                    .get("artist-credit")
                    .and_then(Value::as_array)
                    .map(|credits| {
                        credits
                            .iter()
                            .map(|c| {
                                let name = c.get("name").and_then(Value::as_str).unwrap_or("");
                                let join = c.get("joinphrase").and_then(Value::as_str).unwrap_or("");
                                format!("{name}{join}")
                            })
                            .collect::<String>()
                    })
                    .unwrap_or_default();
                Some(CoverCandidate {
                    title: g.get("title").and_then(Value::as_str).unwrap_or("").to_owned(),
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

    /// Downloads the first of `urls` that answers with a picture body (not checked to be one).
    /// Blocking.
    pub fn download_first(&self, urls: &[String], cancel: &AtomicBool) -> Result<Vec<u8>, FetchCoverError> {
        let mut last_err = FetchCoverError::NotFound;
        for url in urls {
            if cancel.load(Ordering::Relaxed) {
                return Err(FetchCoverError::Cancelled);
            }
            match self.download_image(url) {
                Ok(bytes) => return Ok(bytes),
                Err(e) => last_err = e,
            }
        }
        Err(last_err)
    }

    /// One MusicBrainz release-group search (rate-limited; two retries after a 503, the second
    /// after a longer pause, as MusicBrainz sheds load in short bursts).
    fn mb_release_groups(&self, lucene_q: &str, cancel: &AtomicBool) -> Result<Vec<Value>, FetchCoverError> {
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
                    // The rate-limit wait adds 1.1 s; the second retry waits 2 s more.
                    let pause = Instant::now() + Duration::from_millis(if attempt == 2 { 2000 } else { 0 });
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
        let bytes = resp
            .body_mut()
            .with_config()
            .limit(MAX_SEARCH_BODY_BYTES)
            .read_to_vec()
            .map_err(|_| FetchCoverError::Network)?;
        let val: Value = serde_json::from_slice(&bytes).map_err(|_| FetchCoverError::Network)?;
        Ok(val.get("release-groups").and_then(Value::as_array).cloned().unwrap_or_default())
    }

    fn itunes_results(
        &self,
        term: &str,
        entity: ItunesEntity,
        country: Option<&str>,
        cancel: &AtomicBool,
    ) -> Result<Vec<Value>, FetchCoverError> {
        if cancel.load(Ordering::Relaxed) {
            return Err(FetchCoverError::Cancelled);
        }
        let mut req = self
            .agent
            .get("https://itunes.apple.com/search")
            .query("term", term)
            .query("entity", entity.as_str())
            .query("limit", "12");
        if let Some(c) = country {
            req = req.query("country", c);
        }
        let mut resp = req.call().map_err(|_| FetchCoverError::Network)?;
        match resp.status().as_u16() {
            200 => {}
            403 | 429 | 503 => return Err(FetchCoverError::Busy),
            _ => return Err(FetchCoverError::Network),
        }
        let bytes = resp
            .body_mut()
            .with_config()
            .limit(MAX_SEARCH_BODY_BYTES)
            .read_to_vec()
            .map_err(|_| FetchCoverError::Network)?;
        let val: Value = serde_json::from_slice(&bytes).map_err(|_| FetchCoverError::Network)?;
        Ok(val.get("results").and_then(Value::as_array).cloned().unwrap_or_default())
    }

    fn download_image(&self, url: &str) -> Result<Vec<u8>, FetchCoverError> {
        let mut resp = self.image_agent.get(url).call().map_err(|err| {
            debug!(error = %err, "cover download transport error");
            FetchCoverError::Network
        })?;
        let status = resp.status().as_u16();
        if status == 404 {
            return Err(FetchCoverError::NotFound);
        }
        if status != 200 {
            debug!(status, "cover download status");
            return Err(FetchCoverError::Network);
        }
        resp.body_mut()
            .with_config()
            .limit(MAX_IMAGE_BYTES)
            .read_to_vec()
            .map_err(|_| FetchCoverError::Network)
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
        if cancel.load(Ordering::Relaxed) {
            *had_transient_error = true;
            return None;
        }
        let mut req = self
            .agent
            .get("https://itunes.apple.com/search")
            .query("term", term)
            .query("entity", entity.as_str())
            .query("limit", "8");
        if let Some(c) = country {
            req = req.query("country", c);
        }
        let mut resp = match req.call() {
            Ok(r) => r,
            Err(err) => {
                debug!(error = %err, "iTunes artwork lookup transport error");
                *had_transient_error = true;
                return None;
            }
        };
        let status = resp.status().as_u16();
        if status == 403 || status == 429 || status >= 500 {
            debug!(status, "iTunes artwork lookup transient status");
            *had_transient_error = true;
            return None;
        }
        if status != 200 {
            return None;
        }
        let Ok(bytes) = resp
            .body_mut()
            .with_config()
            .limit(MAX_HTTP_BODY_BYTES)
            .read_to_vec()
        else {
            *had_transient_error = true;
            return None;
        };
        let Ok(val) = serde_json::from_slice::<Value>(&bytes) else {
            return None;
        };
        let results = val.get("results").and_then(Value::as_array)?;
        pick_best_itunes_artwork(results, query, entity)
    }

    fn try_itunes(
        &self,
        query: &ArtworkQuery,
        cancel: &AtomicBool,
        had_transient_error: &mut bool,
    ) -> Option<String> {
        let artist_q = if query.has_searchable_artist() {
            Some(primary_artist(&query.artist))
        } else {
            None
        };

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

        if !query.title.is_empty() && query.has_searchable_artist() {
            let song_term = match artist_q {
                Some(a) => format!("{a} {}", query.title),
                None => query.title.clone(),
            };
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

    fn wait_rate_limit(
        slot: &Mutex<Option<Instant>>,
        min_gap: Duration,
        cancel: &AtomicBool,
    ) -> bool {
        let mut guard = slot.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(prev) = *guard {
            while prev.elapsed() < min_gap {
                if cancel.load(Ordering::Relaxed) {
                    return false;
                }
                let rem = min_gap.saturating_sub(prev.elapsed());
                std::thread::sleep(rem.min(Duration::from_millis(50)));
            }
        }
        if cancel.load(Ordering::Relaxed) {
            return false;
        }
        *guard = Some(Instant::now());
        true
    }

    fn read_json_body(
        resp: &mut ureq::http::Response<ureq::Body>,
        limit: u64,
    ) -> Result<Value, FetchCoverError> {
        let bytes = resp
            .body_mut()
            .with_config()
            .limit(limit)
            .read_to_vec()
            .map_err(|_| FetchCoverError::Network)?;
        serde_json::from_slice(&bytes).map_err(|_| FetchCoverError::Network)
    }

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
            let Some(thumb_url) =
                deezer_url_with(cover_obj, &["cover_medium", "cover_big"], validate_download_url)
            else {
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

    fn try_deezer(
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

        if !query.title.is_empty() && query.has_searchable_artist() {
            let song_term = match artist_q {
                Some(a) => format!("{a} {}", query.title),
                None => query.title.clone(),
            };
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
        let mut results = self.discogs_search(query, token, 12, false, cancel)?;
        if results.is_empty() && query.use_album {
            results = self.discogs_search(query, token, 12, true, cancel)?;
        }
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

    fn try_discogs(
        &self,
        query: &ArtworkQuery,
        token: &str,
        cancel: &AtomicBool,
        had_transient_error: &mut bool,
    ) -> Option<String> {
        let mut results = match self.discogs_search(query, token, 8, false, cancel) {
            Ok(r) => r,
            Err(FetchCoverError::Unauthorized) => return None,
            Err(_) => {
                *had_transient_error = true;
                return None;
            }
        };
        if let Some(url) = pick_best_discogs_artwork(&results, query) {
            return Some(url);
        }
        if results.is_empty() && query.use_album {
            results = match self.discogs_search(query, token, 8, true, cancel) {
                Ok(r) => r,
                Err(FetchCoverError::Unauthorized) => return None,
                Err(_) => {
                    *had_transient_error = true;
                    return None;
                }
            };
            if let Some(url) = pick_best_discogs_artwork(&results, query) {
                return Some(url);
            }
        }
        None
    }

    fn try_musicbrainz_caa(
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

        let Ok(bytes) = resp
            .body_mut()
            .with_config()
            .limit(MAX_HTTP_BODY_BYTES)
            .read_to_vec()
        else {
            *had_transient_error = true;
            return None;
        };
        let Ok(val) = serde_json::from_slice::<Value>(&bytes) else {
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
                let title_norm = normalize_for_match(
                    item.get("title").and_then(Value::as_str).unwrap_or(""),
                );
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
                if let Some(loc) = head_resp.headers().get("location").and_then(|v| v.to_str().ok())
                {
                    let normalized = if let Some(rest) = loc.strip_prefix("http://") {
                        format!("https://{rest}")
                    } else {
                        loc.to_owned()
                    };
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

impl ArtworkResolver for OnlineArtworkResolver {
    fn resolve(&self, query: &ArtworkQuery, cancel: &AtomicBool) -> LookupOutcome {
        if cancel.load(Ordering::Relaxed) {
            return LookupOutcome::TransientError;
        }
        let providers = self.providers();
        if !providers.any_enabled() {
            return LookupOutcome::NotFound;
        }
        let mut had_transient_error = false;
        if providers.itunes
            && let Some(url) = self.try_itunes(query, cancel, &mut had_transient_error)
        {
            return LookupOutcome::Found(url);
        }
        if cancel.load(Ordering::Relaxed) {
            return LookupOutcome::TransientError;
        }
        if providers.deezer
            && let Some(url) = self.try_deezer(query, cancel, &mut had_transient_error)
        {
            return LookupOutcome::Found(url);
        }
        if cancel.load(Ordering::Relaxed) {
            return LookupOutcome::TransientError;
        }
        if providers.musicbrainz
            && let Some(url) = self.try_musicbrainz_caa(query, cancel, &mut had_transient_error)
        {
            return LookupOutcome::Found(url);
        }
        if cancel.load(Ordering::Relaxed) {
            return LookupOutcome::TransientError;
        }
        if let Some(token) = providers.effective_discogs_token()
            && let Some(url) = self.try_discogs(query, token, cancel, &mut had_transient_error)
        {
            return LookupOutcome::Found(url);
        }
        if cancel.load(Ordering::Relaxed) || had_transient_error {
            LookupOutcome::TransientError
        } else {
            LookupOutcome::NotFound
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CacheFile {
    version: u32,
    entries: HashMap<String, CacheEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CacheEntry {
    url: Option<String>,
    fetched_at: u64,
}

pub struct ArtworkCache {
    path: Option<PathBuf>,
    entries: HashMap<String, CacheEntry>,
    transient_backoff: HashMap<String, Instant>,
}

impl ArtworkCache {
    pub fn load(path: Option<PathBuf>) -> Self {
        let now = now_unix_u64();
        let mut entries = HashMap::new();
        if let Some(ref p) = path
            && let Ok(text) = fs::read_to_string(p)
            && let Ok(file) = serde_json::from_str::<CacheFile>(&text)
            && file.version == CACHE_VERSION
        {
            for (k, mut entry) in file.entries {
                if let Some(ref raw_url) = entry.url {
                    let Some(valid) = validate_artwork_url(raw_url) else {
                        continue;
                    };
                    entry.url = Some(valid);
                }
                if is_entry_fresh(&entry, now) {
                    entries.insert(k, entry);
                }
            }
        }
        Self {
            path,
            entries,
            transient_backoff: HashMap::new(),
        }
    }

    pub fn get(&self, key: &str, now_unix: u64, now_inst: Instant) -> CacheLookup {
        if let Some(entry) = self.entries.get(key)
            && is_entry_fresh(entry, now_unix)
        {
            return CacheLookup::Hit(entry.url.clone());
        }
        if let Some(failed_at) = self.transient_backoff.get(key)
            && now_inst.saturating_duration_since(*failed_at) < TRANSIENT_BACKOFF
        {
            return CacheLookup::Backoff;
        }
        CacheLookup::Miss
    }

    pub fn record(
        &mut self,
        key: String,
        outcome: &LookupOutcome,
        now_unix: u64,
        now_inst: Instant,
    ) {
        self.transient_backoff.retain(|_, failed_at| {
            now_inst.saturating_duration_since(*failed_at) < TRANSIENT_BACKOFF
        });
        match outcome {
            LookupOutcome::Found(url) => {
                self.transient_backoff.remove(&key);
                let Some(valid) = validate_artwork_url(url) else {
                    return;
                };
                self.entries.insert(
                    key,
                    CacheEntry {
                        url: Some(valid),
                        fetched_at: now_unix,
                    },
                );
                self.prune_and_persist(now_unix);
            }
            LookupOutcome::NotFound => {
                self.transient_backoff.remove(&key);
                self.entries.insert(
                    key,
                    CacheEntry {
                        url: None,
                        fetched_at: now_unix,
                    },
                );
                self.prune_and_persist(now_unix);
            }
            LookupOutcome::TransientError => {
                self.transient_backoff.insert(key, now_inst);
            }
        }
    }

    pub fn prune_for_providers(&mut self, providers: &ArtworkProviders) {
        let before = self.entries.len();
        self.entries.retain(|_, entry| match entry.url.as_deref() {
            Some(url) => providers.allows_url(url),
            None => false,
        });
        self.transient_backoff.clear();
        if self.entries.len() != before {
            self.prune_and_persist(now_unix_u64());
        }
    }

    fn prune_and_persist(&mut self, now_unix: u64) {
        self.entries
            .retain(|_, entry| is_entry_fresh(entry, now_unix));
        while self.entries.len() > MAX_CACHE_ENTRIES {
            let Some(oldest_key) = self
                .entries
                .iter()
                .min_by(|(k1, e1), (k2, e2)| {
                    e1.fetched_at.cmp(&e2.fetched_at).then_with(|| k1.cmp(k2))
                })
                .map(|(k, _)| k.clone())
            else {
                break;
            };
            self.entries.remove(&oldest_key);
        }

        let Some(ref path) = self.path else {
            return;
        };
        if let Some(parent) = path.parent()
            && fs::create_dir_all(parent).is_err()
        {
            return;
        }
        let file = CacheFile {
            version: CACHE_VERSION,
            entries: self.entries.clone(),
        };
        let seq = CACHE_TMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let tmp = path.with_extension(format!("json.{}.{seq}.tmp", std::process::id()));
        if let Ok(json) = serde_json::to_string(&file)
            && fs::write(&tmp, json).is_ok()
        {
            let _ = fs::rename(&tmp, path);
        }
    }
}

fn is_entry_fresh(entry: &CacheEntry, now_unix: u64) -> bool {
    let ttl = if entry.url.is_some() {
        HIT_TTL_SECS
    } else {
        MISS_TTL_SECS
    };
    now_unix.saturating_sub(entry.fetched_at) < ttl
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn itunes_sized_rewrites_the_artwork_edge() {
        assert_eq!(
            itunes_sized("https://is1-ssl.mzstatic.com/image/thumb/a/b/100x100bb.jpg", 250).as_deref(),
            Some("https://is1-ssl.mzstatic.com/image/thumb/a/b/250x250bb.jpg")
        );
        assert_eq!(
            itunes_sized("https://is1-ssl.mzstatic.com/image/thumb/a/b/60x60bb.jpg", 1000).as_deref(),
            Some("https://is1-ssl.mzstatic.com/image/thumb/a/b/1000x1000bb.jpg")
        );
        assert_eq!(itunes_sized("https://is1-ssl.mzstatic.com/image/a.jpg", 250), None);
        assert_eq!(itunes_sized("https://evil.example/100x100bb.jpg", 250), None);
    }

    fn temp_cache_path(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "rg-art-cache-{}-{tag}-{}.json",
            std::process::id(),
            now_unix_u64()
        ));
        let _ = fs::remove_file(&p);
        p
    }

    #[test]
    fn detects_generic_tags_and_builds_expected_cache_keys() {
        assert!(is_generic_album(""));
        assert!(is_generic_album("Unknown Album"));
        assert!(is_generic_album("Unknown Album (Ado)"));
        assert!(is_generic_album("  SINGLES "));
        assert!(!is_generic_album("Kyougen"));

        assert!(is_generic_artist("Various Artists"));
        assert!(is_generic_artist("Unknown Artist"));
        assert!(!is_generic_artist("Ado"));

        let q_album = ArtworkQuery::from_metadata("Ado", "  Kyougen ", "Usseewa").unwrap();
        assert!(q_album.use_album);
        assert_eq!(q_album.cache_key, "album::ado::kyougen");

        let q_track =
            ArtworkQuery::from_metadata("Ado", "Unknown Album (Ado)", "Usseewa").unwrap();
        assert!(!q_track.use_album);
        assert_eq!(q_track.cache_key, "track::ado::usseewa");

        let q_va = ArtworkQuery::from_metadata("Various Artists", "Arcane OST", "Enemy").unwrap();
        assert!(q_va.use_album);
        assert_eq!(q_va.cache_key, "album_only::arcane ost");

        assert!(ArtworkQuery::from_metadata("Unknown Artist", "Unknown Album", "").is_none());
        assert!(ArtworkQuery::from_metadata("Unknown Artist", "Unknown Album", "Intro").is_none());
        assert!(ArtworkQuery::from_metadata("Various Artists", "Greatest Hits", "Track 1").is_none());
    }

    #[test]
    fn normalizes_primary_artist_and_cleans_album_edition_tags() {
        assert_eq!(primary_artist("Ado; B'z"), "Ado");
        assert_eq!(primary_artist("Robert del Naja feat. Someone"), "Robert del Naja");
        assert_eq!(primary_artist("Simon & Garfunkel"), "Simon & Garfunkel");
        assert_eq!(primary_artist("SawanoHiroyuki[nZk]"), "SawanoHiroyuki[nZk]");

        assert_eq!(clean_album_title("Kyougen (Deluxe Edition) [FLAC]"), "Kyougen");
        assert_eq!(clean_album_title("Hibana Disc 1"), "Hibana");
        assert_eq!(clean_album_title("Lone Trail (Part 2)"), "Lone Trail (Part 2)");
        assert_eq!(clean_album_title("X (Orbital)"), "X (Orbital)");
        assert_eq!(clean_album_title("Daft Punk (Discovery)"), "Daft Punk (Discovery)");
        assert_eq!(clean_album_title("Test (ABCD)"), "Test (ABCD)");
    }

    #[test]
    fn validates_artwork_urls_and_upgrades_itunes_size() {
        let upgraded = upgrade_itunes_artwork_url(
            "https://is1-ssl.mzstatic.com/image/thumb/Music126/v4/24/e9/c2/21UM1IM54074.rgb.jpg/100x100bb.jpg",
        )
        .unwrap();
        assert!(upgraded.ends_with("/512x512bb.jpg"));

        assert!(validate_artwork_url(
            "https://archive.org/download/mbid-123/thumb500.jpg"
        ).is_some());
        assert!(validate_artwork_url(
            "http://is1-ssl.mzstatic.com/image/thumb/x/512x512bb.jpg"
        ).is_none());
        assert!(validate_artwork_url("https://evil.example.com/cover.jpg").is_none());
        assert!(validate_artwork_url("https://user@is1-ssl.mzstatic.com/a.jpg").is_none());
        assert!(validate_artwork_url("https://evil.com\\.mzstatic.com/x.jpg").is_none());
        assert!(validate_artwork_url("https://evil.com%2e.mzstatic.com/x.jpg").is_none());
        assert!(validate_artwork_url(
            &format!("https://is1-ssl.mzstatic.com/{}.jpg", "a".repeat(240))
        ).is_none());

        assert!(is_valid_mbid("aa997ea0-2936-40bd-884d-3af8a0e064dc"));
        assert!(!is_valid_mbid("not-a-uuid"));
        assert_eq!(
            escape_lucene("Ado: (Live!) [2025]"),
            "Ado\\: \\(Live\\!\\) \\[2025\\]"
        );
    }

    #[test]
    fn itunes_scoring_picks_exact_handles_cjk_and_rejects_wrong_artist() {
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
        ];
        assert_eq!(
            pick_best_itunes_artwork(&results, &q_exact, ItunesEntity::Album).as_deref(),
            Some("https://is1-ssl.mzstatic.com/b/512x512bb.jpg")
        );

        let q_cjk = ArtworkQuery::from_metadata("Ado", "狂言", "うっせぇわ").unwrap();
        let cjk_results = vec![json!({
            "artistName": "ADO",
            "collectionName": "Kyougen",
            "artworkUrl100": "https://is1-ssl.mzstatic.com/c/100x100bb.jpg",
        })];
        assert_eq!(
            pick_best_itunes_artwork(&cjk_results, &q_cjk, ItunesEntity::Album).as_deref(),
            Some("https://is1-ssl.mzstatic.com/c/512x512bb.jpg")
        );

        // Short artist name must NOT match inside unrelated Latin words (e.g. "Ado" in "Shadow").
        let q_short = ArtworkQuery::from_metadata("Ado", "Shadow", "Shadow").unwrap();
        let substring_trap = vec![json!({
            "artistName": "Shadow",
            "collectionName": "Shadow",
            "artworkUrl100": "https://is1-ssl.mzstatic.com/trap/100x100bb.jpg",
        })];
        assert!(
            pick_best_itunes_artwork(&substring_trap, &q_short, ItunesEntity::Album).is_none()
        );

        // Whole-token feature match ("Ado" in "Ado feat. Miku") still succeeds.
        let token_feat = vec![json!({
            "artistName": "Ado feat. Miku",
            "collectionName": "Kyougen",
            "artworkUrl100": "https://is1-ssl.mzstatic.com/feat/100x100bb.jpg",
        })];
        assert_eq!(
            pick_best_itunes_artwork(&token_feat, &q_exact, ItunesEntity::Album).as_deref(),
            Some("https://is1-ssl.mzstatic.com/feat/512x512bb.jpg")
        );

        // Cross-script artist (kana/kanji vs romanized) with exact non-trivial album succeeds.
        let q_cross = ArtworkQuery::from_metadata("ヨルシカ", "Elma", "Rain with Cappuccino").unwrap();
        let cross_hit = vec![json!({
            "artistName": "Yorushika",
            "collectionName": "Elma",
            "artworkUrl100": "https://is1-ssl.mzstatic.com/elma/100x100bb.jpg",
        })];
        assert_eq!(
            pick_best_itunes_artwork(&cross_hit, &q_cross, ItunesEntity::Album).as_deref(),
            Some("https://is1-ssl.mzstatic.com/elma/512x512bb.jpg")
        );

        // Same-script wrong artist with exact same album title is strictly rejected.
        let q_same_script = ArtworkQuery::from_metadata("Taylor Swift", "Red", "Red").unwrap();
        let wrong_artist_exact_album = vec![json!({
            "artistName": "King Crimson",
            "collectionName": "Red",
            "artworkUrl100": "https://is1-ssl.mzstatic.com/kc/100x100bb.jpg",
        })];
        assert!(
            pick_best_itunes_artwork(&wrong_artist_exact_album, &q_same_script, ItunesEntity::Album)
                .is_none()
        );
    }

    #[test]
    fn cache_persists_hits_misses_respects_ttl_and_skips_transient_errors() {
        let path = temp_cache_path("roundtrip");
        let now_unix = now_unix_u64();
        let now_inst = Instant::now();
        let url = "https://is1-ssl.mzstatic.com/image/thumb/a/512x512bb.jpg".to_owned();

        let mut cache = ArtworkCache::load(Some(path.clone()));
        cache.record(
            "album::ado::kyougen".to_owned(),
            &LookupOutcome::Found(url.clone()),
            now_unix,
            now_inst,
        );
        cache.record(
            "album::x::missing".to_owned(),
            &LookupOutcome::NotFound,
            now_unix,
            now_inst,
        );
        cache.record(
            "album::x::offline".to_owned(),
            &LookupOutcome::TransientError,
            now_unix,
            now_inst,
        );

        assert_eq!(
            cache.get("album::x::offline", now_unix, now_inst),
            CacheLookup::Backoff
        );
        assert_eq!(
            cache.get(
                "album::x::offline",
                now_unix,
                now_inst + Duration::from_secs(61)
            ),
            CacheLookup::Miss
        );

        let reloaded = ArtworkCache::load(Some(path.clone()));
        assert_eq!(
            reloaded.get("album::ado::kyougen", now_unix, now_inst),
            CacheLookup::Hit(Some(url))
        );
        assert_eq!(
            reloaded.get("album::x::missing", now_unix, now_inst),
            CacheLookup::Hit(None)
        );
        // Transient errors are not persisted across reloads.
        assert_eq!(
            reloaded.get("album::x::offline", now_unix, now_inst),
            CacheLookup::Miss
        );
        // Negative hit expires after 24h while positive hit remains valid up to 30d.
        assert_eq!(
            reloaded.get("album::x::missing", now_unix + MISS_TTL_SECS + 1, now_inst),
            CacheLookup::Miss
        );
        assert!(matches!(
            reloaded.get("album::ado::kyougen", now_unix + MISS_TTL_SECS + 1, now_inst),
            CacheLookup::Hit(Some(_))
        ));

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn cache_evicts_oldest_entries_past_limit() {
        let path = temp_cache_path("evict");
        let base_unix = now_unix_u64();
        let now_inst = Instant::now();
        let mut cache = ArtworkCache::load(Some(path.clone()));

        for i in 0..1005u64 {
            cache.entries.insert(
                format!("k::{i}"),
                CacheEntry {
                    url: Some(format!(
                        "https://is1-ssl.mzstatic.com/image/thumb/{i}/512x512bb.jpg"
                    )),
                    fetched_at: base_unix.saturating_sub(1005 - i),
                },
            );
        }
        cache.record(
            "k::1005".to_owned(),
            &LookupOutcome::Found(
                "https://is1-ssl.mzstatic.com/image/thumb/1005/512x512bb.jpg".to_owned(),
            ),
            base_unix,
            now_inst,
        );

        assert_eq!(cache.entries.len(), MAX_CACHE_ENTRIES);
        assert!(!cache.entries.contains_key("k::0"));
        assert!(!cache.entries.contains_key("k::5"));
        assert!(cache.entries.contains_key("k::6"));
        assert!(cache.entries.contains_key("k::1005"));

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn validates_deezer_and_discogs_cdn_urls_and_lengths() {
        let dz = "https://cdn-images.dzcdn.net/images/cover/231c66c0ed260c708163eff3bb8458da/500x500-000000-80-0-0.jpg";
        assert_eq!(validate_artwork_url(dz).as_deref(), Some(dz));
        assert_eq!(provider_for_url(dz), Some("deezer"));

        let dg_short = "https://i.discogs.com/abc123/rs:fit/g:sm/q:90/h:600/w:600/release.jpeg";
        assert_eq!(validate_artwork_url(dg_short).as_deref(), Some(dg_short));
        assert_eq!(provider_for_url(dg_short), Some("discogs"));

        let dg_long = format!("https://i.discogs.com/{}/cover.jpeg", "x".repeat(260));
        assert!(validate_artwork_url(&dg_long).is_none());
        assert_eq!(validate_download_url(&dg_long).as_deref(), Some(dg_long.as_str()));
    }

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
            Some("https://cdn-images.dzcdn.net/images/cover/231c66c0ed260c708163eff3bb8458da/500x500-000000-80-0-0.jpg")
        );

        let q_wrong = ArtworkQuery::from_metadata("Taylor Swift", "Red", "Red").unwrap();
        let wrong_artist = vec![json!({
            "title": "Red",
            "md5_image": "abcd",
            "cover_big": "https://cdn-images.dzcdn.net/images/cover/abcd/500x500-000000-80-0-0.jpg",
            "artist": { "name": "King Crimson" }
        })];
        assert!(pick_best_deezer_artwork(&wrong_artist, &q_wrong, DeezerEntity::Album).is_none());

        let q_cross = ArtworkQuery::from_metadata("ヨルシカ", "Elma", "Rain").unwrap();
        let cross_hit = vec![json!({
            "title": "Elma",
            "md5_image": "elma",
            "cover_big": "http://cdn-images.dzcdn.net/images/cover/elma/500x500-000000-80-0-0.jpg",
            "artist": { "name": "Yorushika" }
        })];
        assert_eq!(
            pick_best_deezer_artwork(&cross_hit, &q_cross, DeezerEntity::Album).as_deref(),
            Some("https://cdn-images.dzcdn.net/images/cover/elma/500x500-000000-80-0-0.jpg")
        );
    }

    #[test]
    fn discogs_scoring_parses_disambig_and_ignores_spacer_gif() {
        assert_eq!(strip_discogs_disambig("Ado (3)"), "Ado");
        assert_eq!(strip_discogs_disambig("SawanoHiroyuki[nZk]"), "SawanoHiroyuki[nZk]");
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

    #[test]
    fn providers_filter_urls_and_prune_cache() {
        let path = temp_cache_path("prune-providers");
        let now_unix = now_unix_u64();
        let now_inst = Instant::now();
        let mut cache = ArtworkCache::load(Some(path.clone()));
        let it_url = "https://is1-ssl.mzstatic.com/image/thumb/a/512x512bb.jpg".to_owned();
        let dz_url = "https://cdn-images.dzcdn.net/images/cover/abc/500x500-000000-80-0-0.jpg".to_owned();
        let dg_url = "https://i.discogs.com/good/cover.jpg".to_owned();

        cache.record("k::it".to_owned(), &LookupOutcome::Found(it_url.clone()), now_unix, now_inst);
        cache.record("k::dz".to_owned(), &LookupOutcome::Found(dz_url.clone()), now_unix, now_inst);
        cache.record("k::dg".to_owned(), &LookupOutcome::Found(dg_url.clone()), now_unix, now_inst);
        cache.record("k::miss".to_owned(), &LookupOutcome::NotFound, now_unix, now_inst);

        let providers = ArtworkProviders {
            itunes: true,
            deezer: false,
            musicbrainz: false,
            discogs: true,
            discogs_token: "token123".to_owned(),
        };
        assert!(providers.allows_url(&it_url));
        assert!(!providers.allows_url(&dz_url));
        assert!(providers.allows_url(&dg_url));

        cache.prune_for_providers(&providers);
        assert!(matches!(cache.get("k::it", now_unix, now_inst), CacheLookup::Hit(Some(_))));
        assert_eq!(cache.get("k::dz", now_unix, now_inst), CacheLookup::Miss);
        assert!(matches!(cache.get("k::dg", now_unix, now_inst), CacheLookup::Hit(Some(_))));
        assert_eq!(cache.get("k::miss", now_unix, now_inst), CacheLookup::Miss);

        // Discogs disabled when token is empty even if discogs flag is true.
        let no_token = ArtworkProviders { discogs_token: "  ".to_owned(), ..providers };
        assert!(!no_token.allows_url(&dg_url));

        let _ = fs::remove_file(&path);
    }
}

