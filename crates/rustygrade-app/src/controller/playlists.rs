use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use rustygrade_core::{
    Album, CollectionsData, CollectionsStore, CoverPaths, IngestedZipImport, LibraryStore,
    M3uImportResult, PlayStats, PlaylistCovers, PlaylistData, PlaylistIoError, SmartPlaylistData,
    SmartPlaylists, Track, ZipImportReport, build_track_map, claim_next_id,
    collect_playlist_cover_paths, delete_unreferenced_covers_with_extra,
    export_collections_zip_file, export_m3u8_file, export_playlist_zip_file, import_m3u_file,
    prepare_zip_import, read_backup_zip, reorder_by_ids,
};
use tokio::sync::mpsc;

use super::Coordinator;
use super::persist::PersistMsg;
use super::types::{ControllerEvent, NoticeKind, PlaylistExportTarget};
use crate::library_view::ensure_default_smart_playlists;

pub(super) enum PlaylistIoDone {
    M3u(Result<M3uImportResult, PlaylistIoError>),
    Zip {
        replace: bool,
        result: Result<IngestedZipImport, (PlaylistIoError, Vec<PathBuf>)>,
    },
}

struct ImportPanicGuard {
    tx: Option<mpsc::UnboundedSender<PlaylistIoDone>>,
    zip_replace: Option<bool>,
}

impl ImportPanicGuard {
    fn m3u(tx: mpsc::UnboundedSender<PlaylistIoDone>) -> Self {
        Self {
            tx: Some(tx),
            zip_replace: None,
        }
    }

    fn zip(tx: mpsc::UnboundedSender<PlaylistIoDone>, replace: bool) -> Self {
        Self {
            tx: Some(tx),
            zip_replace: Some(replace),
        }
    }

    fn send(mut self, done: PlaylistIoDone) {
        if let Some(tx) = self.tx.take() {
            let _ = tx.send(done);
        }
    }
}

impl Drop for ImportPanicGuard {
    fn drop(&mut self) {
        let Some(tx) = self.tx.take() else {
            return;
        };
        let err = PlaylistIoError::Io(std::io::Error::other("import task panicked unexpectedly"));
        let done = match self.zip_replace {
            Some(replace) => PlaylistIoDone::Zip {
                replace,
                result: Err((err, Vec::new())),
            },
            None => PlaylistIoDone::M3u(Err(err)),
        };
        let _ = tx.send(done);
    }
}

/// Strict lock order: `live` before `pending` (`LibraryStore::write_lock()` -> `live` -> `pending`).
fn sync_live_playlist_covers_with_pending(
    collections: &CollectionsData,
    live: &Mutex<HashSet<PathBuf>>,
    pending: &Mutex<HashSet<PathBuf>>,
) {
    let mut set = HashSet::new();
    collect_playlist_cover_paths(collections, &mut set);
    if let Ok(mut live_guard) = live.lock() {
        if let Ok(pending_guard) = pending.lock() {
            set.extend(pending_guard.iter().cloned());
        }
        *live_guard = set;
    }
}

fn clear_pending_and_sync_live_covers(
    collections: &CollectionsData,
    live: &Mutex<HashSet<PathBuf>>,
    pending: &Mutex<HashSet<PathBuf>>,
) {
    let mut set = HashSet::new();
    collect_playlist_cover_paths(collections, &mut set);
    if let Ok(mut live_guard) = live.lock() {
        if let Ok(mut pending_guard) = pending.lock() {
            pending_guard.clear();
        }
        *live_guard = set;
    }
}

fn resolve_m3u_export_from(
    albums: &[Album],
    collections: &CollectionsData,
    stats: &PlayStats,
    target: PlaylistExportTarget,
    now_ms: i64,
) -> Result<(String, Vec<Track>), &'static str> {
    match target {
        PlaylistExportTarget::Liked => {
            let map = build_track_map(albums);
            let tracks = collections
                .liked
                .iter()
                .filter_map(|id| map.get(id.as_str()).map(|t| (*t).clone()))
                .collect();
            Ok(("Favorites".to_owned(), tracks))
        }
        PlaylistExportTarget::Custom(id) => {
            let Some(p) = collections.playlists.iter().find(|p| p.id == id) else {
                return Err("PLAYLIST NOT FOUND");
            };
            let map = build_track_map(albums);
            let tracks = p
                .track_ids
                .iter()
                .filter_map(|tid| map.get(tid.as_str()).map(|t| (*t).clone()))
                .collect();
            Ok((p.name.clone(), tracks))
        }
        PlaylistExportTarget::Smart(id) => {
            let Some(sp) = collections
                .smart
                .as_deref()
                .unwrap_or(&[])
                .iter()
                .find(|s| s.id == id)
            else {
                return Err("SMART PLAYLIST NOT FOUND");
            };
            let liked_set: HashSet<&str> = collections.liked.iter().map(String::as_str).collect();
            let tracks = SmartPlaylists::evaluate(
                sp,
                albums
                    .iter()
                    .flat_map(|a| a.tracks.iter().map(move |t| (t, a))),
                |tid| stats.get(tid),
                |tid| liked_set.contains(tid),
                now_ms,
            );
            Ok((sp.name.clone(), tracks))
        }
    }
}

fn format_m3u_import_notice(res: &M3uImportResult) -> (NoticeKind, String) {
    let matched = res.exact + res.remapped + res.metadata;
    let mut details = vec![format!("{matched} MATCHED")];
    if res.remapped > 0 {
        details.push(format!("{} REMAPPED", res.remapped));
    }
    if res.metadata > 0 {
        details.push(format!("{} META", res.metadata));
    }
    if res.unmatched > 0 {
        details.push(format!("{} UNMATCHED", res.unmatched));
    }
    let kind = if res.unmatched > 0 {
        NoticeKind::Warn
    } else {
        NoticeKind::Success
    };
    (
        kind,
        format!("IMPORTED {} // {}", res.name, details.join(", ")),
    )
}

fn format_zip_import_notice(report: &ZipImportReport) -> (NoticeKind, String) {
    let mut parts = Vec::new();
    if report.collections_imported {
        parts.push(format!("{} PL", report.playlists_imported));
    }
    if report.liked_added > 0 {
        parts.push(format!("+{} FAV", report.liked_added));
    }
    if report.stats_imported > 0 {
        parts.push(format!("{} STATS", report.stats_imported));
    }
    if report.covers_restored > 0 {
        parts.push(format!("{} COV", report.covers_restored));
    }
    if report.tracks_remapped > 0 {
        parts.push(format!("{} REMAP", report.tracks_remapped));
    }
    if report.tracks_unmatched > 0 {
        parts.push(format!("{} UNMATCHED", report.tracks_unmatched));
    }
    if report.settings_skipped {
        parts.push("SETTINGS SKIPPED".to_owned());
    }
    if parts.is_empty() {
        parts.push("NO CHANGES".to_owned());
    }
    let kind = if report.tracks_unmatched > 0 {
        NoticeKind::Warn
    } else {
        NoticeKind::Success
    };
    (kind, format!("ARCHIVE_IMPORTED // {}", parts.join(", ")))
}

impl Coordinator {
    pub(super) fn sync_live_playlist_covers(&self) {
        sync_live_playlist_covers_with_pending(
            &self.collections,
            &self.live_playlist_covers,
            &self.pending_import_covers,
        );
    }

    pub(super) fn toggle_like(&mut self, track_id: String) {
        if let Some(pos) = self.collections.liked.iter().position(|id| id == &track_id) {
            self.collections.liked.remove(pos);
        } else {
            self.collections.liked.push(track_id);
        }
        self.persist_collections();
    }

    pub(super) fn toggle_like_tracks(&mut self, track_ids: Vec<String>) {
        if track_ids.is_empty() {
            return;
        }
        let liked: HashSet<&str> = self.collections.liked.iter().map(String::as_str).collect();
        let all_liked = track_ids.iter().all(|id| liked.contains(id.as_str()));
        if all_liked {
            let drop_set: HashSet<&str> = track_ids.iter().map(String::as_str).collect();
            let before = self.collections.liked.len();
            self.collections
                .liked
                .retain(|id| !drop_set.contains(id.as_str()));
            let removed = before - self.collections.liked.len();
            self.persist_collections();
            self.notice(
                NoticeKind::Info,
                format!("TRACKS_REMOVED // FAVORITES ({removed})"),
            );
        } else {
            let mut existing: HashSet<String> = self.collections.liked.iter().cloned().collect();
            let mut added = 0usize;
            for id in track_ids {
                if existing.insert(id.clone()) {
                    self.collections.liked.push(id);
                    added += 1;
                }
            }
            if added > 0 {
                self.persist_collections();
                self.notice(
                    NoticeKind::Success,
                    format!("TRACKS_ADDED // FAVORITES ({added})"),
                );
            }
        }
    }

    pub(super) fn create_playlist(&mut self, name: &str) {
        let trimmed = name.trim();
        if trimmed.is_empty() {
            return;
        }
        let next_id = self
            .collections
            .playlists
            .iter()
            .map(|p| p.id)
            .max()
            .unwrap_or(0)
            + 1;
        self.collections.playlists.push(PlaylistData {
            id: next_id,
            name: trimmed.to_owned(),
            ..Default::default()
        });
        self.persist_collections();
    }

    pub(super) async fn delete_playlist(&mut self, id: i64) {
        let old_cover = self
            .collections
            .playlists
            .iter()
            .find(|p| p.id == id)
            .and_then(|p| p.cover_art.clone());
        self.collections.playlists.retain(|p| p.id != id);
        self.persist_collections();
        if let Some(url) = old_cover {
            self.flush_persist().await;
            Self::spawn_covers_gc(vec![url], Arc::clone(&self.live_playlist_covers));
        }
    }

    pub(super) fn rename_playlist(&mut self, id: i64, name: &str) {
        let trimmed = name.trim();
        if !trimmed.is_empty()
            && let Some(p) = self.collections.playlists.iter_mut().find(|p| p.id == id)
        {
            p.name = trimmed.to_owned();
            self.persist_collections();
        }
    }

    pub(super) fn add_tracks_to_playlist(&mut self, playlist_id: i64, track_ids: Vec<String>) {
        if let Some(p) = self
            .collections
            .playlists
            .iter_mut()
            .find(|p| p.id == playlist_id)
        {
            let name = p.name.clone();
            let mut existing: HashSet<String> = p.track_ids.iter().cloned().collect();
            let mut added = 0usize;
            for id in track_ids {
                if existing.insert(id.clone()) {
                    p.track_ids.push(id);
                    added += 1;
                }
            }
            if added > 0 {
                self.persist_collections();
                self.notice(
                    NoticeKind::Success,
                    format!("TRACKS_ADDED // PLAYLIST: {name}"),
                );
            }
        }
    }

    pub(super) fn toggle_playlist_tracks(&mut self, playlist_id: i64, track_ids: Vec<String>) {
        if track_ids.is_empty() {
            return;
        }
        if let Some(p) = self
            .collections
            .playlists
            .iter_mut()
            .find(|p| p.id == playlist_id)
        {
            let name = p.name.clone();
            let set: HashSet<&str> = p.track_ids.iter().map(String::as_str).collect();
            let all_present = track_ids.iter().all(|id| set.contains(id.as_str()));
            let msg = if all_present {
                let drop_set: HashSet<&str> = track_ids.iter().map(String::as_str).collect();
                p.track_ids.retain(|id| !drop_set.contains(id.as_str()));
                (
                    NoticeKind::Info,
                    format!("TRACKS_REMOVED // PLAYLIST: {name}"),
                )
            } else {
                let mut existing: HashSet<String> = p.track_ids.iter().cloned().collect();
                for id in track_ids {
                    if existing.insert(id.clone()) {
                        p.track_ids.push(id);
                    }
                }
                (
                    NoticeKind::Success,
                    format!("TRACKS_ADDED // PLAYLIST: {name}"),
                )
            };
            self.persist_collections();
            self.notice(msg.0, msg.1);
        }
    }

    pub(super) fn remove_track_from_playlist(&mut self, playlist_id: i64, track_id: &str) {
        if let Some(p) = self
            .collections
            .playlists
            .iter_mut()
            .find(|p| p.id == playlist_id)
        {
            p.track_ids.retain(|id| id != track_id);
            self.persist_collections();
        }
    }

    pub(super) fn set_playlist_order(&mut self, playlist_id: i64, track_ids: &[String]) {
        if let Some(p) = self
            .collections
            .playlists
            .iter_mut()
            .find(|p| p.id == playlist_id)
        {
            reorder_by_ids(&mut p.track_ids, track_ids, |s| Some(s.as_str()));
            self.persist_collections();
        }
    }

    pub(super) fn set_liked_order(&mut self, track_ids: &[String]) {
        reorder_by_ids(&mut self.collections.liked, track_ids, |s| Some(s.as_str()));
        self.persist_collections();
    }

    pub(super) async fn set_playlist_cover(&mut self, playlist_id: i64, image_path: PathBuf) {
        let prev = self
            .collections
            .playlists
            .iter()
            .find(|p| p.id == playlist_id)
            .and_then(|p| p.cover_art.clone());
        let live_refs = Arc::clone(&self.live_playlist_covers);
        match tokio::task::spawn_blocking(move || {
            let _guard = LibraryStore::write_lock()
                .lock()
                .expect("library write lock poisoned");
            let url = PlaylistCovers::save(&image_path).map_err(|e| e.to_string())?;
            if let Some(local) = CoverPaths::to_local_path(Some(&url))
                && let Ok(mut guard) = live_refs.lock()
            {
                guard.insert(local);
            }
            Ok::<String, String>(url)
        })
        .await
        {
            Ok(Ok(url)) => {
                if let Some(p) = self
                    .collections
                    .playlists
                    .iter_mut()
                    .find(|p| p.id == playlist_id)
                {
                    p.cover_art = Some(url.clone());
                    self.persist_collections();
                    if let Some(old) = prev.filter(|old| old != &url) {
                        self.flush_persist().await;
                        Self::spawn_covers_gc(vec![old], Arc::clone(&self.live_playlist_covers));
                    }
                } else {
                    self.sync_live_playlist_covers();
                    self.flush_persist().await;
                    Self::spawn_covers_gc(vec![url], Arc::clone(&self.live_playlist_covers));
                }
            }
            Ok(Err(err)) => self.notice(NoticeKind::Error, format!("COVER_FAILED // {err}")),
            Err(err) => self.notice(NoticeKind::Error, format!("COVER_FAILED // {err}")),
        }
    }

    pub(super) async fn remove_playlist_cover(&mut self, playlist_id: i64) {
        if let Some(p) = self
            .collections
            .playlists
            .iter_mut()
            .find(|p| p.id == playlist_id)
            && let Some(prev) = p.cover_art.take()
        {
            self.persist_collections();
            self.flush_persist().await;
            Self::spawn_covers_gc(vec![prev], Arc::clone(&self.live_playlist_covers));
        }
    }

    pub(super) fn save_smart_playlist(&mut self, sp: SmartPlaylistData) {
        let list = self.collections.smart.get_or_insert_with(Vec::new);
        if let Some(existing) = list.iter_mut().find(|x| x.id == sp.id && sp.id > 0) {
            *existing = sp;
        } else {
            let next_id = list.iter().map(|x| x.id).max().unwrap_or(0) + 1;
            let mut created = sp;
            if created.id <= 0 {
                created.id = next_id;
            }
            list.push(created);
        }
        self.persist_collections();
    }

    pub(super) fn delete_smart_playlist(&mut self, id: i64) {
        if let Some(list) = &mut self.collections.smart {
            list.retain(|x| x.id != id);
            self.persist_collections();
        }
    }

    fn spawn_covers_gc(urls: Vec<String>, live_playlist_covers: Arc<Mutex<HashSet<PathBuf>>>) {
        if urls.is_empty() {
            return;
        }
        tokio::task::spawn_blocking(move || {
            let mut seen = HashSet::new();
            let mut candidates = Vec::new();
            for url in urls {
                let Some(local) = CoverPaths::to_local_path(Some(&url)) else {
                    continue;
                };
                let thumb = PlaylistCovers::thumb_for(&local);
                if seen.insert(local.clone()) {
                    candidates.push(local);
                }
                if seen.insert(thumb.clone()) {
                    candidates.push(thumb);
                }
            }
            if candidates.is_empty() {
                return;
            }
            let _guard = LibraryStore::write_lock()
                .lock()
                .expect("library write lock poisoned");
            if let Ok(library) = LibraryStore::load_raw() {
                let collections = CollectionsStore::load();
                let extra_refs = live_playlist_covers
                    .lock()
                    .map(|g| g.clone())
                    .unwrap_or_default();
                delete_unreferenced_covers_with_extra(
                    &candidates,
                    &library,
                    &collections,
                    &extra_refs,
                );
            }
        });
    }

    pub(super) fn start_export_m3u8(&self, target: PlaylistExportTarget, dest: PathBuf) {
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        let (name, tracks) = match resolve_m3u_export_from(
            &self.albums,
            &self.collections,
            &self.stats,
            target,
            now_ms,
        ) {
            Ok(v) => v,
            Err(reason) => {
                self.notice(NoticeKind::Error, format!("EXPORT_FAILED // {reason}"));
                return;
            }
        };
        let ui_tx = self.ui_tx.clone();
        let count = tracks.len();
        tokio::task::spawn_blocking(move || {
            let (kind, text) = match export_m3u8_file(&dest, &name, &tracks) {
                Ok(()) => (
                    NoticeKind::Success,
                    format!("EXPORTED_M3U8 // {name} ({count} TRACKS)"),
                ),
                Err(err) => (NoticeKind::Error, format!("EXPORT_FAILED // {err}")),
            };
            let _ = ui_tx.send(ControllerEvent::Notice { kind, text });
        });
    }

    pub(super) fn start_export_playlist_zip(&self, playlist_id: i64, dest: PathBuf) {
        let Some(playlist) = self
            .collections
            .playlists
            .iter()
            .find(|p| p.id == playlist_id)
            .cloned()
        else {
            self.notice(NoticeKind::Error, "EXPORT_FAILED // PLAYLIST NOT FOUND");
            return;
        };
        let ui_tx = self.ui_tx.clone();
        let name = playlist.name.clone();
        let track_count = playlist.track_ids.len();
        tokio::task::spawn_blocking(move || {
            let (kind, text) = match export_playlist_zip_file(&dest, &playlist) {
                Ok(covers) => (
                    NoticeKind::Success,
                    format!("EXPORTED_ZIP // {name} ({track_count} TRACKS, {covers} COVER)"),
                ),
                Err(err) => (NoticeKind::Error, format!("EXPORT_FAILED // {err}")),
            };
            let _ = ui_tx.send(ControllerEvent::Notice { kind, text });
        });
    }

    pub(super) fn start_export_collections_zip(&self, dest: PathBuf) {
        let collections = self.collections.clone();
        let pl_count = collections.playlists.len();
        let liked_count = collections.liked.len();
        let ui_tx = self.ui_tx.clone();
        tokio::task::spawn_blocking(move || {
            let (kind, text) = match export_collections_zip_file(&dest, &collections) {
                Ok(covers) => (
                    NoticeKind::Success,
                    format!("BACKUP_EXPORTED // {pl_count} PL, {liked_count} FAV, {covers} COV"),
                ),
                Err(err) => (NoticeKind::Error, format!("EXPORT_FAILED // {err}")),
            };
            let _ = ui_tx.send(ControllerEvent::Notice { kind, text });
        });
    }

    pub(super) fn start_import_m3u(&mut self, path: PathBuf) {
        if self.playlist_import_in_flight {
            self.notice(
                NoticeKind::Info,
                "IMPORT_IN_PROGRESS // WAIT FOR CURRENT IMPORT",
            );
            return;
        }
        self.playlist_import_in_flight = true;
        let albums = Arc::clone(&self.albums);
        let source_roots = self.settings.library_folders.clone();
        let guard = ImportPanicGuard::m3u(self.playlist_io_done_tx.clone());
        tokio::task::spawn_blocking(move || {
            let res = import_m3u_file(&path, &albums, &source_roots);
            guard.send(PlaylistIoDone::M3u(res));
        });
    }

    pub(super) fn start_import_zip(&mut self, path: PathBuf, replace: bool) {
        if self.playlist_import_in_flight {
            self.notice(
                NoticeKind::Info,
                "IMPORT_IN_PROGRESS // WAIT FOR CURRENT IMPORT",
            );
            return;
        }
        self.playlist_import_in_flight = true;
        let albums = Arc::clone(&self.albums);
        let source_roots = self.settings.library_folders.clone();
        let live_covers = Arc::clone(&self.live_playlist_covers);
        let pending_covers = Arc::clone(&self.pending_import_covers);
        let guard = ImportPanicGuard::zip(self.playlist_io_done_tx.clone(), replace);
        tokio::task::spawn_blocking(move || {
            let res = (|| -> Result<IngestedZipImport, (PlaylistIoError, Vec<PathBuf>)> {
                let file =
                    std::fs::File::open(&path).map_err(|e| (PlaylistIoError::Io(e), Vec::new()))?;
                let bundle = read_backup_zip(file).map_err(|e| (e, Vec::new()))?;
                let prepared = prepare_zip_import(bundle, &albums, &source_roots)
                    .map_err(|e| (e, Vec::new()))?;
                prepared.ingest_covers(Some(&live_covers), Some(&pending_covers))
            })();
            guard.send(PlaylistIoDone::Zip {
                replace,
                result: res,
            });
        });
    }

    pub(super) async fn handle_playlist_io_done(&mut self, done: PlaylistIoDone) {
        self.playlist_import_in_flight = false;
        match done {
            PlaylistIoDone::M3u(Ok(res)) => {
                let mut used_ids: HashSet<i64> =
                    self.collections.playlists.iter().map(|p| p.id).collect();
                let wanted = used_ids
                    .iter()
                    .copied()
                    .max()
                    .unwrap_or(0)
                    .saturating_add(1);
                let id = claim_next_id(&mut used_ids, wanted);
                self.collections.playlists.push(PlaylistData {
                    id,
                    name: res.name.clone(),
                    cover_art: None,
                    track_ids: res.track_ids.clone(),
                    extra: Default::default(),
                });
                self.persist_collections();
                let (kind, text) = format_m3u_import_notice(&res);
                self.notice(kind, text);
            }
            PlaylistIoDone::M3u(Err(err)) => {
                let kind = if matches!(
                    err,
                    PlaylistIoError::EmptyPlaylist | PlaylistIoError::NoMatchingTracks { .. }
                ) {
                    NoticeKind::Warn
                } else {
                    NoticeKind::Error
                };
                self.notice(kind, format!("IMPORT_FAILED // {err}"));
            }
            PlaylistIoDone::Zip {
                replace,
                result: Ok(ingested),
            } => {
                let restored_paths = ingested.restored_cover_paths.clone();
                let (report, old_covers, stats_changed) =
                    ingested.apply(&mut self.collections, &mut self.stats, replace);
                if report.collections_imported {
                    ensure_default_smart_playlists(&mut self.collections);
                    clear_pending_and_sync_live_covers(
                        &self.collections,
                        &self.live_playlist_covers,
                        &self.pending_import_covers,
                    );
                    let data = self.collections.clone();
                    let _ = self
                        .ui_tx
                        .send(ControllerEvent::CollectionsUpdated(data.clone()));
                    let _ = self.persist_tx.send(PersistMsg::Collections(data));
                } else {
                    clear_pending_and_sync_live_covers(
                        &self.collections,
                        &self.live_playlist_covers,
                        &self.pending_import_covers,
                    );
                }
                if stats_changed {
                    let _ = self
                        .ui_tx
                        .send(ControllerEvent::StatsUpdated(self.stats.clone()));
                    let _ = self.persist_tx.send(PersistMsg::Stats(self.stats.clone()));
                }
                if !restored_paths.is_empty() {
                    let _ = self
                        .ui_tx
                        .send(ControllerEvent::CoversInvalidated(restored_paths));
                }
                if !old_covers.is_empty() {
                    self.flush_persist().await;
                    Self::spawn_covers_gc(old_covers, Arc::clone(&self.live_playlist_covers));
                }
                let (kind, text) = format_zip_import_notice(&report);
                self.notice(kind, text);
            }
            PlaylistIoDone::Zip {
                result: Err((err, written_paths)),
                ..
            } => {
                clear_pending_and_sync_live_covers(
                    &self.collections,
                    &self.live_playlist_covers,
                    &self.pending_import_covers,
                );
                if !written_paths.is_empty() {
                    let urls: Vec<String> = written_paths
                        .iter()
                        .map(|p| CoverPaths::to_file_url(p))
                        .collect();
                    self.flush_persist().await;
                    Self::spawn_covers_gc(urls, Arc::clone(&self.live_playlist_covers));
                }
                self.notice(NoticeKind::Error, format!("IMPORT_FAILED // {err}"));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustygrade_core::{PlayStat, SmartRule};

    #[test]
    fn playlist_import_notices_summarize_counts_and_warn_when_unmatched() {
        let ok = M3uImportResult {
            name: "Roadtrip".to_owned(),
            track_ids: vec!["/a.flac".to_owned(), "/b.flac".to_owned()],
            exact: 1,
            remapped: 1,
            metadata: 0,
            unmatched: 0,
            total_entries: 2,
        };
        let (kind, text) = format_m3u_import_notice(&ok);
        assert_eq!(kind, NoticeKind::Success);
        assert_eq!(text, "IMPORTED Roadtrip // 2 MATCHED, 1 REMAPPED");

        let partial = M3uImportResult {
            name: "Mixed".to_owned(),
            track_ids: vec!["/a.flac".to_owned()],
            exact: 0,
            remapped: 0,
            metadata: 1,
            unmatched: 2,
            total_entries: 3,
        };
        let (kind, text) = format_m3u_import_notice(&partial);
        assert_eq!(kind, NoticeKind::Warn);
        assert_eq!(text, "IMPORTED Mixed // 1 MATCHED, 1 META, 2 UNMATCHED");

        let zip_report = ZipImportReport {
            collections_imported: true,
            liked_added: 3,
            liked_total: 10,
            playlists_imported: 2,
            playlists_total: 4,
            smart_playlists_imported: 1,
            stats_imported: 12,
            settings_skipped: true,
            tracks_exact: 5,
            tracks_remapped: 2,
            tracks_unmatched: 1,
            covers_restored: 2,
        };
        let (zkind, ztext) = format_zip_import_notice(&zip_report);
        assert_eq!(zkind, NoticeKind::Warn);
        assert_eq!(
            ztext,
            "ARCHIVE_IMPORTED // 2 PL, +3 FAV, 12 STATS, 2 COV, 2 REMAP, 1 UNMATCHED, SETTINGS SKIPPED"
        );

        let stats_only = ZipImportReport {
            stats_imported: 5,
            ..Default::default()
        };
        let (skind, stext) = format_zip_import_notice(&stats_only);
        assert_eq!(skind, NoticeKind::Success);
        assert_eq!(stext, "ARCHIVE_IMPORTED // 5 STATS");
    }

    #[test]
    fn import_panic_guard_delivers_error_on_panic_and_clears_when_sent() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = ImportPanicGuard::m3u(tx.clone());
            panic!("boom m3u");
        }));
        assert!(matches!(
            rx.try_recv().unwrap(),
            PlaylistIoDone::M3u(Err(_))
        ));

        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = ImportPanicGuard::zip(tx.clone(), true);
            panic!("boom zip");
        }));
        assert!(matches!(
            rx.try_recv().unwrap(),
            PlaylistIoDone::Zip { replace: true, result: Err((_, written)) } if written.is_empty()
        ));

        let guard = ImportPanicGuard::m3u(tx);
        guard.send(PlaylistIoDone::M3u(Err(PlaylistIoError::EmptyPlaylist)));
        assert!(rx.try_recv().is_ok() && rx.try_recv().is_err());
    }

    #[test]
    fn pending_import_covers_survive_collection_sync_until_cleared() {
        let live = Mutex::new(HashSet::new());
        let pending = Mutex::new(HashSet::new());
        let existing_path = PathBuf::from("/tmp/covers/c_111111111111111111111111.png");
        let imported_path = PathBuf::from("/tmp/covers/c_222222222222222222222222.png");

        let mut collections = CollectionsData {
            playlists: vec![PlaylistData {
                id: 1,
                name: "Existing".to_owned(),
                cover_art: Some(CoverPaths::to_file_url(&existing_path)),
                track_ids: vec![],
                extra: Default::default(),
            }],
            ..Default::default()
        };
        live.lock().unwrap().insert(imported_path.clone());
        pending.lock().unwrap().insert(imported_path.clone());
        collections.liked.push("/music/song.flac".to_owned());

        sync_live_playlist_covers_with_pending(&collections, &live, &pending);
        let snap = live.lock().unwrap().clone();
        assert!(snap.contains(&existing_path) && snap.contains(&imported_path));

        clear_pending_and_sync_live_covers(&collections, &live, &pending);
        let after = live.lock().unwrap().clone();
        assert!(
            pending.lock().unwrap().is_empty()
                && after.contains(&existing_path)
                && !after.contains(&imported_path)
        );
    }

    #[test]
    fn resolve_m3u_export_from_handles_liked_custom_and_smart_playlists() {
        let mk = |id: &str| Track {
            id: id.to_owned(),
            title: id.to_owned(),
            artist: "Art".to_owned(),
            album: "Alb".to_owned(),
            duration: 180.0,
            file_path: id.to_owned(),
            track_number: 1,
            disc_number: 1,
            codec: "FLAC".to_owned(),
            sample_rate: 44100,
            bits_per_sample: 16,
            channels: 2,
            cover_thumb_path: None,
            cover_full_path: None,
        };
        let albums = vec![Album {
            name: "Alb".to_owned(),
            artist: "Art".to_owned(),
            cover_thumb_path: None,
            cover_full_path: None,
            year: 2024,
            added_at: 1,
            tracks: vec![mk("/m/1.flac"), mk("/m/2.flac")],
        }];
        let collections = CollectionsData {
            liked: vec!["/m/2.flac".into()],
            playlists: vec![PlaylistData {
                id: 7,
                name: "Mix".into(),
                cover_art: None,
                track_ids: vec!["/m/1.flac".into(), "/m/2.flac".into()],
                extra: Default::default(),
            }],
            smart: Some(vec![SmartPlaylistData {
                id: 3,
                name: "Frequent".into(),
                match_all: true,
                rules: vec![SmartRule {
                    field: "plays".into(),
                    op: "gte".into(),
                    value: "2".into(),
                }],
                sort: "album".into(),
                limit: 0,
                extra: Default::default(),
            }]),
            ..Default::default()
        };
        let mut stats = PlayStats::default();
        stats.tracks.insert(
            "/m/1.flac".into(),
            PlayStat {
                count: 5,
                last: 100,
            },
        );

        assert_eq!(
            resolve_m3u_export_from(
                &albums,
                &collections,
                &stats,
                PlaylistExportTarget::Liked,
                1000
            )
            .unwrap()
            .1
            .len(),
            1
        );
        assert_eq!(
            resolve_m3u_export_from(
                &albums,
                &collections,
                &stats,
                PlaylistExportTarget::Custom(7),
                1000
            )
            .unwrap()
            .1
            .len(),
            2
        );
        let (name, smart_tracks) = resolve_m3u_export_from(
            &albums,
            &collections,
            &stats,
            PlaylistExportTarget::Smart(3),
            1000,
        )
        .unwrap();
        assert_eq!(name, "Frequent");
        assert_eq!(smart_tracks[0].id, "/m/1.flac");
        assert!(
            resolve_m3u_export_from(
                &albums,
                &collections,
                &stats,
                PlaylistExportTarget::Custom(999),
                1000
            )
            .is_err()
        );
    }
}
