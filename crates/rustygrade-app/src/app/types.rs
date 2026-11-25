use std::fmt;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use rustygrade_core::{
    Album, DirectVolumeBridge, DisplayBackend, ExtraMap, GpuPreference, PerformanceMode, SmartRule,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GpuOption(pub GpuPreference);

impl fmt::Display for GpuOption {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            GpuPreference::Igpu => f.write_str("iGPU Locked (Hybrid Power Saver)"),
            GpuPreference::Auto => f.write_str("Auto (System Default)"),
            GpuPreference::Dgpu => f.write_str("dGPU (High Performance)"),
            GpuPreference::Software => f.write_str("Software CPU (tiny-skia)"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DisplayOption(pub DisplayBackend);

impl fmt::Display for DisplayOption {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            DisplayBackend::Auto => f.write_str("Auto (Wayland / X11)"),
            DisplayBackend::Wayland => f.write_str("Wayland Native"),
            DisplayBackend::X11 => f.write_str("X11 / XWayland"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PerfOption(pub PerformanceMode);

impl fmt::Display for PerfOption {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            PerformanceMode::Auto => f.write_str("Auto (Follow Power Profile)"),
            PerformanceMode::Performance => f.write_str("Performance (60 FPS)"),
            PerformanceMode::Balanced => f.write_str("Balanced (60 FPS)"),
            PerformanceMode::PowerSaver => f.write_str("Power Saver (Animations Off)"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BridgeOption(pub DirectVolumeBridge);

impl fmt::Display for BridgeOption {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            DirectVolumeBridge::VirtualSink => {
                f.write_str("PipeWire Virtual Bridge (takes system slider / default sink)")
            }
            DirectVolumeBridge::HardwareOnly => f.write_str("ALSA Hardware Mixer Only"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplayGainChoice {
    Off,
    Track,
    Album,
}

impl ReplayGainChoice {
    pub const ALL: [Self; 3] = [Self::Off, Self::Track, Self::Album];

    pub fn parse(s: &str) -> Self {
        match s {
            "track" => Self::Track,
            "album" => Self::Album,
            _ => Self::Off,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Track => "track",
            Self::Album => "album",
        }
    }
}

impl fmt::Display for ReplayGainChoice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Track => f.write_str("Track Gain"),
            Self::Album => f.write_str("Album Gain"),
            Self::Off => f.write_str("Off (Bit-Perfect)"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SmartFieldOption {
    pub key: &'static str,
    pub label: &'static str,
}

impl fmt::Display for SmartFieldOption {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SmartOpOption {
    pub op: &'static str,
    pub label: &'static str,
}

impl fmt::Display for SmartOpOption {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SmartSortOption {
    pub key: &'static str,
    pub label: &'static str,
}

impl fmt::Display for SmartSortOption {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Library,
    NowPlaying,
    Playlists,
    Search,
    Settings,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchTab {
    All,
    Tracks,
    Albums,
    Artists,
    Playlists,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LibraryMode {
    Albums,
    Artists,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaylistView {
    Liked,
    Custom(i64),
    Smart(i64),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AlbumKey {
    pub name: String,
    pub artist: String,
}

impl AlbumKey {
    pub fn from_album(album: &Album) -> Self {
        Self {
            name: album.name.clone(),
            artist: album.artist.clone(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TagField {
    Title,
    Artist,
    Album,
    AlbumArtist,
    Year,
    Track,
    Disc,
}

#[derive(Debug, Clone)]
pub struct TagDraft {
    pub is_track: bool,
    pub paths: Vec<PathBuf>,
    pub album_key: Option<String>,
    pub target_label: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub album_artist: String,
    pub year: String,
    pub track: String,
    pub disc: String,
    pub orig_title: String,
    pub orig_artist: String,
    pub orig_album: String,
    pub orig_album_artist: String,
    pub orig_year: String,
    pub orig_track: String,
    pub orig_disc: String,
    pub cover_preview: Option<PathBuf>,
    pub new_cover: Option<PathBuf>,
    pub remove_cover: bool,
    pub saving: bool,
    /// The artist a cover lookup falls back to when the editor has no artist field filled in
    /// (an album edit with ALBUM ARTIST empty).
    pub query_artist: String,
    /// The lookup in flight, by request number (a late answer for another one is dropped).
    pub fetching: Option<u64>,
    /// Where the fetched cover came from (`ITUNES`, `COVER ART ARCHIVE`).
    pub cover_source: Option<&'static str>,
}

#[derive(Debug, Clone)]
pub struct SmartDraft {
    pub id: i64,
    pub name: String,
    pub match_all: bool,
    pub rules: Vec<SmartRule>,
    pub sort: String,
    pub limit: String,
    pub extra: ExtraMap,
    pub match_count: usize,
}

/// Tracks whether the compositor is still delivering `RedrawRequested` frames to an unfocused
/// open main window. After two consecutive playback ticks (>= 500 ms apart) with no presented
/// frame, requests arming `WAKE_ON_PRESENT` and pausing periodic ticks until a frame presents.
#[derive(Debug, Default)]
pub(crate) struct PresentWatchdog {
    suspended: bool,
    unpresented_ticks: u8,
    last_present_seq: u64,
    last_check_at: Option<Instant>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WatchdogAction {
    None,
    ArmAndSuspend { expected_seq: u64 },
}

impl PresentWatchdog {
    pub fn is_suspended(&self) -> bool {
        self.suspended
    }

    pub fn reset(&mut self) {
        self.suspended = false;
        self.unpresented_ticks = 0;
        self.last_check_at = None;
    }

    pub fn on_unfocus(&mut self, seq: u64, now: Instant) {
        self.suspended = false;
        self.unpresented_ticks = 0;
        self.last_present_seq = seq;
        self.last_check_at = Some(now);
    }

    pub fn on_tick(&mut self, seq: u64, now: Instant) -> WatchdogAction {
        if self.suspended {
            return WatchdogAction::None;
        }
        if seq != self.last_present_seq {
            self.last_present_seq = seq;
            self.unpresented_ticks = 0;
            self.last_check_at = Some(now);
            return WatchdogAction::None;
        }
        if self
            .last_check_at
            .is_none_or(|t| now.duration_since(t) >= Duration::from_millis(500))
        {
            self.last_check_at = Some(now);
            self.unpresented_ticks = self.unpresented_ticks.saturating_add(1);
            if self.unpresented_ticks >= 2 {
                return WatchdogAction::ArmAndSuspend { expected_seq: seq };
            }
        }
        WatchdogAction::None
    }

    pub fn confirm_suspend(&mut self, observed_after_arm: u64, expected_seq: u64) -> bool {
        if observed_after_arm != expected_seq {
            self.last_present_seq = observed_after_arm;
            self.unpresented_ticks = 0;
            self.suspended = false;
            false
        } else {
            self.suspended = true;
            true
        }
    }

    pub fn on_presented(&mut self, seq: u64, now: Instant) {
        self.suspended = false;
        self.unpresented_ticks = 0;
        self.last_present_seq = seq;
        self.last_check_at = Some(now);
    }
}
