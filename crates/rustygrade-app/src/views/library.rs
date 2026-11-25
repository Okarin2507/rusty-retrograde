use iced::widget::{button, container, mouse_area, row, space, stack, text};
use iced::{Alignment, Background, Border, Color, Element, Length, Padding};

use super::widgets::title_block;
use super::{bold, flyout_menu, outline_button_style};
use crate::library_view::LibrarySort;
use crate::{AlbumKey, App, LibraryMode, Message, Tab, context_menu, icons, motion, pages, theme};

pub(super) fn view_header(app: &App) -> Element<'_, Message> {
    let tab = app.shown_tab();
    let title = match tab {
        Tab::Library | Tab::NowPlaying => "LIBRARY_DATA",
        Tab::Playlists => "PLAYLISTS",
        Tab::Search => "SEARCH",
        Tab::Settings => "SYSTEM_SETTINGS",
    };
    if tab != Tab::Library {
        return row![title_block(title)]
            .align_y(Alignment::Center)
            .height(28)
            .into();
    }
    if app.select_mode {
        let sel_count = app.selected_albums.len();
        let total = app.albums.len();
        let has_sel = sel_count > 0;
        return row![
            title_block(title),
            space::horizontal(),
            text(format!("{sel_count} / {total} SELECTED"))
                .size(10)
                .color(theme::TEXT_DIM),
            button(text("SELECT_ALL").size(10).font(bold()).color(theme::TEAL))
                .padding([4, 8])
                .style(theme::bare_button_style())
                .on_press(Message::SelectAllAlbums),
            button(
                row![
                    icons::icon(
                        "trash",
                        12.0,
                        if has_sel {
                            theme::ORANGE
                        } else {
                            theme::TEXT_MUTE
                        }
                    ),
                    text("DELETE").size(10).font(bold()).color(if has_sel {
                        theme::ORANGE
                    } else {
                        theme::TEXT_MUTE
                    }),
                ]
                .spacing(4)
                .align_y(Alignment::Center),
            )
            .padding([5, 10])
            .style(move |_t, st| {
                let hot =
                    has_sel && matches!(st, button::Status::Hovered | button::Status::Pressed);
                button::Style {
                    background: Some(Background::Color(if has_sel {
                        theme::with_alpha(theme::ORANGE, if hot { 0.25 } else { 0.14 })
                    } else {
                        Color::TRANSPARENT
                    })),
                    text_color: if has_sel {
                        theme::ORANGE
                    } else {
                        theme::TEXT_MUTE
                    },
                    border: Border {
                        color: if has_sel { theme::ORANGE } else { theme::LINE },
                        width: 1.0,
                        radius: 0.0.into(),
                    },
                    ..Default::default()
                }
            })
            .on_press_maybe(has_sel.then_some(Message::DeleteSelectedAlbums)),
            button(icons::icon("x", 16.0, theme::TEXT_DIM))
                .padding(4)
                .style(theme::bare_button_style())
                .on_press(Message::ExitSelectMode),
        ]
        .spacing(12)
        .align_y(Alignment::Center)
        .height(28)
        .into();
    }
    let chip = |label: &'static str, on: bool, msg: Message| {
        button(text(label).size(10))
            .padding([5, 8])
            .style(theme::chip_style(on))
            .on_press(msg)
    };
    let mut header = row![
        title_block(title),
        row![
            chip(
                "ALBUMS",
                app.library_mode == LibraryMode::Albums,
                Message::SetLibraryMode(LibraryMode::Albums)
            ),
            chip(
                "ARTISTS",
                app.library_mode == LibraryMode::Artists,
                Message::SetLibraryMode(LibraryMode::Artists)
            ),
        ]
        .spacing(4),
        space::horizontal(),
        button(
            row![
                icons::icon("arrowupdown", 12.0, theme::TEXT_DIM),
                text(app.library_sort.label()).size(10).color(theme::TEXT),
            ]
            .spacing(8)
            .align_y(Alignment::Center),
        )
        .padding([6, 12])
        .style(outline_button_style(app.sort_menu_open))
        .on_press(Message::ToggleSortMenu),
    ]
    .spacing(12)
    .align_y(Alignment::Center)
    .height(28);
    if app.library_mode == LibraryMode::Albums {
        header = header.push(
            button(
                row![
                    icons::icon("checksquare", 12.0, theme::TEXT_DIM),
                    text("SELECT").size(10).color(theme::TEXT),
                ]
                .spacing(6)
                .align_y(Alignment::Center),
            )
            .padding([6, 12])
            .style(outline_button_style(false))
            .on_press(Message::EnterSelectMode),
        );
    }
    header.into()
}

pub(super) fn view_library(app: &App, accent: Color) -> Element<'_, Message> {
    let on_grid = !(app.library_mode == LibraryMode::Albums && app.album_detail_open
        || app.library_mode == LibraryMode::Artists && app.artist_detail_open);
    let mut layers = stack![view_library_page(app, accent)]
        .width(Length::Fill)
        .height(Length::Fill);
    if app.sort_menu_open && on_grid {
        let items = LibrarySort::ALL
            .iter()
            .map(|s| {
                (
                    s.label(),
                    *s == app.library_sort,
                    Message::SelectLibrarySort(*s),
                )
            })
            .collect();
        layers = layers
            .push(iced::widget::opaque(
                mouse_area(space::horizontal().width(Length::Fill).height(Length::Fill))
                    .on_press(Message::ToggleSortMenu),
            ))
            .push(
                container(flyout_menu(items, 168.0))
                    .width(Length::Fill)
                    .align_x(Alignment::End)
                    .padding(Padding::ZERO.right(14.0)),
            );
    }
    layers.into()
}

fn view_library_page(app: &App, accent: Color) -> Element<'_, Message> {
    match app.library_mode {
        LibraryMode::Albums => {
            if app.album_detail_open
                && let Some(page) = pages::album_detail(app, accent)
            {
                return page;
            }
            albums_grid(app, accent)
        }
        LibraryMode::Artists => {
            if app.artist_detail_open
                && let Some(page) = pages::artist_detail(app, accent)
            {
                return page;
            }
            pages::artists_grid(app, accent)
        }
    }
}

fn albums_grid(app: &App, accent: Color) -> Element<'_, Message> {
    let total = app.library_albums.len();
    if total == 0 {
        let hint = if app.graphics_report.effective_display.starts_with("Wayland") {
            "USE ADD MUSIC TO SCAN A FOLDER"
        } else {
            "USE ADD MUSIC OR DROP FOLDERS AND FILES ANYWHERE IN THE WINDOW"
        };
        return pages::empty_library_box(hint);
    }
    let (columns, card_w, card_h) = app.grid_layout(pages::GridArea::Library);
    pages::virtual_card_grid(
        app.lib_scroll_id.clone(),
        app.lib_scroll_y,
        app.window_size.height,
        columns,
        card_h,
        total,
        |pos| {
            let idx = *app.library_albums.get(pos)?;
            let a = app.albums.get(idx)?;
            let key = AlbumKey::from_album(a);
            let cover = app
                .covers
                .get_first(&[a.cover_thumb_path.as_deref(), a.cover_full_path.as_deref()]);
            if app.select_mode {
                let selected = app.selected_albums.contains(&key);
                Some(pages::media_card_opts(
                    "ALB",
                    a.name.clone(),
                    a.artist.to_uppercase(),
                    cover,
                    &a.name,
                    card_w,
                    card_h,
                    accent,
                    Message::ToggleAlbumSelected(key.clone()),
                    Some(selected),
                    Some(pages::CardMotion {
                        key: motion::key_of(&key),
                        play: None,
                    }),
                ))
            } else {
                let key_menu = key.clone();
                let card = pages::media_card_opts(
                    "ALB",
                    a.name.clone(),
                    a.artist.to_uppercase(),
                    cover,
                    &a.name,
                    card_w,
                    card_h,
                    accent,
                    Message::SelectAlbum(key.clone()),
                    None,
                    Some(pages::CardMotion {
                        key: motion::key_of(&key),
                        play: Some(Message::PlayAlbum(key)),
                    }),
                );
                Some(context_menu::trigger(card, move |pos| {
                    Message::OpenAlbumMenu(key_menu.clone(), pos)
                }))
            }
        },
    )
}
