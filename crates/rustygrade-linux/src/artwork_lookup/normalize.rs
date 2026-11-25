use std::time::{SystemTime, UNIX_EPOCH};

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

pub(super) fn is_ambiguous_album_only(album: &str) -> bool {
    let norm = normalize_for_match(album);
    norm.chars().count() < 4 || AMBIGUOUS_ALBUM_ONLY_TITLES.contains(&norm.as_str())
}

/// Extracts the leading artist name when tags list multiple collaborations or features.
pub fn primary_artist(artist: &str) -> &str {
    let trimmed = artist.trim();
    let lower = trimmed.to_ascii_lowercase();
    let seps = [";", " / ", " feat. ", " ft. ", " featuring ", " × "];
    let earliest = seps.iter().filter_map(|sep| lower.find(sep)).min();
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
    has_edition_word.then_some(&s[..start])
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

pub(super) fn extract_https_host(url: &str) -> Option<&str> {
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
    let (host, port) = match authority.split_once(':') {
        Some((h, p)) => (h, Some(p)),
        None => (authority, None),
    };
    if host.is_empty() {
        return None;
    }
    if let Some(p) = port
        && (p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()))
    {
        return None;
    }
    Some(host)
}

pub(super) fn resolve_redirect_url(base_url: &str, location: &str) -> Option<String> {
    let loc = normalize_https(location);
    if loc.starts_with("https://") {
        return Some(loc);
    }
    if loc.starts_with('/') {
        let host = extract_https_host(base_url)?;
        return Some(format!("https://{host}{loc}"));
    }
    None
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
    allowed.then(|| trimmed.to_owned())
}

/// Validates that a remote artwork URL is safe, HTTPS, within Discord's 256-char limit,
/// and hosted on one of the expected CDN domains.
pub fn validate_artwork_url(url: &str) -> Option<String> {
    validate_https_cdn_url(url, MAX_DISCORD_IMAGE_LEN)
}

pub(super) fn validate_download_url(url: &str) -> Option<String> {
    validate_https_cdn_url(url, MAX_DOWNLOAD_URL_LEN)
}

pub(super) fn normalize_https(raw: &str) -> String {
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
    let normalized = normalize_https(raw);
    let upgraded = if normalized.contains("/100x100bb") {
        normalized.replacen("/100x100bb", "/512x512bb", 1)
    } else if normalized.contains("/60x60bb") {
        normalized.replacen("/60x60bb", "/512x512bb", 1)
    } else {
        normalized
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

pub(super) fn collapse_spaces(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub(super) fn norm_key(s: &str) -> String {
    collapse_spaces(s).to_lowercase()
}

pub(super) fn normalize_for_match(s: &str) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::artwork_lookup::ArtworkQuery;

    #[test]
    fn normalizes_metadata_keys_editions_and_primary_artist() {
        assert!(
            is_generic_album("")
                && is_generic_album("Unknown Album (Ado)")
                && !is_generic_album("Kyougen")
        );
        assert!(is_generic_artist("Various Artists") && !is_generic_artist("Ado"));

        let q_album = ArtworkQuery::from_metadata("Ado", "  Kyougen ", "Usseewa").unwrap();
        assert!(q_album.use_album);
        assert_eq!(q_album.cache_key, "album::ado::kyougen");
        let q_track = ArtworkQuery::from_metadata("Ado", "Unknown Album (Ado)", "Usseewa").unwrap();
        assert!(!q_track.use_album);
        assert_eq!(q_track.cache_key, "track::ado::usseewa");
        let q_va = ArtworkQuery::from_metadata("Various Artists", "Arcane OST", "Enemy").unwrap();
        assert_eq!(q_va.cache_key, "album_only::arcane ost");
        assert!(ArtworkQuery::from_metadata("Unknown Artist", "Unknown Album", "Intro").is_none());
        assert!(
            ArtworkQuery::from_metadata("Various Artists", "Greatest Hits", "Track 1").is_none()
        );

        assert_eq!(primary_artist("Ado; B'z"), "Ado");
        assert_eq!(
            primary_artist("Robert del Naja feat. Someone"),
            "Robert del Naja"
        );
        assert_eq!(primary_artist("Simon & Garfunkel"), "Simon & Garfunkel");

        assert_eq!(
            clean_album_title("Kyougen (Deluxe Edition) [FLAC]"),
            "Kyougen"
        );
        assert_eq!(clean_album_title("Hibana Disc 1"), "Hibana");
        assert_eq!(
            clean_album_title("Lone Trail (Part 2)"),
            "Lone Trail (Part 2)"
        );
    }

    #[test]
    fn validates_artwork_urls_hosts_ports_mbid_and_lucene() {
        let up =
            upgrade_itunes_artwork_url("http://is1-ssl.mzstatic.com/image/thumb/a/100x100bb.jpg")
                .unwrap();
        assert_eq!(
            up,
            "https://is1-ssl.mzstatic.com/image/thumb/a/512x512bb.jpg"
        );

        assert!(
            validate_artwork_url("https://archive.org/download/mbid-123/thumb500.jpg").is_some()
        );
        assert!(validate_artwork_url("http://is1-ssl.mzstatic.com/a.jpg").is_none());
        assert!(validate_artwork_url("https://evil.example.com/cover.jpg").is_none());
        assert!(validate_artwork_url("https://user@is1-ssl.mzstatic.com/a.jpg").is_none());
        assert!(validate_artwork_url("https://is1-ssl.mzstatic.com:443/a.jpg").is_some());
        assert!(validate_artwork_url("https://is1-ssl.mzstatic.com:bad:1/a.jpg").is_none());
        assert!(
            validate_artwork_url(&format!(
                "https://is1-ssl.mzstatic.com/{}.jpg",
                "a".repeat(240)
            ))
            .is_none()
        );

        let dg_long = format!("https://i.discogs.com/{}/cover.jpeg", "x".repeat(260));
        assert!(validate_artwork_url(&dg_long).is_none());
        assert_eq!(
            validate_download_url(&dg_long).as_deref(),
            Some(dg_long.as_str())
        );

        assert!(is_valid_mbid("aa997ea0-2936-40bd-884d-3af8a0e064dc"));
        assert!(!is_valid_mbid("not-a-uuid"));
        assert_eq!(
            escape_lucene("Ado: (Live!) [2025]"),
            "Ado\\: \\(Live\\!\\) \\[2025\\]"
        );
    }
}
