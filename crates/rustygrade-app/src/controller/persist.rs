use std::sync::Arc;
use std::time::{Duration, Instant};

use rustygrade_core::{
    Album, AppSettings, CollectionsData, CollectionsStore, PlayStats, SessionState, SessionStore,
    build_track_map,
};
use tokio::sync::{mpsc, oneshot};
use tracing::{info, warn};

use super::Coordinator;
use super::types::{ControllerEvent, NoticeKind};
use crate::library_view::ensure_default_smart_playlists;

pub(super) enum PersistMsg {
    Settings(AppSettings),
    Collections(CollectionsData),
    Stats(PlayStats),
    SessionState(SessionState),
    SessionQueue {
        state: SessionState,
        active_ids: Vec<String>,
        original_ids: Vec<String>,
    },
    Flush(oneshot::Sender<()>),
}

pub(super) fn spawn_persist_worker() -> mpsc::UnboundedSender<PersistMsg> {
    let (tx, mut rx) = mpsc::unbounded_channel::<PersistMsg>();
    tokio::task::spawn_blocking(move || {
        let mut seq: u64 = 0;
        while let Some(first) = rx.blocking_recv() {
            let mut settings: Option<AppSettings> = None;
            let mut collections: Option<CollectionsData> = None;
            let mut stats: Option<PlayStats> = None;
            let mut session_state: Option<(u64, SessionState)> = None;
            let mut session_queue: Option<(u64, SessionState, Vec<String>, Vec<String>)> = None;
            let mut flushes: Vec<oneshot::Sender<()>> = Vec::new();

            let mut push = |msg: PersistMsg| {
                seq += 1;
                match msg {
                    PersistMsg::Settings(s) => settings = Some(s),
                    PersistMsg::Collections(c) => collections = Some(c),
                    PersistMsg::Stats(st) => stats = Some(st),
                    PersistMsg::SessionState(s) => session_state = Some((seq, s)),
                    PersistMsg::SessionQueue {
                        state,
                        active_ids,
                        original_ids,
                    } => session_queue = Some((seq, state, active_ids, original_ids)),
                    PersistMsg::Flush(done) => flushes.push(done),
                }
            };

            push(first);
            while let Ok(more) = rx.try_recv() {
                push(more);
            }

            if let Some(s) = settings {
                s.save();
            }
            if let Some(c) = collections {
                CollectionsStore::save(&c);
            }
            if let Some(st) = stats {
                st.save();
            }
            match (session_queue, session_state) {
                (Some((q_seq, q_state, active_ids, orig_ids)), Some((s_seq, s_state))) => {
                    SessionStore::save_queue(&active_ids);
                    SessionStore::save_original_queue(&orig_ids);
                    if s_seq >= q_seq {
                        SessionStore::save_state(&s_state);
                    } else {
                        SessionStore::save_state(&q_state);
                    }
                }
                (Some((_, q_state, active_ids, orig_ids)), None) => {
                    SessionStore::save_queue(&active_ids);
                    SessionStore::save_original_queue(&orig_ids);
                    SessionStore::save_state(&q_state);
                }
                (None, Some((_, s_state))) => {
                    SessionStore::save_state(&s_state);
                }
                (None, None) => {}
            }
            for tx_done in flushes {
                let _ = tx_done.send(());
            }
        }
    });
    tx
}

/// Collections, play stats, the saved session, and the unshuffled queue read at startup.
pub(super) type SavedState = (
    CollectionsData,
    PlayStats,
    Option<(SessionState, Vec<String>)>,
    Option<Vec<String>>,
);

impl Coordinator {
    pub(super) async fn load_initial_state(
        &mut self,
        library_task: tokio::task::JoinHandle<Result<Arc<[Album]>, String>>,
        saved_state_task: tokio::task::JoinHandle<SavedState>,
    ) {
        match library_task.await {
            Ok(Ok(albums)) => {
                info!(albums = albums.len(), "Loaded library");
                self.albums = albums;
            }
            Ok(Err(err)) => {
                warn!(error = %err, "Failed loading library");
                self.notice(NoticeKind::Error, format!("LIBRARY_ERROR // {err}"));
                let _ = self
                    .ui_tx
                    .send(ControllerEvent::LibraryLoaded(Arc::from(Vec::new())));
            }
            Err(err) => {
                warn!(error = %err, "Library load task panicked");
                let _ = self
                    .ui_tx
                    .send(ControllerEvent::LibraryLoaded(Arc::from(Vec::new())));
            }
        }

        let (mut collections, stats, session, orig_queue) =
            saved_state_task.await.unwrap_or_default();

        if ensure_default_smart_playlists(&mut collections) {
            let _ = self
                .persist_tx
                .send(PersistMsg::Collections(collections.clone()));
        }
        self.collections = collections.clone();
        self.sync_live_playlist_covers();
        self.stats = stats.clone();
        let _ = self
            .ui_tx
            .send(ControllerEvent::CollectionsUpdated(collections));
        let _ = self.ui_tx.send(ControllerEvent::StatsUpdated(stats));

        if let Some((state, queue_ids)) = session {
            let track_map = build_track_map(&self.albums);
            if let Some(restored) =
                self.queue
                    .restore_session(&track_map, &state, &queue_ids, orig_queue.as_deref())
            {
                self.duration_s = restored.duration;
                self.position_s = state.position.clamp(0.0, restored.duration.max(0.0));
                self.pending_restore_seek = (self.position_s > 0.0).then_some(self.position_s);
                self.play_tracker
                    .reset_at(Some(restored.id.clone()), self.position_s, 0.0);
                self.load_lyrics_async(&restored);
                self.load_accent_async(Some(&restored));
                let _ = self
                    .ui_tx
                    .send(ControllerEvent::TrackChanged(Some(restored)));
                self.emit_playback();
            }
            self.emit_queue();
            self.sync_mpris().await;
            self.sync_tray().await;
        }
    }

    pub(super) async fn flush_persist(&self) {
        let (done_tx, done_rx) = oneshot::channel();
        if self.persist_tx.send(PersistMsg::Flush(done_tx)).is_ok() {
            let _ = tokio::time::timeout(Duration::from_secs(2), done_rx).await;
        }
    }

    pub(super) fn persist_collections(&self) {
        self.sync_live_playlist_covers();
        let data = self.collections.clone();
        let _ = self
            .ui_tx
            .send(ControllerEvent::CollectionsUpdated(data.clone()));
        let _ = self.persist_tx.send(PersistMsg::Collections(data));
    }

    pub(super) fn save_settings_async(&self) {
        let _ = self
            .persist_tx
            .send(PersistMsg::Settings(self.settings.clone()));
    }

    pub(super) fn save_session_state_async(&mut self) {
        let state = self.queue.session_state(self.position_s);
        self.session_last_saved = Instant::now();
        let _ = self.persist_tx.send(PersistMsg::SessionState(state));
    }

    pub(super) fn save_queue_async(&mut self) {
        let (state, active_ids, original_ids) = self.queue.to_session(self.position_s);
        self.session_last_saved = Instant::now();
        let _ = self.persist_tx.send(PersistMsg::SessionQueue {
            state,
            active_ids,
            original_ids,
        });
    }
}
