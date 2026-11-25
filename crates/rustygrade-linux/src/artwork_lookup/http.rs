use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde_json::Value;
use tracing::debug;

use super::normalize::{resolve_redirect_url, validate_download_url};
use super::{FetchCoverError, MAX_IMAGE_BYTES, OnlineArtworkResolver};

const MAX_IMAGE_REDIRECTS: usize = 5;

impl OnlineArtworkResolver {
    /// Schedules the next slot without holding the mutex across sleep or blocking if cancelled.
    pub(super) fn wait_rate_limit(
        slot: &Mutex<Option<Instant>>,
        min_gap: Duration,
        cancel: &AtomicBool,
    ) -> bool {
        if cancel.load(Ordering::Relaxed) {
            return false;
        }
        let target = {
            let mut guard = slot.lock().unwrap_or_else(|p| p.into_inner());
            let now = Instant::now();
            let next = match *guard {
                Some(prev) => (prev + min_gap).max(now),
                None => now,
            };
            *guard = Some(next);
            next
        };
        while Instant::now() < target {
            if cancel.load(Ordering::Relaxed) {
                return false;
            }
            let rem = target.saturating_duration_since(Instant::now());
            std::thread::sleep(rem.min(Duration::from_millis(50)));
        }
        !cancel.load(Ordering::Relaxed)
    }

    pub(super) fn read_json_body(
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

    /// Downloads the first of `urls` that answers with a picture body (validated HTTPS CDN URLs).
    /// Blocking.
    pub fn download_first(
        &self,
        urls: &[String],
        cancel: &AtomicBool,
    ) -> Result<Vec<u8>, FetchCoverError> {
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

    pub(super) fn download_image(&self, url: &str) -> Result<Vec<u8>, FetchCoverError> {
        let mut current = validate_download_url(url).ok_or(FetchCoverError::NotFound)?;
        for _ in 0..=MAX_IMAGE_REDIRECTS {
            let mut resp = self.image_agent.get(&current).call().map_err(|err| {
                debug!(error = %err, "cover download transport error");
                FetchCoverError::Network
            })?;
            let status = resp.status().as_u16();
            if matches!(status, 301 | 302 | 303 | 307 | 308) {
                let loc = resp
                    .headers()
                    .get("location")
                    .and_then(|v| v.to_str().ok())
                    .ok_or(FetchCoverError::Network)?;
                let next = resolve_redirect_url(&current, loc)
                    .and_then(|u| validate_download_url(&u))
                    .ok_or(FetchCoverError::NotFound)?;
                current = next;
                continue;
            }
            if status == 404 {
                return Err(FetchCoverError::NotFound);
            }
            if status != 200 {
                debug!(status, "cover download status");
                return Err(FetchCoverError::Network);
            }
            return resp
                .body_mut()
                .with_config()
                .limit(MAX_IMAGE_BYTES)
                .read_to_vec()
                .map_err(|_| FetchCoverError::Network);
        }
        Err(FetchCoverError::Network)
    }
}
