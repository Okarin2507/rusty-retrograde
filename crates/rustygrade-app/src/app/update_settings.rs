use std::time::{Duration, Instant};

use iced::{Point, Task};
use rustygrade_linux::{ArtworkProviders, RenderTier, pick_music_files, pick_music_folders};

use super::{App, Message, batch_tasks};
use crate::context_menu;
use crate::controller::{ControllerCommand, NoticeKind};
use crate::egg;
use crate::motion;
use crate::reorder;
use crate::smooth_scroll;

impl App {
    pub(super) fn handle_settings_and_ui_message(&mut self, msg: Message) -> Task<Message> {
        match msg {
            Message::SetEqEnabled(enabled) => {
                self.settings.eq_enabled = enabled;
                let _ = self.cmd_tx.send(ControllerCommand::SetEqEnabled(enabled));
                Task::none()
            }
            Message::SelectEqPreset(preset) => {
                let _ = self.cmd_tx.send(ControllerCommand::SelectEqPreset(preset));
                Task::none()
            }
            Message::SetEqBand { band, gain } => {
                if self.settings.eq_gains.len() < 10 {
                    self.settings.eq_gains.resize(10, 0.0);
                }
                if band < 10 {
                    self.settings.eq_gains[band] = gain;
                    self.settings.eq_preset = "CUSTOM".to_owned();
                }
                let _ = self
                    .cmd_tx
                    .send(ControllerCommand::SetEqBand { band, gain });
                Task::none()
            }
            Message::SelectReplayGain(choice) => {
                self.settings.replay_gain = choice.as_str().to_owned();
                let _ = self
                    .cmd_tx
                    .send(ControllerCommand::SetReplayGain(choice.as_str().to_owned()));
                Task::none()
            }
            Message::SetCrossfadeSeconds(secs) => {
                self.settings.crossfade_seconds = secs;
                let _ = self
                    .cmd_tx
                    .send(ControllerCommand::SetCrossfadeSeconds(secs));
                Task::none()
            }
            Message::PickFiles => {
                self.add_music_menu_open = false;
                Task::perform(pick_music_files(), Message::FilesPicked)
            }
            Message::FilesPicked(Ok(files)) => {
                if !files.is_empty() {
                    let _ = self.cmd_tx.send(ControllerCommand::ScanFiles(files));
                }
                Task::none()
            }
            Message::FilesPicked(Err(err)) => self.toasts.push(NoticeKind::Error, err),
            Message::PickFolders => {
                self.add_music_menu_open = false;
                Task::perform(pick_music_folders(), Message::FoldersPicked)
            }
            Message::FoldersPicked(Ok(folders)) => {
                if !folders.is_empty() {
                    let _ = self.cmd_tx.send(ControllerCommand::ScanFolders(folders));
                }
                Task::none()
            }
            Message::FoldersPicked(Err(err)) => self.toasts.push(NoticeKind::Error, err),
            Message::RemoveLibraryFolder(f) => {
                let _ = self.cmd_tx.send(ControllerCommand::RemoveLibraryFolder(f));
                Task::none()
            }
            Message::RescanLibrary => {
                let _ = self.cmd_tx.send(ControllerCommand::RescanLibrary);
                Task::none()
            }
            Message::ResyncCovers => {
                let _ = self.cmd_tx.send(ControllerCommand::ResyncCovers);
                Task::none()
            }
            Message::SetGpuPreference(opt) => {
                self.settings.gpu_preference = opt.0;
                self.push_settings();
                Task::none()
            }
            Message::SetDisplayBackend(opt) => {
                self.settings.display_backend = opt.0;
                self.push_settings();
                Task::none()
            }
            Message::SetPerformanceMode(opt) => {
                self.settings.performance_mode = opt.0;
                self.render_tier =
                    RenderTier::resolve(self.settings.performance_mode, self.power_profile);
                let snap = self.sync_animations();
                self.push_settings();
                snap
            }
            Message::ToggleAnimations(v) => {
                self.settings.enable_animations = v;
                let snap = self.sync_animations();
                self.push_settings();
                snap
            }
            Message::SetDirectBridge(opt) => {
                self.settings.direct_volume_bridge = opt.0;
                self.push_settings();
                Task::none()
            }
            Message::ToggleTray(v) => {
                self.settings.enable_tray = v;
                self.push_settings();
                Task::none()
            }
            Message::ToggleMpris(v) => {
                self.settings.enable_mpris = v;
                self.push_settings();
                Task::none()
            }
            Message::ToggleDiscordRpc(v) => {
                self.settings.enable_discord_rpc = v;
                self.push_settings();
                Task::none()
            }
            Message::ToggleDiscordArtwork(v) => {
                self.settings.discord_fetch_artwork = v;
                self.push_settings();
                Task::none()
            }
            Message::ToggleArtworkItunes(v) => {
                self.settings.artwork_itunes = v;
                self.sync_artwork_providers();
                self.push_settings();
                Task::none()
            }
            Message::ToggleArtworkDeezer(v) => {
                self.settings.artwork_deezer = v;
                self.sync_artwork_providers();
                self.push_settings();
                Task::none()
            }
            Message::ToggleArtworkMusicbrainz(v) => {
                self.settings.artwork_musicbrainz = v;
                self.sync_artwork_providers();
                self.push_settings();
                Task::none()
            }
            Message::ToggleArtworkDiscogs(v) => {
                self.settings.artwork_discogs = v;
                self.sync_artwork_providers();
                self.push_settings();
                Task::none()
            }
            Message::DiscogsTokenChanged(v) => {
                self.settings.discogs_token = v;
                self.discogs_token_dirty = true;
                self.discogs_token_gen = self.discogs_token_gen.wrapping_add(1);
                let seq = self.discogs_token_gen;
                Task::perform(tokio::time::sleep(Duration::from_millis(400)), move |_| {
                    Message::DiscogsTokenCommit(seq)
                })
            }
            Message::DiscogsTokenCommit(seq) => {
                if self.discogs_token_dirty && (seq == 0 || seq == self.discogs_token_gen) {
                    self.sync_artwork_providers();
                    self.push_settings();
                }
                Task::none()
            }
            Message::ToggleDiscogsTokenVisible => {
                self.discogs_token_visible = !self.discogs_token_visible;
                Task::none()
            }
            Message::ToggleWatchFolders(v) => {
                self.settings.watch_folders = v;
                self.push_settings();
                Task::none()
            }
            Message::ToggleAccentFromCover(v) => {
                self.settings.accent_from_cover = v;
                self.push_settings();
                Task::none()
            }
            Message::ToggleMinimizeToTray(v) => {
                self.settings.minimize_to_tray = Some(v);
                self.push_settings();
                Task::none()
            }
            Message::VisualizerBarPressed(idx) => {
                let (token, reset_gen, matched) = self.egg_morse.press(idx);
                let release_task =
                    Task::perform(tokio::time::sleep(Duration::from_millis(150)), move |_| {
                        Message::VisualizerBarRelease(idx, token)
                    });
                let extra_task = if matched {
                    self.open_egg()
                } else {
                    Task::perform(tokio::time::sleep(Duration::from_secs(3)), move |_| {
                        Message::EggMorseTimeout(reset_gen)
                    })
                };
                batch_tasks(vec![release_task, extra_task])
            }
            Message::VisualizerBarRelease(idx, token) => {
                let _ = self.egg_morse.release_bar(idx, token);
                Task::none()
            }
            Message::EggMorseTimeout(generation) => {
                self.egg_morse.timeout(generation);
                Task::none()
            }
            Message::EggAssetsReady(assets) => {
                self.egg_loading = false;
                if self.egg_open {
                    self.egg_assets = assets;
                }
                Task::none()
            }
            Message::EggImageHover(hovered) => {
                self.egg_hovered = hovered;
                if !self.animations_enabled() {
                    self.egg_color_alpha = if hovered { 1.0 } else { 0.0 };
                }
                Task::none()
            }
            Message::CloseEgg => {
                self.close_egg();
                Task::none()
            }
            Message::ToastExpire(id) => self.toasts.expire(id),
            Message::ToastRemove(id) => {
                self.toasts.remove(id);
                Task::none()
            }
            Message::ToggleSortMenu => {
                self.sort_menu_open = !self.sort_menu_open;
                self.bank_menu_open = false;
                self.add_music_menu_open = false;
                Task::none()
            }
            Message::ToggleBankMenu => {
                self.bank_menu_open = !self.bank_menu_open;
                self.sort_menu_open = false;
                self.add_music_menu_open = false;
                Task::none()
            }
            Message::ToggleAddMusicMenu => {
                self.add_music_menu_open = !self.add_music_menu_open;
                self.sort_menu_open = false;
                self.bank_menu_open = false;
                Task::none()
            }
            Message::ToggleShortcutsModal => {
                self.show_shortcuts_modal = !self.show_shortcuts_modal;
                Task::none()
            }
            Message::ToggleSidebar => {
                self.toggle_sidebar();
                Task::none()
            }
            Message::OpenTrackMenu {
                track,
                album_artist,
                album_year,
                removable_playlist,
                pos,
            } => {
                self.context_menu = Some(context_menu::ContextMenu {
                    pos,
                    kind: context_menu::ContextMenuKind::Track {
                        track: Box::new(track),
                        album_artist,
                        album_year,
                        removable_playlist,
                    },
                });
                Task::none()
            }
            Message::OpenAlbumMenu(key, pos) => {
                self.context_menu = Some(context_menu::ContextMenu {
                    pos,
                    kind: context_menu::ContextMenuKind::Album(key),
                });
                Task::none()
            }
            Message::OpenPlaylistMenu(view, pos) => {
                let tracks = self.detail_tracks(&crate::pages::DetailCtx::Playlist(view));
                self.context_menu = Some(context_menu::ContextMenu {
                    pos,
                    kind: context_menu::ContextMenuKind::Playlist { view, tracks },
                });
                Task::none()
            }
            Message::OpenDetailMenu(ctx, pos) => {
                self.context_menu = Some(context_menu::ContextMenu {
                    pos,
                    kind: context_menu::ContextMenuKind::DetailMore(ctx),
                });
                Task::none()
            }
            Message::CloseContextMenu => {
                self.context_menu = None;
                Task::none()
            }
            Message::ShotStep(i) => self.shot_step(i),
            Message::ShotGo(i) => self.shot_go(i),
            Message::ShotHover(p) => {
                motion::dev::set_cursor(p);
                Task::none()
            }
            Message::ShotWheel(n) => {
                smooth_scroll::dev::wheel(n, Point::new(self.sidebar_width + 400.0, 500.0));
                Task::none()
            }
            Message::ShotDrag(act) => {
                reorder::dev::act(act);
                Task::none()
            }
            Message::ShotTaken(i, s) => self.shot_taken(i, s),
            _ => Task::none(),
        }
    }

    pub(super) fn toggle_sidebar(&mut self) {
        self.settings.sidebar_visible = !self.settings.sidebar_visible;
        self.bank_viewport_h = None;
        let target = if self.settings.sidebar_visible {
            244.0
        } else {
            60.0
        };
        if self.animations_enabled() && self.shot.is_none() {
            self.sidebar_anim = Some((Instant::now(), self.sidebar_width, target));
        } else {
            self.sidebar_width = target;
            self.sidebar_anim = None;
        }
        self.push_settings();
    }

    pub fn open_egg(&mut self) -> Task<Message> {
        self.egg_open = true;
        self.egg_hovered = false;
        self.egg_color_alpha = 0.0;
        self.egg_pulse_phase = 0.0;
        if self.egg_assets.is_none() && !self.egg_loading {
            self.egg_loading = true;
            return Task::perform(tokio::task::spawn_blocking(egg::decode_egg_assets), |r| {
                Message::EggAssetsReady(r.ok().flatten())
            });
        }
        Task::none()
    }

    pub(super) fn close_egg(&mut self) {
        self.egg_open = false;
        self.egg_hovered = false;
        self.egg_color_alpha = 0.0;
        self.egg_assets = None;
    }

    pub(super) fn sync_artwork_providers(&self) {
        rustygrade_linux::artwork_lookup::shared_resolver()
            .set_providers(ArtworkProviders::from_settings(&self.settings));
    }

    pub(super) fn push_settings(&mut self) {
        self.mini_pos_dirty = false;
        self.discogs_token_dirty = false;
        let _ = self
            .cmd_tx
            .send(ControllerCommand::UpdateSettings(self.settings.clone()));
    }
}
