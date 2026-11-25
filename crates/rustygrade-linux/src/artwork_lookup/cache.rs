use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::normalize::{now_unix_u64, validate_artwork_url};
use super::{ArtworkProviders, LookupOutcome};

static CACHE_TMP_SEQ: AtomicU64 = AtomicU64::new(1);

const CACHE_VERSION: u32 = 1;
const HIT_TTL_SECS: u64 = 30 * 86_400;
const MISS_TTL_SECS: u64 = 86_400;
const TRANSIENT_BACKOFF: Duration = Duration::from_secs(60);
const MAX_CACHE_ENTRIES: usize = 1000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CacheLookup {
    Hit(Option<String>),
    Backoff,
    Miss,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CacheFile {
    version: u32,
    entries: HashMap<String, CacheEntry>,
}

#[derive(Serialize)]
struct CacheFileRef<'a> {
    version: u32,
    entries: &'a HashMap<String, CacheEntry>,
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

    /// Used on startup: drops cached hits from currently disabled providers while preserving
    /// 24-hour negative-cache (`NotFound`) entries so missing albums aren't hammered on every launch.
    pub fn prune_disallowed_urls(&mut self, providers: &ArtworkProviders) {
        let before = self.entries.len();
        self.entries.retain(|_, entry| match entry.url.as_deref() {
            Some(url) => providers.allows_url(url),
            None => true,
        });
        if self.entries.len() != before {
            self.prune_and_persist(now_unix_u64());
        }
    }

    /// Used when provider settings change interactively: drops disallowed hits AND clears misses
    /// so previously missing albums can be retried against newly enabled providers.
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
        let file = CacheFileRef {
            version: CACHE_VERSION,
            entries: &self.entries,
        };
        let seq = CACHE_TMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let tmp = path.with_extension(format!("json.{}.{seq}.tmp", std::process::id()));
        if let Ok(json) = serde_json::to_string(&file)
            && fs::write(&tmp, json).is_ok()
            && fs::rename(&tmp, path).is_ok()
        {
            return;
        }
        let _ = fs::remove_file(&tmp);
    }
}

fn is_entry_fresh(entry: &CacheEntry, now_unix: u64) -> bool {
    if entry.fetched_at > now_unix {
        return false;
    }
    let ttl = if entry.url.is_some() {
        HIT_TTL_SECS
    } else {
        MISS_TTL_SECS
    };
    now_unix - entry.fetched_at < ttl
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_cache_path(tag: &str) -> PathBuf {
        let seq = CACHE_TMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!(
            "rg-art-cache-{}-{tag}-{seq}.json",
            std::process::id()
        ));
        let _ = fs::remove_file(&p);
        p
    }

    #[test]
    fn cache_persists_hits_misses_respects_ttl_future_clock_and_evicts() {
        let path = temp_cache_path("roundtrip");
        let now_unix = now_unix_u64();
        let now_inst = Instant::now();
        let url = "https://is1-ssl.mzstatic.com/image/thumb/a/512x512bb.jpg".to_owned();

        let mut cache = ArtworkCache::load(Some(path.clone()));
        cache.record(
            "album::ado::kyougen".into(),
            &LookupOutcome::Found(url.clone()),
            now_unix,
            now_inst,
        );
        cache.record(
            "album::x::missing".into(),
            &LookupOutcome::NotFound,
            now_unix,
            now_inst,
        );
        cache.record(
            "album::x::offline".into(),
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
        assert_eq!(
            reloaded.get("album::x::offline", now_unix, now_inst),
            CacheLookup::Miss
        );
        // Future timestamp is never trusted.
        assert_eq!(
            reloaded.get("album::ado::kyougen", now_unix.saturating_sub(1), now_inst),
            CacheLookup::Miss
        );
        // Miss expires after 24h while hit lasts 30d.
        assert_eq!(
            reloaded.get("album::x::missing", now_unix + MISS_TTL_SECS + 1, now_inst),
            CacheLookup::Miss
        );
        assert!(matches!(
            reloaded.get(
                "album::ado::kyougen",
                now_unix + MISS_TTL_SECS + 1,
                now_inst
            ),
            CacheLookup::Hit(Some(_))
        ));

        // LRU eviction past 1000 entries.
        let evict_path = temp_cache_path("evict");
        let mut evict_cache = ArtworkCache::load(Some(evict_path.clone()));
        for i in 0..1005u64 {
            evict_cache.entries.insert(
                format!("k::{i}"),
                CacheEntry {
                    url: Some(format!(
                        "https://is1-ssl.mzstatic.com/image/thumb/{i}/512x512bb.jpg"
                    )),
                    fetched_at: now_unix.saturating_sub(1005 - i),
                },
            );
        }
        evict_cache.record(
            "k::1005".into(),
            &LookupOutcome::Found(
                "https://is1-ssl.mzstatic.com/image/thumb/1005/512x512bb.jpg".into(),
            ),
            now_unix,
            now_inst,
        );
        assert_eq!(evict_cache.entries.len(), MAX_CACHE_ENTRIES);
        assert!(
            !evict_cache.entries.contains_key("k::0") && evict_cache.entries.contains_key("k::6")
        );

        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(&evict_path);
    }

    #[test]
    fn providers_prune_disallowed_urls_vs_provider_change() {
        let path = temp_cache_path("prune-providers");
        let now_unix = now_unix_u64();
        let now_inst = Instant::now();
        let mut cache = ArtworkCache::load(Some(path.clone()));
        let it_url = "https://is1-ssl.mzstatic.com/image/thumb/a/512x512bb.jpg".to_owned();
        let dz_url =
            "https://cdn-images.dzcdn.net/images/cover/abc/500x500-000000-80-0-0.jpg".to_owned();
        let dg_url = "https://i.discogs.com/good/cover.jpg".to_owned();

        cache.record(
            "k::it".into(),
            &LookupOutcome::Found(it_url.clone()),
            now_unix,
            now_inst,
        );
        cache.record(
            "k::dz".into(),
            &LookupOutcome::Found(dz_url.clone()),
            now_unix,
            now_inst,
        );
        cache.record(
            "k::dg".into(),
            &LookupOutcome::Found(dg_url.clone()),
            now_unix,
            now_inst,
        );
        cache.record(
            "k::miss".into(),
            &LookupOutcome::NotFound,
            now_unix,
            now_inst,
        );

        let providers = ArtworkProviders {
            itunes: true,
            deezer: false,
            musicbrainz: false,
            discogs: true,
            discogs_token: "token123".to_owned(),
        };
        cache.prune_disallowed_urls(&providers);
        assert!(matches!(
            cache.get("k::it", now_unix, now_inst),
            CacheLookup::Hit(Some(_))
        ));
        assert_eq!(cache.get("k::dz", now_unix, now_inst), CacheLookup::Miss);
        assert!(matches!(
            cache.get("k::dg", now_unix, now_inst),
            CacheLookup::Hit(Some(_))
        ));
        // Startup prune keeps 24h misses.
        assert_eq!(
            cache.get("k::miss", now_unix, now_inst),
            CacheLookup::Hit(None)
        );

        // Interactive provider change clears misses too.
        cache.prune_for_providers(&providers);
        assert_eq!(cache.get("k::miss", now_unix, now_inst), CacheLookup::Miss);
        assert!(
            !ArtworkProviders {
                discogs_token: "tok\nen".into(),
                ..providers
            }
            .allows_url(&dg_url)
        );

        let _ = fs::remove_file(&path);
    }
}
