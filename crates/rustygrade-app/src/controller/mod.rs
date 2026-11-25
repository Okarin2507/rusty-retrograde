mod audio;
mod integrations;
mod library;
mod persist;
mod playback;
mod playlists;
mod tags;
pub mod types;

use std::collections::{HashMap, HashSet};
use std::future::pending;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rustygrade_audio::{
    AlsaHwParams, AudioPath, BridgeEvent, CoalescedHwWriter, EngineEvent, EqualizerConfig,
    MpvEngine, PipeWireBridge,
};
use rustygrade_core::{
    Album, AppSettings, CollectionsData, CollectionsStore, CoverPalette, CoverPaths, FolderWatcher,
    LibraryStore, LyricsDoc, PlayStats, SessionStore, migrate_playlist_covers,
};
use rustygrade_linux::{
    ArtworkProviders, DeviceReservation, DiscordRpcService, DiscordRpcStatus, MprisCommand,
    MprisService, PowerProfile, PowerProfileMonitor, RenderTier, SingleInstanceCommand,
    TrayCommand, TrayService, resolve_client_id,
};
use tokio::sync::mpsc;
use tracing::{info, warn};

pub use types::*;

use crate::crossfade::CrossfadeManager;
use crate::playback::{PlayTracker, PlaybackQueue};
use library::{ScanJob, ScanOutcome};
use persist::{PersistMsg, SavedState, spawn_persist_worker};
use playlists::PlaylistIoDone;
use tags::TagBatchResult;

pub(super) struct Coordinator {
    settings: AppSettings,
    mpv: MpvEngine,
    mpv_rx: Option<mpsc::UnboundedReceiver<EngineEvent>>,
    crossfade: CrossfadeManager,
    hw_writer: CoalescedHwWriter,
    reservation: Option<DeviceReservation>,
    pw_bridge: Option<PipeWireBridge>,
    pw_rx: Option<mpsc::UnboundedReceiver<BridgeEvent>>,
    pw_bridge_node_id: Option<u32>,
    direct_card: Option<i32>,
    direct_control: Option<String>,
    output_override: Option<String>,
    output_retries: u32,
    volume_lock_told: bool,
    mpv_next: Option<usize>,
    last_playback_emit: Option<(Instant, i64, bool, f64)>,
    playback_tick_mode: PlaybackTickMode,
    mpris: Option<MprisService>,
    mpris_rx: Option<mpsc::UnboundedReceiver<MprisCommand>>,
    discord_rpc: Option<DiscordRpcService>,
    discord_status_rx: Option<mpsc::UnboundedReceiver<DiscordRpcStatus>>,
    tray: Option<TrayService>,
    tray_rx: Option<mpsc::UnboundedReceiver<TrayCommand>>,
    watcher: Option<FolderWatcher>,
    watch_rx: Option<mpsc::UnboundedReceiver<Vec<PathBuf>>>,
    suppress_watch_paths: HashMap<PathBuf, Instant>,
    pending_watch_paths: Vec<PathBuf>,
    pending_manual_paths: Vec<PathBuf>,
    pending_startup_sync: bool,
    pending_full_rescan: bool,
    pending_cover_resync: bool,
    pending_prune: Option<Vec<PathBuf>>,
    scan_job: Option<ScanJob>,
    playback_failures: u32,
    pending_open_play: Vec<PathBuf>,
    scan_cancel: Option<Arc<AtomicBool>>,
    scan_done_rx: Option<mpsc::UnboundedReceiver<Result<ScanOutcome, String>>>,
    lyrics_done_rx: Option<mpsc::UnboundedReceiver<(String, Option<LyricsDoc>)>>,
    accent_done_rx: Option<mpsc::UnboundedReceiver<(String, Option<CoverPalette>)>>,
    tag_done_tx: mpsc::UnboundedSender<TagBatchResult>,
    tag_done_rx: Option<mpsc::UnboundedReceiver<TagBatchResult>>,
    tag_save_in_flight: bool,
    playlist_io_done_tx: mpsc::UnboundedSender<PlaylistIoDone>,
    playlist_io_done_rx: Option<mpsc::UnboundedReceiver<PlaylistIoDone>>,
    playlist_import_in_flight: bool,
    persist_tx: mpsc::UnboundedSender<PersistMsg>,
    power_profile: PowerProfile,
    render_tier: RenderTier,
    albums: Arc<[Album]>,
    collections: CollectionsData,
    live_playlist_covers: Arc<Mutex<HashSet<PathBuf>>>,
    pending_import_covers: Arc<Mutex<HashSet<PathBuf>>>,
    stats: PlayStats,
    queue: PlaybackQueue,
    play_tracker: PlayTracker,
    mpv_loaded_track_id: Option<String>,
    pending_restore_seek: Option<f64>,
    crossfade_attempted_track_id: Option<String>,
    is_playing: bool,
    position_s: f64,
    duration_s: f64,
    shared_vol: f64,
    shared_muted: bool,
    current_vol: f64,
    current_muted: bool,
    last_audio_path: AudioPath,
    last_hw_params: Option<AlsaHwParams>,
    last_sample_alteration: Option<String>,
    settings_dirty_since: Option<Instant>,
    session_last_saved: Instant,
    ui_tx: mpsc::UnboundedSender<ControllerEvent>,
}

pub(super) async fn recv_opt<T>(rx: &mut Option<mpsc::UnboundedReceiver<T>>) -> Option<T> {
    match rx.as_mut() {
        Some(receiver) => receiver.recv().await,
        None => pending().await,
    }
}

/// Spawns the backend event loop coordinating playback, audio devices, scans, and OS integrations.
pub async fn run_coordinator(
    mut cmd_rx: mpsc::UnboundedReceiver<ControllerCommand>,
    ui_tx: mpsc::UnboundedSender<ControllerEvent>,
    mut startup: StartupContext,
) {
    let cold_open_uris = std::mem::take(&mut startup.open_uris);

    let library_ui_tx = ui_tx.clone();
    let library_task = tokio::task::spawn_blocking(move || {
        if let Err(e) = LibraryStore::recover_stashed_covers() {
            warn!(error = %e, "Recovering stashed covers failed");
        }
        let loaded = LibraryStore::load().map(Arc::<[Album]>::from);
        if let Ok(albums) = &loaded {
            let _ = library_ui_tx.send(ControllerEvent::LibraryLoaded(Arc::clone(albums)));
        }
        loaded.map_err(|e| e.to_string())
    });
    let saved_state_task: tokio::task::JoinHandle<SavedState> = tokio::task::spawn_blocking(|| {
        let mut collections = CollectionsStore::load_or_import();
        if migrate_playlist_covers(&mut collections) {
            CollectionsStore::save(&collections);
        }
        (
            collections,
            PlayStats::load(),
            SessionStore::load(),
            SessionStore::load_original_queue(),
        )
    });

    let (mpv_tx, mpv_rx) = mpsc::unbounded_channel();
    let mut mpv = MpvEngine::new(mpv_tx);
    let hw_writer = CoalescedHwWriter::new();

    let enable_mpris = startup.settings.enable_mpris;
    let enable_tray = startup.settings.enable_tray;
    let ((power_profile, ppd_rx), mpris_res, tray_res) = tokio::join!(
        PowerProfileMonitor::start(),
        async move { enable_mpris.then_some(MprisService::start().await) },
        async move { enable_tray.then_some(TrayService::start().await) },
    );
    let mut ppd_rx = Some(ppd_rx);
    let render_tier = RenderTier::resolve(startup.settings.performance_mode, power_profile);
    mpv.set_poll_interval(Duration::from_millis(render_tier.poll_interval_ms));
    let _ = ui_tx.send(ControllerEvent::PowerTierChanged {
        profile: power_profile,
        tier: render_tier,
    });

    let (mpris, mpris_rx) = match mpris_res {
        Some(Ok((svc, rx))) => (Some(svc), Some(rx)),
        Some(Err(err)) => {
            warn!(error = %err, "MPRIS disabled/unavailable");
            (None, None)
        }
        None => (None, None),
    };

    let (tray, tray_rx) = match tray_res {
        Some(Ok((svc, rx))) => (Some(svc), Some(rx)),
        Some(Err(err)) => {
            warn!(error = %err, "System tray unavailable");
            (None, None)
        }
        None => (None, None),
    };

    let artwork_providers = ArtworkProviders::from_settings(&startup.settings);
    rustygrade_linux::artwork_lookup::shared_resolver().set_providers(artwork_providers.clone());

    let (discord_rpc, discord_status_rx) = if startup.settings.enable_discord_rpc {
        let (tx, rx) = mpsc::unbounded_channel();
        let client_id = resolve_client_id(&startup.settings.discord_client_id);
        let svc = DiscordRpcService::start(
            client_id,
            startup.settings.discord_fetch_artwork,
            artwork_providers,
            CoverPaths::app_dir().join("discord-art-cache.json"),
            tx,
        );
        (Some(svc), Some(rx))
    } else {
        (None, None)
    };

    let persist_tx = spawn_persist_worker();
    let (tag_done_tx, tag_done_rx) = mpsc::unbounded_channel();
    let (playlist_io_done_tx, playlist_io_done_rx) = mpsc::unbounded_channel();
    let initial_vol = startup.settings.volume.clamp(0.0, 1.0);
    let eq_filters = EqualizerConfig::build_filters(
        startup.settings.eq_enabled,
        false,
        &startup.settings.eq_gains,
    );
    let crossfade = CrossfadeManager::new(
        startup.settings.audio_device.clone(),
        startup.settings.replay_gain.clone(),
        eq_filters,
    );

    if !crate::popups::CROSSFADE_CHOICES.contains(&startup.settings.crossfade_seconds) {
        startup.settings.crossfade_seconds = 0;
    }
    let mut coord = Coordinator {
        settings: startup.settings,
        mpv,
        mpv_rx: Some(mpv_rx),
        crossfade,
        hw_writer,
        reservation: None,
        pw_bridge: None,
        pw_rx: None,
        pw_bridge_node_id: None,
        direct_card: None,
        direct_control: None,
        output_override: None,
        output_retries: 0,
        volume_lock_told: false,
        mpv_next: None,
        last_playback_emit: None,
        playback_tick_mode: PlaybackTickMode::Active,
        mpris,
        mpris_rx,
        discord_rpc,
        discord_status_rx,
        tray,
        tray_rx,
        watcher: None,
        watch_rx: None,
        suppress_watch_paths: HashMap::new(),
        pending_watch_paths: Vec::new(),
        pending_manual_paths: Vec::new(),
        pending_startup_sync: false,
        pending_full_rescan: false,
        pending_cover_resync: false,
        pending_prune: None,
        scan_job: None,
        playback_failures: 0,
        pending_open_play: Vec::new(),
        scan_cancel: None,
        scan_done_rx: None,
        lyrics_done_rx: None,
        accent_done_rx: None,
        tag_done_tx,
        tag_done_rx: Some(tag_done_rx),
        tag_save_in_flight: false,
        playlist_io_done_tx,
        playlist_io_done_rx: Some(playlist_io_done_rx),
        playlist_import_in_flight: false,
        persist_tx,
        power_profile,
        render_tier,
        albums: Arc::from(Vec::new()),
        collections: CollectionsData::default(),
        live_playlist_covers: Arc::new(Mutex::new(HashSet::new())),
        pending_import_covers: Arc::new(Mutex::new(HashSet::new())),
        stats: PlayStats::default(),
        queue: PlaybackQueue::default(),
        play_tracker: PlayTracker::default(),
        mpv_loaded_track_id: None,
        pending_restore_seek: None,
        crossfade_attempted_track_id: None,
        is_playing: false,
        position_s: 0.0,
        duration_s: 0.0,
        shared_vol: initial_vol,
        shared_muted: false,
        current_vol: initial_vol,
        current_muted: false,
        last_audio_path: AudioPath {
            input: None,
            output: None,
        },
        last_hw_params: None,
        last_sample_alteration: None,
        settings_dirty_since: None,
        session_last_saved: Instant::now(),
        ui_tx,
    };

    let _ = coord
        .ui_tx
        .send(ControllerEvent::TrayAvailable(coord.tray.is_some()));
    if coord.discord_rpc.is_none() {
        let _ = coord.ui_tx.send(ControllerEvent::DiscordRpcStatus(
            DiscordRpcStatus::Disabled,
        ));
    }
    coord.sync_tray_toggles().await;
    coord
        .load_initial_state(library_task, saved_state_task)
        .await;
    coord.sync_discord_rpc();
    coord.initialize_audio_output().await;
    coord.refresh_devices().await;
    coord.sync_folder_watcher().await;
    if !cold_open_uris.is_empty() {
        coord.open_uris(cold_open_uris).await;
    }
    coord.start_startup_sync();

    let mut hw_poll_tick = tokio::time::interval(Duration::from_secs(1));
    hw_poll_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut fade_interval = tokio::time::interval(Duration::from_millis(40));
    fade_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            cmd = cmd_rx.recv() => {
                match cmd {
                    Some(ControllerCommand::Shutdown) | None => {
                        coord.shutdown().await;
                        let _ = coord.ui_tx.send(ControllerEvent::ShutdownComplete);
                        break;
                    }
                    Some(c) => coord.handle_command(c).await,
                }
            }
            evt = recv_opt(&mut coord.mpv_rx) => {
                match evt {
                    Some(e) => coord.handle_mpv_event(e).await,
                    None => coord.mpv_rx = None,
                }
            }
            spare_evt = recv_opt(&mut coord.crossfade.spare_rx) => {
                match spare_evt {
                    Some(e) => coord.handle_spare_mpv_event(e).await,
                    None => {
                        coord.crossfade.spare_rx = None;
                        if coord.crossfade.is_active() {
                            coord.crossfade.cancel(&coord.mpv, coord.shared_vol, coord.shared_muted);
                        }
                    }
                }
            }
            _ = fade_interval.tick(), if coord.crossfade.is_active() => {
                coord.step_crossfade().await;
            }
            pw_evt = recv_opt(&mut coord.pw_rx) => {
                match pw_evt {
                    Some(e) => coord.handle_pw_bridge_event(e).await,
                    None => {
                        coord.pw_rx = None;
                        coord.pw_bridge = None;
                        coord.pw_bridge_node_id = None;
                        coord.emit_direct_status();
                    }
                }
            }
            ppd = recv_opt(&mut ppd_rx) => {
                match ppd {
                    Some(profile) => {
                        coord.power_profile = profile;
                        coord.apply_render_tier();
                    }
                    None => ppd_rx = None,
                }
            }
            mpris_cmd = recv_opt(&mut coord.mpris_rx) => {
                match mpris_cmd {
                    Some(mc) => coord.handle_mpris_command(mc).await,
                    None => coord.mpris_rx = None,
                }
            }
            discord_st = recv_opt(&mut coord.discord_status_rx) => {
                match discord_st {
                    Some(st) => {
                        let _ = coord.ui_tx.send(ControllerEvent::DiscordRpcStatus(st));
                    }
                    None => coord.discord_status_rx = None,
                }
            }
            tray_cmd = recv_opt(&mut coord.tray_rx) => {
                match tray_cmd {
                    Some(tc) => coord.handle_tray_command(tc).await,
                    None => {
                        coord.tray_rx = None;
                        coord.tray = None;
                        let _ = coord.ui_tx.send(ControllerEvent::TrayAvailable(false));
                    }
                }
            }
            si_cmd = recv_opt(&mut startup.single_instance_rx) => {
                match si_cmd {
                    Some(SingleInstanceCommand::Raise) => {
                        let _ = coord.ui_tx.send(ControllerEvent::WindowCommand(WindowAction::Raise));
                    }
                    Some(SingleInstanceCommand::OpenUris(uris)) => {
                        coord.open_uris(uris).await;
                        let _ = coord.ui_tx.send(ControllerEvent::WindowCommand(WindowAction::Raise));
                    }
                    None => startup.single_instance_rx = None,
                }
            }
            watch_paths = recv_opt(&mut coord.watch_rx) => {
                match watch_paths {
                    Some(paths) => coord.handle_watched_paths(paths),
                    None => coord.watch_rx = None,
                }
            }
            scan_res = recv_opt(&mut coord.scan_done_rx) => {
                coord.handle_scan_done(scan_res).await;
            }
            lyrics_res = recv_opt(&mut coord.lyrics_done_rx) => {
                match lyrics_res {
                    Some((track_id, doc)) => {
                        coord.lyrics_done_rx = None;
                        if coord.queue.current().is_some_and(|t| t.id == track_id) {
                            let _ = coord.ui_tx.send(ControllerEvent::LyricsUpdated {
                                track_id: Some(track_id),
                                lyrics: doc,
                            });
                        }
                    }
                    None => coord.lyrics_done_rx = None,
                }
            }
            accent_res = recv_opt(&mut coord.accent_done_rx) => {
                match accent_res {
                    Some((track_id, palette)) => {
                        coord.accent_done_rx = None;
                        if coord.queue.current().is_some_and(|t| t.id == track_id) {
                            let _ = coord
                                .ui_tx
                                .send(ControllerEvent::AccentPaletteChanged(palette));
                        }
                    }
                    None => coord.accent_done_rx = None,
                }
            }
            tag_res = recv_opt(&mut coord.tag_done_rx) => {
                match tag_res {
                    Some(res) => coord.handle_tag_done(res).await,
                    None => coord.tag_done_rx = None,
                }
            }
            pl_res = recv_opt(&mut coord.playlist_io_done_rx) => {
                match pl_res {
                    Some(done) => coord.handle_playlist_io_done(done).await,
                    None => coord.playlist_io_done_rx = None,
                }
            }
            _ = hw_poll_tick.tick() => {
                coord.poll_external_mixer_and_save().await;
            }
        }
    }
}

impl Coordinator {
    async fn handle_command(&mut self, cmd: ControllerCommand) {
        match cmd {
            ControllerCommand::PlayTrackList {
                tracks,
                start_index,
            } => {
                self.play_track_list(tracks, start_index).await;
            }
            ControllerCommand::JumpToQueueIndex(idx) => self.jump_to_queue_index(idx).await,
            ControllerCommand::PlayNextTracks(tracks) => {
                self.enqueue_with_notice(tracks, true).await
            }
            ControllerCommand::EnqueueTracks(tracks) => {
                self.enqueue_with_notice(tracks, false).await
            }
            ControllerCommand::RemoveQueueIndex(idx) => self.remove_queue_index(idx).await,
            ControllerCommand::ClearQueue => self.clear_queue().await,
            ControllerCommand::TogglePlayPause => self.toggle_play_pause().await,
            ControllerCommand::Next => self.next_track().await,
            ControllerCommand::Previous => self.previous_track().await,
            ControllerCommand::ToggleShuffle => self.toggle_shuffle_mode().await,
            ControllerCommand::CycleRepeat => self.cycle_repeat_mode().await,
            ControllerCommand::SeekSeconds(sec) => self.seek_seconds(sec).await,
            ControllerCommand::SetPlaybackTickMode(mode) => self.set_playback_tick_mode(mode),
            ControllerCommand::SetVolume(vol) => self.apply_volume(vol, true).await,
            ControllerCommand::ToggleMute => self.apply_mute(!self.current_muted, true),
            ControllerCommand::SelectAudioDevice(dev) => self.switch_output_device(dev).await,
            ControllerCommand::RefreshDevices => self.refresh_devices().await,
            ControllerCommand::UpdateSettings(next) => self.apply_updated_settings(next).await,
            ControllerCommand::SetEqEnabled(enabled) => self.set_eq_enabled(enabled).await,
            ControllerCommand::SelectEqPreset(name) => self.select_eq_preset(name).await,
            ControllerCommand::SetEqBand { band, gain } => self.set_eq_band(band, gain),
            ControllerCommand::SetReplayGain(mode) => self.set_replay_gain(mode).await,
            ControllerCommand::SetCrossfadeSeconds(secs) => self.set_crossfade_seconds(secs),
            ControllerCommand::ToggleLike(id) => self.toggle_like(id),
            ControllerCommand::ToggleLikeTracks(ids) => self.toggle_like_tracks(ids),
            ControllerCommand::CreatePlaylist(name) => self.create_playlist(&name),
            ControllerCommand::DeletePlaylist(id) => self.delete_playlist(id).await,
            ControllerCommand::RenamePlaylist { id, name } => self.rename_playlist(id, &name),
            ControllerCommand::AddTracksToPlaylist {
                playlist_id,
                track_ids,
            } => {
                self.add_tracks_to_playlist(playlist_id, track_ids);
            }
            ControllerCommand::TogglePlaylistTracks {
                playlist_id,
                track_ids,
            } => {
                self.toggle_playlist_tracks(playlist_id, track_ids);
            }
            ControllerCommand::RemoveTrackFromPlaylist {
                playlist_id,
                track_id,
            } => {
                self.remove_track_from_playlist(playlist_id, &track_id);
            }
            ControllerCommand::SetPlaylistOrder {
                playlist_id,
                track_ids,
            } => {
                self.set_playlist_order(playlist_id, &track_ids);
            }
            ControllerCommand::SetLikedOrder(ids) => self.set_liked_order(&ids),
            ControllerCommand::SetAlbumOrder {
                name,
                artist,
                track_ids,
            } => {
                self.set_album_order(name, artist, track_ids).await;
            }
            ControllerCommand::SetPlaylistCover {
                playlist_id,
                image_path,
            } => {
                self.set_playlist_cover(playlist_id, image_path).await;
            }
            ControllerCommand::RemovePlaylistCover(id) => self.remove_playlist_cover(id).await,
            ControllerCommand::SaveSmartPlaylist(sp) => self.save_smart_playlist(sp),
            ControllerCommand::DeleteSmartPlaylist(id) => self.delete_smart_playlist(id),
            ControllerCommand::ExportPlaylistM3u8 { target, dest } => {
                self.start_export_m3u8(target, dest)
            }
            ControllerCommand::ExportPlaylistZip { playlist_id, dest } => {
                self.start_export_playlist_zip(playlist_id, dest);
            }
            ControllerCommand::ExportCollectionsZip { dest } => {
                self.start_export_collections_zip(dest)
            }
            ControllerCommand::ImportPlaylistM3u { path } => self.start_import_m3u(path),
            ControllerCommand::ImportCollectionsZip { path, replace } => {
                self.start_import_zip(path, replace)
            }
            ControllerCommand::DeleteAlbums(targets) => self.delete_albums(targets).await,
            ControllerCommand::ScanFolders(folders) => self.scan_folders(folders).await,
            ControllerCommand::ScanFiles(files) => self.scan_files(files),
            ControllerCommand::ImportPaths(paths) => self.import_paths(paths).await,
            ControllerCommand::RemoveLibraryFolder(folder) => {
                self.settings.library_folders.retain(|f| f != &folder);
                self.save_settings_async();
                let _ = self
                    .ui_tx
                    .send(ControllerEvent::SettingsSaved(self.settings.clone()));
                self.sync_folder_watcher().await;
            }
            ControllerCommand::RescanLibrary => self.rescan_library(),
            ControllerCommand::ResyncCovers => self.resync_covers(),
            ControllerCommand::PruneMissing { roots } => {
                if !roots.is_empty() {
                    if self.scan_cancel.is_some() {
                        self.pending_prune = Some(roots);
                    } else {
                        self.start_scan_paths(
                            roots,
                            library::ScanKind::Manual,
                            rustygrade_core::scanner::PrunePolicy::ForceAll,
                        );
                    }
                }
            }
            ControllerCommand::SaveTags {
                paths,
                album_key,
                edit,
            } => {
                self.save_tags(paths, album_key, edit).await;
            }
            ControllerCommand::Shutdown => {}
        }
    }

    pub(super) fn apply_render_tier(&mut self) {
        self.render_tier = RenderTier::resolve(self.settings.performance_mode, self.power_profile);
        let interval = Duration::from_millis(self.render_tier.poll_interval_ms);
        self.mpv.set_poll_interval(interval);
        self.crossfade.set_poll_interval(interval);
        let _ = self.ui_tx.send(ControllerEvent::PowerTierChanged {
            profile: self.power_profile,
            tier: self.render_tier,
        });
    }

    pub(super) fn notice(&self, kind: NoticeKind, text: impl Into<String>) {
        let _ = self.ui_tx.send(ControllerEvent::Notice {
            kind,
            text: text.into(),
        });
    }

    async fn shutdown(&mut self) {
        if let Some(cancel) = &self.scan_cancel {
            cancel.store(true, Ordering::Release);
        }
        self.watcher = None;
        self.watch_rx = None;

        // Briefly drain in-flight tag writes or imports before flushing state to disk.
        let deadline = tokio::time::Instant::now() + Duration::from_millis(350);
        if self.tag_save_in_flight
            && let Some(rx) = self.tag_done_rx.as_mut()
            && let Ok(Some(res)) = tokio::time::timeout_at(deadline, rx.recv()).await
        {
            self.handle_tag_done(res).await;
        }
        if self.playlist_import_in_flight
            && let Some(rx) = self.playlist_io_done_rx.as_mut()
            && let Ok(Some(done)) = tokio::time::timeout_at(deadline, rx.recv()).await
        {
            self.handle_playlist_io_done(done).await;
        }

        let (state, active_ids, original_ids) = self.queue.to_session(self.position_s);
        let _ = self.persist_tx.send(PersistMsg::SessionQueue {
            state,
            active_ids,
            original_ids,
        });
        if self.settings_dirty_since.take().is_some() {
            let _ = self
                .persist_tx
                .send(PersistMsg::Settings(self.settings.clone()));
        }
        self.flush_persist().await;

        self.crossfade.shutdown_async().await;
        self.mpv.shutdown_async().await;
        self.teardown_direct_resources().await;
        if let Some(t) = self.tray.take() {
            t.shutdown().await;
        }
        if let Some(rpc) = self.discord_rpc.take() {
            rpc.shutdown().await;
        }
        self.mpris = None;
        info!("Controller shut down cleanly");
    }
}
