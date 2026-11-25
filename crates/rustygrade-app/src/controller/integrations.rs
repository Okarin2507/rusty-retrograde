use rustygrade_audio::AlsaDirect;
use rustygrade_core::{AppSettings, CoverPaths, DirectVolumeBridge};
use rustygrade_linux::{
    ArtworkProviders, DiscordPlaybackStatus, DiscordPresence, DiscordRpcService, DiscordRpcStatus,
    MprisCommand, MprisService, MprisState, TrayCommand, TrayService, TrayToggles,
    resolve_client_id,
};
use tokio::sync::mpsc;

use super::Coordinator;
use super::types::{ControllerEvent, WindowAction};
use crate::playback::RepeatMode;

impl Coordinator {
    pub(super) async fn handle_mpris_command(&mut self, cmd: MprisCommand) {
        match cmd {
            MprisCommand::Play => self.play_or_resume().await,
            MprisCommand::Pause => {
                if self.crossfade.is_active() {
                    self.crossfade
                        .cancel(&self.mpv, self.shared_vol, self.shared_muted);
                }
                self.mpv.set_paused(true);
            }
            MprisCommand::PlayPause => self.toggle_play_pause().await,
            MprisCommand::Stop => {
                self.crossfade
                    .cancel(&self.mpv, self.shared_vol, self.shared_muted);
                self.mpv.stop_playback();
                self.stop_playback_keep_track(true, true).await;
            }
            MprisCommand::Next => self.next_track().await,
            MprisCommand::Previous => self.previous_track().await,
            MprisCommand::SeekOffsetUs(off) => {
                let target = (self.position_s + off as f64 / 1_000_000.0).max(0.0);
                self.seek_seconds(target).await;
            }
            MprisCommand::SetPositionUs(pos) => {
                self.seek_seconds((pos as f64 / 1_000_000.0).max(0.0)).await;
            }
            MprisCommand::SetVolume(vol) => self.apply_volume(vol, true).await,
            MprisCommand::SetLoopStatus(status) => {
                self.set_repeat_mode(RepeatMode::from_mpris_loop_status(&status))
                    .await;
            }
            MprisCommand::SetShuffle(shuffle) => self.set_shuffle_mode(shuffle).await,
            MprisCommand::Raise => {
                let _ = self
                    .ui_tx
                    .send(ControllerEvent::WindowCommand(WindowAction::Raise));
            }
            MprisCommand::Quit => {
                let _ = self
                    .ui_tx
                    .send(ControllerEvent::WindowCommand(WindowAction::Quit));
            }
        }
    }

    pub(super) async fn handle_tray_command(&mut self, cmd: TrayCommand) {
        match cmd {
            TrayCommand::ToggleWindow => {
                let _ = self
                    .ui_tx
                    .send(ControllerEvent::WindowCommand(WindowAction::Toggle));
            }
            TrayCommand::ShowWindow => {
                let _ = self
                    .ui_tx
                    .send(ControllerEvent::WindowCommand(WindowAction::Raise));
            }
            TrayCommand::ToggleMiniPlayer => {
                let _ = self.ui_tx.send(ControllerEvent::WindowCommand(
                    WindowAction::ToggleMiniPlayer,
                ));
            }
            TrayCommand::RescanLibrary => self.rescan_library(),
            TrayCommand::ToggleMinimizeToTray => {
                let on = self.settings.minimize_to_tray != Some(false);
                let mut next = self.settings.clone();
                next.minimize_to_tray = Some(!on);
                self.apply_updated_settings(next).await;
            }
            TrayCommand::ToggleWatchFolders => {
                let mut next = self.settings.clone();
                next.watch_folders = !next.watch_folders;
                self.apply_updated_settings(next).await;
            }
            TrayCommand::ToggleAccentFromCover => {
                let mut next = self.settings.clone();
                next.accent_from_cover = !next.accent_from_cover;
                self.apply_updated_settings(next).await;
            }
            TrayCommand::Quit => {
                let _ = self
                    .ui_tx
                    .send(ControllerEvent::WindowCommand(WindowAction::Quit));
            }
        }
    }

    pub(super) async fn apply_updated_settings(&mut self, mut next: AppSettings) {
        let prev = self.settings.clone();
        next.volume = prev.volume;
        next.audio_device = prev.audio_device.clone();
        next.library_folders = prev.library_folders.clone();
        next.eq_enabled = prev.eq_enabled;
        next.eq_preset = prev.eq_preset.clone();
        next.eq_gains = prev.eq_gains.clone();
        next.replay_gain = prev.replay_gain.clone();
        next.crossfade_seconds = prev.crossfade_seconds;
        self.settings = next.clone();

        if prev.performance_mode != next.performance_mode {
            self.apply_render_tier();
        }
        if prev.watch_folders != next.watch_folders {
            self.sync_folder_watcher().await;
        }
        if prev.accent_from_cover != next.accent_from_cover {
            let cur = self.queue.current().cloned();
            self.load_accent_async(cur.as_ref());
        }

        if prev.direct_volume_bridge != next.direct_volume_bridge && self.direct_card.is_some() {
            match next.direct_volume_bridge {
                DirectVolumeBridge::VirtualSink => {
                    let dev = self.settings.audio_device.clone();
                    let desc = tokio::task::spawn_blocking(move || {
                        AlsaDirect::list_devices()
                            .into_iter()
                            .find(|d| d.name == dev)
                            .map(|d| d.description)
                    })
                    .await
                    .ok()
                    .flatten()
                    .unwrap_or_else(|| "ALSA DAC".to_owned());
                    self.start_pw_bridge(&desc).await;
                }
                DirectVolumeBridge::HardwareOnly => {
                    self.pw_rx = None;
                    self.pw_bridge_node_id = None;
                    if let Some(mut b) = self.pw_bridge.take() {
                        let _ = tokio::task::spawn_blocking(move || b.shutdown()).await;
                    }
                }
            }
            self.emit_direct_status();
        }

        if prev.enable_mpris != next.enable_mpris {
            if next.enable_mpris && self.mpris.is_none() {
                if let Ok((svc, rx)) = MprisService::start().await {
                    self.mpris = Some(svc);
                    self.mpris_rx = Some(rx);
                    self.sync_mpris().await;
                }
            } else if !next.enable_mpris {
                self.mpris_rx = None;
                self.mpris = None;
            }
        }

        let prev_providers = ArtworkProviders::from_settings(&prev);
        let next_providers = ArtworkProviders::from_settings(&next);
        if prev_providers != next_providers {
            rustygrade_linux::artwork_lookup::shared_resolver()
                .set_providers(next_providers.clone());
        }

        if prev.enable_discord_rpc != next.enable_discord_rpc {
            if next.enable_discord_rpc {
                if let Some(old_rpc) = self.discord_rpc.take() {
                    old_rpc.shutdown().await;
                }
                let _ = self.ui_tx.send(ControllerEvent::DiscordRpcStatus(
                    DiscordRpcStatus::WaitingForDiscord,
                ));
                let (tx, rx) = mpsc::unbounded_channel();
                let client_id = resolve_client_id(&next.discord_client_id);
                self.discord_rpc = Some(DiscordRpcService::start(
                    client_id,
                    next.discord_fetch_artwork,
                    next_providers,
                    CoverPaths::app_dir().join("discord-art-cache.json"),
                    tx,
                ));
                self.discord_status_rx = Some(rx);
                self.sync_discord_rpc();
            } else {
                self.discord_status_rx = None;
                if let Some(rpc) = self.discord_rpc.take() {
                    rpc.shutdown().await;
                }
                let _ = self.ui_tx.send(ControllerEvent::DiscordRpcStatus(
                    DiscordRpcStatus::Disabled,
                ));
            }
        } else if let Some(rpc) = &self.discord_rpc {
            if prev.discord_client_id != next.discord_client_id {
                rpc.set_client_id(resolve_client_id(&next.discord_client_id));
            }
            if prev.discord_fetch_artwork != next.discord_fetch_artwork {
                rpc.set_artwork_enabled(next.discord_fetch_artwork);
            }
            if prev_providers != next_providers {
                rpc.set_artwork_providers(next_providers);
            }
        }

        if prev.enable_tray != next.enable_tray {
            if next.enable_tray && self.tray.is_none() {
                if let Ok((svc, rx)) = TrayService::start().await {
                    self.tray = Some(svc);
                    self.tray_rx = Some(rx);
                    self.sync_tray().await;
                }
            } else if !next.enable_tray {
                self.tray_rx = None;
                if let Some(t) = self.tray.take() {
                    t.shutdown().await;
                }
            }
            let _ = self
                .ui_tx
                .send(ControllerEvent::TrayAvailable(self.tray.is_some()));
        }

        self.settings_dirty_since = None;
        self.save_settings_async();
        self.sync_tray_toggles().await;
        let _ = self.ui_tx.send(ControllerEvent::SettingsSaved(next));
    }

    pub(super) async fn set_mpris_position(&self, seconds: f64) {
        if let Some(mpris) = &self.mpris {
            mpris
                .update_position((seconds.max(0.0) * 1_000_000.0) as i64)
                .await;
        }
    }

    pub(super) fn sync_discord_rpc(&self) {
        let Some(rpc) = &self.discord_rpc else {
            return;
        };
        let track = self.queue.current();
        let status = if track.is_none() || (self.mpv_loaded_track_id.is_none() && !self.is_playing)
        {
            DiscordPlaybackStatus::Stopped
        } else if self.is_playing {
            DiscordPlaybackStatus::Playing
        } else {
            DiscordPlaybackStatus::Paused
        };
        rpc.update_presence(DiscordPresence {
            status,
            title: track.map(|t| t.title.clone()).unwrap_or_default(),
            artist: track.map(|t| t.artist.clone()).unwrap_or_default(),
            album: track.map(|t| t.album.clone()).unwrap_or_default(),
            position_s: self.position_s.max(0.0),
            duration_s: self.duration_s.max(0.0),
            artwork_url: None,
        });
    }

    pub(super) async fn sync_mpris(&self) {
        self.sync_discord_rpc();
        let Some(mpris) = &self.mpris else {
            return;
        };
        let track = self.queue.current();
        let status = if track.is_none() || (self.mpv_loaded_track_id.is_none() && !self.is_playing)
        {
            "Stopped"
        } else if self.is_playing {
            "Playing"
        } else {
            "Paused"
        };
        let art_url = track
            .and_then(|t| {
                t.cover_full_path
                    .as_deref()
                    .or(t.cover_thumb_path.as_deref())
            })
            .map(CoverPaths::to_file_url);
        let state = MprisState {
            playback_status: status.to_owned(),
            track_id: track.map(|t| t.id.clone()),
            title: track.map(|t| t.title.clone()),
            artist: track.map(|t| t.artist.clone()),
            album: track.map(|t| t.album.clone()),
            art_url,
            length_us: (self.duration_s * 1_000_000.0) as i64,
            position_us: (self.position_s * 1_000_000.0) as i64,
            volume: self.current_vol,
            can_go_next: self.queue.can_go_next(),
            can_go_previous: self.queue.can_go_previous(),
            can_play: true,
            can_pause: true,
            can_seek: track.is_some(),
            loop_status: self.queue.repeat().mpris_loop_status().to_owned(),
            shuffle: self.queue.shuffle(),
        };
        mpris.update_state(state).await;
    }

    pub(super) async fn sync_tray(&self) {
        let Some(tray) = &self.tray else {
            return;
        };
        let title = self
            .queue
            .current()
            .map(|t| format!("Rustygrade — {} - {}", t.artist, t.title))
            .unwrap_or_else(|| "Rustygrade".to_owned());
        tray.update_title(title).await;
    }

    pub(super) async fn sync_tray_toggles(&self) {
        let Some(tray) = &self.tray else {
            return;
        };
        tray.update_toggles(TrayToggles {
            minimize_to_tray: self.settings.minimize_to_tray != Some(false),
            watch_folders: self.settings.watch_folders,
            accent_from_cover: self.settings.accent_from_cover,
        })
        .await;
    }
}
