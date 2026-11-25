use std::sync::atomic::Ordering;
use std::sync::{Arc, Weak};

use tokio::sync::mpsc;

use crate::alsa_direct::AudioFormat;
use crate::libmpv::{LibMpvClient, MpvEventMsg, MpvValue};

use super::{AudioPath, EOF_SETTLE, EngineEvent, SharedState, TIME_POS_REQUEST_ID};

pub(super) fn handle_raw_message(
    msg: MpvEventMsg,
    shared: &Arc<SharedState>,
    out_tx: &mpsc::UnboundedSender<EngineEvent>,
    lib: &Weak<LibMpvClient>,
) {
    match msg {
        MpvEventMsg::CommandReply {
            request_id,
            error,
            data,
        } => {
            if request_id == shared.load_request_id.load(Ordering::Acquire) {
                if let Some(reason) = error {
                    shared.current_entry_id.store(-1, Ordering::Release);
                    let path = shared
                        .current_path
                        .lock()
                        .ok()
                        .and_then(|g| g.clone())
                        .unwrap_or_default();
                    let _ = out_tx.send(EngineEvent::PlaybackFailed { path, reason });
                } else if let Some(entry) = data
                    .as_map()
                    .and_then(|m| m.get("playlist_entry_id"))
                    .and_then(MpvValue::as_i64)
                {
                    shared.current_entry_id.store(entry, Ordering::Release);
                }
                return;
            }
            if let Ok(mut ar) = shared.append_requests.lock()
                && let Some(appended) = ar.remove(&request_id)
                && error.is_none()
                && let Some(entry) = data
                    .as_map()
                    .and_then(|m| m.get("playlist_entry_id"))
                    .and_then(MpvValue::as_i64)
                && let Ok(mut q) = shared.queued.lock()
            {
                q.insert(entry, appended);
            }
        }
        MpvEventMsg::GetPropertyReply {
            request_id,
            error,
            data,
        } => {
            if request_id == TIME_POS_REQUEST_ID {
                if error.is_none()
                    && let Some(pos) = data.as_f64()
                    && pos.is_finite()
                {
                    let _ = out_tx.send(EngineEvent::TimePos(pos));
                }
                return;
            }
            if let Ok(mut p) = shared.pending.lock()
                && let Some(tx) = p.remove(&request_id)
            {
                let reply = if error.is_some() {
                    MpvValue::None
                } else {
                    data
                };
                let _ = tx.send(reply);
            }
        }
        MpvEventMsg::FileLoaded => {
            shared.file_loaded.store(true, Ordering::Release);
            let paused = shared.paused.load(Ordering::Acquire);
            shared.playing.store(!paused, Ordering::Release);
            // mpv reports `pause` only when it changes; a file starting under an unchanged
            // pause=no would otherwise never be announced as playing.
            let _ = out_tx.send(EngineEvent::Pause(paused));
            let seek = shared.pending_seek.lock().ok().and_then(|mut g| g.take());
            if let Some(sec) = seek
                && let Some(client) = lib.upgrade()
            {
                let s = sec.to_string();
                client.command(0, &["seek", &s, "absolute"]);
                client.get_property(TIME_POS_REQUEST_ID, "time-pos");
            }
        }
        MpvEventMsg::StartFile { playlist_entry_id } => {
            let next_path = shared
                .queued
                .lock()
                .ok()
                .and_then(|mut q| q.remove(&playlist_entry_id));
            if let Some(path) = next_path {
                shared
                    .current_entry_id
                    .store(playlist_entry_id, Ordering::Release);
                if let Ok(mut cp) = shared.current_path.lock() {
                    *cp = Some(path.clone());
                }
                let _ = out_tx.send(EngineEvent::AdvancedToNext(path));
            }
        }
        MpvEventMsg::EndFile {
            reason,
            playlist_entry_id,
            file_error,
        } => {
            shared.file_loaded.store(false, Ordering::Release);
            shared.playing.store(false, Ordering::Release);
            if reason == "error" {
                // The `eof-reached` just before this was the failure, not the end of the track.
                shared.eof_gen.fetch_add(1, Ordering::AcqRel);
            }
            if reason == "error"
                && playlist_entry_id == shared.current_entry_id.load(Ordering::Acquire)
            {
                shared.current_entry_id.store(-1, Ordering::Release);
                let path = shared
                    .current_path
                    .lock()
                    .ok()
                    .and_then(|g| g.clone())
                    .unwrap_or_default();
                let _ = out_tx.send(EngineEvent::PlaybackFailed {
                    path,
                    reason: file_error.unwrap_or_else(|| "error".to_owned()),
                });
            }
        }
        MpvEventMsg::PropertyChange { name, data } => match name.as_str() {
            "audio-params" => {
                let fmt = parse_audio_format(&data);
                let out = shared.path_out.lock().ok().and_then(|g| g.clone());
                if let Ok(mut g) = shared.path_in.lock() {
                    *g = fmt.clone();
                }
                let _ = out_tx.send(EngineEvent::AudioPathChanged(AudioPath {
                    input: fmt,
                    output: out,
                }));
            }
            "audio-out-params" => {
                let fmt = parse_audio_format(&data);
                let input = shared.path_in.lock().ok().and_then(|g| g.clone());
                if let Ok(mut g) = shared.path_out.lock() {
                    *g = fmt.clone();
                }
                let _ = out_tx.send(EngineEvent::AudioPathChanged(AudioPath {
                    input,
                    output: fmt,
                }));
            }
            "duration" => {
                if let Some(d) = data.as_f64()
                    && d.is_finite()
                {
                    let _ = out_tx.send(EngineEvent::Duration(d));
                }
            }
            "pause" => {
                if let Some(paused) = data.as_bool() {
                    shared.paused.store(paused, Ordering::Release);
                    let loaded = shared.file_loaded.load(Ordering::Acquire);
                    shared.playing.store(!paused && loaded, Ordering::Release);
                    let _ = out_tx.send(EngineEvent::Pause(paused));
                }
            }
            "eof-reached" if data.as_bool() == Some(true) => {
                let generation = shared.eof_gen.fetch_add(1, Ordering::AcqRel) + 1;
                let shared = Arc::clone(shared);
                let tx = out_tx.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(EOF_SETTLE).await;
                    if shared.eof_gen.load(Ordering::Acquire) == generation {
                        let _ = tx.send(EngineEvent::EndOfFile);
                    }
                });
            }
            _ => {}
        },
        MpvEventMsg::Shutdown { unexpected } => {
            if unexpected {
                shared.reset_playback();
                let _ = out_tx.send(EngineEvent::Exited);
            }
        }
    }
}

fn parse_audio_format(val: &MpvValue) -> Option<AudioFormat> {
    let map = val.as_map()?;
    let format = map.get("format").and_then(MpvValue::as_str)?.to_owned();
    let rate = map.get("samplerate").and_then(MpvValue::as_i64)? as i32;
    let channels = map
        .get("channel-count")
        .and_then(MpvValue::as_i64)
        .unwrap_or(0) as i32;
    (rate > 0).then_some(AudioFormat {
        format,
        rate,
        channels,
    })
}
