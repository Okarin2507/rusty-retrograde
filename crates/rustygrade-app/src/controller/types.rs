use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;

use rustygrade_audio::{AudioPath, DirectDevice};
use rustygrade_core::{
    Album, AppSettings, CollectionsData, CoverPalette, LyricsDoc, PlayStats, SmartPlaylistData,
    TagEdit, Track,
};
use rustygrade_linux::{DiscordRpcStatus, PowerProfile, RenderTier, SingleInstanceCommand};
use tokio::sync::mpsc;

use crate::playback::RepeatMode;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceOption {
    pub id: String,
    pub label: String,
    pub is_direct: bool,
}

impl DeviceOption {
    pub(super) fn direct(d: DirectDevice) -> Self {
        Self {
            id: d.name,
            label: d.description,
            is_direct: true,
        }
    }
}

impl fmt::Display for DeviceOption {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.label)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowAction {
    Raise,
    Toggle,
    /// Tray "Mini player": switch between the main window and the mini player.
    ToggleMiniPlayer,
    Quit,
}

/// How a [`ControllerEvent::Notice`] is shown (C# `ToastKind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoticeKind {
    Info,
    Success,
    Warn,
    Error,
}

/// How often `Coordinator::emit_playback_tick` forwards periodic `TimePos` updates to the UI.
/// Explicit state changes (`emit_playback`: play/pause, seek, new track, duration change)
/// always go through regardless of mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaybackTickMode {
    /// Focused main window or open mini-player: every new second and at least every 250 ms.
    Active,
    /// Visible but unfocused main window (tiled side-by-side or on another monitor):
    /// only when the displayed whole second changes (1 Hz).
    Unfocused,
    /// Hidden to tray, minimized, or compositor-suspended: suppress periodic ticks until resumed.
    Paused,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaylistExportTarget {
    Liked,
    Custom(i64),
    Smart(i64),
}

#[derive(Debug, Clone)]
pub enum ControllerCommand {
    SetPlaybackTickMode(PlaybackTickMode),
    PlayTrackList {
        tracks: Vec<Track>,
        start_index: Option<usize>,
    },
    JumpToQueueIndex(usize),
    PlayNextTracks(Vec<Track>),
    EnqueueTracks(Vec<Track>),
    RemoveQueueIndex(usize),
    ClearQueue,
    TogglePlayPause,
    Next,
    Previous,
    ToggleShuffle,
    CycleRepeat,
    SeekSeconds(f64),
    SetVolume(f64),
    ToggleMute,
    SelectAudioDevice(String),
    RefreshDevices,
    UpdateSettings(AppSettings),
    SetEqEnabled(bool),
    SelectEqPreset(String),
    SetEqBand {
        band: usize,
        gain: f64,
    },
    SetReplayGain(String),
    SetCrossfadeSeconds(i32),
    ToggleLike(String),
    ToggleLikeTracks(Vec<String>),
    CreatePlaylist(String),
    DeletePlaylist(i64),
    RenamePlaylist {
        id: i64,
        name: String,
    },
    AddTracksToPlaylist {
        playlist_id: i64,
        track_ids: Vec<String>,
    },
    TogglePlaylistTracks {
        playlist_id: i64,
        track_ids: Vec<String>,
    },
    RemoveTrackFromPlaylist {
        playlist_id: i64,
        track_id: String,
    },
    /// A track row was dragged on a playlist page: `track_ids` is the list as shown.
    SetPlaylistOrder {
        playlist_id: i64,
        track_ids: Vec<String>,
    },
    SetLikedOrder(Vec<String>),
    /// A track row was dragged on an album without track numbers.
    SetAlbumOrder {
        name: String,
        artist: String,
        track_ids: Vec<String>,
    },
    SetPlaylistCover {
        playlist_id: i64,
        image_path: PathBuf,
    },
    RemovePlaylistCover(i64),
    SaveSmartPlaylist(SmartPlaylistData),
    DeleteSmartPlaylist(i64),
    ExportPlaylistM3u8 {
        target: PlaylistExportTarget,
        dest: PathBuf,
    },
    ExportPlaylistZip {
        playlist_id: i64,
        dest: PathBuf,
    },
    ExportCollectionsZip {
        dest: PathBuf,
    },
    ImportPlaylistM3u {
        path: PathBuf,
    },
    ImportCollectionsZip {
        path: PathBuf,
        replace: bool,
    },
    DeleteAlbums(Vec<(String, String)>),
    ScanFolders(Vec<PathBuf>),
    ScanFiles(Vec<PathBuf>),
    ImportPaths(Vec<PathBuf>),
    RemoveLibraryFolder(String),
    /// F5 / tray: rescans every library root without dropping anything; tracks whose files are
    /// gone are reported with [`ControllerEvent::MissingTracks`].
    RescanLibrary,
    /// Settings: drop every cached album cover and extract them again from the files.
    ResyncCovers,
    /// User confirmed [`ControllerEvent::MissingTracks`]: scan `roots` and drop missing tracks.
    PruneMissing {
        roots: Vec<PathBuf>,
    },
    SaveTags {
        paths: Vec<PathBuf>,
        album_key: Option<String>,
        edit: TagEdit,
    },
    Shutdown,
}

#[derive(Debug, Clone)]
pub enum ControllerEvent {
    LibraryLoaded(Arc<[Album]>),
    CollectionsUpdated(CollectionsData),
    StatsUpdated(PlayStats),
    QueueUpdated {
        /// Shared with the controller's queue: a track change sends the same list again for free.
        tracks: Arc<Vec<Track>>,
        index: Option<usize>,
        shuffle: bool,
        repeat: RepeatMode,
        /// How many tracks right after the current one were queued by hand.
        hand_len: usize,
    },
    DevicesUpdated(Vec<DeviceOption>),
    TrackChanged(Option<Track>),
    AccentPaletteChanged(Option<CoverPalette>),
    LyricsUpdated {
        track_id: Option<String>,
        lyrics: Option<LyricsDoc>,
    },
    PlaybackState {
        is_playing: bool,
        position_s: f64,
        duration_s: f64,
    },
    VolumeState {
        volume: f64,
        muted: bool,
    },
    DirectStatus {
        audio_device: String,
        bit_perfect_status: String,
        direct_active: bool,
        direct_volume_hint: String,
        audio_path: AudioPath,
        pw_bridge_node_id: Option<u32>,
        direct_mixer_control: Option<String>,
    },
    PowerTierChanged {
        profile: PowerProfile,
        tier: RenderTier,
    },
    SettingsSaved(AppSettings),
    Notice {
        kind: NoticeKind,
        text: String,
    },
    ScanProgress {
        phase: &'static str,
        scanned: usize,
        total: usize,
    },
    ScanFinished,
    MissingTracks {
        count: usize,
        roots: Vec<PathBuf>,
    },
    TagsSaved {
        first_path: Option<PathBuf>,
        written: usize,
        failed: Vec<String>,
    },
    CoversInvalidated(Vec<PathBuf>),
    /// RESYNC COVERS finished: only images already loaded should reload pixels.
    CoversResynced(Vec<PathBuf>),
    AlbumsDeleteFinished,
    TrayAvailable(bool),
    DiscordRpcStatus(DiscordRpcStatus),
    WindowCommand(WindowAction),
    ShutdownComplete,
}

pub struct StartupContext {
    pub settings: AppSettings,
    pub single_instance_rx: Option<mpsc::UnboundedReceiver<SingleInstanceCommand>>,
    /// Paths / `file://` URIs given on the primary instance's own command line ("Open with").
    pub open_uris: Vec<String>,
}
