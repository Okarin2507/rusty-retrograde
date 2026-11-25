use iced::widget::{button, column, container, row, scrollable, space, text, text_input};
use iced::{Alignment, Background, Border, Color, Element, Length, Padding};
use rustygrade_core::{CoverPaths, PlaylistCovers};

use super::cards::{GridArea, artist_caption, card_grid, empty_state, media_card};
use crate::library_view::artist_key;
use crate::smooth_scroll::smooth;
use crate::views::bold;
use crate::{AlbumKey, App, Message, PlaylistView, SearchTab, context_menu, icons, motion, theme};

const SEARCH_CARDS: usize = 120;

fn tab_btn<'a>(label: &'a str, on: bool, msg: Message) -> Element<'a, Message> {
    button(
        column![
            text(label).size(11).font(bold()).color(if on {
                theme::ORANGE
            } else {
                theme::TEXT_MUTE
            }),
            container(space::horizontal())
                .width(label.len() as f32 * 8.0)
                .height(2)
                .style(theme::fill_style(if on {
                    theme::ORANGE
                } else {
                    Color::TRANSPARENT
                })),
        ]
        .spacing(6),
    )
    .padding([4, 0])
    .style(|_t, _s| button::Style {
        background: None,
        ..Default::default()
    })
    .on_press(msg)
    .into()
}

pub fn search_page<'a>(app: &'a App, accent: Color) -> Element<'a, Message> {
    let field = container(
        row![
            icons::icon("search", 16.0, theme::TEXT_DIM),
            text_input("SEARCH_DATABASE...", &app.search_query)
                .id(app.search_input_id.clone())
                .on_input(Message::SearchChanged)
                .size(14)
                .padding([10, 6])
                .style(|_t, _s| text_input::Style {
                    background: Background::Color(Color::TRANSPARENT),
                    border: Border::default(),
                    icon: theme::TEXT_DIM,
                    placeholder: theme::TEXT_MUTE,
                    value: theme::TEXT,
                    selection: theme::with_alpha(theme::TEAL, 0.3),
                }),
        ]
        .spacing(10)
        .align_y(Alignment::Center),
    )
    .padding([2, 14])
    .width(Length::Fixed(448.0))
    .style(|_t| container::Style {
        background: Some(Background::Color(theme::VOID)),
        border: Border {
            color: theme::TEAL,
            width: 1.0,
            radius: 0.0.into(),
        },
        ..Default::default()
    });

    let has_query = !app.search_query.trim().is_empty();
    let tabs = row![
        tab_btn(
            "ALL",
            app.search_tab == SearchTab::All,
            Message::SetSearchTab(SearchTab::All)
        ),
        tab_btn(
            "TRACKS",
            app.search_tab == SearchTab::Tracks,
            Message::SetSearchTab(SearchTab::Tracks)
        ),
        tab_btn(
            "ALBUMS",
            app.search_tab == SearchTab::Albums,
            Message::SetSearchTab(SearchTab::Albums)
        ),
        tab_btn(
            "ARTISTS",
            app.search_tab == SearchTab::Artists,
            Message::SetSearchTab(SearchTab::Artists)
        ),
        tab_btn(
            "PLAYLISTS",
            app.search_tab == SearchTab::Playlists,
            Message::SetSearchTab(SearchTab::Playlists)
        ),
    ]
    .spacing(24);

    let mut sections: Vec<Element<'a, Message>> = Vec::new();
    let heading = |label: String| -> Element<'a, Message> {
        container(text(label).size(14).font(bold()).color(theme::TEXT_DIM))
            .padding(Padding {
                top: 20.0,
                right: 0.0,
                bottom: 8.0,
                left: 0.0,
            })
            .width(Length::Fill)
            .into()
    };

    let want = |t: SearchTab| app.search_tab == SearchTab::All || app.search_tab == t;
    let (columns, card_w, card_h) = app.grid_layout(GridArea::Search);
    let more = |shown: usize, total: usize| -> Option<Element<'a, Message>> {
        (total > shown).then(|| {
            container(
                text(format!("SHOWING {shown} OF {total} // REFINE THE SEARCH"))
                    .size(10)
                    .color(theme::TEXT_MUTE),
            )
            .padding([12, 0])
            .width(Length::Fill)
            .align_x(Alignment::Center)
            .into()
        })
    };

    if !has_query {
        sections.push(empty_state("SEARCH_READY", "TYPE TO SEARCH THE LIBRARY"));
    } else {
        if want(SearchTab::Tracks) && !app.search_tracks.is_empty() {
            let total = app.search_tracks.len();
            sections.push(heading(format!("TRACKS ({total})")));
            let cards: Vec<Element<'a, Message>> = app
                .search_tracks
                .iter()
                .take(SEARCH_CARDS)
                .enumerate()
                .filter_map(|(i, &(aidx, tidx))| {
                    let a = app.albums.get(aidx)?;
                    let t = a.tracks.get(tidx)?;
                    let cover = app
                        .covers
                        .get_first(&[t.cover_thumb_path.as_deref(), t.cover_full_path.as_deref()]);
                    let card = media_card(
                        "TRK",
                        t.title.clone(),
                        t.artist.to_uppercase(),
                        cover,
                        &t.title,
                        card_w,
                        card_h,
                        accent,
                        Message::PlaySearchTrack(i),
                    );
                    let t_clone = t.clone();
                    let album_artist = a.artist.clone();
                    let album_year = a.year;
                    Some(context_menu::trigger(card, move |pos| {
                        Message::OpenTrackMenu {
                            track: t_clone.clone(),
                            album_artist: album_artist.clone(),
                            album_year,
                            removable_playlist: None,
                            pos,
                        }
                    }))
                })
                .collect();
            sections.push(card_grid(cards, columns));
            sections.extend(more(SEARCH_CARDS, total));
        }
        if want(SearchTab::Albums) && !app.search_albums.is_empty() {
            let total = app.search_albums.len();
            sections.push(heading(format!("ALBUMS ({total})")));
            let cards: Vec<Element<'a, Message>> = app
                .search_albums
                .iter()
                .take(SEARCH_CARDS)
                .filter_map(|&i| app.albums.get(i))
                .map(|a| {
                    let key = AlbumKey::from_album(a);
                    let k_menu = key.clone();
                    let cover = app
                        .covers
                        .get_first(&[a.cover_thumb_path.as_deref(), a.cover_full_path.as_deref()]);
                    let card = media_card(
                        "ALB",
                        a.name.clone(),
                        a.artist.to_uppercase(),
                        cover,
                        &a.name,
                        card_w,
                        card_h,
                        accent,
                        Message::SelectAlbum(key),
                    );
                    context_menu::trigger(card, move |pos| {
                        Message::OpenAlbumMenu(k_menu.clone(), pos)
                    })
                })
                .collect();
            sections.push(card_grid(cards, columns));
            sections.extend(more(SEARCH_CARDS, total));
        }
        if want(SearchTab::Artists) && !app.search_artists.is_empty() {
            let total = app.search_artists.len();
            sections.push(heading(format!("ARTISTS ({total})")));
            let cards: Vec<Element<'a, Message>> = app
                .search_artists
                .iter()
                .take(SEARCH_CARDS)
                .filter_map(|&i| app.artists.get(i))
                .map(|ar| {
                    let cover = ar
                        .album_indices
                        .iter()
                        .filter_map(|&ai| app.albums.get(ai))
                        .find_map(|a| {
                            app.covers.get_first(&[
                                a.cover_thumb_path.as_deref(),
                                a.cover_full_path.as_deref(),
                            ])
                        });
                    media_card(
                        "ART",
                        ar.name.clone(),
                        artist_caption(ar),
                        cover,
                        &ar.name,
                        card_w,
                        card_h,
                        accent,
                        Message::OpenArtist(artist_key(&ar.name)),
                    )
                })
                .collect();
            sections.push(card_grid(cards, columns));
            sections.extend(more(SEARCH_CARDS, total));
        }
        if want(SearchTab::Playlists) && !app.search_playlists.is_empty() {
            sections.push(heading(format!(
                "PLAYLISTS ({})",
                app.search_playlists.len()
            )));
            let cards: Vec<Element<'a, Message>> = app
                .search_playlists
                .iter()
                .map(|(view, name, n)| {
                    let cover = if let PlaylistView::Custom(pid) = *view {
                        app.collections
                            .playlists
                            .iter()
                            .find(|p| p.id == pid)
                            .and_then(|p| {
                                CoverPaths::to_local_path(p.cover_art.as_deref())
                                    .and_then(|full| {
                                        let thumb = PlaylistCovers::thumb_for(&full);
                                        app.covers.get_first(&[
                                            Some(thumb.as_path()),
                                            Some(full.as_path()),
                                        ])
                                    })
                                    .or_else(|| {
                                        p.track_ids
                                            .first()
                                            .and_then(|tid| app.lookup_track(tid))
                                            .and_then(|t| {
                                                app.covers.get_first(&[
                                                    t.cover_thumb_path.as_deref(),
                                                    t.cover_full_path.as_deref(),
                                                ])
                                            })
                                    })
                            })
                    } else {
                        None
                    };
                    let badge = if matches!(*view, PlaylistView::Smart(_)) {
                        "SMRT"
                    } else {
                        "PL"
                    };
                    let card = media_card(
                        badge,
                        name.clone(),
                        format!("{n} TRACKS"),
                        cover,
                        name,
                        card_w,
                        card_h,
                        accent,
                        Message::OpenPlaylistPage(*view),
                    );
                    let target = *view;
                    context_menu::trigger(card, move |pos| Message::OpenPlaylistMenu(target, pos))
                })
                .collect();
            sections.push(card_grid(cards, columns));
        }
        if sections.is_empty() {
            sections.push(empty_state("NO DATA FOUND", "NOTHING MATCHES THE SEARCH"));
        }
    }

    let header_strip = container(
        column![
            field,
            container(tabs).padding(Padding {
                top: 16.0,
                ..Padding::ZERO
            }),
        ]
        .width(Length::Fill),
    )
    .padding([16, 24])
    .width(Length::Fill)
    .style(|_t| container::Style {
        background: Some(Background::Color(theme::PANEL)),
        ..Default::default()
    });

    let search_in = |bg: Color| {
        motion::EnterSpec::new(300, motion::Ease::CubicOut, 8.0, motion::Fade::Cover(bg))
    };
    column![
        motion::enter("search", search_in(theme::PANEL), header_strip),
        container(space::horizontal())
            .width(Length::Fill)
            .height(1)
            .style(theme::fill_style(theme::LINE)),
        motion::enter(
            "search",
            search_in(theme::VOID),
            smooth(
                scrollable(
                    container(column(sections).width(Length::Fill)).padding(Padding {
                        top: 8.0,
                        right: 24.0,
                        bottom: 24.0,
                        left: 24.0,
                    }),
                )
                .height(Length::Fill),
            ),
        ),
    ]
    .spacing(0)
    .height(Length::Fill)
    .into()
}
