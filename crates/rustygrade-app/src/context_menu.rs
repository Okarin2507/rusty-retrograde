//! Right-click and "⋯" action menus (`COMMAND_MENU` and `ALBUM_OPERATIONS`), matching C#'s
//! `MenuTemplate` in `MainWindow.axaml`.

use std::collections::HashSet;

use iced::advanced::layout::{self, Layout};
use iced::advanced::renderer;
use iced::advanced::widget::{Operation, Tree};
use iced::advanced::{Clipboard, Shell, Widget, overlay};
use iced::widget::{button, column, container, mouse_area, opaque, row, scrollable, space, text};
use iced::{
    Alignment, Background, Border, Color, Element, Event, Length, Padding, Point, Rectangle,
    Shadow, Size, Vector, mouse,
};
use rustygrade_core::Track;

use crate::icons;
use crate::pages::{DetailCtx, fit};
use crate::smooth_scroll::smooth;
use crate::theme;
use crate::views::bold;
use crate::{AlbumKey, App, Message, PlaylistView};

const MENU_WIDTH: f32 = 240.0;

#[derive(Debug, Clone)]
pub enum ContextMenuKind {
    Track {
        track: Box<Track>,
        album_artist: String,
        album_year: i32,
        removable_playlist: Option<i64>,
    },
    Album(AlbumKey),
    Playlist {
        view: PlaylistView,
        tracks: Vec<Track>,
    },
    DetailMore(DetailCtx),
}

#[derive(Debug, Clone)]
pub struct ContextMenu {
    pub pos: Point,
    pub kind: ContextMenuKind,
}

/// Wraps any element so a right-click over its bounds publishes `on_right_click(cursor_pos)`
/// without polling cursor position globally.
pub fn trigger<'a, Message: 'a>(
    content: impl Into<Element<'a, Message>>,
    on_right_click: impl Fn(Point) -> Message + 'a,
) -> Element<'a, Message> {
    Element::new(ContextTrigger {
        content: content.into(),
        on_right_click: Some(Box::new(on_right_click)),
        on_left_click: None,
    })
}

/// Wraps a left-clickable control (such as a "⋯" button) so clicking it publishes `on_left_click(cursor_pos)`.
pub fn left_trigger<'a, Message: 'a>(
    content: impl Into<Element<'a, Message>>,
    on_left_click: impl Fn(Point) -> Message + 'a,
) -> Element<'a, Message> {
    Element::new(ContextTrigger {
        content: content.into(),
        on_right_click: None,
        on_left_click: Some(Box::new(on_left_click)),
    })
}

struct ContextTrigger<'a, Message> {
    content: Element<'a, Message>,
    on_right_click: Option<Box<dyn Fn(Point) -> Message + 'a>>,
    on_left_click: Option<Box<dyn Fn(Point) -> Message + 'a>>,
}

impl<Message> Widget<Message, iced::Theme, iced::Renderer> for ContextTrigger<'_, Message> {
    fn size(&self) -> Size<Length> {
        self.content.as_widget().size()
    }

    fn size_hint(&self) -> Size<Length> {
        self.content.as_widget().size_hint()
    }

    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(&self.content)]
    }

    fn diff(&self, tree: &mut Tree) {
        tree.diff_children(std::slice::from_ref(&self.content));
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &iced::Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        self.content
            .as_widget_mut()
            .layout(&mut tree.children[0], renderer, limits)
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &iced::Renderer,
        operation: &mut dyn Operation,
    ) {
        self.content
            .as_widget_mut()
            .operate(&mut tree.children[0], layout, renderer, operation);
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &iced::Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) {
        if let Some(on_left) = &self.on_left_click
            && let Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) = event
            && cursor.is_over(layout.bounds())
        {
            let pos = cursor
                .position()
                .unwrap_or_else(|| layout.bounds().position());
            shell.publish(on_left(pos));
            shell.capture_event();
            return;
        }
        self.content.as_widget_mut().update(
            &mut tree.children[0],
            event,
            layout,
            cursor,
            renderer,
            clipboard,
            shell,
            viewport,
        );
        if shell.is_event_captured() {
            return;
        }
        if let Some(on_right) = &self.on_right_click
            && let Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Right)) = event
            && cursor.is_over(layout.bounds())
        {
            let pos = cursor
                .position()
                .unwrap_or_else(|| layout.bounds().position());
            shell.publish(on_right(pos));
            shell.capture_event();
        }
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &iced::Renderer,
    ) -> mouse::Interaction {
        self.content.as_widget().mouse_interaction(
            &tree.children[0],
            layout,
            cursor,
            viewport,
            renderer,
        )
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut iced::Renderer,
        theme: &iced::Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        self.content.as_widget().draw(
            &tree.children[0],
            renderer,
            theme,
            style,
            layout,
            cursor,
            viewport,
        );
    }

    fn overlay<'b>(
        &'b mut self,
        tree: &'b mut Tree,
        layout: Layout<'b>,
        renderer: &iced::Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'b, Message, iced::Theme, iced::Renderer>> {
        self.content.as_widget_mut().overlay(
            &mut tree.children[0],
            layout,
            renderer,
            viewport,
            translation,
        )
    }
}

struct Entry<'a> {
    kind: EntryKind<'a>,
}

enum EntryKind<'a> {
    Separator,
    Caption(&'a str),
    Action {
        icon: &'static str,
        icon_color: Color,
        label: String,
        danger: bool,
        msg: Message,
    },
    PlaylistRow {
        label: String,
        checked: bool,
        msg: Message,
    },
}

fn action<'a>(
    icon: &'static str,
    icon_color: Color,
    label: impl Into<String>,
    danger: bool,
    msg: Message,
) -> Entry<'a> {
    Entry {
        kind: EntryKind::Action {
            icon,
            icon_color,
            label: label.into(),
            danger,
            msg,
        },
    }
}

fn push_queue_actions<'a>(
    entries: &mut Vec<Entry<'a>>,
    tracks: &[Track],
    dim: Color,
    enqueue_label: &str,
) {
    entries.push(action(
        "playnext",
        dim,
        "PLAY_NEXT",
        false,
        Message::PlayNextTracks(tracks.to_vec()),
    ));
    entries.push(action(
        "listmusic",
        dim,
        enqueue_label,
        false,
        Message::EnqueueTracks(tracks.to_vec()),
    ));
}

fn push_like_all_action<'a>(entries: &mut Vec<Entry<'a>>, app: &App, tracks: &[Track], dim: Color) {
    let track_ids: Vec<String> = tracks.iter().map(|t| t.id.clone()).collect();
    let all_liked = !track_ids.is_empty() && track_ids.iter().all(|id| app.liked_set.contains(id));
    entries.push(action(
        "heart",
        if all_liked { theme::ORANGE } else { dim },
        if all_liked {
            "UNLIKE_ALL_TRACKS"
        } else {
            "LIKE_ALL_TRACKS"
        },
        false,
        Message::ToggleLikeTracks(track_ids),
    ));
}

fn push_custom_playlist_actions<'a>(entries: &mut Vec<Entry<'a>>, app: &App, id: i64, dim: Color) {
    let has_cover = app
        .collections
        .playlists
        .iter()
        .find(|p| p.id == id)
        .is_some_and(|p| p.cover_art.is_some());
    entries.push(action(
        "upload",
        dim,
        "CHANGE_COVER",
        false,
        Message::PickPlaylistCover(id),
    ));
    if has_cover {
        entries.push(action(
            "trash",
            theme::ORANGE,
            "REMOVE_COVER",
            true,
            Message::RemovePlaylistCover(id),
        ));
    }
    entries.push(action(
        "download",
        dim,
        "EXPORT_M3U8",
        false,
        Message::ExportPlaylistM3u8(PlaylistView::Custom(id)),
    ));
    entries.push(action(
        "download",
        dim,
        "EXPORT_ZIP",
        false,
        Message::ExportPlaylistZip(id),
    ));
    entries.push(action(
        "trash",
        theme::ORANGE,
        "DELETE_PLAYLIST",
        true,
        Message::DeletePlaylist(id),
    ));
}

fn push_smart_playlist_actions<'a>(entries: &mut Vec<Entry<'a>>, app: &App, id: i64, dim: Color) {
    if let Some(sp) = app
        .collections
        .smart
        .as_deref()
        .unwrap_or(&[])
        .iter()
        .find(|s| s.id == id)
    {
        entries.push(action(
            "filter",
            theme::TEAL,
            "EDIT_RULES",
            false,
            Message::EditSmartPlaylist(sp.clone()),
        ));
        entries.push(action(
            "download",
            dim,
            "EXPORT_M3U8",
            false,
            Message::ExportPlaylistM3u8(PlaylistView::Smart(id)),
        ));
        entries.push(action(
            "trash",
            theme::ORANGE,
            "DELETE_SMART_PLAYLIST",
            true,
            Message::DeleteSmartPlaylist(id),
        ));
    }
}

fn push_liked_actions<'a>(entries: &mut Vec<Entry<'a>>, dim: Color) {
    entries.push(action(
        "download",
        dim,
        "EXPORT_M3U8",
        false,
        Message::ExportPlaylistM3u8(PlaylistView::Liked),
    ));
}

fn push_playlist_entries<'a>(
    entries: &mut Vec<Entry<'a>>,
    app: &App,
    tracks: &[Track],
    with_sep: bool,
) {
    if with_sep {
        entries.push(Entry {
            kind: EntryKind::Separator,
        });
        entries.push(Entry {
            kind: EntryKind::Caption("ADD_TO_PLAYLIST"),
        });
    }
    if app.collections.playlists.is_empty() {
        entries.push(Entry {
            kind: EntryKind::Caption("NO_DATA"),
        });
        return;
    }
    let track_ids: Vec<String> = tracks.iter().map(|t| t.id.clone()).collect();
    for p in &app.collections.playlists {
        let set: HashSet<&str> = p.track_ids.iter().map(String::as_str).collect();
        let all = !track_ids.is_empty() && track_ids.iter().all(|id| set.contains(id.as_str()));
        entries.push(Entry {
            kind: EntryKind::PlaylistRow {
                label: p.name.clone(),
                checked: all,
                msg: Message::TogglePlaylistTracks {
                    playlist_id: p.id,
                    track_ids: track_ids.clone(),
                },
            },
        });
    }
}

/// Clamps the floating menu origin `(x, y)` and returns `(x, y, max_h)` inside `window`.
fn clamp_menu_position(pos: Point, window: Size, est_h: f32) -> (f32, f32, f32) {
    let max_h = (window.height - 32.0).max(180.0);
    let clamped_h = est_h.min(max_h);
    let x = pos
        .x
        .clamp(8.0, (window.width - MENU_WIDTH - 12.0).max(8.0));
    let y = pos
        .y
        .clamp(8.0, (window.height - clamped_h - 12.0).max(8.0));
    (x, y, max_h)
}

/// Builds the context menu overlay clamped inside `app.window_size`.
pub fn view<'a>(app: &'a App, menu: &'a ContextMenu) -> Element<'a, Message> {
    let dim = theme::TEXT_DIM;
    let mut entries: Vec<Entry<'a>> = Vec::new();
    let (title, title_color) = match &menu.kind {
        ContextMenuKind::Track {
            track,
            album_artist,
            album_year,
            removable_playlist,
        } => {
            let liked = app.liked_set.contains(&track.id);
            entries.push(action(
                "heart",
                if liked { theme::ORANGE } else { dim },
                if liked {
                    "REMOVE_FAVORITE"
                } else {
                    "ADD_TO_FAVORITE"
                },
                false,
                Message::ToggleLike(track.id.clone()),
            ));
            entries.push(action(
                "pencil",
                dim,
                "EDIT_TAGS",
                false,
                Message::OpenTagEditor((**track).clone(), album_artist.clone(), *album_year),
            ));
            push_queue_actions(
                &mut entries,
                std::slice::from_ref(track.as_ref()),
                dim,
                "ADD_TO_QUEUE",
            );
            if let Some(pid) = removable_playlist {
                entries.push(action(
                    "trash",
                    theme::ORANGE,
                    "REMOVE_FROM_PLAYLIST",
                    true,
                    Message::RemoveTrackFromPlaylist {
                        playlist_id: *pid,
                        track_id: track.id.clone(),
                    },
                ));
            }
            push_playlist_entries(
                &mut entries,
                app,
                std::slice::from_ref(track.as_ref()),
                true,
            );
            ("COMMAND_MENU", theme::TEAL)
        }
        ContextMenuKind::Album(key) => {
            let tracks = app
                .find_album(key)
                .map(|a| a.tracks.clone())
                .unwrap_or_default();
            push_queue_actions(&mut entries, &tracks, dim, "ADD_ALL_TO_QUEUE");
            push_like_all_action(&mut entries, app, &tracks, dim);
            entries.push(action(
                "pencil",
                dim,
                "EDIT_ALBUM_TAGS",
                false,
                Message::OpenAlbumTagEditor(key.clone()),
            ));
            entries.push(action(
                "trash",
                theme::ORANGE,
                "DELETE_ALBUM",
                true,
                Message::DeleteAlbum(key.clone(), false),
            ));
            push_playlist_entries(&mut entries, app, &tracks, true);
            ("ALBUM_OPERATIONS", theme::ORANGE)
        }
        ContextMenuKind::Playlist { view, tracks } => {
            push_queue_actions(&mut entries, tracks, dim, "ADD_ALL_TO_QUEUE");
            match view {
                PlaylistView::Custom(id) => {
                    push_like_all_action(&mut entries, app, tracks, dim);
                    push_custom_playlist_actions(&mut entries, app, *id, dim);
                    push_playlist_entries(&mut entries, app, tracks, true);
                    ("PLAYLIST_OPERATIONS", theme::ORANGE)
                }
                PlaylistView::Liked => {
                    push_liked_actions(&mut entries, dim);
                    push_playlist_entries(&mut entries, app, tracks, true);
                    ("FAVORITES_OPERATIONS", theme::GOLD)
                }
                PlaylistView::Smart(id) => {
                    push_smart_playlist_actions(&mut entries, app, *id, dim);
                    push_playlist_entries(&mut entries, app, tracks, true);
                    ("SMART_OPERATIONS", theme::TEAL)
                }
            }
        }
        ContextMenuKind::DetailMore(ctx) => match ctx {
            DetailCtx::Album(key) => {
                let tracks = app
                    .find_album(key)
                    .map(|a| a.tracks.clone())
                    .unwrap_or_default();
                push_queue_actions(&mut entries, &tracks, dim, "ADD_ALL_TO_QUEUE");
                entries.push(action(
                    "pencil",
                    dim,
                    "EDIT_ALBUM_TAGS",
                    false,
                    Message::OpenAlbumTagEditor(key.clone()),
                ));
                entries.push(action(
                    "trash",
                    theme::ORANGE,
                    "DELETE_ALBUM",
                    true,
                    Message::DeleteAlbum(key.clone(), true),
                ));
                ("ALBUM_OPERATIONS", theme::ORANGE)
            }
            DetailCtx::Artist(key) => {
                let tracks: Vec<Track> = app
                    .find_artist(key)
                    .map(|artist| {
                        artist
                            .album_indices
                            .iter()
                            .filter_map(|&idx| app.albums.get(idx))
                            .flat_map(|a| a.tracks.iter().cloned())
                            .collect()
                    })
                    .unwrap_or_default();
                push_queue_actions(&mut entries, &tracks, dim, "ADD_ALL_TO_QUEUE");
                push_like_all_action(&mut entries, app, &tracks, dim);
                push_playlist_entries(&mut entries, app, &tracks, true);
                ("ARTIST_OPERATIONS", theme::TEAL)
            }
            DetailCtx::Playlist(PlaylistView::Custom(id)) => {
                push_custom_playlist_actions(&mut entries, app, *id, dim);
                ("PLAYLIST_OPERATIONS", theme::ORANGE)
            }
            DetailCtx::Playlist(PlaylistView::Smart(id)) => {
                push_smart_playlist_actions(&mut entries, app, *id, dim);
                ("SMART_OPERATIONS", theme::TEAL)
            }
            DetailCtx::Playlist(PlaylistView::Liked) => {
                push_liked_actions(&mut entries, dim);
                ("FAVORITES_OPERATIONS", theme::GOLD)
            }
        },
    };

    let mut est_h: f32 = 36.0;
    let mut rows: Vec<Element<'a, Message>> = Vec::with_capacity(entries.len());
    for e in entries {
        match e.kind {
            EntryKind::Separator => {
                est_h += 9.0;
                rows.push(
                    container(space::horizontal())
                        .width(Length::Fill)
                        .height(1)
                        .style(theme::fill_style(theme::LINE))
                        .into(),
                );
                rows.push(space::vertical().height(4).into());
            }
            EntryKind::Caption(c) => {
                est_h += 22.0;
                rows.push(
                    container(text(c).size(8).font(bold()).color(theme::TEXT_MUTE))
                        .padding([4, 12])
                        .into(),
                );
            }
            EntryKind::Action {
                icon,
                icon_color,
                label,
                danger,
                msg,
            } => {
                est_h += 32.0;
                rows.push(menu_button(
                    row![
                        icons::icon(icon, 14.0, icon_color),
                        text(label).size(12).color(if danger {
                            theme::ORANGE
                        } else {
                            theme::TEXT_SOFT
                        }),
                    ]
                    .spacing(12)
                    .align_y(Alignment::Center)
                    .into(),
                    danger,
                    msg,
                ));
            }
            EntryKind::PlaylistRow {
                label,
                checked,
                msg,
            } => {
                est_h += 30.0;
                let mut content = row![
                    container(space::horizontal())
                        .width(4)
                        .height(4)
                        .style(theme::fill_style(if checked {
                            theme::ORANGE
                        } else {
                            theme::TEXT_MUTE
                        })),
                    text(fit(&label, 170.0, 12.0).into_owned())
                        .size(12)
                        .color(theme::TEXT_SOFT)
                        .width(Length::Fill),
                ]
                .spacing(8)
                .align_y(Alignment::Center);
                if checked {
                    content = content.push(text("✓").size(12).color(theme::ORANGE));
                }
                rows.push(menu_button(content.into(), false, msg));
            }
        }
    }

    let (x, y, max_h) = clamp_menu_position(menu.pos, app.window_size, est_h);

    let header = column![
        container(text(title).size(9).font(bold()).color(title_color))
            .padding([8, 12])
            .width(Length::Fill),
        container(space::horizontal())
            .width(Length::Fill)
            .height(1)
            .style(theme::fill_style(theme::LINE)),
    ];

    let list = smooth(scrollable(column(rows).spacing(1)).height(Length::Shrink));
    let card = opaque(
        container(column![header, list].spacing(4))
            .padding(4)
            .width(Length::Fixed(MENU_WIDTH))
            .max_height(max_h)
            .style(|_t| container::Style {
                background: Some(Background::Color(theme::RAISED)),
                border: Border {
                    color: theme::BORDER_STRONG,
                    width: 1.0,
                    radius: 0.0.into(),
                },
                shadow: Shadow {
                    color: Color::from_rgba(0.0, 0.0, 0.0, 0.65),
                    offset: Vector::new(0.0, 8.0),
                    blur_radius: 24.0,
                },
                ..Default::default()
            }),
    );

    let placed = container(card)
        .width(Length::Fill)
        .height(Length::Fill)
        .padding(Padding {
            top: y,
            left: x,
            right: 0.0,
            bottom: 0.0,
        });

    opaque(
        mouse_area(placed)
            .on_press(Message::CloseContextMenu)
            .on_right_press(Message::CloseContextMenu),
    )
}

fn menu_button<'a>(
    content: Element<'a, Message>,
    danger: bool,
    msg: Message,
) -> Element<'a, Message> {
    button(content)
        .width(Length::Fill)
        .padding([7, 12])
        .style(move |_t, st| {
            let hover = matches!(st, button::Status::Hovered | button::Status::Pressed);
            button::Style {
                background: Some(Background::Color(if hover {
                    if danger {
                        theme::with_alpha(theme::ORANGE, 0.14)
                    } else {
                        theme::LINE_SOFT
                    }
                } else {
                    Color::TRANSPARENT
                })),
                text_color: theme::TEXT,
                border: Border::default(),
                ..Default::default()
            }
        })
        .on_press(msg)
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn menu_position_clamps_near_edges_and_small_windows() {
        let win = Size::new(800.0, 600.0);
        let (x, y, max_h) = clamp_menu_position(Point::new(790.0, 590.0), win, 300.0);
        assert_eq!(max_h, 568.0);
        assert_eq!(x, 800.0 - MENU_WIDTH - 12.0);
        assert_eq!(y, 600.0 - 300.0 - 12.0);

        let (x_tl, y_tl, _) = clamp_menu_position(Point::new(-20.0, -5.0), win, 200.0);
        assert_eq!((x_tl, y_tl), (8.0, 8.0));

        // Tiny window falls back cleanly to minimum floor 8.0 and 180.0 max height.
        let tiny = Size::new(100.0, 120.0);
        let (xt, yt, max_ht) = clamp_menu_position(Point::new(90.0, 90.0), tiny, 400.0);
        assert_eq!(max_ht, 180.0);
        assert_eq!((xt, yt), (8.0, 8.0));
    }
}
