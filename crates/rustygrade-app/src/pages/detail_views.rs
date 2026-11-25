use iced::widget::{button, column, container, row, space, text, text_input};
use iced::{Alignment, Background, Border, Color, Element, Length, Padding};
use rustygrade_core::{CoverPaths, PlaylistCovers, Track};

use super::DetailCtx;
use super::cards::{GridArea, card_grid, empty_state, media_card};
use super::detail::{DetailSpec, detail_page};
use super::rows::track_row;
use crate::library_view::artist_key;
use crate::views::bold;
use crate::{AlbumKey, App, Message, PlaylistView, context_menu, icons, theme};

pub fn album_detail<'a>(app: &'a App, accent: Color) -> Option<Element<'a, Message>> {
    let key = app.selected_album.as_ref()?;
    let album = app.find_album(key)?;
    let cover = app.covers.get_first(&[
        album.cover_full_path.as_deref(),
        album.cover_thumb_path.as_deref(),
    ]);
    let all_liked =
        !album.tracks.is_empty() && album.tracks.iter().all(|t| app.liked_set.contains(&t.id));
    let k_menu = key.clone();
    let extra = row![
        button(icons::icon(
            "heart",
            26.0,
            if all_liked {
                theme::ORANGE
            } else {
                theme::TEXT_DIM
            }
        ))
        .padding(6)
        .style(theme::bare_button_style())
        .on_press(Message::ToggleLikeTracks(
            album.tracks.iter().map(|t| t.id.clone()).collect(),
        )),
        context_menu::left_trigger(
            button(icons::icon("more", 26.0, theme::TEXT_DIM))
                .padding(6)
                .style(theme::bare_button_style())
                .on_press(Message::Noop),
            move |pos| Message::OpenDetailMenu(DetailCtx::Album(k_menu.clone()), pos),
        ),
    ]
    .spacing(16)
    .align_y(Alignment::Center);

    let spec = DetailSpec {
        ctx: DetailCtx::Album(key.clone()),
        chip: "ALBUM_DATA",
        title: album.name.clone(),
        artist: Some(album.artist.clone()),
        year: Some(album.year),
        quality: Track::album_quality_label(&album.tracks),
        cover,
        tint: app.detail_tint.unwrap_or(theme::PANEL),
        show_cover_col: false,
        extra: Some(extra.into()),
        artist_link: Some(artist_key(&album.artist)),
        body: None,
        meta_text: None,
        is_liked_cover: false,
        change_cover_playlist: None,
    };
    Some(detail_page(app, spec, &album.tracks, accent))
}

/// Renders the artist page: header, most played tracks, and album cards.
pub fn artist_detail<'a>(app: &'a App, accent: Color) -> Option<Element<'a, Message>> {
    let key = app.selected_artist.as_deref()?;
    let artist = app.find_artist(key)?;
    let mut artist_albums: Vec<&rustygrade_core::Album> = artist
        .album_indices
        .iter()
        .filter_map(|&i| app.albums.get(i))
        .collect();
    let cover = artist_albums.iter().find_map(|a| {
        app.covers
            .get_first(&[a.cover_full_path.as_deref(), a.cover_thumb_path.as_deref()])
    });
    let ctx = DetailCtx::Artist(key.to_owned());

    let mut played: Vec<(&Track, i32, i64)> = artist_albums
        .iter()
        .flat_map(|a| a.tracks.iter())
        .filter_map(|t| {
            app.stats
                .get(&t.id)
                .filter(|s| s.count > 0)
                .map(|s| (t, s.count, s.last))
        })
        .collect();
    played.sort_by_key(|&(_, count, last)| (std::cmp::Reverse(count), std::cmp::Reverse(last)));
    let popular: Vec<Track> = played
        .into_iter()
        .take(5)
        .map(|(t, _, _)| t.clone())
        .collect();

    let section = |label: String| -> Element<'a, Message> {
        container(text(label).size(14).font(bold()).color(theme::TEXT_DIM))
            .padding(Padding {
                top: 16.0,
                right: 0.0,
                bottom: 12.0,
                left: 0.0,
            })
            .into()
    };
    let mut body = column![].spacing(2);
    if !popular.is_empty() {
        body = body.push(section("MOST PLAYED".to_owned()));
        for (i, t) in popular.iter().enumerate() {
            let play = Message::PlayTrackFromList(popular.clone(), i);
            body = body.push(track_row(app, t, i + 1, &ctx, true, None, play, accent));
        }
    }
    let n = artist_albums.len();
    body = body.push(section(format!("ALBUMS ({n})")));
    let (columns, card_w, card_h) = app.grid_layout(GridArea::Detail);
    let cards: Vec<Element<'a, Message>> = artist_albums
        .iter()
        .map(|a| {
            let k = AlbumKey::from_album(a);
            let k_menu = k.clone();
            let c = app
                .covers
                .get_first(&[a.cover_thumb_path.as_deref(), a.cover_full_path.as_deref()]);
            let sub = if a.year > 0 {
                a.year.to_string()
            } else {
                a.artist.to_uppercase()
            };
            let card = media_card(
                "ALB",
                a.name.clone(),
                sub,
                c,
                &a.name,
                card_w,
                card_h,
                accent,
                Message::SelectAlbum(k),
            );
            context_menu::trigger(card, move |pos| Message::OpenAlbumMenu(k_menu.clone(), pos))
        })
        .collect();
    body = body.push(card_grid(cards, columns));

    artist_albums.sort_by_key(|al| (if al.year == 0 { i32::MAX } else { al.year }, al.added_at));
    let all_tracks: Vec<Track> = artist_albums
        .iter()
        .flat_map(|al| al.tracks.iter().cloned())
        .collect();

    let spec = DetailSpec {
        ctx,
        chip: "ARTIST",
        title: artist.name.clone(),
        artist: None,
        year: None,
        quality: None,
        cover,
        tint: app.detail_tint.unwrap_or(theme::PANEL),
        show_cover_col: true,
        extra: None,
        artist_link: None,
        body: Some(body.into()),
        meta_text: Some(if n == 1 {
            "1 ALBUM".to_owned()
        } else {
            format!("{n} ALBUMS")
        }),
        is_liked_cover: false,
        change_cover_playlist: None,
    };
    Some(detail_page(app, spec, &all_tracks, accent))
}

/// Favorites, custom playlist, or smart playlist detail view.
pub fn playlist_page<'a>(app: &'a App, accent: Color) -> Element<'a, Message> {
    let view = app.playlist_view;
    let liked_tint = Color::from_rgb8(0x50, 0x38, 0xA0);
    let tracks = app.detail_tracks(&DetailCtx::Playlist(view));
    if tracks.is_empty() && matches!(view, PlaylistView::Liked) {
        return container(empty_state(
            "NO DATA FOUND",
            "MARK TRACKS AS 'FAVORITE' TO POPULATE",
        ))
        .style(|_t| container::Style {
            background: Some(Background::Color(theme::PANEL)),
            border: Border {
                color: theme::LINE_SOFT,
                width: 1.0,
                radius: 0.0.into(),
            },
            ..Default::default()
        })
        .width(Length::Fill)
        .height(Length::Fill)
        .into();
    }

    let (chip, title, tint, is_liked_cover, change_cover_playlist, extra): (
        &str,
        String,
        Color,
        bool,
        Option<i64>,
        Option<Element<'a, Message>>,
    ) = match view {
        PlaylistView::Liked => {
            let extra: Element<'a, Message> = context_menu::left_trigger(
                button(icons::icon("more", 26.0, theme::TEXT_DIM))
                    .padding(6)
                    .style(theme::bare_button_style())
                    .on_press(Message::Noop),
                move |pos| Message::OpenDetailMenu(DetailCtx::Playlist(PlaylistView::Liked), pos),
            );
            (
                "PLAYLIST_DATA",
                "FAVORITES".to_owned(),
                liked_tint,
                true,
                None,
                Some(extra),
            )
        }
        PlaylistView::Custom(id) => {
            let name = app
                .collections
                .playlists
                .iter()
                .find(|p| p.id == id)
                .map(|p| p.name.clone())
                .unwrap_or_default();
            let all_liked =
                !tracks.is_empty() && tracks.iter().all(|t| app.liked_set.contains(&t.id));
            let track_ids: Vec<String> = tracks.iter().map(|t| t.id.clone()).collect();
            let extra: Element<'a, Message> = row![
                button(icons::icon(
                    "heart",
                    26.0,
                    if all_liked {
                        theme::ORANGE
                    } else {
                        theme::TEXT_DIM
                    }
                ))
                .padding(6)
                .style(theme::bare_button_style())
                .on_press_maybe(
                    (!track_ids.is_empty()).then_some(Message::ToggleLikeTracks(track_ids))
                ),
                context_menu::left_trigger(
                    button(icons::icon("more", 26.0, theme::TEXT_DIM))
                        .padding(6)
                        .style(theme::bare_button_style())
                        .on_press(Message::Noop),
                    move |pos| {
                        Message::OpenDetailMenu(DetailCtx::Playlist(PlaylistView::Custom(id)), pos)
                    },
                ),
                space::horizontal().width(8),
                text_input("RENAME...", &app.rename_playlist_buf)
                    .style(theme::input_style)
                    .on_input(Message::RenamePlaylistInput)
                    .on_submit(Message::RenamePlaylistSubmit(id))
                    .size(12)
                    .padding([6, 8])
                    .width(160),
                button(text("RENAME").size(10))
                    .padding([7, 10])
                    .style(theme::ghost_button_style(false))
                    .on_press(Message::RenamePlaylistSubmit(id)),
            ]
            .spacing(8)
            .align_y(Alignment::Center)
            .into();
            (
                "PLAYLIST_DATA",
                name,
                app.detail_tint.unwrap_or(theme::PANEL),
                false,
                Some(id),
                Some(extra),
            )
        }
        PlaylistView::Smart(id) => {
            let sp = app
                .collections
                .smart
                .as_deref()
                .unwrap_or(&[])
                .iter()
                .find(|s| s.id == id);
            let extra: Option<Element<'a, Message>> = sp.map(|_| {
                context_menu::left_trigger(
                    button(icons::icon("more", 26.0, theme::TEXT_DIM))
                        .padding(6)
                        .style(theme::bare_button_style())
                        .on_press(Message::Noop),
                    move |pos| {
                        Message::OpenDetailMenu(DetailCtx::Playlist(PlaylistView::Smart(id)), pos)
                    },
                )
            });
            (
                "SMART_DATA",
                sp.map(|s| s.name.clone()).unwrap_or_default(),
                app.detail_tint.unwrap_or(theme::PANEL),
                false,
                None,
                extra,
            )
        }
    };

    let custom_cover = if let PlaylistView::Custom(id) = view {
        app.collections
            .playlists
            .iter()
            .find(|p| p.id == id)
            .and_then(|p| CoverPaths::to_local_path(p.cover_art.as_deref()))
    } else {
        None
    };
    let cover = custom_cover
        .as_deref()
        .and_then(|p| {
            let thumb = PlaylistCovers::thumb_for(p);
            app.covers.get_first(&[Some(p), Some(thumb.as_path())])
        })
        .or_else(|| {
            tracks.first().and_then(|t| {
                app.covers
                    .get_first(&[t.cover_full_path.as_deref(), t.cover_thumb_path.as_deref()])
            })
        });
    let quality = Track::album_quality_label(&tracks);
    let spec = DetailSpec {
        ctx: DetailCtx::Playlist(view),
        chip,
        title,
        artist: if matches!(view, PlaylistView::Liked) {
            Some("USER DATA".to_owned())
        } else {
            None
        },
        year: None,
        quality,
        cover,
        tint,
        show_cover_col: true,
        extra,
        artist_link: None,
        body: None,
        meta_text: None,
        is_liked_cover,
        change_cover_playlist,
    };
    detail_page(app, spec, &tracks, accent)
}
