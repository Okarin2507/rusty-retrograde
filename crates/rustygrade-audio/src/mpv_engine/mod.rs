use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::{mpsc, oneshot, watch};

use crate::alsa_direct::{AlsaDirect, AudioFormat};
use crate::libmpv::{LibMpvClient, LibMpvError, MpvValue};

mod equalizer;
mod events;

pub use equalizer::EqualizerConfig;
use events::handle_raw_message;

#[derive(Debug, Clone)]
pub struct AudioPath {
    pub input: Option<AudioFormat>,
    pub output: Option<AudioFormat>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioDevice {
    pub name: String,
    pub description: String,
}

#[derive(Debug, Clone)]
pub enum EngineEvent {
    TimePos(f64),
    Duration(f64),
    Pause(bool),
    EndOfFile,
    PlaybackFailed { path: String, reason: String },
    Exited,
    AdvancedToNext(String),
    AudioPathChanged(AudioPath),
}

pub(super) struct SharedState {
    pub next_request_id: AtomicU64,
    pub load_request_id: AtomicU64,
    pub current_entry_id: AtomicI64,
    /// Playing = a file is loaded and mpv is not paused. Drives the time-pos poll.
    pub playing: AtomicBool,
    /// mpv's last reported `pause`.
    pub paused: AtomicBool,
    /// A file is loaded (`FileLoaded` seen, no `EndFile`/stop since). An idle mpv reports
    /// `pause=no` too; that is not playback.
    pub file_loaded: AtomicBool,
    pub current_path: Mutex<Option<String>>,
    pub pending_seek: Mutex<Option<f64>>,
    pub append_requests: Mutex<HashMap<u64, String>>,
    pub queued: Mutex<HashMap<i64, String>>,
    pub pending: Mutex<HashMap<u64, oneshot::Sender<MpvValue>>>,
    pub path_in: Mutex<Option<AudioFormat>>,
    pub path_out: Mutex<Option<AudioFormat>>,
    /// Bumped to cancel an end-of-file that is waiting to be reported (see `EOF_SETTLE`).
    pub eof_gen: AtomicU64,
}

impl SharedState {
    fn new() -> Self {
        Self {
            next_request_id: AtomicU64::new(2000),
            load_request_id: AtomicU64::new(0),
            current_entry_id: AtomicI64::new(-1),
            playing: AtomicBool::new(false),
            paused: AtomicBool::new(false),
            file_loaded: AtomicBool::new(false),
            current_path: Mutex::new(None),
            pending_seek: Mutex::new(None),
            append_requests: Mutex::new(HashMap::new()),
            queued: Mutex::new(HashMap::new()),
            pending: Mutex::new(HashMap::new()),
            path_in: Mutex::new(None),
            path_out: Mutex::new(None),
            eof_gen: AtomicU64::new(0),
        }
    }

    pub(super) fn reset_playback(&self) {
        self.eof_gen.fetch_add(1, Ordering::AcqRel);
        self.load_request_id.store(0, Ordering::Release);
        self.current_entry_id.store(-1, Ordering::Release);
        self.playing.store(false, Ordering::Release);
        self.paused.store(false, Ordering::Release);
        self.file_loaded.store(false, Ordering::Release);
        if let Ok(mut cp) = self.current_path.lock() {
            *cp = None;
        }
        if let Ok(mut ps) = self.pending_seek.lock() {
            *ps = None;
        }
        if let Ok(mut ar) = self.append_requests.lock() {
            ar.clear();
        }
        if let Ok(mut q) = self.queued.lock() {
            q.clear();
        }
        if let Ok(mut pi) = self.path_in.lock() {
            *pi = None;
        }
        if let Ok(mut po) = self.path_out.lock() {
            *po = None;
        }
    }

    fn reset_all(&self) {
        self.reset_playback();
        if let Ok(mut p) = self.pending.lock() {
            p.clear();
        }
    }
}

pub(super) const TIME_POS_REQUEST_ID: u64 = 1000;

/// With `keep-open`, a file whose audio output fails to open reports `eof-reached` right before its
/// `end-file` error. The end of file waits this long so a failure is not taken for the track ending
/// (which would step the queue on and lose the position).
pub(super) const EOF_SETTLE: Duration = Duration::from_millis(80);

/// mpv's text for a failed audio output (`MPV_ERROR_AO_INIT_FAILED`): the device, not the file.
pub const AO_INIT_FAILED: &str = "audio output initialization failed";

pub struct MpvEngine {
    pub(super) lib: Option<Arc<LibMpvClient>>,
    shared: Arc<SharedState>,
    audio_device: String,
    replay_gain: String,
    pub(super) audio_filters: String,
    poll_interval: Duration,
    poll_cfg: Option<watch::Sender<Option<Duration>>>,
    event_tx: mpsc::UnboundedSender<EngineEvent>,
}

impl MpvEngine {
    pub fn new(event_tx: mpsc::UnboundedSender<EngineEvent>) -> Self {
        Self {
            lib: None,
            shared: Arc::new(SharedState::new()),
            audio_device: "auto".to_owned(),
            replay_gain: "no".to_owned(),
            audio_filters: String::new(),
            poll_interval: Duration::from_millis(500),
            poll_cfg: None,
            event_tx,
        }
    }

    pub fn is_running(&self) -> bool {
        self.lib.as_ref().is_some_and(|l| l.is_alive())
    }

    pub fn ao_for(device: &str) -> &'static str {
        if device == "null" {
            "null"
        } else if AlsaDirect::is_direct(device) {
            "alsa"
        } else {
            // Works across PipeWire, PulseAudio, and pure-ALSA Linux setups.
            "pipewire,pulse,alsa"
        }
    }

    pub fn normalize_replay_gain(mode: &str) -> &'static str {
        match mode {
            "track" => "track",
            "album" => "album",
            _ => "no",
        }
    }

    pub fn set_poll_interval(&mut self, interval: Duration) {
        let clamped = interval.max(Duration::from_millis(100));
        self.poll_interval = clamped;
        if let Some(tx) = &self.poll_cfg {
            let _ = tx.send(Some(clamped));
        }
    }

    pub fn start(
        &mut self,
        audio_device: Option<&str>,
        replay_gain: Option<&str>,
    ) -> Result<(), LibMpvError> {
        if self.is_running() {
            return Ok(());
        }
        self.stop_internal();

        self.audio_device = audio_device
            .filter(|s| !s.is_empty())
            .unwrap_or("auto")
            .to_owned();
        self.replay_gain = Self::normalize_replay_gain(replay_gain.unwrap_or("no")).to_owned();

        let ao = Self::ao_for(&self.audio_device);
        // RETRO_MPV_LOG=<file>: mpv's own verbose log, for output problems.
        let log_file = std::env::var("RETRO_MPV_LOG")
            .ok()
            .filter(|s| !s.is_empty());
        let mut options: Vec<(&str, &str)> = vec![
            ("config", "no"),
            ("load-scripts", "no"),
            ("ytdl", "no"),
            ("osc", "no"),
            ("load-stats-overlay", "no"),
            ("load-auto-profiles", "no"),
            // `load-scripts=no` does not cover mpv's built-in Lua scripts; each runs its own
            // LuaJIT state and thread. Older libmpv builds lack some of these options, which
            // is fine: a failed option only logs a warning.
            ("load-console", "no"),
            // Older name of `load-console` (a deprecated alias in current mpv).
            ("load-osd-console", "no"),
            ("load-select", "no"),
            ("load-positioning", "no"),
            ("load-commands", "no"),
            ("load-context-menu", "no"),
            ("idle", "yes"),
            ("ao", ao),
            ("audio-device", &self.audio_device),
            ("audio-client-name", "Rustygrade"),
            ("input-media-keys", "no"),
            ("media-controls", "no"),
            ("video", "no"),
            ("vid", "no"),
            ("audio-display", "no"),
            ("force-window", "no"),
            ("keep-open", "yes"),
            ("gapless-audio", "weak"),
            ("prefetch-playlist", "yes"),
            ("replaygain", &self.replay_gain),
            ("af", &self.audio_filters),
            ("terminal", "no"),
            ("msg-level", "all=no"),
        ];
        if let Some(path) = log_file.as_deref() {
            options.push(("log-file", path));
            options.push(("msg-level", "all=v"));
        }

        let (raw_tx, mut raw_rx) = mpsc::unbounded_channel();
        let client = Arc::new(LibMpvClient::start(&options, raw_tx)?);
        client.observe(2, "pause");
        client.observe(3, "eof-reached");
        client.observe(4, "duration");
        client.observe(5, "audio-params");
        client.observe(6, "audio-out-params");

        self.lib = Some(Arc::clone(&client));

        // Event dispatcher task holds a Weak reference to break the Arc<LibMpvClient> cycle.
        let shared = Arc::clone(&self.shared);
        let out_tx = self.event_tx.clone();
        let lib_for_seek = Arc::downgrade(&client);
        tokio::spawn(async move {
            while let Some(msg) = raw_rx.recv().await {
                handle_raw_message(msg, &shared, &out_tx, &lib_for_seek);
            }
        });

        let (poll_tx, mut poll_rx) = watch::channel(Some(self.poll_interval));
        self.poll_cfg = Some(poll_tx);
        let shared_poll = Arc::clone(&self.shared);
        let lib_poll = Arc::clone(&client);
        tokio::spawn(async move {
            loop {
                let Some(wait) = *poll_rx.borrow_and_update() else {
                    break;
                };
                tokio::select! {
                    _ = tokio::time::sleep(wait) => {
                        if shared_poll.playing.load(Ordering::Acquire) {
                            lib_poll.get_property(TIME_POS_REQUEST_ID, "time-pos");
                        }
                    }
                    res = poll_rx.changed() => {
                        if res.is_err() {
                            break;
                        }
                    }
                }
            }
        });

        Ok(())
    }

    pub fn play(&self, file_path: &str, start_seconds: f64) {
        self.shared.current_entry_id.store(-1, Ordering::Release);
        // An end of file still settling belongs to the file being replaced.
        self.shared.eof_gen.fetch_add(1, Ordering::AcqRel);
        if let Ok(mut cp) = self.shared.current_path.lock() {
            *cp = Some(file_path.to_owned());
        }
        if !Path::new(file_path).is_absolute() {
            let _ = self.event_tx.send(EngineEvent::PlaybackFailed {
                path: file_path.to_owned(),
                reason: "not a local file".to_owned(),
            });
            return;
        }
        if let Ok(mut ar) = self.shared.append_requests.lock() {
            ar.clear();
        }
        if let Ok(mut q) = self.shared.queued.lock() {
            q.clear();
        }
        if let Ok(mut ps) = self.shared.pending_seek.lock() {
            *ps = (start_seconds.is_finite() && start_seconds > 0.0).then_some(start_seconds);
        }
        let id = self.shared.next_request_id.fetch_add(1, Ordering::Relaxed);
        self.shared.load_request_id.store(id, Ordering::Release);
        if let Some(lib) = &self.lib {
            lib.command(id, &["loadfile", file_path, "replace"]);
            lib.set_property("pause", "no");
        }
    }

    pub fn set_next(&self, path: Option<&str>) {
        if let Ok(mut ar) = self.shared.append_requests.lock() {
            ar.clear();
        }
        if let Some(lib) = &self.lib {
            lib.command(0, &["playlist-clear"]);
            let Some(p) = path else {
                return;
            };
            if !Path::new(p).is_absolute() {
                return;
            }
            if let Ok(mut q) = self.shared.queued.lock()
                && q.len() > 64
            {
                q.clear();
            }
            let id = self.shared.next_request_id.fetch_add(1, Ordering::Relaxed);
            if let Ok(mut ar) = self.shared.append_requests.lock() {
                ar.insert(id, p.to_owned());
            }
            lib.command(id, &["loadfile", p, "append"]);
        }
    }

    pub fn set_replay_gain(&mut self, mode: &str) {
        self.replay_gain = Self::normalize_replay_gain(mode).to_owned();
        if let Some(lib) = &self.lib {
            lib.set_property("replaygain", &self.replay_gain);
        }
    }

    pub fn set_paused(&self, paused: bool) {
        if let Some(lib) = &self.lib {
            lib.set_property("pause", if paused { "yes" } else { "no" });
        }
    }

    pub fn toggle_pause(&self) {
        if let Some(lib) = &self.lib {
            lib.command(0, &["cycle", "pause"]);
        }
    }

    pub fn seek(&self, seconds: f64) {
        if !seconds.is_finite() {
            return;
        }
        if let Some(lib) = &self.lib {
            let s = seconds.max(0.0).to_string();
            lib.command(0, &["seek", &s, "absolute"]);
            lib.get_property(TIME_POS_REQUEST_ID, "time-pos");
        }
    }

    pub fn set_volume(&self, volume01: f64) {
        if !volume01.is_finite() {
            return;
        }
        if let Some(lib) = &self.lib {
            let v = (volume01.clamp(0.0, 1.0) * 100.0).to_string();
            lib.set_property("volume", &v);
        }
    }

    pub fn set_mute(&self, muted: bool) {
        if let Some(lib) = &self.lib {
            lib.set_property("mute", if muted { "yes" } else { "no" });
        }
    }

    pub fn stop_playback(&self) {
        self.shared.reset_playback();
        if let Some(lib) = &self.lib {
            lib.command(0, &["stop"]);
        }
    }

    pub fn set_audio_filters(&mut self, filters: &str) {
        if self.audio_filters == filters {
            return;
        }
        self.audio_filters = filters.to_owned();
        if let Some(lib) = &self.lib {
            lib.set_property("af", filters);
        }
    }

    pub fn set_audio_device(&mut self, name: &str) {
        if self.audio_device == name {
            return;
        }
        self.audio_device = name.to_owned();
        if let Ok(mut out) = self.shared.path_out.lock() {
            *out = None;
        }
        if let Some(lib) = &self.lib {
            lib.set_property("ao", Self::ao_for(name));
            lib.set_property("audio-device", name);
            lib.command(0, &["ao-reload"]);
        }
    }

    pub fn reload_audio(&self) {
        if let Ok(mut out) = self.shared.path_out.lock() {
            *out = None;
        }
        if let Some(lib) = &self.lib {
            lib.command(0, &["ao-reload"]);
        }
    }

    async fn get_property_async(&self, name: &str) -> Option<MpvValue> {
        let lib = self.lib.as_ref()?;
        let id = self.shared.next_request_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        if let Ok(mut p) = self.shared.pending.lock() {
            p.insert(id, tx);
        }
        if !lib.get_property(id, name) {
            if let Ok(mut p) = self.shared.pending.lock() {
                p.remove(&id);
            }
            return None;
        }
        match tokio::time::timeout(Duration::from_secs(2), rx).await {
            Ok(Ok(val)) => Some(val),
            _ => {
                if let Ok(mut p) = self.shared.pending.lock() {
                    p.remove(&id);
                }
                None
            }
        }
    }

    /// Output devices reported by mpv for PipeWire / PulseAudio (`"auto"` included).
    pub async fn get_audio_devices(&self) -> Vec<AudioDevice> {
        let mut list = Vec::new();
        let Some(MpvValue::Array(arr)) = self.get_property_async("audio-device-list").await else {
            return list;
        };
        for item in arr {
            let Some(map) = item.as_map() else {
                continue;
            };
            let Some(name) = map.get("name").and_then(MpvValue::as_str) else {
                continue;
            };
            if name != "auto" && !name.starts_with("pipewire/") && !name.starts_with("pulse/") {
                continue;
            }
            let desc = map
                .get("description")
                .and_then(MpvValue::as_str)
                .filter(|s| !s.is_empty())
                .unwrap_or(name);
            list.push(AudioDevice {
                name: name.to_owned(),
                description: desc.to_owned(),
            });
        }
        list
    }

    /// Confirms that nothing inside mpv alters samples (`af`, `volume == 100`, `replaygain == no`, `speed == 1`).
    pub async fn get_sample_alteration(&self) -> Option<String> {
        let (af_val, vol_val, rg_val, speed_val) = tokio::join!(
            self.get_property_async("af"),
            self.get_property_async("volume"),
            self.get_property_async("replaygain"),
            self.get_property_async("speed"),
        );

        let Some(MpvValue::Array(af)) = af_val else {
            return Some("MPV DID NOT CONFIRM ITS SETTINGS".to_owned());
        };
        if !af.is_empty() {
            return Some("AN AUDIO FILTER IS ON".to_owned());
        }
        let Some(vol) = vol_val.and_then(|v| v.as_f64()) else {
            return Some("MPV DID NOT CONFIRM ITS SETTINGS".to_owned());
        };
        if (vol - 100.0).abs() > 0.01 {
            return Some("SOFTWARE VOLUME IS ON".to_owned());
        }
        match rg_val {
            Some(MpvValue::Bool(false)) => {}
            Some(MpvValue::String(s)) if s == "no" => {}
            Some(MpvValue::String(_)) => return Some("REPLAYGAIN IS ON".to_owned()),
            _ => return Some("MPV DID NOT CONFIRM ITS SETTINGS".to_owned()),
        }
        let Some(speed) = speed_val.and_then(|v| v.as_f64()) else {
            return Some("MPV DID NOT CONFIRM ITS SETTINGS".to_owned());
        };
        if (speed - 1.0).abs() > 0.0001 {
            return Some("PLAYBACK SPEED IS CHANGED".to_owned());
        }
        None
    }

    fn stop_poll_and_reset(&mut self) -> Option<Arc<LibMpvClient>> {
        if let Some(tx) = self.poll_cfg.take() {
            let _ = tx.send(None);
        }
        self.shared.reset_all();
        self.lib.take()
    }

    fn stop_internal(&mut self) {
        if let Some(lib) = self.stop_poll_and_reset() {
            lib.shutdown();
        }
    }

    pub fn shutdown(&mut self) {
        self.stop_internal();
    }

    pub async fn shutdown_async(&mut self) {
        if let Some(lib) = self.stop_poll_and_reset() {
            let _ = tokio::task::spawn_blocking(move || lib.shutdown()).await;
        }
    }
}

impl Drop for MpvEngine {
    fn drop(&mut self) {
        self.stop_internal();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn equalizer_builds_expected_lavfi_chain() {
        assert_eq!(EqualizerConfig::build_filters(false, false, &[5.0]), "");
        assert_eq!(EqualizerConfig::build_filters(true, true, &[5.0]), "");
        let chain = EqualizerConfig::build_filters(true, false, &[6.0, -0.0, -3.5]);
        assert!(
            chain.starts_with("@eqpre:lavfi=[volume=-6dB],@eq0:lavfi=[equalizer=f=32:t=o:w=1:g=6]")
        );
        assert!(chain.contains("@eq1:lavfi=[equalizer=f=64:t=o:w=1:g=0]"));
        assert!(chain.contains("@eq2:lavfi=[equalizer=f=125:t=o:w=1:g=-3.5]"));
    }

    #[test]
    fn ao_selection_matches_device_type() {
        assert_eq!(MpvEngine::ao_for("null"), "null");
        assert_eq!(MpvEngine::ao_for("alsa/hw:CARD=Pro,DEV=0"), "alsa");
        assert_eq!(MpvEngine::ao_for("auto"), "pipewire,pulse,alsa");
        assert_eq!(MpvEngine::ao_for("pipewire/foo"), "pipewire,pulse,alsa");
    }
}
