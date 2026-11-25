use std::path::PathBuf;
use std::time::{Duration, Instant};

use rustygrade_audio::{AO_INIT_FAILED, AlsaDirect, EngineEvent};
use rustygrade_core::{LyricsLoader, Track, extract_cover_palette};
use tokio::sync::mpsc;
use tracing::{info, warn};

use super::Coordinator;
use super::audio::OUTPUT_RETRIES;
use super::library::failed_track_rescan_target;
use super::persist::PersistMsg;
use super::types::{ControllerEvent, NoticeKind, PlaybackTickMode};
use crate::crossfade::FadeStep;
use crate::playback::{MpvAdvance, RepeatMode};

/// Tracks that fail to load sequentially are skipped up to this many times before stopping.
const MAX_FAILED_SKIPS: u32 = 5;

/// Position ticks closer together than this are merged unless the shown second changes.
const PLAYBACK_TICK_MIN: Duration = Duration::from_millis(250);

fn playback_tick_due(
    mode: PlaybackTickMode,
    last: Option<(Instant, i64, bool, f64)>,
    now: Instant,
    position_s: f64,
    is_playing: bool,
    duration_s: f64,
) -> bool {
    let Some((at, second, playing, duration)) = last else {
        return true;
    };
    if playing != is_playing || (duration - duration_s).abs() > 0.001 {
        return true;
    }
    match mode {
        PlaybackTickMode::Active => {
            second != position_s.floor() as i64 || now.duration_since(at) >= PLAYBACK_TICK_MIN
        }
        PlaybackTickMode::Unfocused => second != position_s.floor() as i64,
        PlaybackTickMode::Paused => false,
    }
}

fn should_emit_on_tick_mode_change(
    prev: PlaybackTickMode,
    next: PlaybackTickMode,
    is_playing: bool,
) -> bool {
    prev != next
        && next != PlaybackTickMode::Paused
        && (prev == PlaybackTickMode::Paused || is_playing)
}

impl Coordinator {
    pub(super) async fn play_track_list(&mut self, tracks: Vec<Track>, start_index: Option<usize>) {
        self.crossfade
            .cancel(&self.mpv, self.shared_vol, self.shared_muted);
        self.queue.set_queue(tracks, start_index);
        self.playback_failures = 0;
        self.emit_queue();
        self.save_queue_async();
        self.start_current_track(0.0).await;
    }

    pub(super) async fn jump_to_queue_index(&mut self, idx: usize) {
        self.playback_failures = 0;
        self.crossfade
            .cancel(&self.mpv, self.shared_vol, self.shared_muted);
        if self.queue.jump_to(idx).is_some() {
            self.emit_queue();
            self.start_current_track(0.0).await;
        }
    }

    pub(super) async fn enqueue_with_notice(&mut self, tracks: Vec<Track>, at_front: bool) {
        let n = tracks.len();
        if n == 0 {
            return;
        }
        let first_title = tracks[0].title.clone();
        self.enqueue_hand(tracks, at_front).await;
        let msg = match (at_front, n) {
            (true, 1) => format!("PLAYS_NEXT // {first_title}"),
            (true, _) => format!("PLAYS_NEXT // {n} TRACKS"),
            (false, 1) => format!("TRACK_ADDED // QUEUE: {first_title}"),
            (false, _) => format!("TRACKS_ADDED // QUEUE ({n})"),
        };
        self.notice(NoticeKind::Success, msg);
    }

    pub(super) async fn enqueue_hand(&mut self, tracks: Vec<Track>, at_front: bool) {
        if tracks.is_empty() {
            return;
        }
        if self.crossfade.is_active() {
            self.crossfade
                .cancel(&self.mpv, self.shared_vol, self.shared_muted);
        }
        self.crossfade_attempted_track_id = None;
        let started_new = if at_front {
            self.queue.play_next(tracks)
        } else {
            self.queue.enqueue(tracks)
        };
        if started_new {
            self.emit_queue();
            self.save_queue_async();
            self.start_current_track(0.0).await;
        } else {
            self.emit_queue();
            self.save_queue_async();
            self.sync_mpv_next();
            self.sync_mpris().await;
        }
    }

    pub(super) async fn remove_queue_index(&mut self, idx: usize) {
        let was_current = self.queue.index() == Some(idx);
        if !self.queue.remove_at(idx) {
            return;
        }
        self.crossfade_attempted_track_id = None;
        if was_current {
            self.crossfade
                .cancel(&self.mpv, self.shared_vol, self.shared_muted);
        } else {
            self.cancel_crossfade_if_target_missing();
        }
        self.emit_queue();
        self.save_queue_async();
        if was_current {
            if self.queue.current().is_some() {
                self.start_current_track(0.0).await;
            } else {
                self.stop_to_idle().await;
            }
        } else {
            self.sync_mpv_next();
            self.sync_mpris().await;
        }
    }

    pub(super) async fn clear_queue(&mut self) {
        self.crossfade
            .cancel(&self.mpv, self.shared_vol, self.shared_muted);
        self.crossfade_attempted_track_id = None;
        self.queue.clear();
        self.emit_queue();
        self.save_queue_async();
        self.stop_to_idle().await;
    }

    pub(super) async fn toggle_play_pause(&mut self) {
        self.playback_failures = 0;
        self.output_retries = 0;
        let had_track = self.mpv_loaded_track_id.is_some();
        let was_running = self.mpv.is_running();
        if had_track {
            if !self.ensure_mpv_running() {
                return;
            }
            if !was_running {
                // Fresh mpv instance has no file loaded yet: reload from the current position.
                let resume = self
                    .pending_restore_seek
                    .take()
                    .unwrap_or(self.position_s)
                    .max(0.0);
                self.start_current_track(resume).await;
                return;
            }
            if self.is_playing && self.crossfade.is_active() {
                self.crossfade
                    .cancel(&self.mpv, self.shared_vol, self.shared_muted);
            }
            self.mpv.toggle_pause();
        } else {
            self.play_or_resume().await;
        }
    }

    pub(super) async fn play_or_resume(&mut self) {
        self.playback_failures = 0;
        self.output_retries = 0;
        if self.mpv_loaded_track_id.is_some() {
            let was_running = self.mpv.is_running();
            if !self.ensure_mpv_running() {
                return;
            }
            if !was_running {
                let resume = self
                    .pending_restore_seek
                    .take()
                    .unwrap_or(self.position_s)
                    .max(0.0);
                self.start_current_track(resume).await;
            } else {
                self.mpv.set_paused(false);
            }
        } else if self.queue.current().is_some() {
            let seek = self.pending_restore_seek.take().unwrap_or(0.0);
            self.start_current_track(seek).await;
        } else if let Some(first) = self.first_library_track() {
            self.queue.set_queue(vec![first], Some(0));
            self.emit_queue();
            self.save_queue_async();
            self.start_current_track(0.0).await;
        }
    }

    pub(super) async fn next_track(&mut self) {
        self.playback_failures = 0;
        self.crossfade
            .cancel(&self.mpv, self.shared_vol, self.shared_muted);
        self.step_queue(false).await;
    }

    pub(super) async fn previous_track(&mut self) {
        self.playback_failures = 0;
        self.crossfade
            .cancel(&self.mpv, self.shared_vol, self.shared_muted);
        self.crossfade_attempted_track_id = None;
        if self.position_s > 3.0 {
            if self.mpv_loaded_track_id.is_some() {
                self.mpv.seek(0.0);
            } else {
                self.pending_restore_seek = Some(0.0);
            }
            self.position_s = 0.0;
            self.emit_playback();
            self.sync_discord_rpc();
            if self.mpv_loaded_track_id.is_none() {
                self.save_session_state_async();
            }
            if let Some(mpris) = &self.mpris {
                mpris.seeked_to(0).await;
            }
        } else if self.queue.step_prev().is_some() {
            self.emit_queue();
            self.start_current_track(0.0).await;
        }
    }

    pub(super) async fn toggle_shuffle_mode(&mut self) {
        self.set_shuffle_mode(!self.queue.shuffle()).await;
    }

    pub(super) async fn set_shuffle_mode(&mut self, enabled: bool) {
        self.crossfade
            .cancel(&self.mpv, self.shared_vol, self.shared_muted);
        self.crossfade_attempted_track_id = None;
        self.queue.set_shuffle(enabled);
        self.emit_queue();
        self.save_queue_async();
        self.sync_mpv_next();
        self.sync_mpris().await;
    }

    pub(super) async fn cycle_repeat_mode(&mut self) {
        let next = self.queue.repeat().cycle();
        self.set_repeat_mode(next).await;
    }

    pub(super) async fn set_repeat_mode(&mut self, mode: RepeatMode) {
        self.queue.set_repeat(mode);
        self.crossfade_attempted_track_id = None;
        let fade_valid = mode != RepeatMode::One
            && self
                .queue
                .auto_next_index()
                .zip(self.crossfade.active_target())
                .is_some_and(|(next_idx, (tid, hint))| {
                    self.queue.find_track_near(hint, tid) == Some(next_idx)
                        && self.queue.crossfade_applies(
                            next_idx,
                            self.settings.crossfade_seconds,
                            self.is_direct_output(),
                        )
                });
        if !fade_valid && self.crossfade.is_active() {
            self.crossfade
                .cancel(&self.mpv, self.shared_vol, self.shared_muted);
        }
        self.emit_queue();
        self.save_session_state_async();
        self.sync_mpv_next();
        self.sync_mpris().await;
    }

    pub(super) async fn seek_seconds(&mut self, sec: f64) {
        let target = if self.duration_s > 0.0 {
            sec.clamp(0.0, self.duration_s)
        } else {
            sec.max(0.0)
        };
        self.crossfade
            .cancel(&self.mpv, self.shared_vol, self.shared_muted);
        if self.mpv_loaded_track_id.is_some() {
            self.mpv.seek(target);
        } else if self.queue.current().is_some() {
            self.pending_restore_seek = Some(target);
        }
        self.position_s = target;
        self.crossfade_attempted_track_id = None;
        self.emit_playback();
        self.sync_discord_rpc();
        if self.mpv_loaded_track_id.is_none() {
            self.save_session_state_async();
        }
        if let Some(mpris) = &self.mpris {
            mpris
                .seeked_to((self.position_s * 1_000_000.0) as i64)
                .await;
        }
    }

    pub(super) fn set_playback_tick_mode(&mut self, mode: PlaybackTickMode) {
        let prev = self.playback_tick_mode;
        self.playback_tick_mode = mode;
        if should_emit_on_tick_mode_change(prev, mode, self.is_playing) {
            self.emit_playback();
        }
    }

    pub(super) fn set_crossfade_seconds(&mut self, secs: i32) {
        let normalized = if crate::popups::CROSSFADE_CHOICES.contains(&secs) {
            secs
        } else {
            0
        };
        self.settings.crossfade_seconds = normalized;
        self.crossfade_attempted_track_id = None;
        if normalized == 0 && self.crossfade.is_active() {
            self.crossfade
                .cancel(&self.mpv, self.shared_vol, self.shared_muted);
        }
        self.sync_mpv_next();
        self.settings_dirty_since = Some(Instant::now());
        let _ = self
            .ui_tx
            .send(ControllerEvent::SettingsSaved(self.settings.clone()));
    }

    pub(super) fn cancel_crossfade_if_target_missing(&mut self) {
        if let Some((tid, hint)) = self
            .crossfade
            .active_target()
            .map(|(id, i)| (id.to_owned(), i))
            && self.queue.find_track_near(hint, &tid).is_none()
        {
            self.crossfade
                .cancel(&self.mpv, self.shared_vol, self.shared_muted);
        }
    }

    /// Updates controller playback metadata, emits UI/MPRIS/Tray state, and starts lyrics/accent jobs.
    async fn activate_playing_track(
        &mut self,
        track: Track,
        pos_s: f64,
        initial_heard: f64,
        duration_override: Option<f64>,
    ) {
        self.mpv_loaded_track_id = Some(track.id.clone());
        self.pending_restore_seek = None;
        self.crossfade_attempted_track_id = None;
        self.position_s = pos_s;
        self.duration_s = duration_override
            .filter(|d| d.is_finite() && *d > 0.0)
            .unwrap_or(track.duration);
        self.is_playing = true;
        self.play_tracker
            .reset_at(Some(track.id.clone()), pos_s, initial_heard);

        self.sync_mpv_next();
        let _ = self
            .ui_tx
            .send(ControllerEvent::TrackChanged(Some(track.clone())));
        self.emit_playback();
        self.load_lyrics_async(&track);
        self.load_accent_async(Some(&track));
        self.save_session_state_async();
        self.sync_mpris().await;
        self.sync_tray().await;
    }

    pub(super) async fn stop_playback_keep_track(&mut self, clear_restore: bool, reset_pos: bool) {
        self.is_playing = false;
        self.mpv_loaded_track_id = None;
        self.crossfade_attempted_track_id = None;
        if clear_restore {
            self.pending_restore_seek = None;
        }
        if reset_pos {
            self.position_s = 0.0;
        }
        self.emit_playback();
        self.save_session_state_async();
        self.sync_mpris().await;
        self.sync_tray().await;
    }

    pub(super) async fn stop_to_idle(&mut self) {
        self.mpv.stop_playback();
        self.mpv_next = None;
        self.mpv_loaded_track_id = None;
        self.pending_restore_seek = None;
        self.crossfade_attempted_track_id = None;
        self.is_playing = false;
        self.position_s = 0.0;
        self.duration_s = 0.0;
        self.play_tracker.reset(None);
        let _ = self.ui_tx.send(ControllerEvent::TrackChanged(None));
        let _ = self.ui_tx.send(ControllerEvent::LyricsUpdated {
            track_id: None,
            lyrics: None,
        });
        self.load_accent_async(None);
        self.emit_playback();
        self.sync_mpris().await;
        self.sync_tray().await;
    }

    pub(super) async fn start_current_track(&mut self, start_seconds: f64) {
        let Some(track) = self.queue.current().cloned() else {
            return;
        };
        if !self.ensure_mpv_running() {
            self.mpv_loaded_track_id = None;
            self.pending_restore_seek = (start_seconds > 0.0).then_some(start_seconds);
            self.is_playing = false;
            self.emit_playback();
            self.sync_mpris().await;
            return;
        }
        self.mpv.play(&track.file_path, start_seconds);
        self.activate_playing_track(track, start_seconds, 0.0, None)
            .await;
    }

    /// Tells mpv which file comes next for gapless playback, or nothing when crossfade/shuffle-wrap applies.
    pub(super) fn sync_mpv_next(&mut self) {
        if self.mpv_loaded_track_id.is_none() {
            self.hand_mpv_next(None);
            return;
        }
        let direct = self.is_direct_output();
        let len = self.queue.tracks().len();
        let at_shuffle_round_end = self.queue.shuffle()
            && self.queue.repeat() == RepeatMode::All
            && len > 1
            && self.queue.index() == Some(len - 1);
        let next = self.queue.auto_next_index().filter(|&idx| {
            !(at_shuffle_round_end && idx == 0)
                && !self
                    .queue
                    .crossfade_applies(idx, self.settings.crossfade_seconds, direct)
        });
        self.hand_mpv_next(next);
    }

    fn hand_mpv_next(&mut self, next: Option<usize>) {
        let next = next.filter(|&i| i < self.queue.tracks().len());
        self.mpv_next = next;
        let path = next.map(|i| self.queue.tracks()[i].file_path.as_str());
        self.mpv.set_next(path);
    }

    pub(super) fn sync_mpv_next_ignoring_crossfade(&mut self) {
        let len = self.queue.tracks().len();
        let at_shuffle_round_end = self.queue.shuffle()
            && self.queue.repeat() == RepeatMode::All
            && len > 1
            && self.queue.index() == Some(len - 1);
        let next = self
            .queue
            .auto_next_index()
            .filter(|&idx| !(at_shuffle_round_end && idx == 0));
        self.hand_mpv_next(next);
    }

    pub(super) async fn step_queue(&mut self, from_eof: bool) {
        if let Some(_next) = self.queue.step_next(from_eof) {
            self.emit_queue();
            self.start_current_track(0.0).await;
        } else if from_eof {
            self.stop_playback_keep_track(true, true).await;
        }
    }

    pub(super) fn load_lyrics_async(&mut self, track: &Track) {
        let id = track.id.clone();
        let path = PathBuf::from(&track.file_path);
        let (tx, rx) = mpsc::unbounded_channel();
        self.lyrics_done_rx = Some(rx);
        tokio::task::spawn_blocking(move || {
            let doc = LyricsLoader::load(&path);
            let _ = tx.send((id, doc));
        });
    }

    pub(super) fn load_accent_async(&mut self, track: Option<&Track>) {
        if !self.settings.accent_from_cover {
            self.accent_done_rx = None;
            let _ = self.ui_tx.send(ControllerEvent::AccentPaletteChanged(None));
            return;
        }
        let Some(track) = track else {
            self.accent_done_rx = None;
            let _ = self.ui_tx.send(ControllerEvent::AccentPaletteChanged(None));
            return;
        };
        let cover_path = track
            .cover_thumb_path
            .as_ref()
            .or(track.cover_full_path.as_ref())
            .cloned();
        let Some(path) = cover_path else {
            self.accent_done_rx = None;
            let _ = self.ui_tx.send(ControllerEvent::AccentPaletteChanged(None));
            return;
        };
        let id = track.id.clone();
        let (tx, rx) = mpsc::unbounded_channel();
        self.accent_done_rx = Some(rx);
        tokio::task::spawn_blocking(move || {
            let pal = extract_cover_palette(&path);
            let _ = tx.send((id, pal));
        });
    }

    pub(super) async fn step_crossfade(&mut self) {
        let step = self.crossfade.step(&self.mpv, self.shared_vol);
        if matches!(step, FadeStep::Completed) {
            let _ = self.finish_crossfade_swap().await;
        }
    }

    async fn finish_crossfade_swap(&mut self) -> bool {
        let Some((target_id, hint_idx)) = self
            .crossfade
            .active_target()
            .map(|(id, i)| (id.to_owned(), i))
        else {
            return false;
        };

        if self.queue.repeat() == RepeatMode::One
            || self.queue.find_track_near(hint_idx, &target_id).is_none()
        {
            self.crossfade
                .cancel(&self.mpv, self.shared_vol, self.shared_muted);
            self.emit_queue();
            self.sync_mpv_next();
            self.sync_mpris().await;
            return false;
        }

        let prev_idx = self.queue.index();
        let queue_len = self.queue.tracks().len();
        let Some(swap) = self.crossfade.complete_swap(
            &mut self.mpv,
            &mut self.mpv_rx,
            self.shared_vol,
            self.shared_muted,
        ) else {
            return false;
        };

        let _ = self.queue.jump_to_track_near(hint_idx, &swap.track.id);
        if prev_idx == Some(queue_len.saturating_sub(1)) && self.queue.index() == Some(0) {
            self.queue.reshuffle_after_wrap();
        }
        if let Some(ap) = swap.audio_path {
            self.last_audio_path = ap;
            self.emit_direct_status();
        }

        self.emit_queue();
        let pos_s = swap.pos_s.unwrap_or(0.0);
        self.activate_playing_track(swap.track, pos_s, pos_s, swap.duration_s)
            .await;
        true
    }

    pub(super) async fn handle_mpv_event(&mut self, evt: EngineEvent) {
        match evt {
            EngineEvent::TimePos(t) => {
                self.position_s = t;
                self.emit_playback_tick();
                self.set_mpris_position(t).await;

                if let Some(track_id) =
                    self.play_tracker
                        .on_time_pos(t, self.duration_s, self.is_playing)
                {
                    self.stats.record_now(&track_id);
                    let stats_save = self.stats.clone();
                    let _ = self
                        .ui_tx
                        .send(ControllerEvent::StatsUpdated(stats_save.clone()));
                    let _ = self.persist_tx.send(PersistMsg::Stats(stats_save));
                }

                let direct = self.is_direct_output();
                if self.is_playing
                    && !direct
                    && !self.crossfade.is_active()
                    && self.settings.crossfade_seconds > 0
                    && self.duration_s > 0.0
                {
                    let remaining = self.duration_s - t;
                    if remaining > 0.25
                        && remaining <= self.settings.crossfade_seconds as f64
                        && let Some(next_idx) = self.queue.auto_next_index()
                        && let Some(cur) = self.queue.current()
                        && self.crossfade_attempted_track_id.as_deref() != Some(cur.id.as_str())
                        && self.queue.crossfade_applies(
                            next_idx,
                            self.settings.crossfade_seconds,
                            direct,
                        )
                        && let Some(next_track) = self.queue.tracks().get(next_idx).cloned()
                    {
                        self.crossfade_attempted_track_id = Some(cur.id.clone());
                        let fade_dur = Duration::from_secs_f64((remaining - 0.1).clamp(0.4, 12.0));
                        let poll = Duration::from_millis(self.render_tier.poll_interval_ms);
                        let muted = self.shared_muted;
                        if let Err(err) = self
                            .crossfade
                            .begin_async(next_track, next_idx, fade_dur, poll, muted)
                            .await
                        {
                            warn!(error = %err, "Crossfade begin failed; falling back to gapless/EOF");
                            self.notice(NoticeKind::Warn, format!("CROSSFADE_FAILED // {err}"));
                            self.sync_mpv_next_ignoring_crossfade();
                        }
                    }
                }
            }
            EngineEvent::Duration(d) => {
                self.duration_s = d;
                self.playback_failures = 0;
                self.emit_playback();
                self.sync_mpris().await;
            }
            EngineEvent::Pause(p) => {
                if p && self.crossfade.is_active() {
                    return;
                }
                let playing = !p && self.mpv_loaded_track_id.is_some();
                if playing == self.is_playing {
                    return;
                }
                self.is_playing = playing;
                self.emit_playback();
                self.save_session_state_async();
                self.sync_mpris().await;
                self.sync_tray().await;
            }
            EngineEvent::EndOfFile => {
                if self.crossfade.is_active() {
                    if !self.finish_crossfade_swap().await {
                        self.step_queue(true).await;
                    }
                } else {
                    self.step_queue(true).await;
                }
            }
            EngineEvent::AdvancedToNext(path) => {
                let expected = self.mpv_next.take();
                match self.queue.on_mpv_advanced(&path, expected) {
                    MpvAdvance::Followed(track) => {
                        self.playback_failures = 0;
                        self.emit_queue();
                        self.activate_playing_track(track, 0.0, 0.0, None).await;
                    }
                    MpvAdvance::Reload(track) => {
                        info!(
                            path,
                            intended = track.file_path,
                            "mpv entered a file that is no longer next; loading the intended one"
                        );
                        self.playback_failures = 0;
                        self.emit_queue();
                        self.start_current_track(0.0).await;
                    }
                    MpvAdvance::Stop => {
                        self.mpv.stop_playback();
                        self.stop_playback_keep_track(true, true).await;
                    }
                }
            }
            EngineEvent::AudioPathChanged(ap) => {
                if ap.output.is_some() {
                    self.output_retries = 0;
                }
                self.last_audio_path = ap;
                if self.direct_card.is_some() {
                    let dev = self.settings.audio_device.clone();
                    let (hw, altered) = tokio::join!(
                        tokio::task::spawn_blocking(move || AlsaDirect::read_hw_params(&dev)),
                        self.mpv.get_sample_alteration()
                    );
                    self.last_hw_params = hw.ok().flatten();
                    self.last_sample_alteration = altered;
                }
                self.emit_direct_status();
            }
            EngineEvent::PlaybackFailed { path, reason } if reason == AO_INIT_FAILED => {
                warn!(path, reason, "mpv could not open the audio output");
                self.handle_output_failure(&path).await;
            }
            EngineEvent::PlaybackFailed { path, reason } => {
                warn!(path, reason, "mpv playback failed");
                let current = self
                    .queue
                    .current()
                    .filter(|t| path.is_empty() || t.file_path == path)
                    .cloned();
                match current {
                    Some(track)
                        if self.mpv_loaded_track_id.as_deref() == Some(track.id.as_str()) =>
                    {
                        self.skip_failed_track(&track, &reason).await;
                    }
                    _ => self.notice(NoticeKind::Error, format!("PLAYBACK_FAILED // {reason}")),
                }
            }
            EngineEvent::Exited => {
                warn!("libmpv exited unexpectedly");
                self.crossfade
                    .cancel(&self.mpv, self.shared_vol, self.shared_muted);
                if self.is_playing {
                    self.notice(
                        NoticeKind::Error,
                        "AUDIO_ENGINE_STOPPED // PRESS PLAY TO RESUME",
                    );
                }
                if self.mpv_loaded_track_id.is_some() && self.position_s > 0.0 {
                    self.pending_restore_seek = Some(self.position_s);
                }
                self.stop_playback_keep_track(false, false).await;
            }
        }
    }

    async fn handle_output_failure(&mut self, path: &str) {
        let Some(track) = self
            .queue
            .current()
            .filter(|t| path.is_empty() || t.file_path == path)
            .filter(|t| self.mpv_loaded_track_id.as_deref() == Some(t.id.as_str()))
            .cloned()
        else {
            self.notice(
                NoticeKind::Error,
                format!("PLAYBACK_FAILED // {}", AO_INIT_FAILED.to_uppercase()),
            );
            return;
        };
        let position = self.position_s.max(0.0);
        let was_playing = self.is_playing;
        self.mpv_loaded_track_id = None;
        self.is_playing = false;
        self.sync_discord_rpc();
        let device = self.mpv_device();
        let direct = AlsaDirect::is_direct(&device);

        self.output_retries += 1;
        if was_playing && self.output_retries <= OUTPUT_RETRIES {
            let backoff = super::audio::OUTPUT_RETRY_BASE * 2u32.pow(self.output_retries - 1);
            info!(
                attempt = self.output_retries,
                ?backoff,
                device,
                "Output failed to open; retrying"
            );
            tokio::time::sleep(backoff).await;
            let free = if direct {
                Self::wait_for_card_free(&device).await
            } else {
                Ok(())
            };
            if free.is_ok()
                && self.queue.current().is_some_and(|t| t.id == track.id)
                && self.mpv_loaded_track_id.is_none()
            {
                self.start_current_track(position).await;
                return;
            }
        }

        self.output_retries = 0;
        let message = if direct {
            let holders = tokio::task::spawn_blocking({
                let dev = device.clone();
                move || AlsaDirect::pcm_holders(&dev)
            })
            .await
            .unwrap_or_default();
            super::audio::direct_failure_message(&holders)
        } else {
            "AUDIO_OUTPUT_FAILED // THE OUTPUT DEVICE COULD NOT BE OPENED. PRESS PLAY TO RETRY"
                .to_owned()
        };
        self.notice(NoticeKind::Error, message);
        self.pending_restore_seek = (position > 0.0).then_some(position);
        self.position_s = position;
        self.emit_playback();
        self.save_session_state_async();
        self.sync_mpris().await;
        self.sync_tray().await;
    }

    async fn skip_failed_track(&mut self, track: &Track, reason: &str) {
        let (missing, rescan_target) = {
            let path = PathBuf::from(&track.file_path);
            let folders = self.settings.library_folders.clone();
            tokio::task::spawn_blocking(move || {
                let exists = path.exists();
                let target = (!exists)
                    .then(|| failed_track_rescan_target(&path, &folders))
                    .flatten();
                (!exists, target)
            })
            .await
            .unwrap_or((false, None))
        };
        if let Some(target) = rescan_target {
            self.handle_watched_paths(vec![target]);
        }
        let title = track.title.to_uppercase();
        self.notice(
            NoticeKind::Error,
            if missing {
                format!("FILE_NOT_FOUND // {title}")
            } else {
                format!("PLAYBACK_FAILED // {title} // {}", reason.to_uppercase())
            },
        );
        self.crossfade
            .cancel(&self.mpv, self.shared_vol, self.shared_muted);
        self.playback_failures += 1;
        if self.playback_failures < MAX_FAILED_SKIPS
            && self.queue.tracks().len() > 1
            && self.queue.step_next(false).is_some()
        {
            self.emit_queue();
            self.start_current_track(0.0).await;
            return;
        }
        self.stop_playback_keep_track(true, true).await;
    }

    pub(super) async fn handle_spare_mpv_event(&mut self, evt: EngineEvent) {
        match evt {
            EngineEvent::AudioPathChanged(ap) => {
                if self.crossfade.is_active() {
                    self.crossfade.set_spare_audio_path(ap);
                }
            }
            EngineEvent::TimePos(t) => {
                if self.crossfade.is_active() {
                    self.crossfade.set_spare_position(t);
                }
            }
            EngineEvent::Duration(d) => {
                if self.crossfade.is_active() {
                    self.crossfade.set_spare_duration(d);
                }
            }
            EngineEvent::PlaybackFailed { path, reason } => {
                warn!(path, reason, "Spare crossfade mpv playback failed");
                if self.crossfade.is_active()
                    && self
                        .crossfade
                        .active_target_path()
                        .is_none_or(|p| p == path || path.is_empty())
                {
                    self.crossfade
                        .cancel(&self.mpv, self.shared_vol, self.shared_muted);
                    self.crossfade_attempted_track_id = self.queue.current().map(|t| t.id.clone());
                    self.sync_mpv_next_ignoring_crossfade();
                    self.notice(NoticeKind::Warn, format!("CROSSFADE_FAILED // {reason}"));
                }
            }
            EngineEvent::Exited => {
                warn!("Spare crossfade mpv exited unexpectedly");
                if self.crossfade.is_active() {
                    self.crossfade
                        .cancel(&self.mpv, self.shared_vol, self.shared_muted);
                    self.crossfade_attempted_track_id = self.queue.current().map(|t| t.id.clone());
                    self.sync_mpv_next_ignoring_crossfade();
                }
                self.crossfade.drop_dead_spare();
            }
            _ => {}
        }
    }

    pub(super) fn first_library_track(&self) -> Option<Track> {
        self.albums.first().and_then(|a| a.tracks.first().cloned())
    }

    pub(super) fn emit_queue(&self) {
        let _ = self.ui_tx.send(ControllerEvent::QueueUpdated {
            tracks: self.queue.shared_tracks(),
            index: self.queue.index(),
            shuffle: self.queue.shuffle(),
            repeat: self.queue.repeat(),
            hand_len: self.queue.hand_len(),
        });
    }

    pub(super) fn emit_playback(&mut self) {
        self.last_playback_emit = Some((
            Instant::now(),
            self.position_s.floor() as i64,
            self.is_playing,
            self.duration_s,
        ));
        let _ = self.ui_tx.send(ControllerEvent::PlaybackState {
            is_playing: self.is_playing,
            position_s: self.position_s,
            duration_s: self.duration_s,
        });
    }

    fn emit_playback_tick(&mut self) {
        if !playback_tick_due(
            self.playback_tick_mode,
            self.last_playback_emit,
            Instant::now(),
            self.position_s,
            self.is_playing,
            self.duration_s,
        ) {
            return;
        }
        self.emit_playback();
    }

    pub(super) fn emit_volume(&self) {
        let _ = self.ui_tx.send(ControllerEvent::VolumeState {
            volume: self.current_vol,
            muted: self.current_muted,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn playback_tick_due_respects_active_unfocused_and_paused_modes() {
        let t0 = Instant::now();
        let last = Some((t0, 10_i64, true, 200.0));
        let at = |ms: u64| t0 + Duration::from_millis(ms);

        let cases = [
            (PlaybackTickMode::Active, None, t0, 0.0, true, 200.0, true),
            (
                PlaybackTickMode::Active,
                last,
                at(100),
                10.4,
                true,
                200.0,
                false,
            ),
            (
                PlaybackTickMode::Active,
                last,
                at(100),
                11.0,
                true,
                200.0,
                true,
            ),
            (
                PlaybackTickMode::Active,
                last,
                at(250),
                10.6,
                true,
                200.0,
                true,
            ),
            (
                PlaybackTickMode::Unfocused,
                last,
                at(300),
                10.6,
                true,
                200.0,
                false,
            ),
            (
                PlaybackTickMode::Unfocused,
                last,
                at(300),
                11.0,
                true,
                200.0,
                true,
            ),
            (PlaybackTickMode::Paused, None, t0, 0.0, true, 200.0, true),
            (
                PlaybackTickMode::Paused,
                last,
                at(5000),
                15.0,
                true,
                200.0,
                false,
            ),
            (
                PlaybackTickMode::Paused,
                last,
                at(10),
                10.1,
                false,
                200.0,
                true,
            ),
            (
                PlaybackTickMode::Paused,
                last,
                at(10),
                10.1,
                true,
                180.0,
                true,
            ),
        ];
        for (mode, prev, now, pos, playing, dur, want) in cases {
            assert_eq!(
                playback_tick_due(mode, prev, now, pos, playing, dur),
                want,
                "{mode:?} {pos}"
            );
        }
    }

    #[test]
    fn tick_mode_change_emits_only_when_resuming_or_upgrading_while_playing() {
        use PlaybackTickMode::{Active, Paused, Unfocused};
        for (prev, next, playing, want) in [
            (Paused, Active, true, true),
            (Paused, Active, false, true),
            (Paused, Unfocused, true, true),
            (Paused, Unfocused, false, true),
            (Unfocused, Active, true, true),
            (Unfocused, Active, false, false),
            (Active, Unfocused, true, true),
            (Active, Unfocused, false, false),
            (Active, Paused, true, false),
            (Unfocused, Paused, true, false),
            (Active, Active, true, false),
        ] {
            assert_eq!(should_emit_on_tick_mode_change(prev, next, playing), want);
        }
    }
}
