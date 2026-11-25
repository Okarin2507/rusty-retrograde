use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::time::{Instant, timeout};
use tracing::{debug, warn};

use crate::artwork_lookup::now_unix_u64;

use super::DiscordPresence;
use super::payload::build_set_activity_payload;

pub(super) const OP_HANDSHAKE: u32 = 0;
pub(super) const OP_FRAME: u32 = 1;
pub(super) const OP_CLOSE: u32 = 2;
pub(super) const OP_PING: u32 = 3;
pub(super) const OP_PONG: u32 = 4;

pub(super) const MAX_FRAME_LEN: usize = 64 * 1024;
const WRITE_TIMEOUT: Duration = Duration::from_secs(2);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Debug)]
enum FrameReadError {
    Closed,
    Io,
}

pub(super) enum HandshakeOutcome {
    Connected(UnixStream, Vec<u8>),
    Rejected,
    Unavailable,
}

/// Lists candidate Discord IPC Unix socket paths across standard, Flatpak, Vesktop, and Snap dirs.
pub fn candidate_socket_paths() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    let mut seen = HashSet::new();
    for var in ["XDG_RUNTIME_DIR", "TMPDIR", "TMP", "TEMP"] {
        if let Some(val) = std::env::var_os(var)
            && !val.is_empty()
        {
            let p = PathBuf::from(val);
            if seen.insert(p.clone()) {
                dirs.push(p);
            }
        }
    }
    let tmp = PathBuf::from("/tmp");
    if seen.insert(tmp.clone()) {
        dirs.push(tmp);
    }

    let prefixes = [
        "",
        "app/com.discordapp.Discord",
        "app/com.discordapp.DiscordCanary",
        "app/com.discordapp.DiscordPTB",
        "app/dev.vencord.Vesktop",
        "snap.discord",
    ];
    let mut out = Vec::new();
    for dir in &dirs {
        for prefix in &prefixes {
            let base = if prefix.is_empty() {
                dir.clone()
            } else {
                dir.join(prefix)
            };
            for idx in 0..10 {
                out.push(base.join(format!("discord-ipc-{idx}")));
            }
        }
    }
    out
}

fn is_client_id_rejection(code: Option<i64>, msg: &str) -> bool {
    if matches!(code, Some(4000..=4005)) {
        return true;
    }
    let lower = msg.to_ascii_lowercase();
    lower.contains("client") || lower.contains("invalid") || lower.contains("oauth")
}

pub(super) async fn connect_and_handshake(path: &Path, client_id: &str) -> HandshakeOutcome {
    let mut conn = match UnixStream::connect(path).await {
        Ok(s) => s,
        Err(err) => {
            debug!(socket = %path.display(), error = %err, "Failed connecting Discord IPC socket");
            return HandshakeOutcome::Unavailable;
        }
    };
    let handshake = json!({
        "v": 1,
        "client_id": client_id,
    });
    if !write_json_frame(&mut conn, OP_HANDSHAKE, &handshake).await {
        return HandshakeOutcome::Unavailable;
    }
    let mut buf = Vec::with_capacity(1024);
    let reply = match timeout(HANDSHAKE_TIMEOUT, async {
        loop {
            match try_pop_frame(&mut buf) {
                Ok(Some(frame)) => return Ok(frame),
                Ok(None) => {}
                Err(()) => return Err(FrameReadError::Io),
            }
            match conn.read_buf(&mut buf).await {
                Ok(0) => return Err(FrameReadError::Closed),
                Ok(_) => {}
                Err(_) => return Err(FrameReadError::Io),
            }
        }
    })
    .await
    {
        Ok(Ok(frame)) => frame,
        Ok(Err(err)) => {
            debug!(?err, "Discord IPC closed during handshake");
            return HandshakeOutcome::Unavailable;
        }
        Err(_) => {
            debug!("Discord IPC handshake timed out");
            return HandshakeOutcome::Unavailable;
        }
    };

    let (op, payload) = reply;
    if op == OP_CLOSE {
        let val = serde_json::from_slice::<Value>(&payload).ok();
        let code = val
            .as_ref()
            .and_then(|v| v.get("code"))
            .and_then(Value::as_i64);
        let msg = val
            .as_ref()
            .and_then(|v| v.get("message"))
            .and_then(Value::as_str)
            .unwrap_or("closed without reason");
        warn!(code, message = msg, "Discord IPC closed handshake");
        return if is_client_id_rejection(code, msg) {
            HandshakeOutcome::Rejected
        } else {
            HandshakeOutcome::Unavailable
        };
    }
    if op != OP_FRAME {
        warn!(op, "Unexpected Discord IPC handshake opcode");
        return HandshakeOutcome::Unavailable;
    }
    let Ok(val) = serde_json::from_slice::<Value>(&payload) else {
        return HandshakeOutcome::Unavailable;
    };
    match val.get("evt").and_then(Value::as_str) {
        Some("READY") => HandshakeOutcome::Connected(conn, buf),
        Some("ERROR") => {
            let code = val
                .get("data")
                .and_then(|d| d.get("code"))
                .and_then(Value::as_i64);
            let msg = val
                .get("data")
                .and_then(|d| d.get("message"))
                .and_then(Value::as_str)
                .unwrap_or("unknown Discord error");
            warn!(code, error = msg, "Discord IPC rejected handshake");
            HandshakeOutcome::Rejected
        }
        other => {
            warn!(evt = ?other, "Unexpected Discord IPC handshake event");
            HandshakeOutcome::Unavailable
        }
    }
}

pub(super) async fn send_activity(
    conn: &mut UnixStream,
    pid: u32,
    nonce: &mut u64,
    presence: &DiscordPresence,
    last_sent: &mut Option<(DiscordPresence, Instant)>,
) -> bool {
    let now_unix_s = now_unix_u64() as i64;
    let payload = build_set_activity_payload(pid, *nonce, presence, now_unix_s);
    *nonce = nonce.wrapping_add(1);
    if write_json_frame(conn, OP_FRAME, &payload).await {
        *last_sent = Some((presence.clone(), Instant::now()));
        true
    } else {
        false
    }
}

pub(super) async fn clear_and_close(conn: &mut UnixStream, pid: u32, nonce: &mut u64) {
    let payload = json!({
        "cmd": "SET_ACTIVITY",
        "args": { "pid": pid, "activity": Value::Null },
        "nonce": nonce.to_string(),
    });
    *nonce = nonce.wrapping_add(1);
    let _ = write_json_frame(conn, OP_FRAME, &payload).await;
    let _ = write_raw_frame(conn, OP_CLOSE, b"{}").await;
}

pub(super) async fn drain_incoming_frames(conn: &mut UnixStream, buf: &mut Vec<u8>) -> bool {
    loop {
        match try_pop_frame(buf) {
            Ok(Some((OP_PING, payload))) => {
                if !write_raw_frame(conn, OP_PONG, &payload).await {
                    return false;
                }
            }
            Ok(Some((OP_CLOSE, payload))) => {
                if let Ok(val) = serde_json::from_slice::<Value>(&payload) {
                    let code = val.get("code").and_then(Value::as_i64);
                    let msg = val.get("message").and_then(Value::as_str);
                    warn!(code, message = msg, "Discord IPC closed connection");
                }
                return false;
            }
            Ok(Some((OP_FRAME, payload))) => {
                if let Ok(val) = serde_json::from_slice::<Value>(&payload)
                    && val.get("evt").and_then(Value::as_str) == Some("ERROR")
                {
                    let msg = val
                        .get("data")
                        .and_then(|d| d.get("message"))
                        .and_then(Value::as_str)
                        .unwrap_or("unknown");
                    warn!(error = msg, "Discord IPC error frame");
                }
            }
            Ok(Some((OP_HANDSHAKE | OP_PONG | _, _))) => {}
            Ok(None) => return true,
            Err(()) => return false,
        }
    }
}

pub(super) fn try_pop_frame(buf: &mut Vec<u8>) -> Result<Option<(u32, Vec<u8>)>, ()> {
    if buf.len() < 8 {
        return Ok(None);
    }
    let op = u32::from_le_bytes(buf[0..4].try_into().expect("4 bytes"));
    let len = u32::from_le_bytes(buf[4..8].try_into().expect("4 bytes")) as usize;
    if len > MAX_FRAME_LEN {
        return Err(());
    }
    let total = 8usize.checked_add(len).ok_or(())?;
    if buf.len() < total {
        return Ok(None);
    }
    let payload = buf[8..total].to_vec();
    buf.drain(..total);
    Ok(Some((op, payload)))
}

pub(super) async fn write_json_frame(conn: &mut UnixStream, op: u32, payload: &Value) -> bool {
    let Ok(bytes) = serde_json::to_vec(payload) else {
        return false;
    };
    write_raw_frame(conn, op, &bytes).await
}

pub(super) async fn write_raw_frame(conn: &mut UnixStream, op: u32, payload: &[u8]) -> bool {
    if payload.len() > MAX_FRAME_LEN {
        return false;
    }
    let mut frame = Vec::with_capacity(8 + payload.len());
    frame.extend_from_slice(&op.to_le_bytes());
    frame.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    frame.extend_from_slice(payload);
    timeout(WRITE_TIMEOUT, async {
        conn.write_all(&frame).await?;
        conn.flush().await
    })
    .await
    .is_ok_and(|r| r.is_ok())
}
