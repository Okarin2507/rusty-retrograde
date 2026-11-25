use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::{Instant, timeout};

use crate::artwork_lookup::ArtworkProviders;
#[cfg(test)]
use crate::artwork_lookup::ArtworkResolver;

mod ipc;
mod payload;
#[cfg(test)]
mod tests;
mod worker;

#[cfg(test)]
pub use ipc::candidate_socket_paths;
pub use payload::{build_set_activity_payload, clamp_discord_str};

/// Default Discord Application ID registered for Rustygrade.
pub const DEFAULT_DISCORD_CLIENT_ID: &str = "1573878488337879223";

const SEND_COALESCE: Duration = Duration::from_secs(1);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_millis(800);
const DRIFT_THRESHOLD_S: f64 = 2.5;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DiscordRpcStatus {
    #[default]
    Disabled,
    WaitingForDiscord,
    Connected,
    Rejected,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiscordPlaybackStatus {
    Stopped,
    Playing,
    Paused,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DiscordPresence {
    pub status: DiscordPlaybackStatus,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub position_s: f64,
    pub duration_s: f64,
    pub artwork_url: Option<String>,
}

pub(super) enum RpcCommand {
    Update(DiscordPresence),
    SetClientId(String),
    SetArtworkEnabled(bool),
    SetArtworkProviders(ArtworkProviders),
    Shutdown,
}

/// Resolves the Discord Application ID from `RETRO_DISCORD_CLIENT_ID`, configured setting, or default.
pub fn resolve_client_id(configured: &str) -> String {
    let env_val = std::env::var("RETRO_DISCORD_CLIENT_ID").ok();
    resolve_client_id_with_env(configured, env_val.as_deref())
}

pub(super) fn resolve_client_id_with_env(configured: &str, env_val: Option<&str>) -> String {
    if let Some(env_id) = env_val {
        let trimmed = env_id.trim();
        if is_valid_client_id(trimmed) {
            return trimmed.to_owned();
        }
    }
    let trimmed = configured.trim();
    if is_valid_client_id(trimmed) {
        return trimmed.to_owned();
    }
    DEFAULT_DISCORD_CLIENT_ID.to_owned()
}

pub fn is_valid_client_id(id: &str) -> bool {
    (17..=20).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_digit())
}

/// Advances a cached `Playing` presence's position by elapsed wall time so reconnects stay in sync.
pub fn adjusted_presence(
    presence: &DiscordPresence,
    received_at: Instant,
    now: Instant,
) -> DiscordPresence {
    let mut adjusted = presence.clone();
    if adjusted.status == DiscordPlaybackStatus::Playing && adjusted.position_s.is_finite() {
        let elapsed = now.saturating_duration_since(received_at).as_secs_f64();
        let pos = adjusted.position_s.max(0.0) + elapsed;
        adjusted.position_s = if adjusted.duration_s.is_finite() && adjusted.duration_s > 0.0 {
            pos.min(adjusted.duration_s)
        } else {
            pos
        };
    }
    adjusted
}

/// Returns `true` when `next` differs meaningfully from `last` (status, track, or >2.5 s seek).
pub fn should_send_presence(
    last: Option<&(DiscordPresence, Instant)>,
    next: &DiscordPresence,
    now: Instant,
) -> bool {
    let Some((prev, prev_at)) = last else {
        return true;
    };
    if prev.status == DiscordPlaybackStatus::Stopped
        && next.status == DiscordPlaybackStatus::Stopped
    {
        return false;
    }
    let duration_changed = prev.duration_s.is_finite() != next.duration_s.is_finite()
        || (prev.duration_s.is_finite()
            && (prev.duration_s.round() as i64) != (next.duration_s.round() as i64));
    if prev.status != next.status
        || prev.title != next.title
        || prev.artist != next.artist
        || prev.album != next.album
        || prev.artwork_url != next.artwork_url
        || duration_changed
    {
        return true;
    }
    if next.status == DiscordPlaybackStatus::Playing {
        if !prev.position_s.is_finite() || !next.position_s.is_finite() {
            return prev.position_s.is_finite() != next.position_s.is_finite();
        }
        let elapsed = now.saturating_duration_since(*prev_at).as_secs_f64();
        let expected_pos = prev.position_s + elapsed;
        return (next.position_s - expected_pos).abs() > DRIFT_THRESHOLD_S;
    }
    false
}

pub struct DiscordRpcService {
    cmd_tx: mpsc::UnboundedSender<RpcCommand>,
    artwork_cancel: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl Drop for DiscordRpcService {
    fn drop(&mut self) {
        self.artwork_cancel.store(true, Ordering::Relaxed);
    }
}

impl DiscordRpcService {
    pub fn start(
        client_id: String,
        artwork_enabled: bool,
        artwork_providers: ArtworkProviders,
        cache_path: PathBuf,
        status_tx: mpsc::UnboundedSender<DiscordRpcStatus>,
    ) -> Self {
        Self::start_with_deps(
            client_id,
            artwork_enabled,
            artwork_providers,
            Some(cache_path),
            crate::artwork_lookup::shared_resolver(),
            status_tx,
            None,
            SEND_COALESCE,
        )
    }

    #[cfg(test)]
    fn start_with_sockets(
        client_id: String,
        status_tx: mpsc::UnboundedSender<DiscordRpcStatus>,
        override_sockets: Option<Vec<PathBuf>>,
    ) -> Self {
        Self::start_with_deps(
            client_id,
            false,
            ArtworkProviders::default(),
            None,
            Arc::new(crate::artwork_lookup::OnlineArtworkResolver::new()),
            status_tx,
            override_sockets,
            Duration::from_millis(10),
        )
    }

    #[cfg(test)]
    fn start_with_artwork_for_test(
        client_id: String,
        artwork_enabled: bool,
        cache_path: Option<PathBuf>,
        resolver: Arc<dyn ArtworkResolver>,
        status_tx: mpsc::UnboundedSender<DiscordRpcStatus>,
        override_sockets: Option<Vec<PathBuf>>,
    ) -> Self {
        Self::start_with_deps(
            client_id,
            artwork_enabled,
            ArtworkProviders::default(),
            cache_path,
            resolver,
            status_tx,
            override_sockets,
            Duration::from_millis(10),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn start_with_deps(
        client_id: String,
        artwork_enabled: bool,
        artwork_providers: ArtworkProviders,
        cache_path: Option<PathBuf>,
        resolver: Arc<dyn crate::artwork_lookup::ArtworkResolver>,
        status_tx: mpsc::UnboundedSender<DiscordRpcStatus>,
        override_sockets: Option<Vec<PathBuf>>,
        send_coalesce: Duration,
    ) -> Self {
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let artwork_cancel = Arc::new(AtomicBool::new(false));
        let artwork = worker::ArtworkConfig {
            enabled: artwork_enabled,
            providers: artwork_providers,
            cache_path,
            resolver,
            cancel: Arc::clone(&artwork_cancel),
        };
        let handle = tokio::spawn(worker::run_rpc_worker(
            client_id,
            artwork,
            cmd_rx,
            status_tx,
            override_sockets,
            send_coalesce,
        ));
        Self {
            cmd_tx,
            artwork_cancel,
            handle: Some(handle),
        }
    }

    pub fn update_presence(&self, presence: DiscordPresence) {
        let _ = self.cmd_tx.send(RpcCommand::Update(presence));
    }

    pub fn set_client_id(&self, client_id: String) {
        let _ = self.cmd_tx.send(RpcCommand::SetClientId(client_id));
    }

    pub fn set_artwork_enabled(&self, enabled: bool) {
        let _ = self.cmd_tx.send(RpcCommand::SetArtworkEnabled(enabled));
    }

    pub fn set_artwork_providers(&self, providers: ArtworkProviders) {
        let _ = self.cmd_tx.send(RpcCommand::SetArtworkProviders(providers));
    }

    pub async fn shutdown(mut self) {
        self.artwork_cancel.store(true, Ordering::Relaxed);
        let _ = self.cmd_tx.send(RpcCommand::Shutdown);
        if let Some(mut handle) = self.handle.take()
            && timeout(SHUTDOWN_TIMEOUT, &mut handle).await.is_err()
        {
            handle.abort();
        }
    }
}
