use std::time::{Duration, SystemTime, UNIX_EPOCH};

use iced::Task;
use iced::widget::operation;
use rustygrade_core::{CoverPaths, PlaylistCovers, SmartPlaylists, Track};

use super::{DetailCtx, NO_COVER_TINT, Page};
use crate::{AlbumKey, App, LibraryMode, Message, PlaylistView, Tab, batch_tasks};

const MAX_HISTORY: usize = 30;

impl App {
    /// The tab on screen under the full-screen player.
    pub fn shown_tab(&self) -> Tab {
        if self.active_tab == Tab::NowPlaying {
            self.previous_tab
        } else {
            self.active_tab
        }
    }

    /// The detail page on screen, if any.
    pub fn current_page(&self) -> Option<Page> {
        match self.shown_tab() {
            Tab::Library => match self.library_mode {
                LibraryMode::Albums if self.album_detail_open => {
                    self.selected_album.clone().map(Page::Album)
                }
                LibraryMode::Artists if self.artist_detail_open => {
                    self.selected_artist.clone().map(Page::Artist)
                }
                _ => None,
            },
            Tab::Playlists if self.smart_draft.is_none() => {
                Some(Page::Playlist(self.playlist_view))
            }
            _ => None,
        }
    }

    /// Saves the current page onto `BACK` history before navigating to `next`.
    pub fn enter_page(&mut self, next: &Page) {
        match self.current_page() {
            Some(cur) if cur != *next => {
                self.page_stack.push(cur);
                if self.page_stack.len() > MAX_HISTORY {
                    self.page_stack.remove(0);
                }
            }
            Some(_) => {}
            None => {
                self.page_stack.clear();
                self.view_before_detail = if self.shown_tab() == Tab::Search {
                    Tab::Search
                } else {
                    Tab::Library
                };
            }
        }
    }

    /// Label shown on the back button of detail pages.
    pub fn back_label(&self) -> &'static str {
        if !self.page_stack.is_empty() {
            "BACK"
        } else if self.view_before_detail == Tab::Search {
            "SEARCH"
        } else {
            "LIBRARY"
        }
    }

    /// Navigates one page back, or returns to the tab where detail browsing began.
    pub fn go_back(&mut self) -> Task<Message> {
        if let Some(page) = self.page_stack.pop() {
            return match page {
                Page::Album(key) => self.open_album(key),
                Page::Artist(key) => self.open_artist(key),
                Page::Playlist(view) => self.open_playlist(view),
            };
        }
        self.reorder_lift = None;
        self.album_detail_open = false;
        self.artist_detail_open = false;
        self.detail_scroll_y = 0.0;
        let to = self.view_before_detail;
        if self.active_tab == Tab::NowPlaying {
            self.previous_tab = to;
        } else {
            self.active_tab = to;
        }
        if to == Tab::Library && self.lib_scroll_raw > 0.0 && self.lib_scroll_y > 0.0 {
            self.lib_restore = Some(self.lib_scroll_raw);
            return Task::perform(tokio::time::sleep(Duration::from_millis(30)), |_| {
                Message::RestoreLibScroll
            });
        }
        Task::none()
    }

    fn show_tab(&mut self, tab: Tab) {
        if self.active_tab != tab {
            if self.active_tab != Tab::NowPlaying {
                self.previous_tab = self.active_tab;
            }
            self.active_tab = tab;
        }
        self.close_tag_editor();
    }

    fn reset_detail_scroll(&mut self) -> Task<Message> {
        self.detail_scroll_y = 0.0;
        self.reorder_lift = None;
        operation::scroll_to(
            self.detail_scroll_id.clone(),
            iced::widget::scrollable::AbsoluteOffset { x: 0.0, y: 0.0 },
        )
    }

    pub fn open_album(&mut self, key: AlbumKey) -> Task<Message> {
        let mut cover = None;
        if let Some(album) = self.find_album(&key) {
            let thumb = album.cover_thumb_path.clone();
            let full = album.cover_full_path.clone();
            cover = thumb.clone().or(full.clone());
            if let Some(p) = thumb {
                self.covers.ensure_thumb(&p);
            }
            if let Some(p) = full {
                self.covers.ensure_full(&p);
            }
        }
        let scroll = self.reset_detail_scroll();
        let tint = self.request_tint(format!("album:{}\u{0}{}", key.name, key.artist), cover);
        self.selected_album = Some(key);
        self.album_detail_open = true;
        self.library_mode = LibraryMode::Albums;
        self.artist_detail_open = false;
        self.show_tab(Tab::Library);
        batch_tasks(vec![scroll, tint])
    }

    pub fn open_artist(&mut self, key: String) -> Task<Message> {
        let scroll = self.reset_detail_scroll();
        let cover = self.find_artist(&key).and_then(|a| {
            a.album_indices
                .iter()
                .filter_map(|&i| self.albums.get(i))
                .find_map(|al| al.cover_thumb_path.clone().or(al.cover_full_path.clone()))
        });
        let tint = self.request_tint(format!("artist:{key}"), cover);
        self.selected_artist = Some(key);
        self.artist_detail_open = true;
        self.library_mode = LibraryMode::Artists;
        self.album_detail_open = false;
        self.show_tab(Tab::Library);
        batch_tasks(vec![scroll, tint])
    }

    pub fn open_playlist(&mut self, view: PlaylistView) -> Task<Message> {
        let scroll = self.reset_detail_scroll();
        self.show_tab(Tab::Playlists);
        self.playlist_view = view;
        self.smart_draft = None;
        if let PlaylistView::Custom(id) = view
            && let Some(p) = self.collections.playlists.iter().find(|p| p.id == id)
        {
            self.rename_playlist_buf = p.name.clone();
        }
        self.rebuild_smart_cache();

        let (tint_key, cover) = match view {
            PlaylistView::Liked => ("liked".to_owned(), None),
            PlaylistView::Custom(id) => {
                let custom = self
                    .collections
                    .playlists
                    .iter()
                    .find(|p| p.id == id)
                    .and_then(|p| CoverPaths::to_local_path(p.cover_art.as_deref()));
                if let Some(ref c) = custom {
                    let thumb = PlaylistCovers::thumb_for(c);
                    self.covers.ensure_full(c);
                    if thumb != *c {
                        self.covers.ensure_thumb(&thumb);
                    }
                }
                let fallback = custom.or_else(|| {
                    self.collections
                        .playlists
                        .iter()
                        .find(|p| p.id == id)
                        .and_then(|p| p.track_ids.first())
                        .and_then(|tid| self.lookup_track(tid))
                        .and_then(|t| t.cover_thumb_path.clone().or(t.cover_full_path.clone()))
                });
                (format!("pl:{id}"), fallback)
            }
            PlaylistView::Smart(id) => (format!("smart:{id}"), None),
        };
        let tint = self.request_tint(tint_key, cover);
        batch_tasks(vec![scroll, tint])
    }

    /// Computes a detail page's header tint color off the UI thread.
    fn request_tint(&mut self, key: String, cover: Option<std::path::PathBuf>) -> Task<Message> {
        let key = format!(
            "{key}\u{1}{}",
            cover
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_default()
        );
        if self.tint_for.as_deref() == Some(key.as_str()) {
            return Task::none();
        }
        self.tint_for = Some(key.clone());
        self.detail_tint_at = None;
        if let Some(c) = self.tint_cache.get(&key) {
            self.detail_tint = Some(*c);
            return Task::none();
        }
        self.detail_tint = None;
        let Some(path) = cover else {
            self.detail_tint = Some(NO_COVER_TINT);
            return Task::none();
        };
        Task::perform(
            async move {
                tokio::task::spawn_blocking(move || {
                    rustygrade_core::extract_cover_palette(&path)
                        .map(|p| crate::theme::color_from_rgb8(p.glow))
                })
                .await
                .ok()
                .flatten()
            },
            move |c| Message::DetailTintReady(key.clone(), c),
        )
    }

    /// Returns the tracks displayed by `ctx` in playback order.
    pub fn detail_tracks(&self, ctx: &DetailCtx) -> Vec<Track> {
        match ctx {
            DetailCtx::Album(key) => self
                .find_album(key)
                .map(|a| a.tracks.clone())
                .unwrap_or_default(),
            DetailCtx::Artist(key) => self
                .find_artist(key)
                .map(|a| {
                    let mut albums: Vec<&rustygrade_core::Album> = a
                        .album_indices
                        .iter()
                        .filter_map(|&i| self.albums.get(i))
                        .collect();
                    albums.sort_by_key(|al| {
                        (if al.year == 0 { i32::MAX } else { al.year }, al.added_at)
                    });
                    albums
                        .iter()
                        .flat_map(|al| al.tracks.iter().cloned())
                        .collect()
                })
                .unwrap_or_default(),
            DetailCtx::Playlist(PlaylistView::Liked) => self
                .collections
                .liked
                .iter()
                .filter_map(|id| self.lookup_track(id).cloned())
                .collect(),
            DetailCtx::Playlist(PlaylistView::Custom(id)) => self
                .collections
                .playlists
                .iter()
                .find(|p| p.id == *id)
                .map(|p| {
                    p.track_ids
                        .iter()
                        .filter_map(|t| self.lookup_track(t).cloned())
                        .collect()
                })
                .unwrap_or_default(),
            DetailCtx::Playlist(PlaylistView::Smart(id)) => {
                if self.playlist_view == PlaylistView::Smart(*id) {
                    self.cached_smart_tracks.clone()
                } else {
                    let now_ms = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .map(|d| d.as_millis() as i64)
                        .unwrap_or(0);
                    self.collections
                        .smart
                        .as_deref()
                        .unwrap_or(&[])
                        .iter()
                        .find(|s| s.id == *id)
                        .map(|sp| {
                            SmartPlaylists::evaluate(
                                sp,
                                self.albums
                                    .iter()
                                    .flat_map(|a| a.tracks.iter().map(move |t| (t, a))),
                                |tid| self.stats.get(tid),
                                |tid| self.liked_set.contains(tid),
                                now_ms,
                            )
                        })
                        .unwrap_or_default()
                }
            }
        }
    }
}
