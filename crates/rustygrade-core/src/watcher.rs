use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::{Notify, mpsc};

use crate::error_log;
use crate::scanner::Scanner;

const DEBOUNCE_DELAY: Duration = Duration::from_secs(4);
const MAX_OPEN_RETRIES: u8 = 5;

#[derive(Default)]
struct PendingQueue {
    paths: HashSet<PathBuf>,
    retries: HashMap<PathBuf, u8>,
    deadline: Option<Instant>,
}

/// Watches the library root folders recursively and emits batches of changed/new/removed audio
/// paths (or nearest surviving watched ancestors on deletion/rename) after a 4-second quiet window.
pub struct FolderWatcher {
    watcher: RecommendedWatcher,
    roots: Arc<Mutex<Vec<PathBuf>>>,
    shutdown: Arc<AtomicBool>,
    wake: Arc<Notify>,
}

impl Drop for FolderWatcher {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        self.wake.notify_one();
    }
}

impl FolderWatcher {
    pub fn start(
        roots: &[PathBuf],
    ) -> Result<(Self, mpsc::UnboundedReceiver<Vec<PathBuf>>), notify::Error> {
        let top_roots = top_level_roots(roots);
        let pending = Arc::new(Mutex::new(PendingQueue::default()));
        let shutdown = Arc::new(AtomicBool::new(false));
        let wake = Arc::new(Notify::new());
        let (tx, rx) = mpsc::unbounded_channel();

        // Watch first, storing only roots that actually started watching.
        let mut active_roots = Vec::new();
        let pending_cb = Arc::clone(&pending);
        let shared_roots = Arc::new(Mutex::new(Vec::new()));
        let cb_roots = Arc::clone(&shared_roots);
        let cb_wake = Arc::clone(&wake);

        let mut watcher = notify::recommended_watcher(move |res: Result<notify::Event, _>| {
            let Ok(ev) = res else {
                return;
            };
            if !matches!(
                ev.kind,
                EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_) | EventKind::Any
            ) {
                return;
            }
            let mut to_queue: Vec<PathBuf> = Vec::new();
            let mut needs_ancestor: Vec<PathBuf> = Vec::new();
            for path in ev.paths {
                if is_hidden_name(&path) {
                    continue;
                }
                if path.is_dir() || (Scanner::is_audio_file(&path) && path.exists()) {
                    to_queue.push(path);
                } else if !path.exists() {
                    let should_rescan_parent = match ev.kind {
                        EventKind::Remove(notify::event::RemoveKind::File) => {
                            Scanner::is_audio_file(&path)
                        }
                        EventKind::Remove(_)
                        | EventKind::Modify(notify::event::ModifyKind::Name(_))
                        | EventKind::Any => true,
                        _ => false,
                    };
                    if should_rescan_parent {
                        needs_ancestor.push(path);
                    }
                }
            }
            if !needs_ancestor.is_empty()
                && let Ok(roots_guard) = cb_roots.lock()
            {
                for path in needs_ancestor {
                    if let Some(anc) = nearest_watched_ancestor(&path, &roots_guard) {
                        to_queue.push(anc);
                    }
                }
            }
            if !to_queue.is_empty()
                && let Ok(mut guard) = pending_cb.lock()
            {
                for path in to_queue {
                    guard.retries.remove(&path);
                    guard.paths.insert(path);
                }
                guard.deadline = Some(Instant::now() + DEBOUNCE_DELAY);
                drop(guard);
                cb_wake.notify_one();
            }
        })?;

        for root in top_roots {
            match watcher.watch(&root, RecursiveMode::Recursive) {
                Ok(()) => active_roots.push(root),
                Err(err) => error_log::write("watch", &format!("{}: {err}", root.display())),
            }
        }
        if let Ok(mut guard) = shared_roots.lock() {
            *guard = active_roots;
        }

        let pending_flush = Arc::clone(&pending);
        let flush_roots = Arc::clone(&shared_roots);
        let flush_shutdown = Arc::clone(&shutdown);
        let flush_wake = Arc::clone(&wake);
        tokio::spawn(async move {
            loop {
                if flush_shutdown.load(Ordering::Acquire) || tx.is_closed() {
                    break;
                }
                let next_action = {
                    let Ok(mut guard) = pending_flush.lock() else {
                        break;
                    };
                    if guard.paths.is_empty() {
                        guard.deadline = None;
                        Err(None)
                    } else {
                        let deadline = guard.deadline.unwrap_or_else(Instant::now);
                        if Instant::now() < deadline {
                            Err(Some(deadline))
                        } else {
                            let PendingQueue {
                                paths,
                                retries,
                                deadline: guard_deadline,
                            } = &mut *guard;
                            let all: Vec<(PathBuf, u8)> = paths
                                .drain()
                                .map(|p| {
                                    let tries = retries.get(&p).copied().unwrap_or(0);
                                    (p, tries)
                                })
                                .collect();
                            *guard_deadline = None;
                            Ok(all)
                        }
                    }
                };

                let all = match next_action {
                    Err(None) => {
                        tokio::select! {
                            _ = flush_wake.notified() => continue,
                            _ = tx.closed() => break,
                        }
                    }
                    Err(Some(deadline)) => {
                        tokio::select! {
                            _ = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => continue,
                            _ = flush_wake.notified() => continue,
                            _ = tx.closed() => break,
                        }
                    }
                    Ok(batch) => batch,
                };

                let roots_snapshot = flush_roots.lock().map(|g| g.clone()).unwrap_or_default();
                let Ok((ready, retry_later)) = tokio::task::spawn_blocking(move || {
                    let mut ready = Vec::new();
                    let mut retry_later = Vec::new();
                    let mut vanished = Vec::new();
                    for (p, tries) in all {
                        if p.is_dir() {
                            if !is_hidden_name(&p) {
                                ready.push(p);
                            }
                        } else if p.exists() {
                            match File::open(&p) {
                                Ok(_) => ready.push(p),
                                Err(e) if e.kind() == ErrorKind::PermissionDenied => {}
                                Err(_) if tries < MAX_OPEN_RETRIES => {
                                    retry_later.push((p, tries + 1));
                                }
                                Err(_) => {}
                            }
                        } else if !is_hidden_name(&p) {
                            vanished.push(p);
                        }
                    }
                    for p in vanished {
                        if let Some(anc) = nearest_watched_ancestor(&p, &roots_snapshot) {
                            ready.push(anc);
                        }
                    }
                    (collapse_watched_paths(ready), retry_later)
                })
                .await
                else {
                    continue;
                };

                if !retry_later.is_empty()
                    && let Ok(mut guard) = pending_flush.lock()
                {
                    for (p, next_tries) in retry_later {
                        guard.retries.insert(p.clone(), next_tries);
                        guard.paths.insert(p);
                    }
                    guard
                        .deadline
                        .get_or_insert_with(|| Instant::now() + DEBOUNCE_DELAY);
                }

                if !ready.is_empty() && tx.send(ready).is_err() {
                    break;
                }
            }
        });

        Ok((
            Self {
                watcher,
                roots: shared_roots,
                shutdown,
                wake,
            },
            rx,
        ))
    }

    pub fn update_roots(&mut self, next_roots: &[PathBuf]) {
        let next = top_level_roots(next_roots);
        let prev = self.roots.lock().map(|g| g.clone()).unwrap_or_default();
        for old in &prev {
            if !next.contains(old) {
                let _ = self.watcher.unwatch(old);
            }
        }
        let mut active = Vec::with_capacity(next.len());
        for r in next {
            if prev.contains(&r) {
                active.push(r);
            } else if let Err(err) = self.watcher.watch(&r, RecursiveMode::Recursive) {
                error_log::write("watch", &format!("{}: {err}", r.display()));
            } else {
                active.push(r);
            }
        }
        if let Ok(mut guard) = self.roots.lock() {
            *guard = active;
        }
    }
}

pub fn nearest_watched_ancestor(path: &Path, roots: &[PathBuf]) -> Option<PathBuf> {
    if path.is_dir() && roots.iter().any(|r| path.starts_with(r)) {
        return Some(path.to_path_buf());
    }
    let mut cur = path.parent();
    while let Some(dir) = cur {
        if dir.is_dir() && roots.iter().any(|r| dir.starts_with(r)) {
            return Some(dir.to_path_buf());
        }
        cur = dir.parent();
    }
    None
}

pub(crate) fn is_hidden_name(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with('.'))
}

pub(crate) fn collapse_watched_paths(mut paths: Vec<PathBuf>) -> Vec<PathBuf> {
    paths.sort_by(|a, b| {
        a.as_os_str()
            .len()
            .cmp(&b.as_os_str().len())
            .then_with(|| a.cmp(b))
    });
    paths.dedup();
    let mut kept_dirs: Vec<PathBuf> = Vec::new();
    let mut kept: Vec<PathBuf> = Vec::new();
    for p in paths {
        if kept_dirs.iter().any(|d| p.starts_with(d)) {
            continue;
        }
        if p.is_dir() {
            kept_dirs.push(p.clone());
        }
        kept.push(p);
    }
    kept
}

fn top_level_roots(roots: &[PathBuf]) -> Vec<PathBuf> {
    // `components().collect()` normalizes trailing `/` without lossy UTF-8 conversion.
    let mut cleaned: Vec<PathBuf> = roots
        .iter()
        .filter(|p| p.is_dir())
        .map(|p| p.components().collect::<PathBuf>())
        .collect();
    cleaned.sort_by_key(|p| p.as_os_str().len());

    let mut kept: Vec<PathBuf> = Vec::new();
    for r in cleaned {
        if !kept.iter().any(|k| r.starts_with(k)) {
            kept.push(r);
        }
    }
    kept
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::TempDir;
    use std::fs;

    #[test]
    fn nearest_watched_ancestor_walks_up_to_existing_watched_dir() {
        let base = TempDir::new("watch");
        let root = base.join("Music");
        let artist = root.join("Artist");
        fs::create_dir_all(&artist).unwrap();
        let roots = vec![root.clone()];

        assert_eq!(
            nearest_watched_ancestor(&artist.join("RemovedAlbum/01.flac"), &roots),
            Some(artist)
        );
        assert_eq!(
            nearest_watched_ancestor(&root.join("GoneArtist/GoneAlbum/02.mp3"), &roots),
            Some(root)
        );
        assert_eq!(
            nearest_watched_ancestor(&base.join("Other/03.flac"), &roots),
            None
        );
    }

    #[test]
    fn hidden_names_and_path_collapsing() {
        assert!(is_hidden_name(Path::new("/music/.goutputstream-XYZ")));
        assert!(is_hidden_name(Path::new("/music/.tmp.flac")));
        assert!(!is_hidden_name(Path::new("/music/track.flac")));

        let base = TempDir::new("collapse");
        let a = base.join("AlbumA");
        let b = base.join("AlbumB");
        fs::create_dir_all(&a).unwrap();
        fs::create_dir_all(&b).unwrap();
        let collapsed = collapse_watched_paths(vec![
            a.join("01.flac"),
            a,
            b.join("02.flac"),
            base.to_path_buf(),
        ]);
        assert_eq!(collapsed, vec![base.to_path_buf()]);
    }
}
