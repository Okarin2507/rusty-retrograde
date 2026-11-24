//! FETCH ONLINE in the tag editor: searches iTunes, Deezer, MusicBrainz + Cover Art Archive, and
//! Discogs (using the providers enabled in settings) for covers to choose from, then saves the
//! chosen one to a temp file that the tag writer embeds on SAVE, exactly like a picked file.
//!
//! The picker gets each service's candidates with their thumbnails (250 px, held in memory only
//! while it is open); only the chosen cover is downloaded at full size. The files live in
//! `~/.rustygrade/tmp/fetched-covers`, one unique name per download (the cover cache keys
//! previews by path). They are removed once the editor closes or saves, and whatever was left
//! behind (a crash, a page change mid-edit) is swept at startup.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use iced::widget::image::Handle;
use rustygrade_core::CoverPaths;
use rustygrade_linux::artwork_lookup::{ArtworkQuery, CoverCandidate, FetchCoverError, shared_resolver};

static SEQ: AtomicU64 = AtomicU64::new(1);

/// Which service a search asks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Service {
    Itunes,
    Deezer,
    MusicBrainz,
    Discogs,
}

/// A candidate whose thumbnail arrived.
#[derive(Debug, Clone)]
pub struct Found {
    pub cand: CoverCandidate,
    pub thumb: Handle,
}

/// The cover picker over the tag editor: its query fields and what each service returned.
#[derive(Debug, Clone, Default)]
pub struct Picker {
    pub artist: String,
    pub album: String,
    /// The track title (a single-track edit with no album searches songs by it).
    pub title: String,
    /// The search these results belong to.
    pub seq: u64,
    pub itunes: Vec<Found>,
    pub deezer: Vec<Found>,
    pub musicbrainz: Vec<Found>,
    pub discogs: Vec<Found>,
    pub itunes_pending: bool,
    pub deezer_pending: bool,
    pub musicbrainz_pending: bool,
    pub discogs_pending: bool,
    /// A service failed (shown when nothing was found at all).
    pub error: Option<String>,
    /// The chosen candidate, by thumbnail URL.
    pub selected: Option<String>,
    /// The chosen cover is downloading at full size.
    pub downloading: bool,
}

impl Picker {
    /// iTunes first (best match leading), then Deezer, MusicBrainz, and Discogs.
    pub fn items(&self) -> impl Iterator<Item = &Found> {
        self.itunes
            .iter()
            .chain(&self.deezer)
            .chain(&self.musicbrainz)
            .chain(&self.discogs)
    }

    pub fn total_found(&self) -> usize {
        self.itunes.len() + self.deezer.len() + self.musicbrainz.len() + self.discogs.len()
    }

    pub fn selected_candidate(&self) -> Option<&CoverCandidate> {
        let key = self.selected.as_deref()?;
        self.items().map(|f| &f.cand).find(|c| c.thumb_url == key)
    }

    pub fn searching(&self) -> bool {
        self.itunes_pending || self.deezer_pending || self.musicbrainz_pending || self.discogs_pending
    }

    /// Takes one service's answer to search `seq` (a stale one is ignored).
    pub fn receive(&mut self, seq: u64, service: Service, res: Result<Vec<Found>, String>) {
        if seq != self.seq {
            return;
        }
        let found = match res {
            Ok(v) => v,
            Err(e) => {
                if !e.starts_with("NO COVER") {
                    let name = match service {
                        Service::Itunes => "ITUNES",
                        Service::Deezer => "DEEZER",
                        Service::MusicBrainz => "MUSICBRAINZ",
                        Service::Discogs => "DISCOGS",
                    };
                    self.error = Some(format!("{name}: {e}"));
                }
                Vec::new()
            }
        };
        match service {
            Service::Itunes => {
                self.itunes = found;
                self.itunes_pending = false;
            }
            Service::Deezer => {
                self.deezer = found;
                self.deezer_pending = false;
            }
            Service::MusicBrainz => {
                self.musicbrainz = found;
                self.musicbrainz_pending = false;
            }
            Service::Discogs => {
                self.discogs = found;
                self.discogs_pending = false;
            }
        }
        if self.selected.is_none() {
            let best = self.items().find(|f| f.cand.best).map(|f| f.cand.thumb_url.clone());
            self.selected = best;
        }
    }
}

pub fn dir() -> PathBuf {
    CoverPaths::app_dir().join("tmp").join("fetched-covers")
}

/// Removes what earlier runs left behind.
pub fn sweep() {
    let _ = std::fs::remove_dir_all(dir());
}

/// Deletes a fetched file; any other path (a cover the user picked) is left alone.
pub fn discard(path: &Path) {
    if path.starts_with(dir()) {
        let _ = std::fs::remove_file(path);
    }
}

/// Whether a lookup can be made from these fields at all.
pub fn query(artist: &str, album: &str, title: &str) -> Option<ArtworkQuery> {
    ArtworkQuery::from_metadata(artist, album, title)
}

fn message(e: FetchCoverError) -> String {
    match e {
        FetchCoverError::NotFound => "NO COVER FOUND ONLINE".to_owned(),
        FetchCoverError::Network => "LOOKUP FAILED // CHECK THE CONNECTION".to_owned(),
        FetchCoverError::Busy => "SERVICE BUSY // TRY AGAIN SHORTLY".to_owned(),
        FetchCoverError::Unauthorized => "INVALID DISCOGS TOKEN".to_owned(),
        FetchCoverError::Cancelled => "LOOKUP_CANCELLED".to_owned(),
    }
}

/// Runs `job` on its own thread (the lookups block) and awaits it.
async fn off_thread<T: Send + 'static>(job: impl FnOnce() -> Result<T, String> + Send + 'static) -> Result<T, String> {
    let (tx, rx) = iced::futures::channel::oneshot::channel();
    std::thread::Builder::new()
        .name("cover-fetch".into())
        .spawn(move || {
            let _ = tx.send(job());
        })
        .map_err(|e| e.to_string())?;
    rx.await.unwrap_or_else(|_| Err("LOOKUP_ABORTED".to_owned()))
}

/// One service's candidates with their thumbnails (downloaded in parallel; a candidate whose
/// thumbnail fails, e.g. a release group without cover art, is dropped).
pub async fn search(query: ArtworkQuery, service: Service, cancel: Arc<AtomicBool>) -> Result<Vec<Found>, String> {
    off_thread(move || {
        let resolver = shared_resolver();
        let cands = match service {
            Service::Itunes => resolver.itunes_candidates(&query, &cancel),
            Service::Deezer => resolver.deezer_candidates(&query, &cancel),
            Service::MusicBrainz => resolver.musicbrainz_candidates(&query, &cancel),
            Service::Discogs => resolver.discogs_candidates(&query, &cancel),
        }
        .map_err(message)?;
        let thumbs: Vec<Option<Vec<u8>>> = std::thread::scope(|scope| {
            let jobs: Vec<_> = cands
                .iter()
                .map(|c| {
                    let (resolver, cancel) = (&resolver, &cancel);
                    scope.spawn(move || {
                        let bytes = resolver.download_first(std::slice::from_ref(&c.thumb_url), cancel).ok()?;
                        image::guess_format(&bytes).ok().map(|_| bytes)
                    })
                })
                .collect();
            jobs.into_iter().map(|j| j.join().ok().flatten()).collect()
        });
        if cancel.load(Ordering::Relaxed) {
            return Err("LOOKUP_CANCELLED".to_owned());
        }
        Ok(cands
            .into_iter()
            .zip(thumbs)
            .filter_map(|(cand, bytes)| Some(Found { cand, thumb: Handle::from_bytes(bytes?) }))
            .collect())
    })
    .await
}

/// Downloads the chosen cover at full size and writes it to a new temp file: `(path, source)`.
pub async fn download(cand: CoverCandidate, cancel: Arc<AtomicBool>) -> Result<(PathBuf, &'static str), String> {
    off_thread(move || {
        let bytes = shared_resolver().download_first(&cand.full_urls, &cancel).map_err(message)?;
        if cancel.load(Ordering::Relaxed) {
            return Err("LOOKUP_CANCELLED".to_owned());
        }
        let (bytes, ext) = normalize(bytes)?;
        let dir = dir();
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let name = format!(
            "{}-{}-{}.{ext}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed),
            rustygrade_linux::artwork_lookup::now_unix_u64()
        );
        let path = dir.join(name);
        std::fs::write(&path, bytes).map_err(|e| e.to_string())?;
        Ok((path, cand.source))
    })
    .await
}

/// Keeps JPEG / PNG as downloaded (after checking they decode); anything else the tag writer may
/// not embed (WebP, ...) is re-encoded as JPEG.
fn normalize(bytes: Vec<u8>) -> Result<(Vec<u8>, &'static str), String> {
    let format = image::guess_format(&bytes).map_err(|_| "DOWNLOAD IS NOT AN IMAGE".to_owned())?;
    let img = image::load_from_memory_with_format(&bytes, format)
        .map_err(|_| "DOWNLOAD IS NOT A READABLE IMAGE".to_owned())?;
    match format {
        image::ImageFormat::Jpeg => Ok((bytes, "jpg")),
        image::ImageFormat::Png => Ok((bytes, "png")),
        _ => {
            let mut out = std::io::Cursor::new(Vec::new());
            image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 92)
                .encode_image(&img.to_rgb8())
                .map_err(|e| e.to_string())?;
            Ok((out.into_inner(), "jpg"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn found(url: &str, best: bool) -> Found {
        Found {
            cand: CoverCandidate {
                title: url.to_owned(),
                artist: String::new(),
                year: None,
                thumb_url: url.to_owned(),
                full_urls: vec![url.to_owned()],
                source: "ITUNES",
                best,
            },
            thumb: Handle::from_bytes(Vec::new()),
        }
    }

    #[test]
    fn picker_keeps_order_preselects_best_and_drops_stale_answers() {
        let mut p = Picker {
            seq: 2,
            itunes_pending: true,
            deezer_pending: true,
            musicbrainz_pending: true,
            discogs_pending: true,
            ..Picker::default()
        };
        p.receive(1, Service::Itunes, Ok(vec![found("stale", true)]));
        assert!(p.itunes.is_empty() && p.itunes_pending);
        p.receive(2, Service::Discogs, Ok(vec![found("dg", false)]));
        p.receive(2, Service::MusicBrainz, Ok(vec![found("mb", false)]));
        p.receive(2, Service::Deezer, Ok(vec![found("dz", false)]));
        p.receive(2, Service::Itunes, Ok(vec![found("best", true), found("other", false)]));
        let order: Vec<&str> = p.items().map(|f| f.cand.thumb_url.as_str()).collect();
        assert_eq!(order, ["best", "other", "dz", "mb", "dg"]);
        assert_eq!(p.total_found(), 5);
        assert_eq!(p.selected_candidate().map(|c| c.thumb_url.as_str()), Some("best"));
        assert!(!p.searching());
        p.receive(2, Service::Itunes, Err("LOOKUP FAILED // CHECK THE CONNECTION".to_owned()));
        assert!(p.error.is_some());
    }

    #[test]
    fn normalize_keeps_png_and_rejects_garbage() {
        let mut png = std::io::Cursor::new(Vec::new());
        image::RgbImage::new(4, 4).write_to(&mut png, image::ImageFormat::Png).unwrap();
        let (out, ext) = normalize(png.into_inner()).unwrap();
        assert_eq!(ext, "png");
        assert!(image::load_from_memory(&out).is_ok());
        assert!(normalize(b"<html>not found</html>".to_vec()).is_err());
    }

    #[test]
    fn normalize_reencodes_other_formats_as_jpeg() {
        let mut bmp = std::io::Cursor::new(Vec::new());
        image::RgbImage::new(4, 4).write_to(&mut bmp, image::ImageFormat::WebP).unwrap();
        let (out, ext) = normalize(bmp.into_inner()).unwrap();
        assert_eq!(ext, "jpg");
        assert_eq!(image::guess_format(&out).unwrap(), image::ImageFormat::Jpeg);
    }
}
