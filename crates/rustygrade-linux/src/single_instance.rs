use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use tokio::sync::mpsc;
use tracing::warn;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SingleInstanceCommand {
    Raise,
    /// One or more paths / `file://` URIs forwarded by a secondary launch, in argument order.
    OpenUris(Vec<String>),
}

pub enum SingleInstanceOutcome {
    Primary(
        SingleInstanceGuard,
        mpsc::UnboundedReceiver<SingleInstanceCommand>,
    ),
    SecondaryNotified,
}

/// Cross-instance Unix domain socket keyed per user and per `RETRO_APPDIR`.
pub struct SingleInstance;

pub struct SingleInstanceGuard {
    socket_path: PathBuf,
    shutdown: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    /// Exclusive `flock` on `app_dir/.instance.lock`, released when the guard drops (after the
    /// socket is removed, as fields drop after `Drop::drop`).
    _lock: Option<File>,
}

enum InstanceLock {
    Held(File),
    Busy,
    /// The lock file cannot be used (read-only app dir, no flock support): socket only.
    Unavailable,
}

fn take_instance_lock(app_dir: &Path) -> InstanceLock {
    let _ = fs::create_dir_all(app_dir);
    let file = match OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(app_dir.join(".instance.lock"))
    {
        Ok(f) => f,
        Err(err) => {
            warn!(error = %err, "Cannot open single-instance lock file");
            return InstanceLock::Unavailable;
        }
    };
    match file.try_lock() {
        Ok(()) => InstanceLock::Held(file),
        Err(TryLockError::WouldBlock) => InstanceLock::Busy,
        Err(TryLockError::Error(err)) => {
            warn!(error = %err, "Cannot lock single-instance lock file");
            InstanceLock::Unavailable
        }
    }
}

/// Sends the request to the running primary; `true` once it has been written.
fn notify_primary(socket_path: &Path, args: &[String]) -> bool {
    let Ok(mut stream) = UnixStream::connect(socket_path) else {
        return false;
    };
    let _ = stream.set_write_timeout(Some(Duration::from_millis(500)));
    if stream.write_all(encode_request(args).as_bytes()).is_err() {
        return false;
    }
    let _ = stream.flush();
    let _ = stream.shutdown(std::net::Shutdown::Write);
    true
}

impl Drop for SingleInstanceGuard {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        // Unblock `listener.incoming()` so the listener thread and socket fd shut down immediately.
        let _ = UnixStream::connect(&self.socket_path);
        let _ = fs::remove_file(&self.socket_path);
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
    }
}

fn current_uid_suffix() -> String {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if let Ok(meta) = fs::metadata("/proc/self") {
            return format!("uid{}", meta.uid());
        }
    }
    std::env::var("USER")
        .ok()
        .filter(|u| {
            !u.is_empty()
                && u.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        })
        .unwrap_or_else(|| "uid".to_owned())
}

impl SingleInstance {
    pub fn socket_path_for(app_dir: &Path) -> PathBuf {
        let hash = crate::fnv1a_64(app_dir.to_string_lossy().as_bytes());
        if let Ok(runtime_dir) = std::env::var("XDG_RUNTIME_DIR")
            && !runtime_dir.trim().is_empty()
        {
            return PathBuf::from(runtime_dir).join(format!("rustygrade-{hash:016x}.sock"));
        }
        let user = current_uid_suffix();
        PathBuf::from(format!("/tmp/rustygrade-{user}-{hash:016x}.sock"))
    }

    /// Becomes the primary instance or forwards `args` (paths / URIs) to the running one.
    ///
    /// Wire protocol: one `OPEN:<uri>` line per argument (or a single `SHOW` line when
    /// there are none), terminated by EOF. Arguments containing newlines are dropped.
    pub fn acquire(app_dir: &Path, args: &[String]) -> Result<SingleInstanceOutcome, String> {
        let socket_path = Self::socket_path_for(app_dir);

        // The flock decides who is primary, so two launches at once cannot both bind the socket
        // (or remove each other's). Only the lock holder touches the socket file.
        let lock = match take_instance_lock(app_dir) {
            InstanceLock::Held(file) => Some(file),
            InstanceLock::Busy => {
                // The primary may still be starting up and not listening yet.
                for _ in 0..30 {
                    if notify_primary(&socket_path, args) {
                        return Ok(SingleInstanceOutcome::SecondaryNotified);
                    }
                    thread::sleep(Duration::from_millis(100));
                }
                // Never become a second primary next to the lock holder: both would write the library.
                warn!("Another Rustygrade instance holds the lock but does not answer; exiting");
                return Ok(SingleInstanceOutcome::SecondaryNotified);
            }
            InstanceLock::Unavailable => {
                if notify_primary(&socket_path, args) {
                    return Ok(SingleInstanceOutcome::SecondaryNotified);
                }
                None
            }
        };

        if socket_path.exists() {
            let _ = fs::remove_file(&socket_path);
        }
        if let Some(parent) = socket_path.parent() {
            let _ = fs::create_dir_all(parent);
        }

        let listener = UnixListener::bind(&socket_path)
            .map_err(|e| format!("Failed to bind single-instance socket: {e}"))?;
        let _ = fs::set_permissions(&socket_path, fs::Permissions::from_mode(0o600));
        let (tx, rx) = mpsc::unbounded_channel();
        let shutdown = Arc::new(AtomicBool::new(false));
        let thread_shutdown = Arc::clone(&shutdown);

        let thread = thread::Builder::new()
            .name("rustygrade-single-instance".to_owned())
            .spawn(move || {
                for incoming in listener.incoming() {
                    if thread_shutdown.load(Ordering::Acquire) {
                        break;
                    }
                    let Ok(stream) = incoming else {
                        break;
                    };
                    let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
                    let reader = BufReader::new(stream);
                    let mut uris = Vec::new();
                    let mut got_any = false;
                    for line in reader.lines() {
                        let Ok(line) = line else {
                            break;
                        };
                        got_any = true;
                        let line = line.strip_suffix('\r').unwrap_or(&line);
                        if let Some(uri) = line.strip_prefix("OPEN:")
                            && !uri.trim().is_empty()
                        {
                            uris.push(uri.to_owned());
                        }
                    }
                    if !got_any && uris.is_empty() {
                        continue;
                    }
                    let cmd = if uris.is_empty() {
                        SingleInstanceCommand::Raise
                    } else {
                        SingleInstanceCommand::OpenUris(uris)
                    };
                    if tx.send(cmd).is_err() {
                        break;
                    }
                }
            })
            .map_err(|e| {
                let _ = fs::remove_file(&socket_path);
                format!("Failed to spawn single-instance listener: {e}")
            })?;

        Ok(SingleInstanceOutcome::Primary(
            SingleInstanceGuard {
                socket_path,
                shutdown,
                thread: Some(thread),
                _lock: lock,
            },
            rx,
        ))
    }
}

fn encode_request(args: &[String]) -> String {
    let mut msg = String::new();
    for uri in args {
        if uri.trim().is_empty() || uri.contains('\n') || uri.contains('\r') {
            continue;
        }
        msg.push_str("OPEN:");
        msg.push_str(uri);
        msg.push('\n');
    }
    if msg.is_empty() {
        msg.push_str("SHOW\n");
    }
    msg
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn primary_receives_secondary_commands_and_cleans_up_socket() {
        let dir = std::env::temp_dir().join(format!("rustygrade-si-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let sock = SingleInstance::socket_path_for(&dir);
        let _ = fs::remove_file(&sock);

        let SingleInstanceOutcome::Primary(guard, mut rx) =
            SingleInstance::acquire(&dir, &[]).expect("primary acquire")
        else {
            panic!("expected Primary");
        };

        assert!(matches!(
            SingleInstance::acquire(
                &dir,
                &[
                    "/music/a b.flac".to_owned(),
                    "file:///music/c.flac".to_owned(),
                    "bad\npath".to_owned(),
                ]
            )
            .expect("secondary open"),
            SingleInstanceOutcome::SecondaryNotified
        ));
        assert!(matches!(
            SingleInstance::acquire(&dir, &[]).expect("secondary raise"),
            SingleInstanceOutcome::SecondaryNotified
        ));

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        let cmd1 = rt
            .block_on(async { tokio::time::timeout(Duration::from_secs(2), rx.recv()).await })
            .unwrap()
            .unwrap();
        assert_eq!(
            cmd1,
            SingleInstanceCommand::OpenUris(vec![
                "/music/a b.flac".to_owned(),
                "file:///music/c.flac".to_owned(),
            ])
        );
        let cmd2 = rt
            .block_on(async { tokio::time::timeout(Duration::from_secs(2), rx.recv()).await })
            .unwrap()
            .unwrap();
        assert_eq!(cmd2, SingleInstanceCommand::Raise);

        drop(guard);
        assert!(!sock.exists());

        match SingleInstance::acquire(&dir, &[]).expect("re-acquire") {
            SingleInstanceOutcome::Primary(g, _) => drop(g),
            SingleInstanceOutcome::SecondaryNotified => panic!("expected Primary after release"),
        }
        let _ = fs::remove_dir_all(&dir);
    }
}
