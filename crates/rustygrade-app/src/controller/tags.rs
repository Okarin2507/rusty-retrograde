use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Instant;

use rustygrade_core::scanner::{PrunePolicy, ScanOptions};
use rustygrade_core::{CoverPaths, LibraryStore, Scanner, TagEdit, TagWriter};

use super::Coordinator;
use super::types::{ControllerEvent, NoticeKind};

pub(super) struct TagBatchResult {
    pub(super) first_path: Option<PathBuf>,
    pub(super) written: usize,
    pub(super) ok_paths: Vec<PathBuf>,
    pub(super) failed: Vec<String>,
    pub(super) changes_cover: bool,
    pub(super) old_covers: Vec<PathBuf>,
}

fn push_unique_path(out: &mut Vec<PathBuf>, seen: &mut HashSet<PathBuf>, path: PathBuf) {
    if seen.insert(path.clone()) {
        out.push(path);
    }
}

impl Coordinator {
    pub(super) async fn save_tags(
        &mut self,
        paths: Vec<PathBuf>,
        album_key: Option<String>,
        edit: TagEdit,
    ) {
        let first_path = paths.first().cloned();
        let reject = |this: &Self, msg: String| {
            this.notice(NoticeKind::Error, msg.clone());
            let _ = this.ui_tx.send(ControllerEvent::TagsSaved {
                first_path: first_path.clone(),
                written: 0,
                failed: vec![msg],
            });
        };

        if self.scan_cancel.is_some() {
            reject(
                self,
                "WAIT FOR THE RUNNING SCAN BEFORE SAVING TAGS.".to_owned(),
            );
            return;
        }
        if self.tag_save_in_flight {
            reject(self, "TAG WRITE ALREADY IN PROGRESS.".to_owned());
            return;
        }

        let path_set: HashSet<&Path> = paths.iter().map(PathBuf::as_path).collect();
        let playing_conflict = self.is_playing
            && self
                .queue
                .current()
                .is_some_and(|t| path_set.contains(Path::new(&t.file_path)));
        if playing_conflict {
            reject(
                self,
                "Pause or switch tracks before editing the currently playing file.".to_owned(),
            );
            return;
        }
        let spare_conflict = self
            .crossfade
            .active_target_path()
            .is_some_and(|ap| path_set.contains(Path::new(ap)));
        if spare_conflict {
            reject(
                self,
                "Unload or switch tracks before editing the currently active file.".to_owned(),
            );
            return;
        }

        let changes_cover = edit.changes_cover();
        let mut affected_targets: Vec<(String, String)> = Vec::new();
        let mut target_seen: HashSet<(String, String)> = HashSet::new();
        let mut old_covers: Vec<PathBuf> = Vec::new();
        let mut cover_seen: HashSet<PathBuf> = HashSet::new();

        if changes_cover {
            for a in self.albums.iter() {
                if a.tracks
                    .iter()
                    .any(|t| path_set.contains(Path::new(&t.file_path)))
                {
                    if target_seen.insert((a.name.clone(), a.artist.clone())) {
                        affected_targets.push((a.name.clone(), a.artist.clone()));
                    }
                    if let Some(p) = &a.cover_thumb_path {
                        push_unique_path(&mut old_covers, &mut cover_seen, p.clone());
                    }
                    if let Some(p) = &a.cover_full_path {
                        push_unique_path(&mut old_covers, &mut cover_seen, p.clone());
                    }
                }
            }
            if let Some(k) = &album_key
                && !k.is_empty()
            {
                push_unique_path(&mut old_covers, &mut cover_seen, CoverPaths::thumb_path(k));
                push_unique_path(&mut old_covers, &mut cover_seen, CoverPaths::full_path(k));
            }
            if let Some(new_name) = &edit.album {
                let trimmed = new_name.trim();
                if !trimmed.is_empty() {
                    push_unique_path(
                        &mut old_covers,
                        &mut cover_seen,
                        CoverPaths::thumb_path(trimmed),
                    );
                    push_unique_path(
                        &mut old_covers,
                        &mut cover_seen,
                        CoverPaths::full_path(trimmed),
                    );
                }
            }
        }

        if changes_cover && !affected_targets.is_empty() {
            self.flush_persist().await;
        }
        let now = Instant::now();
        for p in &paths {
            self.suppress_watch_paths.insert(p.clone(), now);
        }

        self.tag_save_in_flight = true;
        let extra_pinned = self
            .live_playlist_covers
            .lock()
            .map(|g| g.clone())
            .unwrap_or_default();
        let tx = self.tag_done_tx.clone();
        tokio::task::spawn_blocking(move || {
            let (written, errs) = TagWriter::write_batch(&paths, &edit);
            let err_paths: HashSet<PathBuf> = errs.iter().map(|(p, _)| p.clone()).collect();
            let ok_paths: Vec<PathBuf> = paths
                .into_iter()
                .filter(|p| !err_paths.contains(p))
                .collect();
            let failed: Vec<String> = errs
                .into_iter()
                .map(|(p, e)| {
                    let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("file");
                    format!("{name}: {e}")
                })
                .collect();
            if !ok_paths.is_empty() {
                if changes_cover && !affected_targets.is_empty() {
                    let _ = LibraryStore::clear_covers(&affected_targets, &extra_pinned);
                }
                let _ = Scanner::scan(
                    &ok_paths,
                    ScanOptions {
                        prune: PrunePolicy::Never,
                        max_threads: None,
                        force: true,
                        resync_covers: false,
                    },
                    None,
                    None,
                );
            }
            let _ = tx.send(TagBatchResult {
                first_path,
                written,
                ok_paths,
                failed,
                changes_cover,
                old_covers,
            });
        });
    }

    pub(super) async fn handle_tag_done(&mut self, res: TagBatchResult) {
        self.tag_save_in_flight = false;
        let now = Instant::now();
        for p in &res.ok_paths {
            self.suppress_watch_paths.insert(p.clone(), now);
        }

        let mut invalidated = res.old_covers;
        if res.written > 0 {
            self.reload_library(true).await;
            if res.changes_cover {
                let ok_set: HashSet<&Path> = res.ok_paths.iter().map(PathBuf::as_path).collect();
                let mut seen: HashSet<PathBuf> = invalidated.iter().cloned().collect();
                for a in self.albums.iter() {
                    if a.tracks
                        .iter()
                        .any(|t| ok_set.contains(Path::new(&t.file_path)))
                    {
                        for p in [&a.cover_thumb_path, &a.cover_full_path]
                            .into_iter()
                            .flatten()
                        {
                            push_unique_path(&mut invalidated, &mut seen, p.clone());
                        }
                    }
                }
            }
            if res.failed.is_empty() {
                self.notice(
                    NoticeKind::Success,
                    format!("TAGS_SAVED // {} FILE(S)", res.written),
                );
            } else {
                self.notice(
                    NoticeKind::Error,
                    format!(
                        "TAGS_SAVED // {} OK, {} FAILED",
                        res.written,
                        res.failed.len()
                    ),
                );
            }
        } else if let Some(first) = res.failed.first() {
            self.notice(NoticeKind::Error, format!("TAG_WRITE_FAILED // {first}"));
        }
        if !invalidated.is_empty() {
            let _ = self
                .ui_tx
                .send(ControllerEvent::CoversInvalidated(invalidated));
        }
        let _ = self.ui_tx.send(ControllerEvent::TagsSaved {
            first_path: res.first_path,
            written: res.written,
            failed: res.failed,
        });
    }
}
