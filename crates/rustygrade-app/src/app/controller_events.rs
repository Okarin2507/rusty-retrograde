use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use iced::Task;
use rustygrade_core::{
    Album, CollectionsData, CoverPalette, ExtraMap, SmartPlaylistData, SmartPlaylists,
};
use rustygrade_linux::RenderTier;

use super::{AlbumKey, App, Message, PlaylistView, Tab};
use crate::controller::{ControllerEvent, WindowAction};
use crate::library_view::{
    artist_key, fold, group_artists, search_albums, search_artists, sort_albums, sort_artists,
};
use crate::pages;
use crate::theme;

impl App {
    pub(super) fn handle_controller_event(&mut self, evt: ControllerEvent) -> Task<Message> {
        match evt {
            ControllerEvent::LibraryLoaded(albums) => self.apply_library(albums),
            ControllerEvent::CollectionsUpdated(col) => self.apply_collections(col),
            ControllerEvent::StatsUpdated(stats) => {
                self.stats = stats;
                if !self.stats_seen {
                    self.stats_seen = true;
                    if self.library_sort.uses_stats() {
                        self.rebuild_library_view();
                    }
                }
                self.rebuild_smart_counts();
                if matches!(self.playlist_view, PlaylistView::Smart(_)) {
                    self.rebuild_smart_cache();
                }
                self.recompute_smart_draft_count();
            }
            ControllerEvent::QueueUpdated {
                tracks,
                index,
                shuffle,
                repeat,
                hand_len,
            } => {
                self.queue_tracks = tracks;
                self.queue_index = index;
                self.shuffle = shuffle;
                self.repeat_mode = repeat;
                self.queue_hand_len = hand_len;
            }
            ControllerEvent::DevicesUpdated(devs) => {
                self.devices = devs;
            }
            ControllerEvent::TrackChanged(track) => {
                let prev_id = self.current_track.as_ref().map(|t| t.id.as_str());
                let next_id = track.as_ref().map(|t| t.id.as_str());
                let same_track = prev_id.is_some() && prev_id == next_id;
                let cover_changed = same_track
                    && self
                        .current_track
                        .as_ref()
                        .zip(track.as_ref())
                        .is_some_and(|(a, b)| {
                            a.cover_thumb_path != b.cover_thumb_path
                                || a.cover_full_path != b.cover_full_path
                        });
                if prev_id != next_id {
                    self.lyrics = None;
                    self.seeking_pos = None;
                }
                if let Some(t) = &track {
                    if let Some(p) = t.cover_thumb_path.as_deref() {
                        self.covers.ensure_thumb(p);
                    }
                    if let Some(p) = t.cover_full_path.as_deref() {
                        self.covers.ensure_full(p);
                    }
                }
                self.current_track = track;
                if cover_changed {
                    self.fs_blur_tried = None;
                    self.fs_blur = None;
                }
            }
            ControllerEvent::AccentPaletteChanged(palette) => {
                let pal = palette.unwrap_or(CoverPalette::ORBIT_DEFAULT);
                let animate = self.animations_enabled()
                    && self.window_visible
                    && self.main_window.is_some()
                    && !self.main_window_minimized;
                self.accent_anim.set_target(pal, animate);
                if !animate {
                    self.orbit_theme = theme::orbit_theme(self.accent_anim.current_accent);
                }
            }
            ControllerEvent::LyricsUpdated { track_id, lyrics } => {
                if track_id.as_ref() == self.current_track.as_ref().map(|t| &t.id) {
                    self.lyrics = lyrics;
                }
            }
            ControllerEvent::PlaybackState {
                is_playing,
                position_s,
                duration_s,
            } => {
                let was_playing = self.is_playing;
                self.is_playing = is_playing;
                self.position_s = position_s;
                self.duration_s = duration_s;

                if self.is_unfocused_open_main() && was_playing && is_playing {
                    self.arm_watchdog_on_tick();
                }
            }
            ControllerEvent::VolumeState { volume, muted } => {
                self.muted = muted;
                if self.dragging_volume.is_none() {
                    self.volume = volume;
                    self.settings.volume = volume;
                }
            }
            ControllerEvent::DirectStatus {
                audio_device,
                bit_perfect_status,
                direct_active,
                direct_volume_hint,
                audio_path,
                pw_bridge_node_id,
                direct_mixer_control,
            } => {
                self.audio_device = audio_device;
                self.bit_perfect_status = bit_perfect_status;
                self.direct_active = direct_active;
                self.direct_volume_hint = direct_volume_hint;
                self.audio_path = audio_path;
                self.pw_bridge_node_id = pw_bridge_node_id;
                self.direct_mixer_control = direct_mixer_control;
            }
            ControllerEvent::PowerTierChanged { profile, tier } => {
                self.power_profile = profile;
                self.render_tier = tier;
                return self.sync_animations();
            }
            ControllerEvent::SettingsSaved(s) => {
                let (mini_x, mini_y) = (self.settings.mini_x, self.settings.mini_y);
                let discogs_token = if self.discogs_token_dirty {
                    Some(self.settings.discogs_token.clone())
                } else {
                    None
                };
                self.settings = s;
                if self.mini_pos_dirty {
                    self.settings.mini_x = mini_x;
                    self.settings.mini_y = mini_y;
                }
                if let Some(tok) = discogs_token {
                    self.settings.discogs_token = tok;
                }
                self.render_tier =
                    RenderTier::resolve(self.settings.performance_mode, self.power_profile);
                self.sync_artwork_providers();
                return self.sync_animations();
            }
            ControllerEvent::Notice { kind, text } => {
                return self.toasts.push(kind, text);
            }
            ControllerEvent::ScanProgress {
                phase,
                scanned,
                total,
            } => {
                let started = self
                    .scan
                    .as_ref()
                    .map_or_else(std::time::Instant::now, |s| s.started);
                self.scan = Some(crate::toasts::ScanState {
                    phase,
                    scanned,
                    total,
                    started,
                });
            }
            ControllerEvent::ScanFinished => {
                self.scan = None;
            }
            ControllerEvent::MissingTracks { count, roots } => {
                self.confirm = Some(crate::dialogs::ConfirmDialog {
                    title: "WARNING: MISSING_FILES",
                    target_label: "TARGETS: ",
                    target: format!("{count} TRACK{}", if count == 1 { "" } else { "S" }),
                    target_orange: true,
                    detail: None,
                    note: "NOT ON DISK. REMOVE FROM LIBRARY?",
                    confirm: "REMOVE",
                    action: crate::dialogs::ConfirmAction::PruneMissing(roots),
                });
            }
            ControllerEvent::AlbumsDeleteFinished => {
                self.deleting_albums = false;
                let missing_selected = self
                    .selected_album
                    .as_ref()
                    .is_some_and(|sel| self.find_album(sel).is_none());
                if missing_selected {
                    self.selected_album = None;
                    self.album_detail_open = false;
                }
                let albums = &self.albums;
                let artists = &self.artists;
                self.page_stack.retain(|p| match p {
                    pages::Page::Album(k) => albums
                        .iter()
                        .any(|a| a.name == k.name && a.artist == k.artist),
                    pages::Page::Artist(k) => artists.iter().any(|a| artist_key(&a.name) == *k),
                    pages::Page::Playlist(_) => true,
                });
            }
            ControllerEvent::CoversInvalidated(paths) => {
                self.covers.invalidate_paths(&paths);
                self.clear_tint_for_paths(&paths);
                self.refresh_backdrop_if_current_cover(&paths);
            }
            ControllerEvent::CoversResynced(paths) => {
                self.covers.refresh_loaded(&paths);
                self.tint_cache.clear();
                self.tint_for = None;
                self.refresh_backdrop_if_current_cover(&paths);
            }
            ControllerEvent::TagsSaved {
                first_path,
                written,
                failed,
            } => {
                if let Some(draft) = &mut self.tag_draft
                    && (draft.saving || draft.paths.first() == first_path.as_ref())
                {
                    if !failed.is_empty() {
                        draft.saving = false;
                        self.tag_status = Some(Err(failed.join("; ")));
                    } else if written > 0 {
                        self.tag_draft = None;
                        self.tag_status = None;
                        self.end_cover_fetches();
                    } else {
                        draft.saving = false;
                    }
                }
            }
            ControllerEvent::TrayAvailable(available) => {
                self.tray_available = available;
            }
            ControllerEvent::DiscordRpcStatus(status) => {
                self.discord_rpc_status = status;
            }
            ControllerEvent::WindowCommand(action) => match action {
                WindowAction::Raise => {
                    if self.mini_window.is_some() {
                        return self.restore_main_from_mini();
                    }
                    return self.open_or_focus_main_window();
                }
                WindowAction::Toggle => {
                    if self.window_visible && self.main_window.is_some() {
                        return self.hide_main_window();
                    } else if self.mini_window.is_some() {
                        return self.restore_main_from_mini();
                    } else {
                        return self.open_or_focus_main_window();
                    }
                }
                WindowAction::ToggleMiniPlayer => return self.toggle_mini_player(),
                WindowAction::Quit => return self.begin_shutdown(),
            },
            ControllerEvent::ShutdownComplete => return iced::exit(),
        }
        Task::none()
    }

    pub(super) fn apply_library(&mut self, albums: Arc<[Album]>) {
        self.reorder_lift = None;
        self.covers.sync_albums(&albums);
        self.covers.sync_collections(&self.collections);
        self.artists = group_artists(&albums);
        self.albums = albums;
        self.page_stack.retain(|p| match p {
            pages::Page::Album(k) => self
                .albums
                .iter()
                .any(|a| a.name == k.name && a.artist == k.artist),
            pages::Page::Artist(k) => self.artists.iter().any(|a| artist_key(&a.name) == *k),
            pages::Page::Playlist(_) => true,
        });
        self.rebuild_track_map();
        self.rebuild_library_view();
        self.rebuild_smart_counts();
        self.rebuild_search();
        self.rebuild_smart_cache();
        self.recompute_smart_draft_count();
        if let Some(t) = &self.current_track {
            if let Some(p) = t.cover_thumb_path.as_deref() {
                self.covers.ensure_thumb(p);
            }
            if let Some(p) = t.cover_full_path.as_deref() {
                self.covers.ensure_full(p);
            }
        }
    }

    pub(super) fn apply_collections(&mut self, col: CollectionsData) {
        self.covers.sync_collections(&col);
        self.collections = col;
        self.page_stack.retain(|p| match p {
            pages::Page::Playlist(PlaylistView::Custom(id)) => {
                self.collections.playlists.iter().any(|pl| pl.id == *id)
            }
            pages::Page::Playlist(PlaylistView::Smart(id)) => self
                .collections
                .smart
                .as_deref()
                .unwrap_or(&[])
                .iter()
                .any(|sp| sp.id == *id),
            _ => true,
        });
        match self.playlist_view {
            PlaylistView::Custom(id)
                if !self.collections.playlists.iter().any(|pl| pl.id == id) =>
            {
                self.leave_deleted_page(PlaylistView::Custom(id));
            }
            PlaylistView::Smart(id)
                if !self
                    .collections
                    .smart
                    .as_deref()
                    .unwrap_or(&[])
                    .iter()
                    .any(|sp| sp.id == id) =>
            {
                self.leave_deleted_page(PlaylistView::Smart(id));
            }
            _ => {}
        }
        self.rebuild_liked_set();
        self.rebuild_smart_counts();
        self.rebuild_search();
        self.rebuild_smart_cache();
        self.recompute_smart_draft_count();
    }

    pub(super) fn rebuild_search(&mut self) {
        self.search_tracks.clear();
        self.search_playlists.clear();
        self.search_albums.clear();
        self.search_artists.clear();
        let q = fold(self.search_query.trim());
        if q.is_empty() {
            return;
        }
        for (aidx, a) in self.albums.iter().enumerate() {
            for (tidx, t) in a.tracks.iter().enumerate() {
                if fold(&t.title).contains(&q) || fold(&t.artist).contains(&q) {
                    self.search_tracks.push((aidx, tidx));
                }
            }
        }
        self.search_albums = search_albums(&self.albums, &q);
        self.search_artists = search_artists(&self.artists, &q);
        for p in &self.collections.playlists {
            if fold(&p.name).contains(&q) {
                self.search_playlists.push((
                    PlaylistView::Custom(p.id),
                    p.name.clone(),
                    p.track_ids.len(),
                ));
            }
        }
        for sp in self.collections.smart.iter().flatten() {
            if fold(&sp.name).contains(&q) {
                let count = self.smart_counts.get(&sp.id).copied().unwrap_or(0);
                self.search_playlists
                    .push((PlaylistView::Smart(sp.id), sp.name.clone(), count));
            }
        }
    }

    pub(super) fn rebuild_library_view(&mut self) {
        self.library_albums = sort_albums(&self.albums, self.library_sort, &self.stats);
        self.library_artists =
            sort_artists(&self.artists, &self.albums, self.library_sort, &self.stats);
        let mut sb: Vec<usize> = (0..self.albums.len()).collect();
        sb.sort_by_cached_key(|&i| fold(&self.albums[i].name));
        self.sidebar_albums = sb;
        let albums = &self.albums;
        self.selected_albums
            .retain(|k| albums.iter().any(|a| AlbumKey::from_album(a) == *k));

        let album_exists = self
            .selected_album
            .as_ref()
            .is_some_and(|key| self.find_album(key).is_some());
        if !album_exists {
            self.selected_album = None;
            self.album_detail_open = false;
        }
        let artist_exists = self
            .selected_artist
            .as_deref()
            .is_some_and(|key| self.find_artist(key).is_some());
        if !artist_exists {
            self.selected_artist = None;
            self.artist_detail_open = false;
        }
    }

    pub(super) fn rebuild_smart_counts(&mut self) {
        self.smart_counts.clear();
        let now_ms = current_unix_ms();
        for sp in self.collections.smart.iter().flatten() {
            let count = SmartPlaylists::count(
                sp,
                self.albums
                    .iter()
                    .flat_map(|a| a.tracks.iter().map(move |t| (t, a))),
                |tid| self.stats.get(tid),
                |tid| self.liked_set.contains(tid),
                now_ms,
            );
            self.smart_counts.insert(sp.id, count);
        }
    }

    pub(super) fn recompute_smart_draft_count(&mut self) {
        let Some(d) = &mut self.smart_draft else {
            return;
        };
        let now_ms = current_unix_ms();
        let sp = SmartPlaylistData {
            id: d.id,
            name: d.name.clone(),
            match_all: d.match_all,
            rules: d.rules.clone(),
            sort: d.sort.clone(),
            limit: d.limit.trim().parse::<usize>().unwrap_or(0),
            extra: ExtraMap::new(),
        };
        d.match_count = SmartPlaylists::count(
            &sp,
            self.albums
                .iter()
                .flat_map(|a| a.tracks.iter().map(move |t| (t, a))),
            |tid| self.stats.get(tid),
            |tid| self.liked_set.contains(tid),
            now_ms,
        );
    }

    fn rebuild_track_map(&mut self) {
        self.track_map.clear();
        for (aidx, album) in self.albums.iter().enumerate() {
            for (tidx, track) in album.tracks.iter().enumerate() {
                self.track_map.insert(track.id.clone(), (aidx, tidx));
            }
        }
    }

    fn rebuild_liked_set(&mut self) {
        self.liked_set = self.collections.liked.iter().cloned().collect();
    }

    pub(crate) fn rebuild_smart_cache(&mut self) {
        let PlaylistView::Smart(id) = self.playlist_view else {
            self.cached_smart_tracks.clear();
            return;
        };
        let smart_list = self.collections.smart.as_deref().unwrap_or(&[]);
        let Some(sp) = smart_list.iter().find(|s| s.id == id) else {
            self.cached_smart_tracks.clear();
            return;
        };
        let now_ms = current_unix_ms();
        self.cached_smart_tracks = SmartPlaylists::evaluate(
            sp,
            self.albums
                .iter()
                .flat_map(|a| a.tracks.iter().map(move |t| (t, a))),
            |tid| self.stats.get(tid),
            |tid| self.liked_set.contains(tid),
            now_ms,
        );
    }

    pub(crate) fn leave_deleted_page(&mut self, view: PlaylistView) {
        self.page_stack
            .retain(|p| *p != pages::Page::Playlist(view));
        if self.playlist_view == view {
            self.playlist_view = PlaylistView::Liked;
            if self.shown_tab() == Tab::Playlists {
                self.album_detail_open = false;
                self.artist_detail_open = false;
                self.page_stack.clear();
                if self.active_tab == Tab::NowPlaying {
                    self.previous_tab = Tab::Library;
                } else {
                    self.active_tab = Tab::Library;
                }
            }
        }
    }
}

fn current_unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
