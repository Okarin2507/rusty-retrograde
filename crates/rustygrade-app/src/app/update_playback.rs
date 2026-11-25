use std::time::{Duration, Instant};

use iced::Task;
use iced::widget::operation;
use rustygrade_core::Track;

use super::{App, LibraryMode, Message, Tab};
use crate::controller::{ControllerCommand, NoticeKind};
use crate::library_view::artist_key;
use crate::pages;
use crate::theme;

impl App {
    pub(super) fn handle_playback_nav_message(&mut self, msg: Message) -> Task<Message> {
        match msg {
            Message::GoHome => {
                self.reorder_lift = None;
                self.active_tab = Tab::Library;
                self.lib_scroll_y = 0.0;
                self.lib_scroll_raw = 0.0;
                self.lib_restore = None;
                self.select_mode = false;
                self.selected_albums.clear();
                self.album_detail_open = false;
                self.artist_detail_open = false;
                self.close_tag_editor();
                self.page_stack.clear();
                self.search_query.clear();
                self.search_gen += 1;
                self.rebuild_search();
                operation::scroll_to(
                    self.lib_scroll_id.clone(),
                    iced::widget::scrollable::AbsoluteOffset { x: 0.0, y: 0.0 },
                )
            }
            Message::SelectTab(tab) => {
                self.reorder_lift = None;
                if self.active_tab != Tab::NowPlaying
                    && tab != Tab::NowPlaying
                    && tab != self.active_tab
                {
                    self.page_stack.clear();
                }
                if self.active_tab != tab {
                    if self.active_tab != Tab::NowPlaying {
                        self.previous_tab = self.active_tab;
                    }
                    self.active_tab = tab;
                    if tab != Tab::Library {
                        self.select_mode = false;
                        self.selected_albums.clear();
                    }
                }
                self.close_tag_editor();
                self.show_shortcuts_modal = false;
                Task::none()
            }
            Message::SetLibraryMode(mode) => {
                self.reorder_lift = None;
                self.library_mode = mode;
                self.lib_scroll_y = 0.0;
                self.lib_scroll_raw = 0.0;
                self.lib_restore = None;
                self.select_mode = false;
                self.selected_albums.clear();
                self.album_detail_open = false;
                self.artist_detail_open = false;
                self.settings.library_view = match mode {
                    LibraryMode::Albums => "albums".to_owned(),
                    LibraryMode::Artists => "artists".to_owned(),
                };
                self.push_settings();
                operation::scroll_to(
                    self.lib_scroll_id.clone(),
                    iced::widget::scrollable::AbsoluteOffset { x: 0.0, y: 0.0 },
                )
            }
            Message::SelectLibrarySort(sort) => {
                self.reorder_lift = None;
                self.sort_menu_open = false;
                self.lib_scroll_y = 0.0;
                self.lib_scroll_raw = 0.0;
                self.lib_restore = None;
                self.library_sort = sort;
                self.settings.library_sort = sort.as_str().to_owned();
                self.rebuild_library_view();
                self.push_settings();
                operation::scroll_to(
                    self.lib_scroll_id.clone(),
                    iced::widget::scrollable::AbsoluteOffset { x: 0.0, y: 0.0 },
                )
            }
            Message::SelectAlbum(key) => {
                self.enter_page(&pages::Page::Album(key.clone()));
                self.open_album(key)
            }
            Message::OpenArtist(key) => {
                self.enter_page(&pages::Page::Artist(key.clone()));
                self.open_artist(key)
            }
            Message::OpenCurrentArtist => {
                let Some(t) = &self.current_track else {
                    return Task::none();
                };
                let album_artist = self
                    .track_map
                    .get(&t.id)
                    .and_then(|&(a, _)| self.albums.get(a))
                    .map(|a| a.artist.clone());
                let key = album_artist
                    .map(|a| artist_key(&a))
                    .filter(|k| self.find_artist(k).is_some())
                    .unwrap_or_else(|| artist_key(&t.artist));
                if self.find_artist(&key).is_some() {
                    Task::done(Message::OpenArtist(key))
                } else {
                    let text = format!("ARTIST_NOT_IN_LIBRARY // {}", t.artist);
                    self.toasts.push(NoticeKind::Info, text)
                }
            }
            Message::OpenPlaylistPage(view) => {
                self.enter_page(&pages::Page::Playlist(view));
                self.open_playlist(view)
            }
            Message::Back => self.go_back(),
            Message::FocusSearch => {
                self.page_stack.clear();
                if self.active_tab != Tab::Search && self.active_tab != Tab::NowPlaying {
                    self.previous_tab = self.active_tab;
                }
                self.active_tab = Tab::Search;
                operation::focus(self.search_input_id.clone())
            }
            Message::SearchChanged(q) => {
                self.search_query = q;
                self.search_gen += 1;
                let generation = self.search_gen;
                Task::perform(tokio::time::sleep(Duration::from_millis(150)), move |_| {
                    Message::SearchDebounced(generation)
                })
            }
            Message::SearchDebounced(generation) => {
                if generation == self.search_gen {
                    self.rebuild_search();
                }
                Task::none()
            }
            Message::SetSearchTab(t) => {
                self.search_tab = t;
                Task::none()
            }
            Message::SetBankPlaylists(on) => {
                if self.bank_playlists != on {
                    self.bank_playlists = on;
                    self.bank_scroll_y = 0.0;
                    self.bank_viewport_h = None;
                    return operation::scroll_to(
                        self.bank_scroll_id.clone(),
                        iced::widget::scrollable::AbsoluteOffset { x: 0.0, y: 0.0 },
                    );
                }
                Task::none()
            }
            Message::LibScrolled(_) if self.lib_restore.is_some() => Task::none(),
            Message::LibScrolled(y) => {
                self.lib_scroll_raw = y.max(0.0);
                let q = (y.max(0.0) / 48.0).floor() * 48.0;
                if (self.lib_scroll_y - q).abs() >= 1.0 {
                    self.lib_scroll_y = q;
                }
                Task::none()
            }
            Message::DetailScrolled(y) => {
                let q = (y.max(0.0) / 32.0).floor() * 32.0;
                if (self.detail_scroll_y - q).abs() >= 1.0 {
                    self.detail_scroll_y = q;
                }
                Task::none()
            }
            Message::BankScrolled(y, viewport_h) => {
                let q = (y.max(0.0) / 32.0).floor() * 32.0;
                if (self.bank_scroll_y - q).abs() >= 1.0 {
                    self.bank_scroll_y = q;
                }
                if viewport_h > 0.0 {
                    self.bank_viewport_h = Some(viewport_h);
                }
                Task::none()
            }
            Message::RestoreLibScroll => {
                let Some(y) = self.lib_restore.take() else {
                    return Task::none();
                };
                self.lib_scroll_raw = y;
                self.lib_scroll_y = (y / 48.0).floor() * 48.0;
                operation::scroll_to(
                    self.lib_scroll_id.clone(),
                    iced::widget::scrollable::AbsoluteOffset { x: 0.0, y },
                )
            }
            Message::PlayDetail(ctx, idx) => {
                let tracks = self.detail_tracks(&ctx);
                if !tracks.is_empty() {
                    let start_index = idx.map(|i| i.min(tracks.len() - 1));
                    let _ = self.cmd_tx.send(ControllerCommand::PlayTrackList {
                        tracks,
                        start_index,
                    });
                }
                Task::none()
            }
            Message::QueueDetail(ctx) => {
                let tracks = self.detail_tracks(&ctx);
                if !tracks.is_empty() {
                    let _ = self.cmd_tx.send(ControllerCommand::EnqueueTracks(tracks));
                }
                Task::none()
            }
            Message::PlaySearchTrack(i) => {
                let tracks: Vec<Track> = self
                    .search_tracks
                    .iter()
                    .filter_map(|&(aidx, tidx)| self.albums.get(aidx)?.tracks.get(tidx).cloned())
                    .collect();
                if i < tracks.len() {
                    let _ = self.cmd_tx.send(ControllerCommand::PlayTrackList {
                        tracks,
                        start_index: Some(i),
                    });
                }
                Task::none()
            }
            Message::PlayAlbum(key) => {
                if let Some(album) = self.find_album(&key)
                    && !album.tracks.is_empty()
                {
                    let _ = self.cmd_tx.send(ControllerCommand::PlayTrackList {
                        tracks: album.tracks.clone(),
                        start_index: None,
                    });
                }
                Task::none()
            }
            Message::PlayTrackFromList(tracks, idx) => {
                let _ = self.cmd_tx.send(ControllerCommand::PlayTrackList {
                    tracks,
                    start_index: Some(idx),
                });
                Task::none()
            }
            Message::PlayNextTracks(tracks) => {
                if !tracks.is_empty() {
                    let _ = self.cmd_tx.send(ControllerCommand::PlayNextTracks(tracks));
                }
                Task::none()
            }
            Message::EnqueueTracks(tracks) => {
                if !tracks.is_empty() {
                    let _ = self.cmd_tx.send(ControllerCommand::EnqueueTracks(tracks));
                }
                Task::none()
            }
            Message::JumpToQueueIndex(idx) => {
                let _ = self.cmd_tx.send(ControllerCommand::JumpToQueueIndex(idx));
                Task::none()
            }
            Message::RemoveQueueIndex(idx) => {
                let _ = self.cmd_tx.send(ControllerCommand::RemoveQueueIndex(idx));
                Task::none()
            }
            Message::ClearQueue => {
                let _ = self.cmd_tx.send(ControllerCommand::ClearQueue);
                Task::none()
            }
            Message::TogglePlayPause => {
                let _ = self.cmd_tx.send(ControllerCommand::TogglePlayPause);
                Task::none()
            }
            Message::Previous => {
                let _ = self.cmd_tx.send(ControllerCommand::Previous);
                Task::none()
            }
            Message::Next => {
                let _ = self.cmd_tx.send(ControllerCommand::Next);
                Task::none()
            }
            Message::ToggleShuffle => {
                let _ = self.cmd_tx.send(ControllerCommand::ToggleShuffle);
                Task::none()
            }
            Message::CycleRepeat => {
                let _ = self.cmd_tx.send(ControllerCommand::CycleRepeat);
                Task::none()
            }
            Message::SeekChanged(sec) => {
                self.seeking_pos = None;
                self.position_s = sec;
                let _ = self.cmd_tx.send(ControllerCommand::SeekSeconds(sec));
                Task::none()
            }
            Message::SeekDragging(sec) => {
                self.seeking_pos = Some(sec);
                Task::none()
            }
            Message::SeekCommit => {
                if let Some(sec) = self.seeking_pos.take() {
                    self.position_s = sec;
                    let _ = self.cmd_tx.send(ControllerCommand::SeekSeconds(sec));
                }
                Task::none()
            }
            Message::VolumeDragging(v) => {
                self.dragging_volume = Some(v);
                self.volume = v;
                let should_send = self
                    .last_volume_emit
                    .is_none_or(|t| t.elapsed() >= Duration::from_millis(40));
                if should_send {
                    self.last_volume_emit = Some(Instant::now());
                    let _ = self.cmd_tx.send(ControllerCommand::SetVolume(v));
                }
                Task::none()
            }
            Message::VolumeCommit => {
                if let Some(v) = self.dragging_volume.take() {
                    self.last_volume_emit = None;
                    let _ = self.cmd_tx.send(ControllerCommand::SetVolume(v));
                }
                Task::none()
            }
            Message::ToggleMute => {
                let _ = self.cmd_tx.send(ControllerCommand::ToggleMute);
                Task::none()
            }
            Message::SelectDevice(opt) => {
                self.audio_device = opt.id.clone();
                let _ = self
                    .cmd_tx
                    .send(ControllerCommand::SelectAudioDevice(opt.id));
                Task::none()
            }
            Message::ToggleQueuePanel => {
                self.queue_open = !self.queue_open;
                self.audio_open = false;
                Task::none()
            }
            Message::ToggleAudioPanel => {
                self.audio_open = !self.audio_open;
                self.queue_open = false;
                if self.audio_open {
                    let _ = self.cmd_tx.send(ControllerCommand::RefreshDevices);
                }
                Task::none()
            }
            Message::SetAudioOutputTab(on) => {
                self.audio_output_tab = on;
                Task::none()
            }
            Message::SetFsLyricsTab(on) => {
                self.fs_lyrics_tab = on;
                self.fs_last_lyric = None;
                Task::none()
            }
            Message::LyricsScrolled(offset, viewport_h, content_h) => {
                if let Some((from, to, start)) = self.lyric_anim {
                    let now = Instant::now();
                    let dur = Duration::from_millis(350);
                    let prev_t = now.checked_sub(Duration::from_millis(60)).unwrap_or(start);
                    let y0 = crate::motion::lerp(
                        from,
                        to,
                        crate::motion::progress(start, prev_t, dur, crate::motion::Ease::CubicOut),
                    );
                    let y1 = crate::motion::lerp(
                        from,
                        to,
                        crate::motion::progress(start, now, dur, crate::motion::Ease::CubicOut),
                    );
                    if offset < y0.min(y1) - 8.0 || offset > y0.max(y1) + 8.0 {
                        self.lyric_anim = None;
                    }
                }
                self.lyrics_vp = Some((offset, viewport_h, content_h));
                Task::none()
            }
            Message::DetailTintReady(key, color) => {
                if let Some(c) = color {
                    self.tint_cache.insert(key.clone(), c);
                }
                if self.tint_for.as_deref() == Some(key.as_str()) {
                    self.detail_tint = Some(color.unwrap_or(pages::NO_COVER_TINT));
                    self.detail_tint_at = self.animations_enabled().then(Instant::now);
                }
                Task::none()
            }
            Message::FsBlurReady(id, handle, cover) => {
                if self.current_track.as_ref().is_some_and(|t| t.id == id) {
                    self.fs_cover = cover.map(|h| (id.clone(), h));
                    self.set_fs_blur(handle.map(|h| (id, h)));
                }
                Task::none()
            }
            Message::FsAnimDone(leg_start) => {
                self.fs_anim_done(leg_start);
                Task::none()
            }
            Message::AnimTick(now) => {
                if !self.animations_enabled() {
                    self.last_frame_at = None;
                    return Task::none();
                }
                let anim_phase = ((self.is_playing || self.deleting_albums)
                    && (self.window_visible || self.mini_window.is_some()))
                    || self.mini_hovered
                    || self.egg_open;
                if let Some(prev) = self.last_frame_at {
                    let dt = now.saturating_duration_since(prev).as_secs_f32().min(0.1);
                    if self.is_playing || self.deleting_albums {
                        self.anim_phase += (dt as f64) * 0.7;
                    }
                    if self.mini_window.is_some() && self.mini_hovered {
                        self.mini_marquee_phase += dt;
                    }
                    if self.egg_open {
                        self.egg_pulse_phase =
                            (self.egg_pulse_phase + dt * 2.2) % std::f32::consts::TAU;
                        let target = if self.egg_hovered { 1.0 } else { 0.0 };
                        if self.egg_color_alpha < target {
                            self.egg_color_alpha = (self.egg_color_alpha + dt).min(1.0);
                        } else if self.egg_color_alpha > target {
                            self.egg_color_alpha = (self.egg_color_alpha - dt).max(0.0);
                        }
                    }
                }
                self.last_frame_at = anim_phase.then_some(now);
                if let Some((start, from, to)) = self.sidebar_anim {
                    let t =
                        (now.saturating_duration_since(start).as_secs_f32() / 0.2).clamp(0.0, 1.0);
                    let ease = crate::motion::Ease::CubicInOut.apply(t);
                    self.sidebar_width = from + (to - from) * ease;
                    if t >= 1.0 {
                        self.sidebar_width = to;
                        self.sidebar_anim = None;
                    }
                }
                if self.accent_anim.is_animating() {
                    let _ = self.accent_anim.tick_at(now);
                    self.orbit_theme = theme::orbit_theme(self.accent_anim.current_accent);
                }
                self.motion_tick(now)
            }
            _ => Task::none(),
        }
    }

    pub(super) fn step_volume(&mut self, dir: f64) {
        let v = (((self.volume * 15.0).round() + dir) / 15.0).clamp(0.0, 1.0);
        self.volume = v;
        let _ = self.cmd_tx.send(ControllerCommand::SetVolume(v));
    }
}
