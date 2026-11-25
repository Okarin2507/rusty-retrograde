use std::fs;
use std::path::Path;
use std::sync::Mutex;

pub(crate) enum LoadStatus<T> {
    Missing,
    Ok(T),
    Corrupt,
}

/// Loads `path`, backing up any corrupt primary to `corrupt` and restoring from `backup`
/// (whether primary was corrupt or missing).
pub(crate) fn load_with_backup<T>(
    path: &Path,
    backup: &Path,
    corrupt: &Path,
    parse: impl Fn(&[u8]) -> Option<T>,
) -> LoadStatus<T> {
    let primary_exists = path.exists();
    if primary_exists {
        if let Ok(bytes) = fs::read(path)
            && let Some(val) = parse(&bytes)
        {
            return LoadStatus::Ok(val);
        }
        let _ = fs::copy(path, corrupt);
    }
    if backup.exists()
        && let Ok(bytes) = fs::read(backup)
        && let Some(val) = parse(&bytes)
    {
        let _ = fs::copy(backup, path);
        return LoadStatus::Ok(val);
    }
    if primary_exists {
        LoadStatus::Corrupt
    } else {
        LoadStatus::Missing
    }
}

/// Atomically writes `bytes` to `path` via a `.json.tmp` sibling, copying any existing `path` to
/// `backup` first. Caller must already hold any cross-thread store lock.
pub(crate) fn save_atomic_bytes(
    path: &Path,
    backup: Option<&Path>,
    bytes: &[u8],
) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    if let Err(e) = fs::write(&tmp, bytes) {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    if let Some(bak) = backup
        && path.exists()
    {
        let _ = fs::copy(path, bak);
    }
    fs::rename(&tmp, path)
}

pub(crate) fn save_atomic_locked(
    lock: &Mutex<()>,
    path: &Path,
    backup: Option<&Path>,
    bytes: &[u8],
) -> std::io::Result<()> {
    let _guard = lock.lock().unwrap_or_else(|e| e.into_inner());
    save_atomic_bytes(path, backup, bytes)
}
