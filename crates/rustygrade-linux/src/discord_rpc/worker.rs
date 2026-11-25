use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use tokio::io::AsyncReadExt;
use tokio::net::UnixStream;
use tokio::sync::mpsc;
use tokio::time::{Instant, MissedTickBehavior, interval_at, sleep_until};
use tracing::debug;

use crate::artwork_lookup::{
    ArtworkCache, ArtworkProviders, ArtworkQuery, ArtworkResolver, CacheLookup, LookupOutcome,
    now_unix_u64,
};

use super::ipc::{
    HandshakeOutcome, candidate_socket_paths, clear_and_close, connect_and_handshake,
    drain_incoming_frames, send_activity,
};
use super::{
    DiscordPlaybackStatus, DiscordPresence, DiscordRpcStatus, RpcCommand, adjusted_presence,
    should_send_presence,
};

const RECONNECT_INTERVAL: Duration = Duration::from_secs(15);
const PRESENCE_RETRY_THROTTLE: Duration = Duration::from_secs(3);

enum ConnectStep {
    Done,
    RestartWithNewId,
    Shutdown,
}

enum ConnectedSelect {
    Cmd(Option<RpcCommand>),
    Artwork(Option<(u64, String, LookupOutcome)>),
    Flush,
    Read(std::io::Result<usize>),
}

pub(super) struct ArtworkConfig {
    pub enabled: bool,
    pub providers: ArtworkProviders,
    pub cache_path: Option<PathBuf>,
    pub resolver: Arc<dyn ArtworkResolver>,
    pub cancel: Arc<AtomicBool>,
}

struct WorkerState<'a> {
    client_id: String,
    pid: u32,
    nonce: u64,
    latest_presence: Option<(DiscordPresence, Instant)>,
    last_sent: Option<(DiscordPresence, Instant)>,
    pending_flush: Option<Instant>,
    send_coalesce: Duration,
    stream: Option<UnixStream>,
    read_buf: Vec<u8>,
    last_attempt: Option<Instant>,
    rejected: bool,
    current_status: Option<DiscordRpcStatus>,
    status_tx: mpsc::UnboundedSender<DiscordRpcStatus>,
    override_sockets: Option<Vec<PathBuf>>,
    cmd_rx: &'a mut mpsc::UnboundedReceiver<RpcCommand>,
    artwork_enabled: bool,
    artwork_providers: ArtworkProviders,
    artwork_generation: u64,
    artwork_cache: ArtworkCache,
    artwork_resolver: Arc<dyn ArtworkResolver>,
    artwork_cancel: Arc<AtomicBool>,
    artwork_inflight_key: Option<String>,
    artwork_pending: Option<ArtworkQuery>,
    artwork_tx: mpsc::UnboundedSender<(u64, String, LookupOutcome)>,
    artwork_rx: &'a mut mpsc::UnboundedReceiver<(u64, String, LookupOutcome)>,
}

impl<'a> WorkerState<'a> {
    fn emit_status(&mut self, next: DiscordRpcStatus) {
        if self.current_status != Some(next) {
            self.current_status = Some(next);
            let _ = self.status_tx.send(next);
        }
    }

    fn drop_connection(&mut self) {
        self.stream = None;
        self.read_buf.clear();
        self.last_sent = None;
        self.pending_flush = None;
        let fallback = if self.rejected {
            DiscordRpcStatus::Rejected
        } else {
            DiscordRpcStatus::WaitingForDiscord
        };
        self.emit_status(fallback);
    }

    fn prepare_presence_artwork(&mut self, presence: &mut DiscordPresence, allow_network: bool) {
        if !self.artwork_enabled
            || !self.artwork_providers.any_enabled()
            || presence.status == DiscordPlaybackStatus::Stopped
        {
            presence.artwork_url = None;
            self.artwork_pending = None;
            return;
        }
        let Some(query) =
            ArtworkQuery::from_metadata(&presence.artist, &presence.album, &presence.title)
        else {
            presence.artwork_url = None;
            self.artwork_pending = None;
            return;
        };

        match self
            .artwork_cache
            .get(&query.cache_key, now_unix_u64(), std::time::Instant::now())
        {
            CacheLookup::Hit(Some(url)) if !self.artwork_providers.allows_url(&url) => {
                presence.artwork_url = None;
                if allow_network {
                    self.schedule_artwork_lookup(query);
                }
            }
            CacheLookup::Hit(url) => {
                presence.artwork_url = url;
                self.artwork_pending = None;
            }
            CacheLookup::Backoff => {
                presence.artwork_url = None;
                self.artwork_pending = None;
            }
            CacheLookup::Miss => {
                presence.artwork_url = None;
                if allow_network {
                    self.schedule_artwork_lookup(query);
                }
            }
        }
    }

    fn prepare_latest_artwork_if_active(&mut self, allow_network: bool) {
        if let Some((mut presence, received_at)) = self.latest_presence.take() {
            self.prepare_presence_artwork(&mut presence, allow_network);
            self.latest_presence = Some((presence, received_at));
        }
    }

    fn schedule_artwork_lookup(&mut self, query: ArtworkQuery) {
        if self.artwork_inflight_key.as_deref() == Some(query.cache_key.as_str()) {
            self.artwork_pending = None;
            return;
        }
        if self.artwork_inflight_key.is_some() {
            self.artwork_pending = Some(query);
            return;
        }
        let key = query.cache_key.clone();
        let generation = self.artwork_generation;
        self.artwork_inflight_key = Some(key.clone());
        let resolver = Arc::clone(&self.artwork_resolver);
        let cancel = Arc::clone(&self.artwork_cancel);
        let tx = self.artwork_tx.clone();
        tokio::task::spawn_blocking(move || {
            let outcome = resolver.resolve(&query, &cancel);
            let _ = tx.send((generation, key, outcome));
        });
    }

    async fn send_or_coalesce(&mut self, presence: &DiscordPresence) {
        let now = Instant::now();
        if !should_send_presence(self.last_sent.as_ref(), presence, now) {
            self.pending_flush = None;
            return;
        }
        let immediate = presence.status == DiscordPlaybackStatus::Stopped
            || self.last_sent.as_ref().is_none_or(|(_, sent_at)| {
                now.saturating_duration_since(*sent_at) >= self.send_coalesce
            });
        if immediate {
            self.pending_flush = None;
            if let Some(conn) = self.stream.as_mut()
                && !send_activity(
                    conn,
                    self.pid,
                    &mut self.nonce,
                    presence,
                    &mut self.last_sent,
                )
                .await
            {
                self.drop_connection();
            }
        } else if let Some((_, sent_at)) = &self.last_sent {
            self.pending_flush
                .get_or_insert(*sent_at + self.send_coalesce);
        }
    }

    async fn on_artwork_finished(&mut self, generation: u64, key: String, outcome: LookupOutcome) {
        if generation != self.artwork_generation {
            return;
        }
        self.artwork_inflight_key = None;
        self.artwork_cache.record(
            key.clone(),
            &outcome,
            now_unix_u64(),
            std::time::Instant::now(),
        );

        if self.artwork_enabled
            && let LookupOutcome::Found(url) = outcome
            && self.artwork_providers.allows_url(&url)
            && let Some((ref mut presence, received_at)) = self.latest_presence
            && presence.status != DiscordPlaybackStatus::Stopped
            && ArtworkQuery::from_metadata(&presence.artist, &presence.album, &presence.title)
                .is_some_and(|q| q.cache_key == key)
        {
            presence.artwork_url = Some(url);
            if self.stream.is_some() {
                let adj = adjusted_presence(presence, received_at, Instant::now());
                self.send_or_coalesce(&adj).await;
            }
        }

        if let Some(pending) = self.artwork_pending.take()
            && self.artwork_enabled
            && let Some((mut p, received_at)) = self.latest_presence.take()
        {
            let still_matches = p.status != DiscordPlaybackStatus::Stopped
                && ArtworkQuery::from_metadata(&p.artist, &p.album, &p.title)
                    .is_some_and(|q| q.cache_key == pending.cache_key);
            if still_matches {
                self.prepare_presence_artwork(&mut p, self.stream.is_some());
            }
            let has_art = p.artwork_url.is_some();
            self.latest_presence = Some((p.clone(), received_at));
            if self.stream.is_some() && has_art {
                let adj = adjusted_presence(&p, received_at, Instant::now());
                self.send_or_coalesce(&adj).await;
            }
        }
    }

    async fn handle_set_artwork_providers(&mut self, providers: ArtworkProviders) {
        if self.artwork_providers == providers {
            return;
        }
        self.artwork_providers = providers.clone();
        self.artwork_resolver.set_providers(providers);
        self.artwork_generation = self.artwork_generation.wrapping_add(1);
        self.artwork_inflight_key = None;
        self.artwork_pending = None;
        self.artwork_cache
            .prune_for_providers(&self.artwork_providers);

        if let Some((mut p, received_at)) = self.latest_presence.take() {
            let prev_art = p.artwork_url.clone();
            let allow_network = self.stream.is_some();
            self.prepare_presence_artwork(&mut p, allow_network);
            let next_art = p.artwork_url.clone();
            self.latest_presence = Some((p.clone(), received_at));
            if allow_network && prev_art != next_art {
                let adj = adjusted_presence(&p, received_at, Instant::now());
                self.send_or_coalesce(&adj).await;
            }
        }
    }

    async fn handle_set_artwork_enabled(&mut self, enabled: bool) {
        if self.artwork_enabled == enabled {
            return;
        }
        self.artwork_enabled = enabled;
        if !enabled {
            self.artwork_pending = None;
            if let Some((ref mut presence, received_at)) = self.latest_presence
                && presence.artwork_url.take().is_some()
                && self.stream.is_some()
            {
                let adj = adjusted_presence(presence, received_at, Instant::now());
                self.send_or_coalesce(&adj).await;
            }
        } else if let Some((mut p, received_at)) = self.latest_presence.take() {
            let allow_network = self.stream.is_some();
            self.prepare_presence_artwork(&mut p, allow_network);
            let has_art = p.artwork_url.is_some();
            self.latest_presence = Some((p.clone(), received_at));
            if allow_network && has_art {
                let adj = adjusted_presence(&p, received_at, Instant::now());
                self.send_or_coalesce(&adj).await;
            }
        }
    }

    async fn flush_latest_if_connected(&mut self) {
        let Some((ref presence, received_at)) = self.latest_presence else {
            return;
        };
        let adj = adjusted_presence(presence, received_at, Instant::now());
        if let Some(conn) = self.stream.as_mut()
            && !send_activity(conn, self.pid, &mut self.nonce, &adj, &mut self.last_sent).await
        {
            self.drop_connection();
        }
    }

    async fn connect_until_done(&mut self) -> bool {
        loop {
            match self.try_connect_once().await {
                ConnectStep::Done => {
                    if self.stream.is_some() {
                        self.prepare_latest_artwork_if_active(true);
                        self.flush_latest_if_connected().await;
                    }
                    return true;
                }
                ConnectStep::RestartWithNewId => continue,
                ConnectStep::Shutdown => return false,
            }
        }
    }

    async fn try_connect_once(&mut self) -> ConnectStep {
        self.last_attempt = Some(Instant::now());
        self.read_buf.clear();
        self.last_sent = None;
        self.pending_flush = None;

        let paths: Vec<PathBuf> = match &self.override_sockets {
            Some(list) => list.clone(),
            None => candidate_socket_paths(),
        };

        for path in paths {
            let client_id = self.client_id.clone();
            let outcome = {
                let mut fut = std::pin::pin!(connect_and_handshake(&path, &client_id));
                loop {
                    tokio::select! {
                        res = &mut fut => break res,
                        cmd = self.cmd_rx.recv() => match cmd {
                            Some(RpcCommand::Update(mut p)) => {
                                self.prepare_presence_artwork(&mut p, false);
                                self.latest_presence = Some((p, Instant::now()));
                            }
                            Some(RpcCommand::SetClientId(next_id)) => {
                                if next_id != self.client_id {
                                    self.client_id = next_id;
                                    self.rejected = false;
                                    return ConnectStep::RestartWithNewId;
                                }
                            }
                            Some(RpcCommand::SetArtworkEnabled(enabled)) => {
                                self.handle_set_artwork_enabled(enabled).await;
                            }
                            Some(RpcCommand::SetArtworkProviders(providers)) => {
                                self.handle_set_artwork_providers(providers).await;
                            }
                            Some(RpcCommand::Shutdown) | None => return ConnectStep::Shutdown,
                        },
                        res = self.artwork_rx.recv() => {
                            if let Some((art_gen, key, art_outcome)) = res {
                                self.on_artwork_finished(art_gen, key, art_outcome).await;
                            }
                        }
                    }
                }
            };
            match outcome {
                HandshakeOutcome::Connected(mut conn, leftover) => {
                    debug!(socket = %path.display(), "Connected to Discord IPC");
                    self.read_buf = leftover;
                    if !self.read_buf.is_empty()
                        && !drain_incoming_frames(&mut conn, &mut self.read_buf).await
                    {
                        self.read_buf.clear();
                        continue;
                    }
                    self.stream = Some(conn);
                    self.rejected = false;
                    self.emit_status(DiscordRpcStatus::Connected);
                    return ConnectStep::Done;
                }
                HandshakeOutcome::Rejected => {
                    self.rejected = true;
                    self.emit_status(DiscordRpcStatus::Rejected);
                    return ConnectStep::Done;
                }
                HandshakeOutcome::Unavailable => {}
            }
        }
        self.emit_status(DiscordRpcStatus::WaitingForDiscord);
        ConnectStep::Done
    }

    async fn handle_update_connected(&mut self, mut presence: DiscordPresence) {
        let now = Instant::now();
        self.prepare_presence_artwork(&mut presence, true);
        self.latest_presence = Some((presence.clone(), now));
        self.send_or_coalesce(&presence).await;
    }
}

async fn wait_flush(deadline: Option<Instant>) {
    match deadline {
        Some(at) => sleep_until(at).await,
        None => std::future::pending().await,
    }
}

pub(super) async fn run_rpc_worker(
    client_id: String,
    artwork: ArtworkConfig,
    mut cmd_rx: mpsc::UnboundedReceiver<RpcCommand>,
    status_tx: mpsc::UnboundedSender<DiscordRpcStatus>,
    override_sockets: Option<Vec<PathBuf>>,
    send_coalesce: Duration,
) {
    let (artwork_tx, mut artwork_rx) = mpsc::unbounded_channel();
    artwork.resolver.set_providers(artwork.providers.clone());
    let mut artwork_cache = ArtworkCache::load(artwork.cache_path);
    artwork_cache.prune_disallowed_urls(&artwork.providers);
    let mut state = WorkerState {
        client_id,
        pid: std::process::id(),
        nonce: 1,
        latest_presence: None,
        last_sent: None,
        pending_flush: None,
        send_coalesce,
        stream: None,
        read_buf: Vec::with_capacity(4096),
        last_attempt: None,
        rejected: false,
        current_status: None,
        status_tx,
        override_sockets,
        cmd_rx: &mut cmd_rx,
        artwork_enabled: artwork.enabled,
        artwork_providers: artwork.providers,
        artwork_generation: 0,
        artwork_cache,
        artwork_resolver: artwork.resolver,
        artwork_cancel: artwork.cancel,
        artwork_inflight_key: None,
        artwork_pending: None,
        artwork_tx,
        artwork_rx: &mut artwork_rx,
    };

    let mut reconnect_tick = interval_at(Instant::now() + RECONNECT_INTERVAL, RECONNECT_INTERVAL);
    reconnect_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);

    if !state.connect_until_done().await {
        return;
    }

    loop {
        if state.stream.is_some() {
            let flush_at = state.pending_flush;
            let mut conn = state.stream.take().expect("checked is_some");
            let outcome = tokio::select! {
                cmd = state.cmd_rx.recv() => ConnectedSelect::Cmd(cmd),
                art = state.artwork_rx.recv() => ConnectedSelect::Artwork(art),
                _ = wait_flush(flush_at), if flush_at.is_some() => ConnectedSelect::Flush,
                res = conn.read_buf(&mut state.read_buf) => ConnectedSelect::Read(res),
            };
            state.stream = Some(conn);

            match outcome {
                ConnectedSelect::Cmd(Some(RpcCommand::Update(presence))) => {
                    state.handle_update_connected(presence).await;
                }
                ConnectedSelect::Cmd(Some(RpcCommand::SetClientId(next_id))) => {
                    if next_id != state.client_id {
                        state.client_id = next_id;
                        state.rejected = false;
                        if let Some(mut old) = state.stream.take() {
                            clear_and_close(&mut old, state.pid, &mut state.nonce).await;
                        }
                        if !state.connect_until_done().await {
                            return;
                        }
                    }
                }
                ConnectedSelect::Cmd(Some(RpcCommand::SetArtworkEnabled(enabled))) => {
                    state.handle_set_artwork_enabled(enabled).await;
                }
                ConnectedSelect::Cmd(Some(RpcCommand::SetArtworkProviders(providers))) => {
                    state.handle_set_artwork_providers(providers).await;
                }
                ConnectedSelect::Cmd(Some(RpcCommand::Shutdown) | None) => {
                    if let Some(mut c) = state.stream.take() {
                        clear_and_close(&mut c, state.pid, &mut state.nonce).await;
                    }
                    return;
                }
                ConnectedSelect::Artwork(Some((art_gen, key, art_outcome))) => {
                    state.on_artwork_finished(art_gen, key, art_outcome).await;
                }
                ConnectedSelect::Artwork(None) => {}
                ConnectedSelect::Flush => {
                    state.pending_flush = None;
                    let now = Instant::now();
                    if let Some((ref presence, received_at)) = state.latest_presence {
                        let adj = adjusted_presence(presence, received_at, now);
                        if should_send_presence(state.last_sent.as_ref(), &adj, now)
                            && let Some(c) = state.stream.as_mut()
                            && !send_activity(
                                c,
                                state.pid,
                                &mut state.nonce,
                                &adj,
                                &mut state.last_sent,
                            )
                            .await
                        {
                            state.drop_connection();
                        }
                    }
                }
                ConnectedSelect::Read(Ok(0) | Err(_)) => {
                    state.drop_connection();
                }
                ConnectedSelect::Read(Ok(_)) => {
                    let mut conn = state.stream.take().expect("just put back");
                    let ok = drain_incoming_frames(&mut conn, &mut state.read_buf).await;
                    state.stream = Some(conn);
                    if !ok {
                        state.drop_connection();
                    }
                }
            }
        } else {
            tokio::select! {
                cmd = state.cmd_rx.recv() => match cmd {
                    Some(RpcCommand::Update(mut presence)) => {
                        state.prepare_presence_artwork(&mut presence, false);
                        let is_active = presence.status != DiscordPlaybackStatus::Stopped;
                        state.latest_presence = Some((presence, Instant::now()));
                        if is_active
                            && !state.rejected
                            && state.last_attempt.is_none_or(|t| t.elapsed() >= PRESENCE_RETRY_THROTTLE)
                            && !state.connect_until_done().await
                        {
                            return;
                        }
                    }
                    Some(RpcCommand::SetClientId(next_id)) => {
                        if next_id != state.client_id || state.rejected {
                            state.client_id = next_id;
                            state.rejected = false;
                            if !state.connect_until_done().await {
                                return;
                            }
                        }
                    }
                    Some(RpcCommand::SetArtworkEnabled(enabled)) => {
                        state.handle_set_artwork_enabled(enabled).await;
                    }
                    Some(RpcCommand::SetArtworkProviders(providers)) => {
                        state.handle_set_artwork_providers(providers).await;
                    }
                    Some(RpcCommand::Shutdown) | None => return,
                },
                res = state.artwork_rx.recv() => {
                    if let Some((art_gen, key, art_outcome)) = res {
                        state.on_artwork_finished(art_gen, key, art_outcome).await;
                    }
                }
                _ = reconnect_tick.tick() => {
                    if !state.rejected && !state.connect_until_done().await {
                        return;
                    }
                }
            }
        }
    }
}
