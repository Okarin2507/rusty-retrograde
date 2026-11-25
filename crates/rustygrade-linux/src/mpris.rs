use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, RwLock};

use tokio::sync::mpsc;
use tracing::{debug, warn};
use zbus::object_server::SignalEmitter;
use zbus::{ObjectPath, OwnedObjectPath, OwnedVariant, Variant};

#[derive(Debug, Clone, PartialEq)]
pub struct MprisState {
    pub playback_status: String,
    pub track_id: Option<String>,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub art_url: Option<String>,
    pub length_us: i64,
    pub position_us: i64,
    pub volume: f64,
    pub can_go_next: bool,
    pub can_go_previous: bool,
    pub can_play: bool,
    pub can_pause: bool,
    pub can_seek: bool,
    pub loop_status: String,
    pub shuffle: bool,
}

impl Default for MprisState {
    fn default() -> Self {
        Self {
            playback_status: "Stopped".to_owned(),
            track_id: None,
            title: None,
            artist: None,
            album: None,
            art_url: None,
            length_us: 0,
            position_us: 0,
            volume: 0.5,
            can_go_next: true,
            can_go_previous: true,
            can_play: true,
            can_pause: true,
            can_seek: true,
            loop_status: "None".to_owned(),
            shuffle: false,
        }
    }
}

/// A valid, unique MPRIS track id for a track id (its file path): a hash under the app's own
/// namespace, since the spec reserves `/org/mpris` (C# used `/org/retrograde/track/...` too).
pub fn track_object_path(track_id: &str) -> String {
    let hash = crate::fnv1a_64(track_id.as_bytes());
    format!("/org/rustygrade/track/t{hash:016x}")
}

/// Positions further apart than this are a jump (seek, restart) and get a `Seeked` signal.
const SEEK_JUMP_US: u64 = 2_000_000;

impl MprisState {
    fn metadata_map(&self) -> HashMap<String, OwnedVariant> {
        let mut map = HashMap::new();
        let Some(id) = self.track_id.as_deref() else {
            return map;
        };
        let obj_str = track_object_path(id);
        if let Ok(path) = ObjectPath::try_from(obj_str.as_str())
            && let Ok(val) = OwnedVariant::try_from(Variant::from(path))
        {
            map.insert("mpris:trackid".to_owned(), val);
        }
        if let Some(title) = self.title.as_deref()
            && !title.is_empty()
            && let Ok(val) = OwnedVariant::try_from(Variant::from(title))
        {
            map.insert("xesam:title".to_owned(), val);
        }
        if let Some(artist) = self.artist.as_deref()
            && !artist.is_empty()
            && let Ok(val) = OwnedVariant::try_from(Variant::from(vec![artist.to_owned()]))
        {
            map.insert("xesam:artist".to_owned(), val);
        }
        if let Some(album) = self.album.as_deref()
            && !album.is_empty()
            && let Ok(val) = OwnedVariant::try_from(Variant::from(album))
        {
            map.insert("xesam:album".to_owned(), val);
        }
        if let Some(art_url) = self.art_url.as_deref()
            && !art_url.is_empty()
            && let Ok(val) = OwnedVariant::try_from(Variant::from(art_url))
        {
            map.insert("mpris:artUrl".to_owned(), val);
        }
        if self.length_us > 0
            && let Ok(val) = OwnedVariant::try_from(Variant::from(self.length_us))
        {
            map.insert("mpris:length".to_owned(), val);
        }
        map
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum MprisCommand {
    Play,
    Pause,
    PlayPause,
    Stop,
    Next,
    Previous,
    SeekOffsetUs(i64),
    SetPositionUs(i64),
    SetVolume(f64),
    SetLoopStatus(String),
    SetShuffle(bool),
    Raise,
    Quit,
}

struct MediaPlayer2Root {
    cmd_tx: mpsc::UnboundedSender<MprisCommand>,
}

#[zbus::interface(name = "org.mpris.MediaPlayer2")]
impl MediaPlayer2Root {
    #[zbus(property)]
    async fn can_quit(&self) -> bool {
        true
    }

    #[zbus(property)]
    async fn can_raise(&self) -> bool {
        true
    }

    #[zbus(property)]
    async fn has_track_list(&self) -> bool {
        false
    }

    #[zbus(property)]
    async fn identity(&self) -> &str {
        "Rustygrade"
    }

    #[zbus(property)]
    async fn desktop_entry(&self) -> &str {
        "rustygrade"
    }

    #[zbus(property)]
    async fn supported_uri_schemes(&self) -> Vec<String> {
        vec!["file".to_owned()]
    }

    #[zbus(property)]
    async fn supported_mime_types(&self) -> Vec<String> {
        vec![
            "audio/flac".to_owned(),
            "audio/mpeg".to_owned(),
            "audio/ogg".to_owned(),
            "audio/mp4".to_owned(),
            "audio/wav".to_owned(),
            "audio/x-wav".to_owned(),
            "audio/opus".to_owned(),
        ]
    }

    async fn raise(&self) {
        let _ = self.cmd_tx.send(MprisCommand::Raise);
    }

    async fn quit(&self) {
        let _ = self.cmd_tx.send(MprisCommand::Quit);
    }
}

struct MediaPlayer2Player {
    state: Arc<RwLock<MprisState>>,
    /// Live playback position, updated on every poll without a PropertiesChanged signal
    /// (clients read Position on demand, as the spec intends).
    position_us: Arc<AtomicI64>,
    cmd_tx: mpsc::UnboundedSender<MprisCommand>,
}

impl MediaPlayer2Player {
    fn read_with<R>(&self, f: impl FnOnce(&MprisState) -> R) -> R {
        match self.state.read() {
            Ok(g) => f(&g),
            Err(p) => f(&p.into_inner()),
        }
    }
}

#[zbus::interface(name = "org.mpris.MediaPlayer2.Player")]
impl MediaPlayer2Player {
    #[zbus(property)]
    async fn playback_status(&self) -> String {
        self.read_with(|s| s.playback_status.clone())
    }

    #[zbus(property)]
    async fn loop_status(&self) -> String {
        self.read_with(|s| s.loop_status.clone())
    }

    #[zbus(property)]
    async fn set_loop_status(&self, status: String) {
        let _ = self.cmd_tx.send(MprisCommand::SetLoopStatus(status));
    }

    #[zbus(property)]
    async fn rate(&self) -> f64 {
        1.0
    }

    #[zbus(property)]
    async fn set_rate(&self, _rate: f64) {}

    #[zbus(property)]
    async fn shuffle(&self) -> bool {
        self.read_with(|s| s.shuffle)
    }

    #[zbus(property)]
    async fn set_shuffle(&self, shuffle: bool) {
        let _ = self.cmd_tx.send(MprisCommand::SetShuffle(shuffle));
    }

    #[zbus(property)]
    async fn metadata(&self) -> HashMap<String, OwnedVariant> {
        self.read_with(MprisState::metadata_map)
    }

    #[zbus(property)]
    async fn volume(&self) -> f64 {
        self.read_with(|s| s.volume)
    }

    #[zbus(property)]
    async fn set_volume(&self, volume: f64) {
        if !volume.is_finite() {
            return;
        }
        let _ = self
            .cmd_tx
            .send(MprisCommand::SetVolume(volume.clamp(0.0, 1.0)));
    }

    #[zbus(property)]
    async fn position(&self) -> i64 {
        self.position_us.load(Ordering::Relaxed)
    }

    #[zbus(property)]
    async fn minimum_rate(&self) -> f64 {
        1.0
    }

    #[zbus(property)]
    async fn maximum_rate(&self) -> f64 {
        1.0
    }

    #[zbus(property)]
    async fn can_go_next(&self) -> bool {
        self.read_with(|s| s.can_go_next)
    }

    #[zbus(property)]
    async fn can_go_previous(&self) -> bool {
        self.read_with(|s| s.can_go_previous)
    }

    #[zbus(property)]
    async fn can_play(&self) -> bool {
        self.read_with(|s| s.can_play)
    }

    #[zbus(property)]
    async fn can_pause(&self) -> bool {
        self.read_with(|s| s.can_pause)
    }

    #[zbus(property)]
    async fn can_seek(&self) -> bool {
        self.read_with(|s| s.can_seek)
    }

    #[zbus(property)]
    async fn can_control(&self) -> bool {
        true
    }

    async fn next(&self) {
        let _ = self.cmd_tx.send(MprisCommand::Next);
    }

    async fn previous(&self) {
        let _ = self.cmd_tx.send(MprisCommand::Previous);
    }

    async fn pause(&self) {
        let _ = self.cmd_tx.send(MprisCommand::Pause);
    }

    async fn play_pause(&self) {
        let _ = self.cmd_tx.send(MprisCommand::PlayPause);
    }

    async fn stop(&self) {
        let _ = self.cmd_tx.send(MprisCommand::Stop);
    }

    async fn play(&self) {
        let _ = self.cmd_tx.send(MprisCommand::Play);
    }

    async fn seek(&self, offset: i64) {
        let _ = self.cmd_tx.send(MprisCommand::SeekOffsetUs(offset));
    }

    async fn set_position(&self, track_id: OwnedObjectPath, position: i64) {
        if !position_matches_track(&self.state, track_id.as_str(), position) {
            return;
        }
        let _ = self.cmd_tx.send(MprisCommand::SetPositionUs(position));
    }

    async fn open_uri(&self, _uri: String) {}

    #[zbus(signal)]
    async fn seeked(emitter: &SignalEmitter<'_>, position: i64) -> zbus::Result<()>;
}

fn position_matches_track(state: &RwLock<MprisState>, obj_path: &str, position_us: i64) -> bool {
    if position_us < 0 {
        return false;
    }
    let guard = state.read().unwrap_or_else(|p| p.into_inner());
    let Some(id) = guard.track_id.as_deref() else {
        return false;
    };
    if track_object_path(id) != obj_path {
        return false;
    }
    guard.length_us <= 0 || position_us <= guard.length_us
}

struct StateDiff {
    playback_status: bool,
    metadata: bool,
    volume: bool,
    loop_status: bool,
    shuffle: bool,
    can_go_next: bool,
    can_go_previous: bool,
    can_play: bool,
    can_pause: bool,
    can_seek: bool,
}

impl StateDiff {
    fn between(prev: &MprisState, next: &MprisState) -> Self {
        Self {
            playback_status: prev.playback_status != next.playback_status,
            metadata: prev.track_id != next.track_id
                || prev.title != next.title
                || prev.artist != next.artist
                || prev.album != next.album
                || prev.art_url != next.art_url
                || prev.length_us != next.length_us,
            volume: (prev.volume - next.volume).abs() > 0.002,
            loop_status: prev.loop_status != next.loop_status,
            shuffle: prev.shuffle != next.shuffle,
            can_go_next: prev.can_go_next != next.can_go_next,
            can_go_previous: prev.can_go_previous != next.can_go_previous,
            can_play: prev.can_play != next.can_play,
            can_pause: prev.can_pause != next.can_pause,
            can_seek: prev.can_seek != next.can_seek,
        }
    }
}

const MPRIS_PATH: &str = "/org/mpris/MediaPlayer2";
const MPRIS_BUS_NAME: &str = "org.mpris.MediaPlayer2.rustygrade";

pub struct MprisService {
    conn: zbus::Connection,
    state: Arc<RwLock<MprisState>>,
    position_us: Arc<AtomicI64>,
}

impl MprisService {
    pub async fn start() -> Result<(Self, mpsc::UnboundedReceiver<MprisCommand>), String> {
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let state = Arc::new(RwLock::new(MprisState::default()));
        let position_us = Arc::new(AtomicI64::new(0));

        let root = MediaPlayer2Root {
            cmd_tx: cmd_tx.clone(),
        };
        let player = MediaPlayer2Player {
            state: Arc::clone(&state),
            position_us: Arc::clone(&position_us),
            cmd_tx,
        };

        let conn = zbus::connection::Builder::session()
            .name(MPRIS_BUS_NAME)
            .serve_at(MPRIS_PATH, root)
            .serve_at(MPRIS_PATH, player)
            .build()
            .await
            .map_err(|e| format!("MPRIS D-Bus connection failed: {e}"))?;

        debug!(bus_name = MPRIS_BUS_NAME, "MPRIS service started");
        Ok((
            Self {
                conn,
                state,
                position_us,
            },
            cmd_rx,
        ))
    }

    /// Updates the live position. Emits `Seeked` only when it jumped by more than 2 s
    /// during active playback; ordinary progress and non-playing state are silent.
    pub async fn update_position(&self, position_us: i64) {
        let pos = position_us.max(0);
        let prev = self.position_us.swap(pos, Ordering::Relaxed);
        let is_playing = self
            .state
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .playback_status
            == "Playing";
        if is_playing && prev >= 0 && pos.abs_diff(prev) > SEEK_JUMP_US {
            self.emit_seeked(pos).await;
        }
    }

    /// An explicit seek: stores the position and always emits `Seeked`.
    pub async fn seeked_to(&self, position_us: i64) {
        let pos = position_us.max(0);
        self.position_us.store(pos, Ordering::Relaxed);
        self.emit_seeked(pos).await;
    }

    pub async fn update_state(&self, next: MprisState) {
        self.position_us
            .store(next.position_us.max(0), Ordering::Relaxed);
        let diff = {
            let mut guard = match self.state.write() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            let prev = std::mem::replace(&mut *guard, next);
            StateDiff::between(&prev, &guard)
        };

        let Ok(iface) = self
            .conn
            .object_server()
            .interface::<_, MediaPlayer2Player>(MPRIS_PATH)
            .await
        else {
            return;
        };
        let emitter = iface.signal_emitter();
        let player = iface.get().await;

        if diff.playback_status
            && let Err(e) = player.playback_status_changed(emitter).await
        {
            warn!(error = %e, "Failed emitting MPRIS PlaybackStatus change");
        }
        if diff.metadata {
            let _ = player.metadata_changed(emitter).await;
        }
        if diff.volume {
            let _ = player.volume_changed(emitter).await;
        }
        if diff.loop_status {
            let _ = player.loop_status_changed(emitter).await;
        }
        if diff.shuffle {
            let _ = player.shuffle_changed(emitter).await;
        }
        if diff.can_go_next {
            let _ = player.can_go_next_changed(emitter).await;
        }
        if diff.can_go_previous {
            let _ = player.can_go_previous_changed(emitter).await;
        }
        if diff.can_play {
            let _ = player.can_play_changed(emitter).await;
        }
        if diff.can_pause {
            let _ = player.can_pause_changed(emitter).await;
        }
        if diff.can_seek {
            let _ = player.can_seek_changed(emitter).await;
        }
    }

    pub async fn emit_seeked(&self, position_us: i64) {
        if let Ok(iface) = self
            .conn
            .object_server()
            .interface::<_, MediaPlayer2Player>(MPRIS_PATH)
            .await
        {
            let _ = MediaPlayer2Player::seeked(iface.signal_emitter(), position_us.max(0)).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn trackid_of(id: &str) -> String {
        let state = MprisState {
            track_id: Some(id.to_owned()),
            ..MprisState::default()
        };
        let map = state.metadata_map();
        let val = map.get("mpris:trackid").expect("trackid present");
        let path = ObjectPath::try_from(val.clone()).expect("trackid is an object path");
        path.as_str().to_owned()
    }

    #[test]
    fn trackid_and_set_position_guards() {
        let ids = [
            "/music/Ado/01 唱.flac",
            "/music/Ado/01 踊.flac",
            "/music/Ado/01 __.flac",
            "/music/Ado/01 ..flac",
            "",
            "/music/a b/c#d?.flac",
        ];
        let mut seen = std::collections::HashSet::new();
        for id in ids {
            let path = trackid_of(id);
            assert!(path.starts_with("/org/rustygrade/track/"), "{path}");
            assert!(!path.starts_with("/org/mpris"));
            assert!(ObjectPath::try_from(path.as_str()).is_ok());
            assert_eq!(path, track_object_path(id), "stable across calls");
            assert!(seen.insert(path), "duplicate trackid for {id:?}");
        }
        assert!(MprisState::default().metadata_map().is_empty());

        let state = RwLock::new(MprisState {
            track_id: Some("/music/a.flac".to_owned()),
            length_us: 10_000_000,
            ..MprisState::default()
        });
        let valid_obj = track_object_path("/music/a.flac");
        let wrong_obj = track_object_path("/music/b.flac");
        assert!(position_matches_track(&state, &valid_obj, 5_000_000));
        assert!(!position_matches_track(&state, &valid_obj, -1));
        assert!(!position_matches_track(&state, &valid_obj, 10_000_001));
        assert!(!position_matches_track(&state, &wrong_obj, 5_000_000));
    }
}
