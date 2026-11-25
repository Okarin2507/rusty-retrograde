use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use rustygrade_core::scanner::{PrunePolicy, ScanOptions};
use rustygrade_core::{
    Album, CoverPaths, FolderWatcher, LibraryStore, ScanResult, Scanner, Track, build_track_map,
    deduplicate_and_sweep_covers, nearest_watched_ancestor, with_album_track_order,
};
use tokio::sync::mpsc;
use tracing::{info, warn};

use super::Coordinator;
use super::types::{ControllerEvent, NoticeKind};
use crate::playback::PlaybackQueue;

pub(super) struct ScanOutcome {
    pub(super) result: ScanResult,
    pub(super) covers_migrated: bool,
    pub(super) changed_paths: Vec<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ScanKind {
    Startup,
    Manual,
    Watch,
    CoverResync,
}

#[derive(Debug, Clone)]
pub(super) struct ScanJob {
    pub(super) kind: ScanKind,
    pub(super) roots: Vec<PathBuf>,
    pub(super) prune: PrunePolicy,
}

pub(super) fn resolve_open_uri(raw: &str) -> Option<PathBuf> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let path = if raw.len() >= 7 && raw[..7].eq_ignore_ascii_case("file://") {
        CoverPaths::to_local_path(Some(raw))?
    } else {
        PathBuf::from(raw)
    };
    path.exists()
        .then(|| std::fs::canonicalize(&path).unwrap_or(path))
}

fn library_roots(folders: &[String], albums: &[Album]) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = folders
        .iter()
        .filter(|f| !f.trim().is_empty())
        .map(PathBuf::from)
        .chain(
            albums
                .iter()
                .flat_map(|a| a.tracks.iter())
                .filter_map(|t| Path::new(&t.file_path).parent())
                .filter(|d| !d.as_os_str().is_empty())
                .map(Path::to_path_buf),
        )
        .collect();
    dirs.sort_by(|a, b| {
        a.as_os_str()
            .len()
            .cmp(&b.as_os_str().len())
            .then_with(|| a.cmp(b))
    });
    dirs.dedup();
    let mut roots: Vec<PathBuf> = Vec::new();
    for d in dirs {
        if !roots.iter().any(|r| d.starts_with(r)) {
            roots.push(d);
        }
    }
    roots
}

fn album_cover_paths(albums: &[Album]) -> Vec<PathBuf> {
    let mut seen = HashSet::new();
    albums
        .iter()
        .flat_map(|a| [a.cover_thumb_path.clone(), a.cover_full_path.clone()])
        .flatten()
        .filter(|p| seen.insert(p.clone()))
        .collect()
}

fn reconcile_queue_with_library(
    queue: &mut PlaybackQueue,
    track_map: &std::collections::HashMap<&str, &Track>,
) -> (bool, bool) {
    if queue.is_empty() {
        return (false, false);
    }
    let missing_ids: HashSet<String> = queue
        .all_track_ids()
        .into_iter()
        .filter(|id| !track_map.contains_key(id.as_str()))
        .collect();
    let (q_changed, cur_removed) = if missing_ids.is_empty() {
        (false, false)
    } else {
        queue.remove_tracks_by_id(&missing_ids)
    };
    if !queue.is_empty() {
        queue.refresh_from_library(track_map);
    }
    (q_changed, cur_removed)
}

pub(super) fn failed_track_rescan_target(
    file_path: &Path,
    library_folders: &[String],
) -> Option<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::with_capacity(library_folders.len() * 2);
    for folder in library_folders {
        let p = PathBuf::from(folder);
        if let Ok(canon) = std::fs::canonicalize(&p)
            && !roots.contains(&canon)
        {
            roots.push(canon);
        }
        if !roots.contains(&p) {
            roots.push(p);
        }
    }
    nearest_watched_ancestor(file_path, &roots)
}

fn scan_summary(r: &ScanResult) -> String {
    let mut parts = Vec::new();
    if r.added_tracks > 0 {
        parts.push(format!("{} NEW", r.added_tracks));
    }
    if r.updated_tracks > 0 {
        parts.push(format!("{} UPDATED", r.updated_tracks));
    }
    if r.removed_tracks > 0 {
        parts.push(format!("{} REMOVED", r.removed_tracks));
    } else if r.removed_albums > 0 {
        parts.push(format!("{} EMPTY ALBUMS REMOVED", r.removed_albums));
    }
    if r.covers > 0 {
        parts.push(format!("{} COVERS", r.covers));
    }
    if r.merged_albums > 0 {
        parts.push(format!("{} SPLIT ALBUMS JOINED", r.merged_albums));
    }
    parts.join(", ")
}

const WATCH_SUPPRESS_TTL: Duration = Duration::from_secs(5);

impl Coordinator {
    pub(super) async fn reload_library(&mut self, reconcile_queue: bool) {
        let albums = match tokio::task::spawn_blocking(LibraryStore::load).await {
            Ok(Ok(albums)) => Arc::<[Album]>::from(albums),
            Ok(Err(err)) => {
                warn!(error = %err, "Failed loading library");
                self.notice(NoticeKind::Error, format!("LIBRARY_ERROR // {err}"));
                return;
            }
            Err(err) => {
                warn!(error = %err, "Library load task panicked");
                self.notice(NoticeKind::Error, format!("LIBRARY_ERROR // {err}"));
                return;
            }
        };
        info!(albums = albums.len(), "Loaded library");
        self.albums = Arc::clone(&albums);
        if reconcile_queue && !self.queue.is_empty() {
            let track_map = build_track_map(&self.albums);
            let (q_changed, cur_removed) =
                reconcile_queue_with_library(&mut self.queue, &track_map);
            if cur_removed {
                self.crossfade
                    .cancel(&self.mpv, self.shared_vol, self.shared_muted);
                self.emit_queue();
                self.save_queue_async();
                self.stop_to_idle().await;
            } else if q_changed {
                self.cancel_crossfade_if_target_missing();
                self.save_queue_async();
                self.sync_mpv_next();
                self.sync_mpris().await;
            }
            if !cur_removed {
                if let Some(cur) = self.queue.current().cloned() {
                    self.load_accent_async(Some(&cur));
                    let _ = self.ui_tx.send(ControllerEvent::TrackChanged(Some(cur)));
                    self.sync_discord_rpc();
                }
                self.emit_queue();
            }
        }
        let _ = self.ui_tx.send(ControllerEvent::LibraryLoaded(albums));
    }

    pub(super) async fn open_uris(&mut self, uris: Vec<String>) {
        let paths: Vec<PathBuf> = uris.iter().filter_map(|u| resolve_open_uri(u)).collect();
        if paths.is_empty() {
            return;
        }
        let audio: Vec<PathBuf> = paths
            .iter()
            .filter(|p| p.is_file() && Scanner::is_audio_file(p))
            .cloned()
            .collect();
        if !audio.is_empty() {
            let found = self.tracks_for_paths(&audio);
            if found.len() == audio.len() {
                self.pending_open_play.clear();
                self.play_track_list(found, Some(0)).await;
            } else {
                self.pending_open_play = audio;
            }
        }
        self.import_paths(paths).await;
    }

    async fn play_pending_open(&mut self) {
        if self.pending_open_play.is_empty() {
            return;
        }
        let found = self.tracks_for_paths(&self.pending_open_play);
        if found.len() < self.pending_open_play.len() && self.scan_cancel.is_some() {
            return;
        }
        self.pending_open_play.clear();
        if !found.is_empty() {
            self.play_track_list(found, Some(0)).await;
        }
    }

    fn tracks_for_paths(&self, paths: &[PathBuf]) -> Vec<Track> {
        paths
            .iter()
            .filter_map(|p| {
                let canon = std::fs::canonicalize(p).ok();
                let file_name = p.file_name();
                self.albums
                    .iter()
                    .flat_map(|a| a.tracks.iter())
                    .find(|t| {
                        let tp = Path::new(&t.file_path);
                        if tp == p.as_path() {
                            return true;
                        }
                        tp.file_name() == file_name
                            && canon.is_some()
                            && std::fs::canonicalize(tp).ok() == canon
                    })
                    .cloned()
            })
            .collect()
    }

    pub(super) async fn set_album_order(
        &mut self,
        name: String,
        artist: String,
        track_ids: Vec<String>,
    ) {
        if self.scan_cancel.is_some() {
            self.notice(
                NoticeKind::Warn,
                "SCAN_IN_PROGRESS // REORDER AGAIN WHEN IT FINISHES",
            );
            let _ = self
                .ui_tx
                .send(ControllerEvent::LibraryLoaded(Arc::clone(&self.albums)));
            return;
        }
        let (n, a, ids) = (name.clone(), artist.clone(), track_ids.clone());
        let res =
            tokio::task::spawn_blocking(move || LibraryStore::set_album_track_order(&n, &a, &ids))
                .await
                .map_err(|e| e.to_string())
                .and_then(|r| r.map_err(|e| e.to_string()));
        match res {
            Ok(true) => {
                self.albums = Arc::from(with_album_track_order(
                    &self.albums,
                    &name,
                    &artist,
                    &track_ids,
                ));
            }
            Ok(false) => {
                // Revert optimistic UI reorder when the target album wasn't found.
                let _ = self
                    .ui_tx
                    .send(ControllerEvent::LibraryLoaded(Arc::clone(&self.albums)));
            }
            Err(err) => {
                self.notice(NoticeKind::Error, format!("REORDER_FAILED // {err}"));
                let _ = self
                    .ui_tx
                    .send(ControllerEvent::LibraryLoaded(Arc::clone(&self.albums)));
            }
        }
    }

    pub(super) async fn delete_albums(&mut self, targets: Vec<(String, String)>) {
        if targets.is_empty() {
            let _ = self.ui_tx.send(ControllerEvent::AlbumsDeleteFinished);
            return;
        }
        if self.scan_cancel.is_some() {
            self.notice(
                NoticeKind::Warn,
                "SCAN_IN_PROGRESS // DELETE AGAIN WHEN IT FINISHES",
            );
            let _ = self.ui_tx.send(ControllerEvent::AlbumsDeleteFinished);
            return;
        }
        let target_set: HashSet<(&str, &str)> = targets
            .iter()
            .map(|(n, a)| (n.as_str(), a.as_str()))
            .collect();
        let del_ids: HashSet<String> = self
            .albums
            .iter()
            .filter(|a| target_set.contains(&(a.name.as_str(), a.artist.as_str())))
            .flat_map(|a| a.tracks.iter().map(|t| t.id.clone()))
            .collect();

        self.flush_persist().await;
        let extra_pinned = self
            .live_playlist_covers
            .lock()
            .map(|g| g.clone())
            .unwrap_or_default();
        let targets_owned = targets.clone();
        let res = tokio::task::spawn_blocking(move || {
            LibraryStore::delete_albums(&targets_owned, &extra_pinned)
        })
        .await
        .map_err(|e| e.to_string())
        .and_then(|r| r.map_err(|e| e.to_string()));
        if let Err(err) = res {
            self.notice(NoticeKind::Error, format!("PURGE_FAILED // {err}"));
            let _ = self.ui_tx.send(ControllerEvent::AlbumsDeleteFinished);
            return;
        }

        let (q_changed, cur_removed) = self.queue.remove_tracks_by_id(&del_ids);
        if cur_removed {
            self.crossfade
                .cancel(&self.mpv, self.shared_vol, self.shared_muted);
            self.emit_queue();
            self.save_queue_async();
            self.stop_to_idle().await;
        } else if q_changed {
            self.cancel_crossfade_if_target_missing();
            self.emit_queue();
            self.save_queue_async();
            self.sync_mpv_next();
            self.sync_mpris().await;
        }

        // Queue was already reconciled above; skip duplicating the full-track scan inside reload.
        self.reload_library(false).await;
        let _ = self.ui_tx.send(ControllerEvent::AlbumsDeleteFinished);
        let msg = if targets.len() == 1 {
            format!("DATA_PURGED // {}", targets[0].0)
        } else {
            format!("DATA_PURGED // {} ALBUMS", targets.len())
        };
        self.notice(NoticeKind::Info, msg);
    }

    pub(super) async fn scan_folders(&mut self, folders: Vec<PathBuf>) {
        if folders.is_empty() {
            return;
        }
        if self.scan_cancel.is_some() {
            self.notice(
                NoticeKind::Info,
                "SCAN_IN_PROGRESS // TRY AGAIN WHEN IT IS DONE",
            );
            return;
        }
        for f in &folders {
            let s = f.to_string_lossy().into_owned();
            if !self.settings.library_folders.contains(&s) {
                self.settings.library_folders.push(s);
            }
        }
        self.save_settings_async();
        let _ = self
            .ui_tx
            .send(ControllerEvent::SettingsSaved(self.settings.clone()));
        self.sync_folder_watcher().await;

        let all_folders: Vec<PathBuf> = self
            .settings
            .library_folders
            .iter()
            .map(PathBuf::from)
            .collect();
        self.start_scan_paths(all_folders, ScanKind::Manual, PrunePolicy::Safe);
    }

    pub(super) fn scan_files(&mut self, paths: Vec<PathBuf>) {
        let to_scan: Vec<PathBuf> = paths
            .into_iter()
            .filter(|p| p.is_file() && Scanner::is_audio_file(p))
            .collect();
        if to_scan.is_empty() {
            return;
        }
        if self.scan_cancel.is_some() {
            for p in to_scan {
                if !self.pending_manual_paths.contains(&p) {
                    self.pending_manual_paths.push(p);
                }
            }
            self.notice(NoticeKind::Info, "SCAN_IN_PROGRESS // FILES QUEUED");
        } else {
            self.start_scan_paths(to_scan, ScanKind::Manual, PrunePolicy::Safe);
        }
    }

    pub(super) async fn import_paths(&mut self, paths: Vec<PathBuf>) {
        if paths.is_empty() {
            return;
        }
        let mut dirs_added = false;
        let mut to_scan = Vec::new();
        for p in paths {
            if p.is_dir() {
                let p = std::fs::canonicalize(&p).unwrap_or(p);
                let already_covered = self.settings.library_folders.iter().any(|f| {
                    let root = Path::new(f);
                    let root_canon = std::fs::canonicalize(root).ok();
                    p.starts_with(root) || root_canon.is_some_and(|rc| p.starts_with(rc))
                });
                if !already_covered {
                    self.settings
                        .library_folders
                        .push(p.to_string_lossy().into_owned());
                    dirs_added = true;
                }
                to_scan.push(p);
            } else if p.is_file() && Scanner::is_audio_file(&p) {
                to_scan.push(p);
            }
        }
        if dirs_added {
            self.save_settings_async();
            let _ = self
                .ui_tx
                .send(ControllerEvent::SettingsSaved(self.settings.clone()));
            self.sync_folder_watcher().await;
        }
        if !to_scan.is_empty() {
            if self.scan_cancel.is_some() {
                for p in to_scan {
                    if !self.pending_manual_paths.contains(&p) {
                        self.pending_manual_paths.push(p);
                    }
                }
            } else {
                self.start_scan_paths(to_scan, ScanKind::Manual, PrunePolicy::Safe);
            }
        }
    }

    pub(super) fn start_startup_sync(&mut self) {
        let roots = library_roots(&self.settings.library_folders, &self.albums);
        if self.scan_cancel.is_some() {
            self.pending_startup_sync = true;
            return;
        }
        self.start_scan_paths(roots, ScanKind::Startup, PrunePolicy::Safe);
    }

    pub(super) fn rescan_library(&mut self) {
        let roots = library_roots(&self.settings.library_folders, &self.albums);
        if roots.is_empty() {
            self.notice(NoticeKind::Info, "RESCAN // LIBRARY IS EMPTY");
            return;
        }
        if self.scan_cancel.is_some() {
            self.pending_full_rescan = true;
            self.notice(NoticeKind::Info, "SCAN_IN_PROGRESS // RESCAN QUEUED");
        } else {
            self.start_scan_paths(roots, ScanKind::Manual, PrunePolicy::Safe);
        }
    }

    pub(super) fn resync_covers(&mut self) {
        let roots = library_roots(&self.settings.library_folders, &self.albums);
        if roots.is_empty() {
            self.notice(NoticeKind::Info, "RESYNC_COVERS // LIBRARY IS EMPTY");
            return;
        }
        if self.scan_cancel.is_some() {
            self.pending_cover_resync = true;
            self.notice(NoticeKind::Info, "SCAN_IN_PROGRESS // COVER RESYNC QUEUED");
            return;
        }
        self.start_scan_paths(roots, ScanKind::CoverResync, PrunePolicy::Never);
    }

    pub(super) async fn sync_folder_watcher(&mut self) {
        if !self.settings.watch_folders || self.settings.library_folders.is_empty() {
            self.watcher = None;
            self.watch_rx = None;
            return;
        }
        let roots: Vec<PathBuf> = self
            .settings
            .library_folders
            .iter()
            .map(PathBuf::from)
            .collect();
        if let Some(mut watcher) = self.watcher.take() {
            match tokio::task::spawn_blocking(move || {
                watcher.update_roots(&roots);
                watcher
            })
            .await
            {
                Ok(w) => self.watcher = Some(w),
                Err(err) => warn!(error = %err, "Failed updating folder watcher roots"),
            }
        } else {
            match tokio::task::spawn_blocking(move || FolderWatcher::start(&roots)).await {
                Ok(Ok((watcher, rx))) => {
                    self.watcher = Some(watcher);
                    self.watch_rx = Some(rx);
                }
                Ok(Err(err)) => warn!(error = %err, "Failed starting folder watcher"),
                Err(err) => warn!(error = %err, "Folder watcher init task failed"),
            }
        }
    }

    pub(super) fn handle_watched_paths(&mut self, paths: Vec<PathBuf>) {
        let now = Instant::now();
        self.suppress_watch_paths
            .retain(|_, at| now.saturating_duration_since(*at) < WATCH_SUPPRESS_TTL);
        let paths: Vec<PathBuf> = paths
            .into_iter()
            .filter(|p| !self.suppress_watch_paths.contains_key(p))
            .collect();
        if paths.is_empty() {
            return;
        }
        if self.scan_cancel.is_some() {
            for p in paths {
                if !self.pending_watch_paths.contains(&p) && !self.pending_manual_paths.contains(&p)
                {
                    self.pending_watch_paths.push(p);
                }
            }
        } else {
            self.start_scan_paths(paths, ScanKind::Watch, PrunePolicy::Safe);
        }
    }

    pub(super) async fn handle_scan_done(&mut self, scan_res: Option<Result<ScanOutcome, String>>) {
        self.scan_cancel = None;
        self.scan_done_rx = None;
        let job = self.scan_job.take();
        let _ = self.ui_tx.send(ControllerEvent::ScanFinished);
        let resynced = job
            .as_ref()
            .is_some_and(|j| j.kind == ScanKind::CoverResync);
        match scan_res {
            Some(Ok(outcome)) => {
                if resynced {
                    let mut seen = HashSet::new();
                    let mut cover_paths = album_cover_paths(&self.albums);
                    seen.extend(cover_paths.iter().cloned());
                    self.reload_library(true).await;
                    for p in album_cover_paths(&self.albums) {
                        if seen.insert(p.clone()) {
                            cover_paths.push(p);
                        }
                    }
                    let _ = self
                        .ui_tx
                        .send(ControllerEvent::CoversResynced(cover_paths));
                } else {
                    if outcome.result.changed() || outcome.covers_migrated {
                        self.reload_library(true).await;
                    }
                    if !outcome.changed_paths.is_empty() {
                        let _ = self
                            .ui_tx
                            .send(ControllerEvent::CoversInvalidated(outcome.changed_paths));
                    }
                }
                if let Some(job) = job {
                    self.report_scan(&job, &outcome.result);
                }
            }
            Some(Err(err)) => {
                self.notice(NoticeKind::Error, format!("SCAN_FAILED // {err}"));
            }
            None => {}
        }
        self.drain_pending_watch_scan();
        self.play_pending_open().await;
    }

    fn drain_pending_watch_scan(&mut self) {
        if self.scan_cancel.is_some() {
            return;
        }
        if let Some(roots) = self.pending_prune.take() {
            self.start_scan_paths(roots, ScanKind::Manual, PrunePolicy::ForceAll);
            return;
        }
        if self.pending_cover_resync {
            self.pending_cover_resync = false;
            let roots = library_roots(&self.settings.library_folders, &self.albums);
            if !roots.is_empty() {
                self.pending_full_rescan = false;
                self.pending_watch_paths
                    .retain(|p| !roots.iter().any(|r| p.starts_with(r)));
                self.start_scan_paths(roots, ScanKind::CoverResync, PrunePolicy::Never);
                return;
            }
        }
        if self.pending_full_rescan {
            self.pending_full_rescan = false;
            let roots = library_roots(&self.settings.library_folders, &self.albums);
            self.pending_watch_paths
                .retain(|p| !roots.iter().any(|r| p.starts_with(r)));
            if !roots.is_empty() {
                self.start_scan_paths(roots, ScanKind::Manual, PrunePolicy::Safe);
                return;
            }
        }
        if self.pending_startup_sync {
            self.pending_startup_sync = false;
            let roots = library_roots(&self.settings.library_folders, &self.albums);
            self.pending_watch_paths
                .retain(|p| !roots.iter().any(|r| p.starts_with(r)));
            self.start_scan_paths(roots, ScanKind::Startup, PrunePolicy::Safe);
            return;
        }
        if !self.pending_manual_paths.is_empty() {
            let batch = std::mem::take(&mut self.pending_manual_paths);
            self.start_scan_paths(batch, ScanKind::Manual, PrunePolicy::Safe);
            return;
        }
        if !self.pending_watch_paths.is_empty() {
            let batch = std::mem::take(&mut self.pending_watch_paths);
            self.start_scan_paths(batch, ScanKind::Watch, PrunePolicy::Safe);
        }
    }

    pub(super) fn start_scan_paths(
        &mut self,
        paths: Vec<PathBuf>,
        kind: ScanKind,
        prune: PrunePolicy,
    ) {
        let is_startup = kind == ScanKind::Startup;
        if (!is_startup && paths.is_empty()) || self.scan_cancel.is_some() {
            return;
        }
        let threads = self.render_tier.scan_threads;
        let ui_tx = self.ui_tx.clone();
        let report_progress = matches!(kind, ScanKind::Manual | ScanKind::CoverResync);
        let resync_covers = kind == ScanKind::CoverResync;
        if report_progress {
            let _ = ui_tx.send(ControllerEvent::Notice {
                kind: NoticeKind::Info,
                text: if prune == PrunePolicy::ForceAll {
                    "SCAN_STARTED // REMOVING MISSING FILES".to_owned()
                } else if resync_covers {
                    "COVER_RESYNC_STARTED".to_owned()
                } else {
                    "SCAN_STARTED".to_owned()
                },
            });
            let _ = ui_tx.send(ControllerEvent::ScanProgress {
                phase: "discovering",
                scanned: 0,
                total: 0,
            });
        }
        self.scan_job = Some(ScanJob {
            kind,
            roots: paths.clone(),
            prune,
        });

        let cancel = Arc::new(AtomicBool::new(false));
        let cancel_worker = Arc::clone(&cancel);
        let (done_tx, done_rx) = mpsc::unbounded_channel();
        self.scan_cancel = Some(cancel);
        self.scan_done_rx = Some(done_rx);
        let collections_snapshot = is_startup.then(|| self.collections.clone());
        let live_playlist_covers = Arc::clone(&self.live_playlist_covers);

        tokio::task::spawn_blocking(move || {
            let mut last = Instant::now();
            let mut cb = |p: rustygrade_core::ScanProgress| {
                if report_progress && last.elapsed() >= Duration::from_millis(120) {
                    last = Instant::now();
                    let _ = ui_tx.send(ControllerEvent::ScanProgress {
                        phase: p.phase,
                        scanned: p.scanned,
                        total: p.total,
                    });
                }
            };
            let result = if is_startup && paths.is_empty() {
                Ok(ScanResult::default())
            } else {
                Scanner::scan(
                    &paths,
                    ScanOptions {
                        prune,
                        max_threads: Some(threads),
                        force: false,
                        resync_covers,
                    },
                    Some(&cancel_worker),
                    Some(&mut cb),
                )
                .map_err(|e| e.to_string())
            };
            let (covers_migrated, changed_paths) =
                if is_startup && result.is_ok() && !cancel_worker.load(Ordering::Relaxed) {
                    match deduplicate_and_sweep_covers(
                        collections_snapshot.as_ref(),
                        Some(&live_playlist_covers),
                    ) {
                        Ok(dedup) => (dedup.migrated_albums > 0, dedup.changed_paths),
                        Err(err) => {
                            warn!(error = %err, "Cover deduplication/sweep failed");
                            (false, Vec::new())
                        }
                    }
                } else {
                    (false, Vec::new())
                };
            let _ = done_tx.send(result.map(|r| ScanOutcome {
                result: r,
                covers_migrated,
                changed_paths,
            }));
        });
    }

    fn report_scan(&self, job: &ScanJob, r: &ScanResult) {
        match job.kind {
            ScanKind::Startup => {
                if r.changed() {
                    self.notice(
                        NoticeKind::Success,
                        format!("LIBRARY_SYNCED // {}", scan_summary(r)),
                    );
                }
                if r.missing_tracks > 0 {
                    self.notice(
                        NoticeKind::Warn,
                        format!(
                            "MISSING_FILES // {} TRACK(S) IN OFFLINE FOLDER — PRESS F5 TO REMOVE",
                            r.missing_tracks
                        ),
                    );
                }
            }
            ScanKind::Manual => {
                if r.changed() {
                    self.notice(
                        NoticeKind::Success,
                        format!("SCAN_COMPLETE // {}", scan_summary(r)),
                    );
                } else if r.missing_tracks == 0 {
                    self.notice(NoticeKind::Info, "SCAN_COMPLETE // NO CHANGES");
                }
                if job.prune != PrunePolicy::ForceAll && r.missing_tracks > 0 {
                    let _ = self.ui_tx.send(ControllerEvent::MissingTracks {
                        count: r.missing_tracks,
                        roots: job.roots.clone(),
                    });
                }
            }
            ScanKind::Watch => {
                if r.changed() {
                    self.notice(
                        NoticeKind::Success,
                        format!("LIBRARY_UPDATED // {}", scan_summary(r)),
                    );
                }
            }
            ScanKind::CoverResync => {
                self.notice(
                    NoticeKind::Success,
                    format!("COVERS_RESYNCED // {} ALBUM(S)", r.covers),
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn album_at(paths: &[&str]) -> Album {
        Album {
            name: "A".to_owned(),
            artist: "Artist".to_owned(),
            cover_thumb_path: None,
            cover_full_path: None,
            year: 0,
            added_at: 0,
            tracks: paths
                .iter()
                .map(|p| Track {
                    id: (*p).to_owned(),
                    title: String::new(),
                    artist: String::new(),
                    album: "A".to_owned(),
                    duration: 0.0,
                    file_path: (*p).to_owned(),
                    track_number: 0,
                    disc_number: 0,
                    codec: String::new(),
                    sample_rate: 0,
                    bits_per_sample: 0,
                    channels: 0,
                    cover_thumb_path: None,
                    cover_full_path: None,
                })
                .collect(),
        }
    }

    #[test]
    fn resolve_open_uri_handles_plain_paths_file_urls_and_missing() {
        let dir = std::env::temp_dir().join(format!("rustygrade-open-uri-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let file = dir.join("a b.flac");
        std::fs::write(&file, b"x").unwrap();
        let canon = std::fs::canonicalize(&file).unwrap();

        assert_eq!(
            resolve_open_uri(file.to_str().unwrap()),
            Some(canon.clone())
        );
        assert_eq!(
            resolve_open_uri(&CoverPaths::to_file_url(&file)),
            Some(canon)
        );
        assert_eq!(
            resolve_open_uri(dir.join("missing.flac").to_str().unwrap()),
            None
        );
        assert_eq!(resolve_open_uri("file:///definitely/not/here.flac"), None);
        assert_eq!(resolve_open_uri("   "), None);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn library_roots_merge_folders_and_track_dirs_without_nesting() {
        let folders = vec!["/music".to_owned(), "/music/Rock".to_owned(), String::new()];
        let albums = vec![
            album_at(&["/music/Rock/A/01.flac", "/music/Rock/A/02.flac"]),
            album_at(&["/mnt/usb/Jazz/B/01.flac"]),
            album_at(&["/musicbox/D/01.flac"]),
        ];
        assert_eq!(
            library_roots(&folders, &albums),
            vec![
                PathBuf::from("/music"),
                PathBuf::from("/musicbox/D"),
                PathBuf::from("/mnt/usb/Jazz/B"),
            ]
        );
        assert!(library_roots(&[], &[]).is_empty());
    }

    #[test]
    fn scan_summary_lists_only_nonzero_parts() {
        let r = ScanResult {
            added_tracks: 3,
            removed_tracks: 1,
            covers: 2,
            ..ScanResult::default()
        };
        assert_eq!(scan_summary(&r), "3 NEW, 1 REMOVED, 2 COVERS");
        assert_eq!(scan_summary(&ScanResult::default()), "");
    }

    #[test]
    fn reconcile_queue_with_library_drops_missing_and_refreshes_surviving_tracks() {
        let old_thumb = Some(PathBuf::from("/tmp/old_thumb.jpg"));
        let new_cover = Some(PathBuf::from("/tmp/c_migrated.jpg"));
        let mk = |id: &str, title: &str, cov: Option<PathBuf>| Track {
            id: id.to_owned(),
            title: title.to_owned(),
            cover_thumb_path: cov.clone(),
            cover_full_path: cov,
            ..album_at(&[id]).tracks.into_iter().next().unwrap()
        };
        let mut queue = PlaybackQueue::default();
        queue.set_queue(
            vec![
                mk("/music/01.flac", "Removed", old_thumb.clone()),
                mk("/music/02.flac", "Old Title", old_thumb),
            ],
            Some(0),
        );
        let updated = Album {
            tracks: vec![mk("/music/02.flac", "New Title", new_cover.clone())],
            cover_thumb_path: new_cover.clone(),
            cover_full_path: new_cover.clone(),
            ..album_at(&[])
        };
        let albums = [updated];
        let map = build_track_map(&albums);
        let (q_changed, cur_removed) = reconcile_queue_with_library(&mut queue, &map);
        assert!(q_changed && cur_removed);
        assert_eq!(queue.tracks().len(), 1);
        assert_eq!(queue.tracks()[0].title, "New Title");
        assert_eq!(queue.tracks()[0].cover_full_path, new_cover);
    }

    #[test]
    fn failed_track_rescan_target_respects_library_roots_and_walks_up() {
        let base =
            std::env::temp_dir().join(format!("rustygrade-failed-rescan-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let lib_root = base.join("Music");
        let existing_album = lib_root.join("ExistingAlbum");
        let deleted_album = lib_root.join("DeletedAlbum");
        let outside_dir = base.join("Outside");
        std::fs::create_dir_all(&existing_album).unwrap();
        std::fs::create_dir_all(&outside_dir).unwrap();

        let folders = vec![
            lib_root.to_string_lossy().into_owned(),
            base.join("MissingRoot").to_string_lossy().into_owned(),
        ];
        assert_eq!(
            std::fs::canonicalize(
                failed_track_rescan_target(&existing_album.join("01.flac"), &folders).unwrap()
            )
            .unwrap(),
            std::fs::canonicalize(&existing_album).unwrap()
        );
        assert_eq!(
            std::fs::canonicalize(
                failed_track_rescan_target(&deleted_album.join("01.flac"), &folders).unwrap()
            )
            .unwrap(),
            std::fs::canonicalize(&lib_root).unwrap()
        );
        assert_eq!(
            failed_track_rescan_target(&outside_dir.join("loose.flac"), &folders),
            None
        );
        assert_eq!(
            failed_track_rescan_target(&base.join("MissingRoot/Album/01.flac"), &folders),
            None
        );

        let _ = std::fs::remove_dir_all(&base);
    }
}
