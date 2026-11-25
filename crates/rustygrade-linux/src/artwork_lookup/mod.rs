use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::{Duration, Instant};

use thiserror::Error;

pub mod cache;
mod deezer;
mod discogs;
mod http;
mod itunes;
mod musicbrainz;
pub mod normalize;
mod scoring;

pub use cache::{ArtworkCache, CacheLookup};
pub use discogs::strip_discogs_disambig;
pub use normalize::{
    clean_album_title, contains_cjk, escape_lucene, is_generic_album, is_generic_artist,
    is_valid_mbid, now_unix_u64, primary_artist, provider_for_url, upgrade_itunes_artwork_url,
    validate_artwork_url,
};
use normalize::{collapse_spaces, is_ambiguous_album_only, norm_key};

pub(super) const USER_AGENT: &str = concat!(
    "Rustygrade/",
    env!("CARGO_PKG_VERSION"),
    " (https://github.com/Okarin2507/rusty-rustygrade)"
);
pub(super) const HTTP_TIMEOUT: Duration = Duration::from_secs(4);
pub(super) const IMAGE_TIMEOUT: Duration = Duration::from_secs(15);
pub(super) const MAX_HTTP_BODY_BYTES: u64 = 256 * 1024;
pub(super) const MAX_SEARCH_BODY_BYTES: u64 = 1024 * 1024;
pub(super) const MAX_IMAGE_BYTES: u64 = 8 * 1024 * 1024;
pub(super) const MAX_CANDIDATES: usize = 8;

/// Edge of a cover fetched for the tag editor (iTunes renders any size; the Cover Art Archive
/// serves 250 / 500 / 1200, so it gets 1200). It is embedded in every file of an album edit,
/// so it stays modest; the app's own copy is capped at 720 px anyway.
pub const FETCH_COVER_EDGE: u32 = 1000;

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

pub trait ArtworkResolver: Send + Sync {
    fn resolve(&self, query: &ArtworkQuery, cancel: &AtomicBool) -> LookupOutcome;
    fn set_providers(&self, _providers: ArtworkProviders) {}
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

fn sanitize_token(raw: &str) -> String {
    raw.chars()
        .filter(|c| !c.is_control())
        .collect::<String>()
        .trim()
        .to_owned()
}

impl ArtworkProviders {
    pub fn from_settings(s: &rustygrade_core::AppSettings) -> Self {
        Self {
            itunes: s.artwork_itunes,
            deezer: s.artwork_deezer,
            musicbrainz: s.artwork_musicbrainz,
            discogs: s.artwork_discogs,
            discogs_token: sanitize_token(&s.discogs_token),
        }
    }

    pub fn effective_discogs_token(&self) -> Option<&str> {
        self.discogs
            .then(|| self.discogs_token.trim())
            .filter(|t| !t.is_empty() && t.bytes().all(|b| b.is_ascii_graphic()))
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
            (
                false,
                format!("track::{}::{}", norm_key(&artist), norm_key(&title)),
            )
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

    pub(super) fn has_cjk(&self) -> bool {
        contains_cjk(&self.artist) || contains_cjk(&self.album) || contains_cjk(&self.title)
    }

    pub(super) fn has_searchable_artist(&self) -> bool {
        !is_generic_artist(&self.artist)
    }
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

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum FetchCoverError {
    #[error("No matching cover found.")]
    NotFound,
    #[error("Network error while fetching cover.")]
    Network,
    /// The service is rate-limiting or overloaded (MusicBrainz answers 503).
    #[error("Cover service is busy; try again shortly.")]
    Busy,
    /// Discogs rejected the configured Personal Access Token (401/403).
    #[error("Discogs token was rejected (401/403).")]
    Unauthorized,
    #[error("Cover fetch cancelled.")]
    Cancelled,
}

/// Online metadata-based album artwork resolver (iTunes -> Deezer -> MusicBrainz + CAA -> Discogs).
pub struct OnlineArtworkResolver {
    pub(super) agent: ureq::Agent,
    pub(super) image_agent: ureq::Agent,
    pub(super) mb_last_call: Mutex<Option<Instant>>,
    pub(super) discogs_last_call: Mutex<Option<Instant>>,
    providers: RwLock<ArtworkProviders>,
}

/// The process-wide resolver: Discord RPC and the tag editor share it, so the MusicBrainz
/// rate-limit gap and the connection pool are shared too.
pub fn shared_resolver() -> Arc<OnlineArtworkResolver> {
    static SHARED: OnceLock<Arc<OnlineArtworkResolver>> = OnceLock::new();
    Arc::clone(SHARED.get_or_init(|| Arc::new(OnlineArtworkResolver::new())))
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
            .max_redirects(0)
            .max_redirects_will_error(false)
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
        let mut guard = self.providers.write().unwrap_or_else(|p| p.into_inner());
        *guard = providers;
    }

    pub fn providers(&self) -> ArtworkProviders {
        self.providers
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }
}

impl ArtworkResolver for OnlineArtworkResolver {
    fn set_providers(&self, providers: ArtworkProviders) {
        OnlineArtworkResolver::set_providers(self, providers);
    }

    fn resolve(&self, query: &ArtworkQuery, cancel: &AtomicBool) -> LookupOutcome {
        if cancel.load(std::sync::atomic::Ordering::Relaxed) {
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
        if cancel.load(std::sync::atomic::Ordering::Relaxed) {
            return LookupOutcome::TransientError;
        }
        if providers.deezer
            && let Some(url) = self.try_deezer(query, cancel, &mut had_transient_error)
        {
            return LookupOutcome::Found(url);
        }
        if cancel.load(std::sync::atomic::Ordering::Relaxed) {
            return LookupOutcome::TransientError;
        }
        if providers.musicbrainz
            && let Some(url) = self.try_musicbrainz_caa(query, cancel, &mut had_transient_error)
        {
            return LookupOutcome::Found(url);
        }
        if cancel.load(std::sync::atomic::Ordering::Relaxed) {
            return LookupOutcome::TransientError;
        }
        if let Some(token) = providers.effective_discogs_token()
            && let Some(url) = self.try_discogs(query, token, cancel, &mut had_transient_error)
        {
            return LookupOutcome::Found(url);
        }
        if cancel.load(std::sync::atomic::Ordering::Relaxed) || had_transient_error {
            LookupOutcome::TransientError
        } else {
            LookupOutcome::NotFound
        }
    }
}
