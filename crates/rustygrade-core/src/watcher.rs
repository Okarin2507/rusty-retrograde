use std::collections::HashSet;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::mpsc;

use crate::error_log;
use crate::scanner::Scanner;

const DEBOUNCE_DELAY: Duration = Duration::from_secs(4);

/// Watches the library root folders recursively and emits batches of changed/new/removed audio
/// paths (or nearest surviving watched ancestors on deletion/rename) after a 4-second quiet window.
pub struct FolderWatcher {
    watcher: RecommendedWatcher,
    roots: Arc<Mutex<Vec<PathBuf>>>,
}

impl FolderWatcher {
    pub fn start(
        roots: &[PathBuf],
    ) -> Result<(Self, mpsc::UnboundedReceiver<Vec<PathBuf>>), notify::Error> {
        let top_roots = top_level_roots(roots);
        let shared_roots = Arc::new(Mutex::new(top_roots.clone()));
        let pending = Arc::new(Mutex::new((HashSet::<PathBuf>::new(), Instant::now())));
        let (tx, rx) = mpsc::unbounded_channel();

        let pending_cb = Arc::clone(&pending);
        let cb_roots = Arc::clone(&shared_roots);
        let mut watcher = notify::recommended_watcher(move |res: Result<notify::Event, _>| {
            let Ok(ev) = res else {
                return;
            };
            if !matches!(
                ev.kind,
                EventKind::Create(_)
                    | EventKind::Modify(_)
                    | EventKind::Remove(_)
                    | EventKind::Any
            ) {
                return;
            }
            let mut to_queue: Vec<PathBuf> = Vec::new();
            let mut needs_ancestor: Vec<PathBuf> = Vec::new();
            for path in ev.paths {
                if path.is_dir() || (Scanner::is_audio_file(&path) && path.exists()) {
                    to_queue.push(path);
                } else if !path.exists() && !is_hidden_name(&path) {
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
                    guard.0.insert(path);
                }
                guard.1 = Instant::now();
            }
        })?;

        for root in &top_roots {
            if let Err(err) = watcher.watch(root, RecursiveMode::Recursive) {
                error_log::write("watch", &format!("{}: {err}", root.display()));
            }
        }

        let pending_flush = Arc::clone(&pending);
        let flush_roots = Arc::clone(&shared_roots);
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(1));
            loop {
                ticker.tick().await;
                if tx.is_closed() {
                    break;
                }
                let ready = {
                    let all: Vec<PathBuf> = {
                        let Ok(mut guard) = pending_flush.lock() else {
                            continue;
                        };
                        if guard.0.is_empty() || guard.1.elapsed() < DEBOUNCE_DELAY {
                            continue;
                        }
                        guard.0.drain().collect()
                    };
                    let mut ready = Vec::new();
                    let mut vanished = Vec::new();
                    for p in all {
                        if p.is_dir() {
                            ready.push(p);
                        } else if p.exists() {
                            if File::open(&p).is_ok() {
                                ready.push(p);
                            } else if let Ok(mut guard) = pending_flush.lock() {
                                guard.0.insert(p);
                                guard.1 = Instant::now();
                            }
                        } else if !is_hidden_name(&p) {
                            vanished.push(p);
                        }
                    }
                    if !vanished.is_empty()
                        && let Ok(roots_guard) = flush_roots.lock()
                    {
                        for p in vanished {
                            if let Some(anc) = nearest_watched_ancestor(&p, &roots_guard) {
                                ready.push(anc);
                            }
                        }
                    }
                    collapse_watched_paths(ready)
                };
                if !ready.is_empty() && tx.send(ready).is_err() {
                    break;
                }
            }
        });

        Ok((
            Self {
                watcher,
                roots: shared_roots,
            },
            rx,
        ))
    }

    pub fn update_roots(&mut self, next_roots: &[PathBuf]) {
        let next = top_level_roots(next_roots);
        let prev = self
            .roots
            .lock()
            .map(|g| g.clone())
            .unwrap_or_default();
        for old in &prev {
            if !next.contains(old) {
                let _ = self.watcher.unwatch(old);
            }
        }
        for r in &next {
            if !prev.contains(r) {
                let _ = self.watcher.watch(r, RecursiveMode::Recursive);
            }
        }
        if let Ok(mut guard) = self.roots.lock() {
            *guard = next;
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
    let mut kept: Vec<PathBuf> = Vec::new();
    for p in paths {
        let covered = kept.iter().any(|d| d.is_dir() && p.starts_with(d));
        if !covered {
            kept.push(p);
        }
    }
    kept
}

fn top_level_roots(roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut cleaned: Vec<PathBuf> = roots
        .iter()
        .filter(|p| p.is_dir())
        .map(|p| {
            let s = p.to_string_lossy();
            let trimmed = s.trim_end_matches('/');
            if trimmed.is_empty() {
                PathBuf::from("/")
            } else {
                PathBuf::from(trimmed)
            }
        })
        .collect();
    cleaned.sort_by_key(|p| p.as_os_str().len());

    let mut kept: Vec<PathBuf> = Vec::new();
    for r in cleaned {
        let is_child = kept.iter().any(|k| Path::new(&r).starts_with(k));
        if !is_child {
            kept.push(r);
        }
    }
    kept
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn nearest_watched_ancestor_walks_up_to_existing_watched_dir() {
        let base = std::env::temp_dir().join(format!("rustygrade-watch-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let root = base.join("Music");
        let artist = root.join("Artist");
        fs::create_dir_all(&artist).unwrap();
        let roots = vec![root.clone()];

        let deleted_track = artist.join("RemovedAlbum").join("01.flac");
        assert_eq!(
            nearest_watched_ancestor(&deleted_track, &roots),
            Some(artist.clone())
        );

        let deleted_sub = root.join("GoneArtist").join("GoneAlbum").join("02.mp3");
        assert_eq!(
            nearest_watched_ancestor(&deleted_sub, &roots),
            Some(root.clone())
        );

        let outside = base.join("Other").join("03.flac");
        assert_eq!(nearest_watched_ancestor(&outside, &roots), None);

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn hidden_names_and_path_collapsing() {
        assert!(is_hidden_name(Path::new("/music/.goutputstream-XYZ")));
        assert!(is_hidden_name(Path::new("/music/.tmp.flac")));
        assert!(!is_hidden_name(Path::new("/music/track.flac")));

        let base = std::env::temp_dir().join(format!("rustygrade-collapse-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let a = base.join("AlbumA");
        let b = base.join("AlbumB");
        fs::create_dir_all(&a).unwrap();
        fs::create_dir_all(&b).unwrap();
        let collapsed = collapse_watched_paths(vec![
            a.join("01.flac"),
            a.clone(),
            b.join("02.flac"),
            base.clone(),
        ]);
        assert_eq!(collapsed, vec![base.clone()]);

        let _ = fs::remove_dir_all(&base);
    }
}
