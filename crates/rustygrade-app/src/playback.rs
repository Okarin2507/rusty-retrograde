use std::collections::hash_map::RandomState;
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::hash::{BuildHasher, Hasher};
use std::io::Read;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use rustygrade_core::{PlayStats, SessionState, Track};

use crate::crossfade::CrossfadeMath;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RepeatMode {
    #[default]
    Off = 0,
    All = 1,
    One = 2,
}

impl RepeatMode {
    pub(crate) fn from_i32(v: i32) -> Self {
        match v {
            1 => Self::All,
            2 => Self::One,
            _ => Self::Off,
        }
    }

    pub(crate) fn as_i32(self) -> i32 {
        self as i32
    }

    pub fn cycle(self) -> Self {
        match self {
            Self::Off => Self::All,
            Self::All => Self::One,
            Self::One => Self::Off,
        }
    }

    pub fn icon_name(self) -> &'static str {
        match self {
            Self::One => "repeat1",
            Self::Off | Self::All => "repeat",
        }
    }

    pub fn is_active(self) -> bool {
        self != Self::Off
    }

    pub fn mpris_loop_status(self) -> &'static str {
        match self {
            Self::Off => "None",
            Self::All => "Playlist",
            Self::One => "Track",
        }
    }

    pub fn from_mpris_loop_status(status: &str) -> Self {
        match status {
            "Playlist" => Self::All,
            "Track" => Self::One,
            _ => Self::Off,
        }
    }
}

struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    fn from_os_entropy() -> Self {
        let mut buf = [0_u8; 8];
        let mut seed = if let Ok(mut f) = File::open("/dev/urandom")
            && f.read_exact(&mut buf).is_ok()
        {
            u64::from_le_bytes(buf)
        } else {
            0
        };
        if seed == 0 {
            let mut hasher = RandomState::new().build_hasher();
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos() as u64);
            hasher.write_u64(nanos);
            hasher.write_usize(buf.as_ptr() as usize);
            seed = hasher.finish();
        }
        if seed == 0 {
            seed = 0x9e37_79b9_7f4a_7c15;
        }
        Self { state: seed }
    }

    fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn next_usize(&mut self, bound: usize) -> usize {
        debug_assert!(bound > 0);
        if bound <= 1 {
            return 0;
        }
        let bound_u64 = bound as u64;
        let limit = u64::MAX - (u64::MAX % bound_u64);
        loop {
            let v = self.next_u64();
            if v < limit {
                return (v % bound_u64) as usize;
            }
        }
    }
}

static SHUFFLE_RNG: LazyLock<Mutex<SplitMix64>> =
    LazyLock::new(|| Mutex::new(SplitMix64::from_os_entropy()));

fn next_rand_usize(bound: usize) -> usize {
    match SHUFFLE_RNG.lock() {
        Ok(mut rng) => rng.next_usize(bound),
        Err(poisoned) => poisoned.into_inner().next_usize(bound),
    }
}

fn shuffle_slice<T>(slice: &mut [T]) {
    let n = slice.len();
    for i in (1..n).rev() {
        let j = next_rand_usize(i + 1);
        slice.swap(i, j);
    }
}

/// The play queue.
///
/// `tracks` is the play order: what follows the current track is what plays next, in shuffle too.
/// Shuffle stays scoped to the current album or playlist: turning it on keeps the current track
/// first and shuffles the remaining tracks in the queue randomly; turning it off restores the
/// unshuffled queue order at the current track.
///
/// Tracks queued by hand form a line right after the current track: ADD_TO_QUEUE joins the end of
/// that line, PLAY_NEXT goes to its front. The line plays before the rest of the queue.
/// What [`PlaybackQueue::on_mpv_advanced`] found when mpv moved on into its queued file by itself.
#[derive(Debug, Clone)]
pub enum MpvAdvance {
    /// mpv entered the file it was handed: this track is now current and already playing.
    Followed(Track),
    /// The queue was edited right at the boundary and mpv entered a file that is no longer next:
    /// the intended track is now current and has to be loaded (C# `OnAdvanced` → `PlayIndexAsync`).
    Reload(Track),
    /// Nothing is meant to follow: stop.
    Stop,
}

#[derive(Debug, Default)]
pub struct PlaybackQueue {
    /// Shared with the UI ([`PlaybackQueue::shared_tracks`]): moving to another track only changes
    /// `index`, so sending the queue costs nothing; an edit copies it once (`Arc::make_mut`).
    tracks: Arc<Vec<Track>>,
    /// The queue from before shuffle (C# `_unshuffledQueue`); `None` while not shuffling, or after
    /// a restart that did not keep it.
    unshuffled: Option<Vec<Track>>,
    index: Option<usize>,
    shuffle: bool,
    repeat: RepeatMode,
    /// How many of the tracks right after the current one were queued by hand.
    hand: usize,
}

impl PlaybackQueue {
    pub fn tracks(&self) -> &[Track] {
        &self.tracks
    }

    /// The play order, shared (no copy) for the UI.
    pub fn shared_tracks(&self) -> Arc<Vec<Track>> {
        Arc::clone(&self.tracks)
    }

    pub fn is_empty(&self) -> bool {
        self.tracks.is_empty()
    }

    pub fn index(&self) -> Option<usize> {
        self.index
    }

    pub fn current(&self) -> Option<&Track> {
        self.index.and_then(|i| self.tracks.get(i))
    }

    pub fn shuffle(&self) -> bool {
        self.shuffle
    }

    pub fn repeat(&self) -> RepeatMode {
        self.repeat
    }

    /// How many upcoming tracks were queued by hand (they sit right after the current one).
    pub fn hand_len(&self) -> usize {
        self.hand
    }

    pub fn set_repeat(&mut self, repeat: RepeatMode) {
        self.repeat = repeat;
    }

    pub fn can_go_next(&self) -> bool {
        match self.index {
            Some(i) => {
                i + 1 < self.tracks.len()
                    || (self.repeat == RepeatMode::All && !self.tracks.is_empty())
            }
            None => !self.tracks.is_empty(),
        }
    }

    pub fn can_go_previous(&self) -> bool {
        match self.index {
            Some(i) => i > 0 || (self.repeat == RepeatMode::All && self.tracks.len() > 1),
            None => false,
        }
    }

    /// A new current track: moving on to the next track in order uses up one hand-queued track;
    /// repeating the current track (`new_index == index`) preserves the hand-queued line; a jump
    /// anywhere else ends the line (what it skipped stays as ordinary tracks).
    fn note_track_change(&mut self, new_index: usize) {
        if self.index == Some(new_index) {
            return;
        }
        let in_order = self.index.is_some_and(|i| i + 1 == new_index);
        self.hand = if in_order {
            self.hand.saturating_sub(1)
        } else {
            0
        };
    }

    fn set_index(&mut self, new_index: usize) {
        self.note_track_change(new_index);
        self.index = Some(new_index);
    }

    /// Sets the active queue from an album or playlist. When shuffle is enabled:
    /// - `Some(idx)` keeps that track first and shuffles the remaining tracks in `tracks`.
    /// - `None` shuffles all tracks in `tracks` and starts from a random track at index `0`.
    pub fn set_queue(&mut self, tracks: Vec<Track>, start_index: Option<usize>) {
        if tracks.is_empty() {
            self.clear();
            return;
        }
        self.hand = 0;
        if self.shuffle {
            self.unshuffled = Some(tracks.clone());
            self.tracks = Arc::new(tracks);
            self.index = start_index.map(|i| i.min(self.tracks.len() - 1));
            self.build_order();
            self.index = Some(0);
        } else {
            let idx = start_index.unwrap_or(0).min(tracks.len() - 1);
            self.tracks = Arc::new(tracks);
            self.index = Some(idx);
            self.unshuffled = None;
        }
    }

    pub fn jump_to(&mut self, idx: usize) -> Option<Track> {
        if idx < self.tracks.len() {
            self.set_index(idx);
            self.tracks.get(idx).cloned()
        } else {
            None
        }
    }

    pub fn find_track_near(&self, hint: usize, track_id: &str) -> Option<usize> {
        let len = self.tracks.len();
        if len == 0 {
            return None;
        }
        if hint < len && self.tracks[hint].id == track_id {
            return Some(hint);
        }
        for dist in 1..=len {
            if let Some(left) = hint.checked_sub(dist)
                && left < len
                && self.tracks[left].id == track_id
            {
                return Some(left);
            }
            let right = hint.saturating_add(dist);
            if right < len && self.tracks[right].id == track_id {
                return Some(right);
            }
        }
        None
    }

    pub fn jump_to_track_near(&mut self, hint: usize, track_id: &str) -> Option<usize> {
        let idx = self.find_track_near(hint, track_id)?;
        self.set_index(idx);
        Some(idx)
    }

    pub fn set_shuffle(&mut self, enabled: bool) {
        if self.shuffle == enabled {
            return;
        }
        self.shuffle = enabled;
        self.hand = 0; // the queue is rebuilt around the current track
        if enabled {
            if self.tracks.is_empty() {
                return;
            }
            self.unshuffled = Some(self.tracks.to_vec());
            self.build_order();
        } else {
            self.restore_unshuffled();
        }
    }

    /// Builds a fresh round within the current queue: the current track stays first when known,
    /// and every other track in the queue follows in random order.
    fn build_order(&mut self) {
        match self.index {
            Some(cur) if cur < self.tracks.len() => {
                let tracks = Arc::make_mut(&mut self.tracks);
                let current = tracks.remove(cur);
                shuffle_slice(tracks.as_mut_slice());
                tracks.insert(0, current);
                self.index = Some(0);
            }
            _ => shuffle_slice(Arc::make_mut(&mut self.tracks).as_mut_slice()),
        }
    }

    /// Restores the queue from before shuffle, at the track now playing.
    fn restore_unshuffled(&mut self) {
        let Some(back) = self.unshuffled.take() else {
            return;
        };
        let Some(current) = self.current().cloned() else {
            self.tracks = Arc::new(back);
            self.index = None;
            return;
        };
        if let Some(idx) = back.iter().position(|t| t.id == current.id) {
            self.tracks = Arc::new(back);
            self.index = Some(idx);
        } else {
            let mut list = Vec::with_capacity(back.len() + 1);
            list.push(current);
            list.extend(back);
            self.tracks = Arc::new(list);
            self.index = Some(0);
        }
    }

    /// Returns the track index that should play next automatically (gapless / EOF), the current
    /// one on `RepeatMode::One`.
    pub fn auto_next_index(&self) -> Option<usize> {
        if self.tracks.is_empty() {
            return None;
        }
        let cur = self.index?;
        if self.repeat == RepeatMode::One {
            return Some(cur);
        }
        if cur + 1 < self.tracks.len() {
            Some(cur + 1)
        } else if self.repeat == RepeatMode::All {
            Some(0)
        } else {
            None
        }
    }

    /// C# `CrossfadeApplies`: whether the change from the current track to `next` fades.
    pub fn crossfade_applies(&self, next: usize, crossfade_seconds: i32, is_direct: bool) -> bool {
        let Some(cur) = self.index else {
            return false;
        };
        if next == cur {
            return false;
        }
        let (Some(current), Some(following)) = (self.tracks.get(cur), self.tracks.get(next)) else {
            return false;
        };
        CrossfadeMath::should_crossfade(
            crossfade_seconds,
            self.repeat,
            is_direct,
            self.shuffle,
            current,
            following,
        )
    }

    pub fn step_next(&mut self, from_eof: bool) -> Option<Track> {
        let len = self.tracks.len();
        if len == 0 {
            return None;
        }
        let cur = match self.index {
            Some(i) => i,
            None => {
                self.hand = 0;
                self.index = Some(0);
                return self.tracks.first().cloned();
            }
        };
        if from_eof && self.repeat == RepeatMode::One {
            self.set_index(cur);
            return self.tracks.get(cur).cloned();
        }
        if cur + 1 < len {
            self.set_index(cur + 1);
            return self.tracks.get(cur + 1).cloned();
        }
        if self.repeat == RepeatMode::All {
            self.hand = 0;
            if self.shuffle && len > 1 {
                // Every track has had its turn: deal a new round, not opening with the one just played.
                let last_id = self.tracks[cur].id.clone();
                let tracks = Arc::make_mut(&mut self.tracks);
                shuffle_slice(tracks.as_mut_slice());
                if tracks[0].id == last_id
                    && let Some(other) = tracks.iter().position(|t| t.id != last_id)
                {
                    tracks.swap(0, other);
                }
            }
            self.index = Some(0);
            return self.tracks.first().cloned();
        }
        None
    }

    /// Deals a new shuffle round after a `RepeatMode::All` wrap that already started `tracks[0]`.
    pub fn reshuffle_after_wrap(&mut self) {
        if self.shuffle && self.repeat == RepeatMode::All && self.tracks.len() > 1 {
            shuffle_slice(&mut Arc::make_mut(&mut self.tracks)[1..]);
        }
    }

    pub fn step_prev(&mut self) -> Option<Track> {
        let len = self.tracks.len();
        if len == 0 {
            return None;
        }
        let cur = self.index.unwrap_or(0);
        let target = if cur > 0 {
            cur - 1
        } else if self.repeat == RepeatMode::All {
            len - 1
        } else {
            0
        };
        self.set_index(target);
        self.tracks.get(target).cloned()
    }

    /// mpv ran from the current file into the one queued behind it (`path`); `expected` is the
    /// slot that was handed to it as next (C# `_queuedNextIndex`). When the queue changed at the
    /// boundary (a track queued by hand just as mpv moved on), mpv's file is not the intended one:
    /// the intended next track becomes current instead and must be loaded, so the hand-queued line
    /// is not skipped.
    pub fn on_mpv_advanced(&mut self, path: &str, expected: Option<usize>) -> MpvAdvance {
        if let Some(e) = expected
            && self.tracks.get(e).is_some_and(|t| t.file_path == path)
        {
            self.set_index(e);
            return MpvAdvance::Followed(self.tracks[e].clone());
        }
        match self.auto_next_index() {
            Some(want) => {
                self.set_index(want);
                MpvAdvance::Reload(self.tracks[want].clone())
            }
            None => MpvAdvance::Stop,
        }
    }

    /// PLAY_NEXT: right after the current track, ahead of the tracks queued before. Returns `true`
    /// when nothing was queued and this started a new queue (the caller starts playback).
    pub fn play_next(&mut self, tracks: Vec<Track>) -> bool {
        self.enqueue_by_hand(tracks, true)
    }

    /// ADD_TO_QUEUE: after the current track and the tracks queued by hand before it, without
    /// interrupting playback. Returns `true` when this started a new queue.
    pub fn enqueue(&mut self, tracks: Vec<Track>) -> bool {
        self.enqueue_by_hand(tracks, false)
    }

    fn enqueue_by_hand(&mut self, list: Vec<Track>, at_front: bool) -> bool {
        if list.is_empty() {
            return false;
        }
        let Some(cur) = self.index.filter(|&i| i < self.tracks.len()) else {
            // Nothing queued: start playing, as addToQueue does in the Electron build.
            self.set_queue(list, Some(0));
            return true;
        };
        let upcoming = self.tracks.len() - cur - 1;
        let offset = if at_front { 0 } else { self.hand.min(upcoming) };
        self.keep_unshuffled_insert(&list, offset);
        let n = list.len();
        let at = cur + 1 + offset;
        Arc::make_mut(&mut self.tracks).splice(at..at, list);
        self.hand = self.hand.min(upcoming) + n;
        false
    }

    /// Tracks queued by hand go to the same place in the queue from before: after the current track
    /// (and the line).
    fn keep_unshuffled_insert(&mut self, list: &[Track], offset: usize) {
        let Some(current_id) = self.current().map(|t| t.id.clone()) else {
            return;
        };
        let Some(back) = self.unshuffled.as_mut() else {
            return;
        };
        let pos = match back.iter().position(|t| t.id == current_id) {
            Some(at) => (at + 1 + offset).min(back.len()),
            None => back.len(),
        };
        back.splice(pos..pos, list.iter().cloned());
    }

    pub fn remove_at(&mut self, idx: usize) -> bool {
        if idx >= self.tracks.len() {
            return false;
        }
        if let Some(cur) = self.index {
            if idx == cur {
                // The track after it becomes current: one hand-queued track used up.
                self.hand = self.hand.saturating_sub(1);
            } else if idx > cur && idx - cur - 1 < self.hand {
                self.hand -= 1;
            }
        }
        let removed = Arc::make_mut(&mut self.tracks).remove(idx);
        if let Some(back) = self.unshuffled.as_mut() {
            // The queue from before keeps no more copies of this track than the play order still
            // has, so a duplicate id elsewhere is never dropped by mistake.
            let left = self.tracks.iter().filter(|t| t.id == removed.id).count();
            while back.iter().filter(|t| t.id == removed.id).count() > left {
                match back.iter().rposition(|t| t.id == removed.id) {
                    Some(at) => {
                        back.remove(at);
                    }
                    None => break,
                }
            }
        }
        self.index = match self.index {
            None => None,
            Some(_) if self.tracks.is_empty() => None,
            Some(cur) if idx < cur => Some(cur - 1),
            Some(cur) if idx == cur => (cur < self.tracks.len()).then_some(cur),
            Some(cur) => Some(cur),
        };
        true
    }

    pub fn clear(&mut self) {
        self.unshuffled = None;
        self.tracks = Arc::default();
        self.index = None;
        self.hand = 0;
    }

    pub fn all_track_ids(&self) -> HashSet<String> {
        let mut ids: HashSet<String> = self.tracks.iter().map(|t| t.id.clone()).collect();
        if let Some(back) = &self.unshuffled {
            ids.extend(back.iter().map(|t| t.id.clone()));
        }
        ids
    }

    /// Drops all tracks whose id is in `ids`, keeping `index` and `hand` consistent when the current
    /// track survives, or clearing playback state (`current_was_removed = true`) like C# `RemoveTracksAsync`.
    /// Returns `(queue_changed, current_was_removed)`.
    pub fn remove_tracks_by_id(&mut self, ids: &HashSet<String>) -> (bool, bool) {
        if ids.is_empty() {
            return (false, false);
        }
        let had_in_tracks = self.tracks.iter().any(|t| ids.contains(&t.id));
        let had_in_back = self
            .unshuffled
            .as_ref()
            .is_some_and(|b| b.iter().any(|t| ids.contains(&t.id)));
        if !had_in_tracks && !had_in_back {
            return (false, false);
        }

        let cur_idx = self.index.filter(|&i| i < self.tracks.len());
        let current_removed = cur_idx
            .and_then(|i| self.tracks.get(i))
            .is_some_and(|t| ids.contains(&t.id));

        if let Some(back) = self.unshuffled.as_mut() {
            back.retain(|t| !ids.contains(&t.id));
        }

        if current_removed {
            Arc::make_mut(&mut self.tracks).retain(|t| !ids.contains(&t.id));
            self.index = None;
            self.hand = 0;
            return (true, true);
        }

        let mut removed_before_cur = 0usize;
        let mut removed_in_hand = 0usize;
        if let Some(ci) = cur_idx {
            for (i, t) in self.tracks.iter().enumerate() {
                if ids.contains(&t.id) {
                    if i < ci {
                        removed_before_cur += 1;
                    } else if i > ci && (i - ci - 1) < self.hand {
                        removed_in_hand += 1;
                    }
                }
            }
        }
        Arc::make_mut(&mut self.tracks).retain(|t| !ids.contains(&t.id));
        if self.tracks.is_empty() {
            self.index = None;
            self.hand = 0;
        } else if let Some(ci) = cur_idx {
            let nc = (ci - removed_before_cur).min(self.tracks.len() - 1);
            let upcoming = self.tracks.len() - nc - 1;
            self.index = Some(nc);
            self.hand = self.hand.saturating_sub(removed_in_hand).min(upcoming);
        }
        (true, false)
    }

    pub fn refresh_from_library(&mut self, track_map: &HashMap<&str, &Track>) {
        let fresh = |t: &mut Track| {
            if let Some(&f) = track_map.get(t.id.as_str()) {
                *t = f.clone();
            }
        };
        Arc::make_mut(&mut self.tracks).iter_mut().for_each(fresh);
        if let Some(back) = self.unshuffled.as_mut() {
            back.iter_mut().for_each(fresh);
        }
    }

    /// Restores a saved queue. A shuffled queue keeps its saved order; the queue from before
    /// comes from `original_queue_ids` when it was saved.
    pub fn restore_session(
        &mut self,
        track_map: &HashMap<&str, &Track>,
        state: &SessionState,
        queue_ids: &[String],
        original_queue_ids: Option<&[String]>,
    ) -> Option<Track> {
        let resolve = |ids: &[String]| -> Vec<Track> {
            ids.iter()
                .filter_map(|id| track_map.get(id.as_str()).map(|t| (*t).clone()))
                .collect()
        };
        self.shuffle = state.shuffle;
        self.repeat = RepeatMode::from_i32(state.repeat_mode);
        self.hand = 0;
        self.unshuffled = if state.shuffle {
            original_queue_ids
                .map(resolve)
                .filter(|orig| !orig.is_empty())
        } else {
            None
        };
        self.tracks = Arc::new(resolve(queue_ids));

        if self.tracks.is_empty() {
            self.index = None;
            return None;
        }

        let matched = state
            .current_id
            .as_deref()
            .and_then(|cid| self.tracks.iter().position(|t| t.id == cid))
            .or_else(|| {
                (state.index >= 0 && (state.index as usize) < self.tracks.len())
                    .then_some(state.index as usize)
            })
            .or(Some(0));
        self.index = matched;
        self.current().cloned()
    }

    /// The playback state alone (current track, position, shuffle, repeat), without the queue.
    pub fn session_state(&self, position_s: f64) -> SessionState {
        SessionState {
            current_id: self.current().map(|t| t.id.clone()),
            index: self.index.map(|i| i as i32).unwrap_or(-1),
            position: position_s.max(0.0),
            shuffle: self.shuffle,
            repeat_mode: self.repeat.as_i32(),
        }
    }

    /// `(state, play order ids, ids of the queue from before shuffle)`; the last is empty when
    /// there is none.
    pub fn to_session(&self, position_s: f64) -> (SessionState, Vec<String>, Vec<String>) {
        let state = self.session_state(position_s);
        let active_ids = self.tracks.iter().map(|t| t.id.clone()).collect();
        let original_ids = self
            .unshuffled
            .iter()
            .flatten()
            .map(|t| t.id.clone())
            .collect();
        (state, active_ids, original_ids)
    }
}

#[derive(Debug, Default)]
pub struct PlayTracker {
    track_id: Option<String>,
    heard_seconds: f64,
    last_pos: Option<f64>,
    recorded: bool,
}

impl PlayTracker {
    pub fn reset(&mut self, track_id: Option<String>) {
        self.reset_at(track_id, 0.0, 0.0);
    }

    pub fn reset_at(&mut self, track_id: Option<String>, start_pos: f64, initial_heard: f64) {
        self.track_id = track_id;
        self.heard_seconds = initial_heard.max(0.0);
        // Track playback from `start_pos` so resuming mid-track does not credit `0..start_pos`,
        // while a crossfade overlap can carry its initial heard credit forward.
        self.last_pos = Some(start_pos.max(0.0));
        self.recorded = false;
    }

    /// Listening time needed: the C# threshold less one poll gap, since the polls never land exactly
    /// on the end (short tracks have to be heard in full, which the summed poll deltas never reach).
    fn needed(duration: f64) -> f64 {
        let full = PlayStats::threshold(duration);
        (full - Self::POLL_SLACK_S).max(full * 0.5)
    }

    /// The longest position poll interval (power-saver tier).
    const POLL_SLACK_S: f64 = 1.0;

    /// Accumulates active listening time without letting seeks trigger spurious play counts.
    pub fn on_time_pos(&mut self, pos: f64, duration: f64, is_playing: bool) -> Option<String> {
        let id = self.track_id.as_ref()?;
        if !is_playing || self.recorded {
            self.last_pos = Some(pos);
            return None;
        }
        if let Some(prev) = self.last_pos {
            let delta = pos - prev;
            if delta > 0.0 && delta <= 2.5 {
                self.heard_seconds += delta;
            }
        }
        self.last_pos = Some(pos);
        if self.heard_seconds >= Self::needed(duration) {
            self.recorded = true;
            Some(id.clone())
        } else {
            None
        }
    }
}

#[cfg(test)]
#[path = "playback_tests.rs"]
mod tests;
