mod controller_events;
pub mod message;
mod shortcuts;
pub mod types;
mod update_collections;
mod update_playback;
mod update_settings;
mod update_tags;
mod window;

pub use message::Message;
pub use types::*;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use iced::futures::{SinkExt, Stream};
use iced::widget::{Id, operation};
use iced::{Element, Size, Subscription, Task, Theme, event};
use rustygrade_audio::AudioPath;
use rustygrade_core::{
    Album, AppSettings, ArtistSummary, CollectionsData, CoverPaths, LyricsDoc, PlayStats, Track,
};
use rustygrade_linux::{
    DiscordRpcStatus, GraphicsPolicyReport, PowerProfile, RenderTier, SingleInstance,
    SingleInstanceOutcome, apply_startup_graphics_policy,
};
use tokio::sync::mpsc;

use crate::context_menu;
use crate::controller::{
    ControllerCommand, ControllerEvent, DeviceOption, PlaybackTickMode, StartupContext,
    run_coordinator,
};
use crate::cover_fetch;
use crate::covers_ui::CoverCache;
use crate::devshot;
use crate::dialogs;
use crate::egg;
use crate::fullscreen;
use crate::library_view::{LibrarySort, artist_key};
use crate::motion;
use crate::pages;
use crate::playback::RepeatMode;
use crate::theme::{self, AccentAnimator};
use crate::toasts;
use crate::views;

use window::{normalize_open_arg, open_main_window};

pub(super) struct BootstrapHandoff {
    pub cmd_rx: mpsc::UnboundedReceiver<ControllerCommand>,
    pub startup: StartupContext,
}

static BOOTSTRAP_HANDOFF: OnceLock<Mutex<Option<BootstrapHandoff>>> = OnceLock::new();
/// Incremented on every presented frame of the main window (`window::Event::RedrawRequested`),
/// used to detect when a Wayland compositor suspends frame callbacks on another workspace.
static PRESENT_SEQ: AtomicU64 = AtomicU64::new(0);
/// Armed only while `App` has paused ticks due to compositor suspension, so the very next
/// main-window `RedrawRequested` wakes `App` once.
static WAKE_ON_PRESENT: AtomicBool = AtomicBool::new(false);
static MAIN_WINDOW_ID: Mutex<Option<iced::window::Id>> = Mutex::new(None);
static INITIAL_CONFIG: OnceLock<Mutex<Option<InitialAppConfig>>> = OnceLock::new();

pub(super) fn set_main_window_id(id: Option<iced::window::Id>) {
    if let Ok(mut slot) = MAIN_WINDOW_ID.lock() {
        *slot = id;
    }
}

pub(super) fn batch_tasks<T: 'static>(mut tasks: Vec<Task<T>>) -> Task<T> {
    match tasks.len() {
        0 => Task::none(),
        1 => tasks.pop().unwrap_or_else(Task::none),
        _ => Task::batch(tasks),
    }
}

/// Starts the Iced desktop daemon and coordinator background loop.
pub fn run() -> iced::Result {
    // Before any thread exists, so every thread shares the capped arenas.
    let arenas_capped = rustygrade_linux::limit_malloc_arenas();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    tracing::debug!(arenas_capped, "malloc arena limit");

    // A panic is written to native-error.log (as the original's fatal errors), then reported as usual.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        rustygrade_core::error_log::write("fatal", &info.to_string());
        default_hook(info);
    }));

    let args: Vec<String> = std::env::args().collect();
    let skip_single_instance = args.iter().any(|a| a == "--no-single-instance");
    let open_args: Vec<String> = args
        .iter()
        .skip(1)
        .filter(|a| !a.starts_with("--") && !a.trim().is_empty())
        .map(|a| normalize_open_arg(a))
        .collect();

    let settings = AppSettings::load();
    // SAFETY: called on the main thread at startup before SingleInstance, Tokio, or Iced threads spawn.
    let graphics_report = unsafe { apply_startup_graphics_policy(&settings) };

    let _single_guard;
    let single_instance_rx = if !skip_single_instance {
        match SingleInstance::acquire(&CoverPaths::app_dir(), &open_args) {
            Ok(SingleInstanceOutcome::SecondaryNotified) => {
                return Ok(());
            }
            Ok(SingleInstanceOutcome::Primary(guard, rx)) => {
                _single_guard = Some(guard);
                Some(rx)
            }
            Err(_) => {
                _single_guard = None;
                None
            }
        }
    } else {
        _single_guard = None;
        None
    };

    // Only the primary instance owns the temp files of cover fetches.
    if _single_guard.is_some() {
        std::thread::spawn(cover_fetch::sweep);
    }

    let initial_vol = settings.volume.clamp(0.0, 1.0);
    let initial_device = settings.audio_device.clone();
    let initial_tier = RenderTier::resolve(settings.performance_mode, PowerProfile::Balanced);

    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let handoff = BootstrapHandoff {
        cmd_rx,
        startup: StartupContext {
            settings: settings.clone(),
            single_instance_rx,
            open_uris: open_args,
        },
    };
    let _ = BOOTSTRAP_HANDOFF.set(Mutex::new(Some(handoff)));

    let app_init = InitialAppConfig {
        cmd_tx,
        settings,
        graphics_report,
        initial_vol,
        initial_device,
        initial_tier,
    };
    let _ = INITIAL_CONFIG.set(Mutex::new(Some(app_init)));

    // Avoid allocating a full-window 4x MSAA render target in i915 shmem; quads/borders, text,
    // SVGs, and album covers do not go through the MSAA triangle blit.
    iced::daemon(App::boot, App::update, App::view)
        .title(App::title)
        .subscription(App::subscription)
        .theme(App::theme)
        .antialiasing(false)
        .executor::<CappedRuntime>()
        .run()
}

/// iced's tokio executor with a few worker threads instead of one per CPU: the controller and
/// the UI streams need little, and blocking work goes to `spawn_blocking` (pool left as is).
struct CappedRuntime(tokio::runtime::Runtime);

impl iced::Executor for CappedRuntime {
    fn new() -> Result<Self, iced::futures::io::Error> {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(3)
            .thread_name("rustygrade-rt")
            .enable_all()
            .build()
            .map(Self)
    }

    #[allow(clippy::let_underscore_future)]
    fn spawn(&self, future: impl Future<Output = ()> + Send + 'static) {
        let _ = self.0.spawn(future);
    }

    fn enter<R>(&self, f: impl FnOnce() -> R) -> R {
        let _guard = self.0.enter();
        f()
    }

    fn block_on<T>(&self, future: impl Future<Output = T>) -> T {
        self.0.block_on(future)
    }
}

struct InitialAppConfig {
    cmd_tx: mpsc::UnboundedSender<ControllerCommand>,
    settings: AppSettings,
    graphics_report: GraphicsPolicyReport,
    initial_vol: f64,
    initial_device: String,
    initial_tier: RenderTier,
}

pub struct App {
    pub cmd_tx: mpsc::UnboundedSender<ControllerCommand>,
    pub main_window: Option<iced::window::Id>,
    pub mini_window: Option<iced::window::Id>,
    pub main_window_focused: bool,
    pub main_window_minimized: bool,
    pub window_visible: bool,
    pub(crate) playback_tick_mode: PlaybackTickMode,
    pub(crate) watchdog: PresentWatchdog,
    pub shutting_down: bool,
    pub active_tab: Tab,
    pub previous_tab: Tab,
    pub library_mode: LibraryMode,
    pub album_detail_open: bool,
    pub search_query: String,
    pub search_input_id: Id,
    pub library_sort: LibrarySort,
    pub graphics_report: GraphicsPolicyReport,
    pub settings: AppSettings,
    pub power_profile: PowerProfile,
    pub render_tier: RenderTier,
    pub covers: CoverCache,
    pub accent_anim: AccentAnimator,
    pub orbit_theme: Theme,
    /// Monotonic animation phase; speed is integrated per frame so play/pause never jumps.
    pub anim_phase: f64,
    /// Mini-player was moved since the last `push_settings()`.
    pub mini_pos_dirty: bool,
    pub last_frame_at: Option<Instant>,
    pub hovered_files: usize,
    /// The sidebar's DATA_BANK shows playlists instead of albums.
    pub bank_playlists: bool,
    pub artist_detail_open: bool,
    pub search_tab: SearchTab,
    /// Indices `(album_idx, track_idx)` into `self.albums` for matching search tracks.
    pub(crate) search_tracks: Vec<(usize, usize)>,
    pub search_playlists: Vec<(PlaylistView, String, usize)>,
    pub create_playlist_open: bool,
    /// Player-bar popups: the queue, and the sound popup with its output tab.
    pub queue_open: bool,
    pub audio_open: bool,
    pub audio_output_tab: bool,
    /// Full-screen player reveal state and decoded backdrop/cover textures.
    pub fs_anim: Option<fullscreen::FsAnim>,
    pub fs_cover: Option<(String, iced::widget::image::Handle)>,
    pub fs_blur: Option<(String, iced::widget::image::Handle)>,
    pub fs_lyrics_tab: bool,
    pub(crate) fs_last_lyric: Option<usize>,
    pub lyrics_scroll_id: Id,
    pub(crate) detail_tint: Option<iced::Color>,
    pub(crate) detail_tint_at: Option<Instant>,
    pub(crate) tint_cache: HashMap<String, iced::Color>,
    pub(crate) fs_blur_prev: Option<iced::widget::image::Handle>,
    pub(crate) fs_blur_at: Option<Instant>,
    pub(crate) lyrics_vp: Option<(f32, f32, f32)>,
    pub(crate) lyric_anim: Option<(f32, f32, Instant)>,
    /// Developer screenshot run (`RETRO_SHOT`), see `devshot.rs`.
    pub(crate) shot: Option<devshot::Plan>,
    pub(crate) shot_started: bool,
    pub(crate) dropped_files: Vec<PathBuf>,
    pub show_shortcuts_modal: bool,
    pub albums: Arc<[Album]>,
    pub artists: Vec<ArtistSummary>,
    /// The library grid order (every album / artist, never filtered by the search).
    pub library_albums: Vec<usize>,
    pub library_artists: Vec<usize>,
    /// Sidebar album indices sorted A-Z.
    pub sidebar_albums: Vec<usize>,
    pub select_mode: bool,
    pub selected_albums: HashSet<AlbumKey>,
    pub deleting_albums: bool,
    pub context_menu: Option<context_menu::ContextMenu>,
    pub add_music_menu_open: bool,
    pub smart_counts: HashMap<i64, usize>,
    pub sidebar_width: f32,
    pub sidebar_anim: Option<(Instant, f32, f32)>,
    pub(crate) last_titlebar_press: Option<Instant>,
    pub mini_hovered: bool,
    pub mini_marquee_phase: f32,
    pub lib_scroll_y: f32,
    /// Unquantized grid offset and the grid scrollable's id, to put the grid back after a detail page.
    pub lib_scroll_raw: f32,
    pub lib_scroll_id: Id,
    pub detail_scroll_id: Id,
    /// The row being dragged on a detail page: its page, index, and the page's track ids when lifted.
    pub reorder_lift: Option<(pages::DetailCtx, usize, Vec<String>)>,
    /// Offset to put the grid back to; scroll reports are ignored until then.
    pub lib_restore: Option<f32>,
    pub detail_scroll_y: f32,
    pub bank_scroll_y: f32,
    pub bank_viewport_h: Option<f32>,
    pub bank_scroll_id: Id,
    /// SEARCH results.
    pub search_albums: Vec<usize>,
    pub search_artists: Vec<usize>,
    pub(crate) search_gen: u64,
    /// Play stats reached the library order once (they may arrive after the library).
    pub(crate) stats_seen: bool,
    pub toasts: toasts::Toasts,
    pub scan: Option<toasts::ScanState>,
    pub confirm: Option<dialogs::ConfirmDialog>,
    pub bg_prompt: bool,
    pub bg_remember: bool,
    pub sort_menu_open: bool,
    pub bank_menu_open: bool,
    pub backup_import_replace: bool,
    /// Main window size, for the grid columns and the queue popup height.
    pub window_size: Size,
    /// The page whose cover color was last asked for, and the track whose backdrop was last blurred.
    pub(crate) tint_for: Option<String>,
    pub(crate) fs_blur_tried: Option<String>,
    /// Detail pages opened from a detail page, for BACK (at most 30), and where the first one opened from.
    pub(crate) page_stack: Vec<pages::Page>,
    pub(crate) view_before_detail: Tab,
    pub create_input_id: Id,
    pub queue_hand_len: usize,
    pub track_map: HashMap<String, (usize, usize)>,
    pub liked_set: HashSet<String>,
    pub cached_smart_tracks: Vec<Track>,
    pub collections: CollectionsData,
    pub stats: PlayStats,
    pub selected_album: Option<AlbumKey>,
    pub selected_artist: Option<String>,
    pub queue_tracks: Arc<Vec<Track>>,
    pub queue_index: Option<usize>,
    pub shuffle: bool,
    pub repeat_mode: RepeatMode,
    pub current_track: Option<Track>,
    pub lyrics: Option<LyricsDoc>,
    pub is_playing: bool,
    pub position_s: f64,
    pub duration_s: f64,
    pub seeking_pos: Option<f64>,
    pub volume: f64,
    pub dragging_volume: Option<f64>,
    pub last_volume_emit: Option<Instant>,
    pub muted: bool,
    pub tray_available: bool,
    pub devices: Vec<DeviceOption>,
    pub audio_device: String,
    pub bit_perfect_status: String,
    /// Direct (bit-perfect) output is playing to the card: EQ, ReplayGain, crossfade bypassed.
    pub direct_active: bool,
    pub direct_volume_hint: String,
    pub audio_path: AudioPath,
    pub pw_bridge_node_id: Option<u32>,
    pub direct_mixer_control: Option<String>,
    pub playlist_view: PlaylistView,
    pub new_playlist_name: String,
    pub rename_playlist_buf: String,
    pub smart_draft: Option<SmartDraft>,
    pub tag_draft: Option<TagDraft>,
    pub tag_status: Option<Result<String, String>>,
    /// Request number of the latest cover lookup and the flag that cancels it.
    pub cover_fetch_seq: u64,
    pub cover_fetch_cancel: Option<Arc<AtomicBool>>,
    /// Cover files fetched for the open editor, deleted when it closes or saves.
    pub fetched_covers: Vec<PathBuf>,
    /// FETCH ONLINE's picker, over the tag editor.
    pub cover_picker: Option<cover_fetch::Picker>,
    pub discogs_token_visible: bool,
    pub(crate) discogs_token_dirty: bool,
    discogs_token_gen: u64,
    pub discord_rpc_status: DiscordRpcStatus,
    pub egg_morse: egg::EggMorseState,
    pub egg_open: bool,
    pub egg_hovered: bool,
    pub egg_color_alpha: f32,
    pub egg_pulse_phase: f32,
    pub egg_assets: Option<egg::EggAssets>,
    pub egg_loading: bool,
}

impl App {
    fn boot() -> (Self, Task<Message>) {
        let cfg = INITIAL_CONFIG
            .get()
            .and_then(|m| m.lock().ok())
            .and_then(|mut g| g.take())
            .expect("initial app config missing");

        let (main_id, open_task) = open_main_window();
        set_main_window_id(Some(main_id));
        let sort = LibrarySort::from_key(&cfg.settings.library_sort);
        let lib_mode = if cfg.settings.library_view == "artists" {
            LibraryMode::Artists
        } else {
            LibraryMode::Albums
        };

        let sidebar_visible = cfg.settings.sidebar_visible;
        let accent_anim = AccentAnimator::default();
        let orbit_theme = theme::orbit_theme(accent_anim.current_accent);

        let state = Self {
            cmd_tx: cfg.cmd_tx,
            main_window: Some(main_id),
            mini_window: None,
            main_window_focused: true,
            main_window_minimized: false,
            window_visible: true,
            playback_tick_mode: PlaybackTickMode::Active,
            watchdog: PresentWatchdog::default(),
            shutting_down: false,
            active_tab: Tab::Library,
            previous_tab: Tab::Library,
            library_mode: lib_mode,
            album_detail_open: false,
            search_query: String::new(),
            search_input_id: Id::new("library-search"),
            library_sort: sort,
            graphics_report: cfg.graphics_report,
            settings: cfg.settings,
            power_profile: PowerProfile::Balanced,
            render_tier: cfg.initial_tier,
            covers: CoverCache::default(),
            accent_anim,
            orbit_theme,
            anim_phase: 0.0,
            mini_pos_dirty: false,
            last_frame_at: None,
            hovered_files: 0,
            bank_playlists: false,
            artist_detail_open: false,
            search_tab: SearchTab::All,
            search_tracks: Vec::new(),
            search_playlists: Vec::new(),
            create_playlist_open: false,
            queue_open: false,
            audio_open: false,
            audio_output_tab: false,
            fs_anim: None,
            fs_cover: None,
            fs_blur: None,
            fs_lyrics_tab: false,
            fs_last_lyric: None,
            lyrics_scroll_id: Id::unique(),
            detail_tint: None,
            detail_tint_at: None,
            tint_cache: HashMap::new(),
            fs_blur_prev: None,
            fs_blur_at: None,
            lyrics_vp: None,
            lyric_anim: None,
            shot: devshot::Plan::from_env(),
            shot_started: false,
            dropped_files: Vec::new(),
            show_shortcuts_modal: false,
            albums: Arc::from(Vec::new()),
            artists: Vec::new(),
            library_albums: Vec::new(),
            library_artists: Vec::new(),
            sidebar_albums: Vec::new(),
            select_mode: false,
            selected_albums: HashSet::new(),
            deleting_albums: false,
            context_menu: None,
            add_music_menu_open: false,
            smart_counts: HashMap::new(),
            sidebar_width: if sidebar_visible { 244.0 } else { 60.0 },
            sidebar_anim: None,
            last_titlebar_press: None,
            mini_hovered: false,
            mini_marquee_phase: 0.0,
            lib_scroll_y: 0.0,
            lib_scroll_raw: 0.0,
            lib_scroll_id: Id::unique(),
            detail_scroll_id: Id::unique(),
            reorder_lift: None,
            lib_restore: None,
            detail_scroll_y: 0.0,
            bank_scroll_y: 0.0,
            bank_viewport_h: None,
            bank_scroll_id: Id::unique(),
            search_albums: Vec::new(),
            search_artists: Vec::new(),
            search_gen: 0,
            stats_seen: false,
            toasts: toasts::Toasts::default(),
            scan: None,
            confirm: None,
            bg_prompt: false,
            bg_remember: false,
            sort_menu_open: false,
            bank_menu_open: false,
            backup_import_replace: false,
            window_size: Size::new(1200.0, 800.0),
            tint_for: None,
            fs_blur_tried: None,
            page_stack: Vec::new(),
            view_before_detail: Tab::Library,
            create_input_id: Id::new("create-playlist-name"),
            queue_hand_len: 0,
            track_map: HashMap::new(),
            liked_set: HashSet::new(),
            cached_smart_tracks: Vec::new(),
            collections: CollectionsData::default(),
            stats: PlayStats::default(),
            selected_album: None,
            selected_artist: None,
            queue_tracks: Arc::default(),
            queue_index: None,
            shuffle: false,
            repeat_mode: RepeatMode::Off,
            current_track: None,
            lyrics: None,
            is_playing: false,
            position_s: 0.0,
            duration_s: 0.0,
            seeking_pos: None,
            volume: cfg.initial_vol,
            dragging_volume: None,
            last_volume_emit: None,
            muted: false,
            tray_available: false,
            devices: vec![DeviceOption {
                id: cfg.initial_device.clone(),
                label: cfg.initial_device.clone(),
                is_direct: false,
            }],
            audio_device: cfg.initial_device,
            bit_perfect_status: String::new(),
            direct_active: false,
            direct_volume_hint: String::new(),
            audio_path: AudioPath {
                input: None,
                output: None,
            },
            pw_bridge_node_id: None,
            direct_mixer_control: None,
            playlist_view: PlaylistView::Liked,
            new_playlist_name: String::new(),
            rename_playlist_buf: String::new(),
            smart_draft: None,
            tag_draft: None,
            tag_status: None,
            cover_fetch_seq: 0,
            cover_fetch_cancel: None,
            fetched_covers: Vec::new(),
            cover_picker: None,
            discogs_token_visible: false,
            discogs_token_dirty: false,
            discogs_token_gen: 0,
            discord_rpc_status: DiscordRpcStatus::Disabled,
            egg_morse: egg::EggMorseState::default(),
            egg_open: false,
            egg_hovered: false,
            egg_color_alpha: 0.0,
            egg_pulse_phase: 0.0,
            egg_assets: None,
            egg_loading: false,
        };

        state.sync_artwork_providers();
        motion::set_enabled(state.animations_enabled());
        (state, open_task.map(Message::WindowOpened))
    }

    fn title(&self, window_id: iced::window::Id) -> String {
        if self.mini_window == Some(window_id) {
            return match &self.current_track {
                Some(t) => format!("{} — {} | Rustygrade Mini", t.title, t.artist),
                None => "Rustygrade Mini".to_owned(),
            };
        }
        if let Some(track) = &self.current_track {
            format!("{} — {} | Rustygrade", track.title, track.artist)
        } else {
            "Rustygrade".to_owned()
        }
    }

    fn theme(&self, _window: iced::window::Id) -> Theme {
        self.orbit_theme.clone()
    }

    pub fn animations_enabled(&self) -> bool {
        window::animations_active(self.settings.enable_animations, self.render_tier)
    }

    fn needs_anim_tick(&self) -> bool {
        if !self.animations_enabled() {
            return false;
        }
        let main_active = self.window_visible
            && self.main_window.is_some()
            && !self.main_window_minimized
            && !self.watchdog.is_suspended();
        let mini_no_cover = self.current_track.as_ref().is_none_or(|t| {
            self.covers
                .get_first(&[t.cover_thumb_path.as_deref(), t.cover_full_path.as_deref()])
                .is_none()
        });
        let mini_active =
            self.mini_window.is_some() && (self.mini_hovered || (self.is_playing && mini_no_cover));
        (main_active
            && (self.motion_running()
                || self.sidebar_anim.is_some()
                || self.accent_anim.is_animating()
                || self.toasts.animating()
                || self.egg_open
                || self.scan.is_some()
                || self.deleting_albums))
            || mini_active
    }

    pub(super) fn sync_animations(&mut self) -> Task<Message> {
        let on = self.animations_enabled();
        motion::set_enabled(on);
        if on {
            return Task::none();
        }
        self.last_frame_at = None;
        self.fs_anim = None;
        self.detail_tint_at = None;
        self.fs_blur_at = None;
        self.fs_blur_prev = None;
        self.mini_marquee_phase = 0.0;
        self.egg_color_alpha = if self.egg_hovered { 1.0 } else { 0.0 };
        if let Some((_, _, to)) = self.sidebar_anim.take() {
            self.sidebar_width = to;
        }
        if self.accent_anim.is_animating() {
            self.accent_anim.finish();
            self.orbit_theme = theme::orbit_theme(self.accent_anim.current_accent);
        }
        self.toasts.finish_exiting();
        if let Some((_, to, _)) = self.lyric_anim.take() {
            return operation::scroll_to(
                self.lyrics_scroll_id.clone(),
                iced::widget::scrollable::AbsoluteOffset { x: 0.0, y: to },
            );
        }
        Task::none()
    }

    fn subscription(&self) -> Subscription<Message> {
        let controller_sub = Subscription::run(controller_event_stream).map(Message::Controller);
        let close_req_sub = iced::window::close_requests().map(Message::WindowCloseRequested);
        let close_evt_sub = iced::window::close_events().map(Message::WindowClosed);
        let events_sub = event::listen_with(shortcuts::map_runtime_event);
        let present_sub = event::listen_raw(window::map_present_event);

        let mut subs = vec![
            controller_sub,
            close_req_sub,
            close_evt_sub,
            events_sub,
            present_sub,
        ];
        if self.needs_anim_tick() {
            subs.push(iced::time::every(motion::FRAME).map(Message::AnimTick));
        }
        Subscription::batch(subs)
    }

    fn update(&mut self, msg: Message) -> Task<Message> {
        let fs_was_open = self.active_tab == Tab::NowPlaying;
        let is_open_menu = matches!(
            msg,
            Message::OpenTrackMenu { .. }
                | Message::OpenAlbumMenu(..)
                | Message::OpenPlaylistMenu(..)
                | Message::OpenDetailMenu(..)
        );
        // Flyouts and context menus close on whatever the user does next (excluding background events).
        let background = matches!(
            msg,
            Message::ToggleSortMenu
                | Message::ToggleBankMenu
                | Message::ToggleAddMusicMenu
                | Message::AnimTick(_)
                | Message::Controller(_)
                | Message::ToastExpire(_)
                | Message::ToastRemove(_)
                | Message::WindowResized(..)
                | Message::WindowFocused(..)
                | Message::WindowMoved(..)
                | Message::MainWindowMinimized(..)
                | Message::WindowPresented(..)
                | Message::SearchDebounced(_)
                | Message::DiscogsTokenCommit(_)
                | Message::DetailTintReady(..)
                | Message::FsBlurReady(..)
                | Message::FsAnimDone(_)
                | Message::CoverPickerResults(..)
                | Message::TagCoverFetched(..)
                | Message::ShotGo(_)
                | Message::ShotWheel(_)
                | Message::ShotDrag(_)
                | Message::LyricsScrolled(..)
                | Message::ShotHover(_)
                | Message::ShotTaken(..)
                | Message::MiniHover(_)
                | Message::VisualizerBarRelease(..)
                | Message::EggMorseTimeout(_)
                | Message::EggAssetsReady(_)
                | Message::EggImageHover(_)
                | Message::LibScrolled(_)
                | Message::RestoreLibScroll
                | Message::DetailScrolled(_)
                | Message::BankScrolled(..)
                | Message::KeyShortcut { .. }
                | Message::FocusCheckedShortcut { .. }
        );
        if !background {
            self.sort_menu_open = false;
            self.bank_menu_open = false;
            self.add_music_menu_open = false;
            if !is_open_menu {
                self.context_menu = None;
            }
        }
        let task = self.update_inner(msg);
        self.fs_sync(fs_was_open);
        let backdrop = self.fs_refresh_backdrop();
        self.sync_playback_tick_mode();
        let lyrics_scroll = self.fs_lyric_scroll();
        batch_tasks(vec![task, backdrop, lyrics_scroll])
    }

    fn update_inner(&mut self, msg: Message) -> Task<Message> {
        match msg {
            Message::Noop => Task::none(),
            Message::WindowOpened(_)
            | Message::MiniWindowOpened(_)
            | Message::WindowCloseRequested(_)
            | Message::WindowClosed(_)
            | Message::WindowFocused(..)
            | Message::WindowMoved(..)
            | Message::WindowResized(..)
            | Message::MainWindowMinimized(..)
            | Message::WindowPresented(_)
            | Message::FileHovered
            | Message::FilesHoveredLeft
            | Message::FileDropped(_)
            | Message::FlushDrops
            | Message::ToggleMiniPlayer
            | Message::DragMiniWindow
            | Message::CloseMiniPlayerToMain
            | Message::MiniHover(_)
            | Message::DragMainWindow
            | Message::MinimizeMainWindow
            | Message::ToggleMaximizeMainWindow
            | Message::RequestCloseMainWindow
            | Message::ResizeMainWindow(_)
            | Message::BackgroundRemember(_)
            | Message::BackgroundChoice(_)
            | Message::ForceExit => self.handle_window_message(msg),

            Message::KeyShortcut { .. } | Message::FocusCheckedShortcut { .. } => {
                self.handle_shortcut_message(msg)
            }

            Message::GoHome
            | Message::OpenArtist(_)
            | Message::OpenCurrentArtist
            | Message::OpenPlaylistPage(_)
            | Message::SelectTab(_)
            | Message::SetLibraryMode(_)
            | Message::SelectLibrarySort(_)
            | Message::SelectAlbum(_)
            | Message::Back
            | Message::SearchChanged(_)
            | Message::SearchDebounced(_)
            | Message::FocusSearch
            | Message::SetSearchTab(_)
            | Message::SetBankPlaylists(_)
            | Message::LibScrolled(_)
            | Message::DetailScrolled(_)
            | Message::BankScrolled(..)
            | Message::RestoreLibScroll
            | Message::PlayDetail(..)
            | Message::QueueDetail(_)
            | Message::PlaySearchTrack(_)
            | Message::PlayAlbum(_)
            | Message::PlayTrackFromList(..)
            | Message::PlayNextTracks(_)
            | Message::EnqueueTracks(_)
            | Message::JumpToQueueIndex(_)
            | Message::RemoveQueueIndex(_)
            | Message::ClearQueue
            | Message::TogglePlayPause
            | Message::Previous
            | Message::Next
            | Message::ToggleShuffle
            | Message::CycleRepeat
            | Message::SeekChanged(_)
            | Message::SeekDragging(_)
            | Message::SeekCommit
            | Message::VolumeDragging(_)
            | Message::VolumeCommit
            | Message::ToggleMute
            | Message::SelectDevice(_)
            | Message::ToggleQueuePanel
            | Message::ToggleAudioPanel
            | Message::SetAudioOutputTab(_)
            | Message::SetFsLyricsTab(_)
            | Message::LyricsScrolled(..)
            | Message::DetailTintReady(..)
            | Message::FsBlurReady(..)
            | Message::FsAnimDone(_)
            | Message::AnimTick(_) => self.handle_playback_nav_message(msg),

            Message::ToggleCreatePlaylist(_)
            | Message::NewPlaylistNameChanged(_)
            | Message::CreatePlaylist
            | Message::DeletePlaylist(_)
            | Message::RenamePlaylistInput(_)
            | Message::RenamePlaylistSubmit(_)
            | Message::ToggleLike(_)
            | Message::ToggleLikeTracks(_)
            | Message::TogglePlaylistTracks { .. }
            | Message::RemoveTrackFromPlaylist { .. }
            | Message::RemovePlaylistCover(_)
            | Message::NewSmartPlaylist
            | Message::EditSmartPlaylist(_)
            | Message::SmartDraftNameChanged(_)
            | Message::SmartRuleFieldChanged(..)
            | Message::SmartRuleOpChanged(..)
            | Message::SmartRuleValueChanged(..)
            | Message::AddSmartRule
            | Message::RemoveSmartRule(_)
            | Message::SmartDraftSortChanged(_)
            | Message::SmartDraftLimitChanged(_)
            | Message::SmartDraftMatchAllChanged(_)
            | Message::SaveSmartPlaylist
            | Message::CancelSmartPlaylist
            | Message::DeleteSmartPlaylist(_)
            | Message::EnterSelectMode
            | Message::ExitSelectMode
            | Message::ToggleAlbumSelected(_)
            | Message::SelectAllAlbums
            | Message::DeleteSelectedAlbums
            | Message::DeleteAlbum(..)
            | Message::ReorderLift(..)
            | Message::ReorderDrop(_)
            | Message::ReorderEdge(_)
            | Message::PickPlaylistCover(_)
            | Message::PlaylistCoverPicked(..)
            | Message::ImportPlaylistPick
            | Message::ImportBackupFromSettingsPick
            | Message::PlaylistImportPicked(..)
            | Message::ZipReplaceInspected(..)
            | Message::SetBackupImportReplace(_)
            | Message::ExportCollectionsZipPick
            | Message::CollectionsZipExportPicked(_)
            | Message::ExportPlaylistM3u8(_)
            | Message::PlaylistM3u8ExportPicked(..)
            | Message::ExportPlaylistZip(_)
            | Message::PlaylistZipExportPicked(..)
            | Message::ConfirmAccept
            | Message::ConfirmCancel => self.handle_collection_message(msg),

            Message::OpenTagEditor(..)
            | Message::OpenAlbumTagEditor(_)
            | Message::CloseTagEditor
            | Message::SetTagField(..)
            | Message::TagPickCover
            | Message::TagFetchCover
            | Message::CoverPickerArtist(_)
            | Message::CoverPickerAlbum(_)
            | Message::CoverPickerSearch
            | Message::CoverPickerResults(..)
            | Message::CoverPickerSelect(_)
            | Message::CoverPickerUse
            | Message::CoverPickerClose
            | Message::TagCoverFetched(..)
            | Message::TagCoverPicked(_)
            | Message::TagRemoveCover
            | Message::SaveTrackTags => self.handle_tag_message(msg),

            Message::Controller(evt) => self.handle_controller_event(evt),

            Message::SetEqEnabled(_)
            | Message::SelectEqPreset(_)
            | Message::SetEqBand { .. }
            | Message::SelectReplayGain(_)
            | Message::SetCrossfadeSeconds(_)
            | Message::PickFiles
            | Message::FilesPicked(_)
            | Message::PickFolders
            | Message::FoldersPicked(_)
            | Message::RemoveLibraryFolder(_)
            | Message::RescanLibrary
            | Message::ResyncCovers
            | Message::SetGpuPreference(_)
            | Message::SetDisplayBackend(_)
            | Message::SetPerformanceMode(_)
            | Message::ToggleAnimations(_)
            | Message::SetDirectBridge(_)
            | Message::ToggleTray(_)
            | Message::ToggleMpris(_)
            | Message::ToggleDiscordRpc(_)
            | Message::ToggleDiscordArtwork(_)
            | Message::ToggleArtworkItunes(_)
            | Message::ToggleArtworkDeezer(_)
            | Message::ToggleArtworkMusicbrainz(_)
            | Message::ToggleArtworkDiscogs(_)
            | Message::DiscogsTokenChanged(_)
            | Message::DiscogsTokenCommit(_)
            | Message::ToggleDiscogsTokenVisible
            | Message::ToggleWatchFolders(_)
            | Message::ToggleAccentFromCover(_)
            | Message::ToggleMinimizeToTray(_)
            | Message::VisualizerBarPressed(_)
            | Message::VisualizerBarRelease(..)
            | Message::EggMorseTimeout(_)
            | Message::EggAssetsReady(_)
            | Message::EggImageHover(_)
            | Message::CloseEgg
            | Message::ToastExpire(_)
            | Message::ToastRemove(_)
            | Message::ToggleSortMenu
            | Message::ToggleBankMenu
            | Message::ToggleAddMusicMenu
            | Message::ToggleShortcutsModal
            | Message::ToggleSidebar
            | Message::OpenTrackMenu { .. }
            | Message::OpenAlbumMenu(..)
            | Message::OpenPlaylistMenu(..)
            | Message::OpenDetailMenu(..)
            | Message::CloseContextMenu
            | Message::ShotStep(_)
            | Message::ShotGo(_)
            | Message::ShotHover(_)
            | Message::ShotWheel(_)
            | Message::ShotDrag(_)
            | Message::ShotTaken(..) => self.handle_settings_and_ui_message(msg),
        }
    }

    /// Resolves display title/artist/etc. lookup by track ID.
    pub fn lookup_track(&self, track_id: &str) -> Option<&Track> {
        let &(aidx, tidx) = self.track_map.get(track_id)?;
        self.albums.get(aidx)?.tracks.get(tidx)
    }

    pub fn find_album(&self, key: &AlbumKey) -> Option<&Album> {
        self.albums
            .iter()
            .find(|a| a.name == key.name && a.artist == key.artist)
    }

    pub fn find_artist(&self, key: &str) -> Option<&ArtistSummary> {
        self.artists.iter().find(|a| artist_key(&a.name) == key)
    }

    /// Cancels any cover lookup in flight and deletes temporary cover files for the editor.
    pub(crate) fn end_cover_fetches(&mut self) {
        self.cover_picker = None;
        if let Some(cancel) = self.cover_fetch_cancel.take() {
            cancel.store(true, Ordering::Relaxed);
        }
        for p in self.fetched_covers.drain(..) {
            cover_fetch::discard(&p);
        }
    }

    pub(crate) fn clear_tint_for_paths(&mut self, paths: &[PathBuf]) {
        if paths.is_empty() {
            return;
        }
        let path_strs: Vec<String> = paths
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        let matches_path = |key: &str| {
            key.split_once('\u{1}')
                .is_some_and(|(_, tail)| !tail.is_empty() && path_strs.iter().any(|p| tail == p))
        };
        self.tint_cache.retain(|k, _| !matches_path(k));
        if self.tint_for.as_deref().is_some_and(matches_path) {
            self.tint_for = None;
            self.detail_tint = None;
            self.detail_tint_at = None;
        }
    }

    /// The fullscreen backdrop is a blur of the playing track's cover: clear so it rebuilds once.
    pub(super) fn refresh_backdrop_if_current_cover(&mut self, paths: &[PathBuf]) {
        if self.current_track.as_ref().is_some_and(|t| {
            t.cover_thumb_path
                .as_ref()
                .is_some_and(|p| paths.contains(p))
                || t.cover_full_path
                    .as_ref()
                    .is_some_and(|p| paths.contains(p))
        }) {
            self.fs_blur_tried = None;
            self.fs_blur = None;
        }
    }

    /// Header color fade-in progress (0..1): immediate when cached or none, 250 ms linear otherwise.
    pub fn detail_tint_alpha(&self) -> f32 {
        match (self.detail_tint, self.detail_tint_at) {
            (None, _) => 0.0,
            (Some(_), None) => 1.0,
            (Some(_), Some(at)) => motion::progress(
                at,
                Instant::now(),
                Duration::from_millis(250),
                motion::Ease::Linear,
            ),
        }
    }

    /// Full-screen backdrop cross-fade progress (0..1 over 500 ms).
    pub fn fs_blur_fade(&self) -> f32 {
        self.fs_blur_at.map_or(1.0, |at| {
            motion::progress(
                at,
                Instant::now(),
                Duration::from_millis(500),
                motion::Ease::Linear,
            )
        })
    }

    /// Cross-fades the full-screen backdrop while open, otherwise swaps immediately.
    pub fn set_fs_blur(&mut self, next: Option<(String, iced::widget::image::Handle)>) {
        let old = std::mem::replace(&mut self.fs_blur, next).map(|(_, h)| h);
        if self.active_tab == Tab::NowPlaying
            && self.animations_enabled()
            && (old.is_some() || self.fs_blur.is_some())
        {
            self.fs_blur_prev = old;
            self.fs_blur_at = Some(Instant::now());
        } else {
            self.fs_blur_prev = None;
            self.fs_blur_at = None;
        }
    }

    pub(super) fn motion_running(&self) -> bool {
        self.animations_enabled()
            && (self.detail_tint_at.is_some()
                || self.fs_blur_at.is_some()
                || self.lyric_anim.is_some())
    }

    pub(super) fn motion_tick(&mut self, now: Instant) -> Task<Message> {
        if self
            .detail_tint_at
            .is_some_and(|at| now.saturating_duration_since(at) >= Duration::from_millis(250))
        {
            self.detail_tint_at = None;
        }
        if self
            .fs_blur_at
            .is_some_and(|at| now.saturating_duration_since(at) >= Duration::from_millis(500))
        {
            self.fs_blur_at = None;
            self.fs_blur_prev = None;
        }
        let Some((from, to, start)) = self.lyric_anim else {
            return Task::none();
        };
        let t = motion::progress(
            start,
            now,
            Duration::from_millis(350),
            motion::Ease::CubicOut,
        );
        if t >= 1.0 {
            self.lyric_anim = None;
        }
        operation::scroll_to(
            self.lyrics_scroll_id.clone(),
            iced::widget::scrollable::AbsoluteOffset {
                x: 0.0,
                y: motion::lerp(from, to, t),
            },
        )
    }

    fn view(&self, window_id: iced::window::Id) -> Element<'_, Message> {
        if self.mini_window == Some(window_id) {
            views::render_mini_player(self)
        } else {
            views::render_app(self)
        }
    }
}

fn controller_event_stream() -> impl Stream<Item = ControllerEvent> {
    iced::stream::channel(
        64,
        |mut output: iced::futures::channel::mpsc::Sender<ControllerEvent>| async move {
            let handoff = BOOTSTRAP_HANDOFF
                .get()
                .and_then(|m| m.lock().ok())
                .and_then(|mut g| g.take());

            let Some(handoff) = handoff else {
                return;
            };

            let (ui_tx, mut ui_rx) = mpsc::unbounded_channel();
            tokio::spawn(run_coordinator(handoff.cmd_rx, ui_tx, handoff.startup));

            while let Some(evt) = ui_rx.recv().await {
                if output.send(evt).await.is_err() {
                    break;
                }
            }
        },
    )
}

/// Track time as `m:ss`, or `h:mm:ss` from one hour on.
pub fn format_mm_ss(seconds: f64) -> String {
    let total = if seconds.is_finite() {
        seconds.max(0.0).floor() as u64
    } else {
        0
    };
    let (h, m, s) = (total / 3600, total / 60 % 60, total % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{}:{s:02}", total / 60)
    }
}

#[cfg(test)]
mod tests {
    use super::format_mm_ss;

    #[test]
    fn track_time_is_m_ss_like_the_original() {
        assert_eq!(format_mm_ss(0.0), "0:00");
        assert_eq!(format_mm_ss(163.9), "2:43");
        assert_eq!(format_mm_ss(600.0), "10:00");
        assert_eq!(format_mm_ss(3723.0), "1:02:03");
        assert_eq!(format_mm_ss(-5.0), "0:00");
        assert_eq!(format_mm_ss(f64::NAN), "0:00");
    }
}
