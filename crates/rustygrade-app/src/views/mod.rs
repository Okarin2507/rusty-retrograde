mod cover_picker;
mod library;
mod mini_player;
mod player_bar;
mod settings;
mod shortcuts;
mod sidebar;
mod smart_modal;
mod tag_editor;
mod titlebar;
mod widgets;

pub use mini_player::render_mini_player;
pub(crate) use widgets::{SeekStripSpec, seek_strip};

use iced::widget::{button, column, container, mouse_area, opaque, space, stack, text};
use iced::{Alignment, Background, Border, Color, Element, Font, Length, Padding};

use crate::{App, Message, Tab, context_menu, dialogs, egg, fullscreen, popups, theme, toasts};

pub(crate) fn bold() -> Font {
    Font {
        weight: iced::font::Weight::Bold,
        ..Font::DEFAULT
    }
}

/// A text button that looks like plain text and lightens on hover (artist links).
pub(crate) fn link_style(color: Color) -> impl Fn(&iced::Theme, button::Status) -> button::Style {
    move |_t, st| button::Style {
        background: None,
        text_color: if matches!(st, button::Status::Hovered) {
            Color::WHITE
        } else {
            color
        },
        border: Border::default(),
        ..Default::default()
    }
}

/// A retro flyout: a column of menu entries (label, selected, message) on a raised card.
pub(crate) fn flyout_menu<'a>(
    items: Vec<(&'a str, bool, Message)>,
    width: f32,
) -> Element<'a, Message> {
    let entries: Vec<Element<'a, Message>> = items
        .into_iter()
        .map(|(label, selected, msg)| {
            button(text(label).size(10).color(if selected {
                theme::ORANGE
            } else {
                theme::TEXT_SOFT
            }))
            .width(Length::Fill)
            .padding([8, 12])
            .style(move |_t, st| button::Style {
                background: Some(Background::Color(
                    if matches!(st, button::Status::Hovered) {
                        theme::LINE_SOFT
                    } else if selected {
                        theme::with_alpha(theme::ORANGE, 0.08)
                    } else {
                        Color::TRANSPARENT
                    },
                )),
                text_color: theme::TEXT,
                border: Border::default(),
                ..Default::default()
            })
            .on_press(msg)
            .into()
        })
        .collect();
    opaque(
        container(column(entries))
            .padding(4)
            .width(width)
            .style(|_t| container::Style {
                background: Some(Background::Color(theme::RAISED)),
                border: Border {
                    color: theme::BORDER_STRONG,
                    width: 1.0,
                    radius: 0.0.into(),
                },
                shadow: iced::Shadow {
                    color: Color::from_rgba(0.0, 0.0, 0.0, 0.6),
                    offset: iced::Vector::new(0.0, 8.0),
                    blur_radius: 24.0,
                },
                ..Default::default()
            }),
    )
}

pub(crate) fn outline_button_style(
    open: bool,
) -> impl Fn(&iced::Theme, button::Status) -> button::Style {
    move |_t, st| {
        let hot = open || matches!(st, button::Status::Hovered);
        button::Style {
            background: Some(Background::Color(if hot {
                theme::LINE_SOFT
            } else {
                Color::TRANSPARENT
            })),
            text_color: theme::TEXT,
            border: Border {
                color: if hot { theme::TEXT_MUTE } else { theme::LINE },
                width: 1.0,
                radius: 0.0.into(),
            },
            ..Default::default()
        }
    }
}

fn view_drop_overlay<'a>(accent: Color) -> Element<'a, Message> {
    let overlay = container(
        container(
            column![
                text("DROP MUSIC TO IMPORT").size(18).color(accent),
                text("Folders are added & scanned; single tracks are scanned directly.")
                    .size(13)
                    .color(theme::TEXT_DIM),
            ]
            .spacing(6)
            .align_x(Alignment::Center),
        )
        .padding(28)
        .style(theme::modal_style),
    )
    .width(Length::Fill)
    .height(Length::Fill)
    .align_x(Alignment::Center)
    .align_y(Alignment::Center)
    .style(|_t| container::Style {
        background: Some(Background::Color(theme::with_alpha(theme::VOID, 0.76))),
        ..Default::default()
    });
    opaque(overlay)
}

pub fn render_app(app: &App) -> Element<'_, Message> {
    let accent = app.accent_anim.current_accent;
    let shown = app.shown_tab();
    let body = match shown {
        Tab::Library | Tab::NowPlaying => library::view_library(app, accent),
        Tab::Playlists => crate::pages::playlist_page(app, accent),
        Tab::Search => crate::pages::search_page(app, accent),
        Tab::Settings => settings::view_settings(app),
    };

    let full_bleed = match shown {
        Tab::Library => {
            (app.library_mode == crate::LibraryMode::Albums && app.album_detail_open)
                || (app.library_mode == crate::LibraryMode::Artists && app.artist_detail_open)
        }
        Tab::Playlists => true,
        _ => false,
    };
    let stripe = iced::widget::row![
        container(space::horizontal())
            .width(Length::FillPortion(1))
            .height(2)
            .style(theme::fill_style(theme::ORANGE)),
        container(space::horizontal())
            .width(Length::FillPortion(1))
            .height(2)
            .style(theme::fill_style(theme::GOLD)),
        container(space::horizontal())
            .width(Length::FillPortion(1))
            .height(2)
            .style(theme::fill_style(theme::TEAL)),
    ]
    .width(Length::Fill);

    let content = column![
        stripe,
        if full_bleed || shown == Tab::Search {
            Element::from(
                container(body)
                    .padding(if full_bleed {
                        Padding::ZERO
                    } else {
                        Padding {
                            top: 0.0,
                            right: 16.0,
                            bottom: 0.0,
                            left: 16.0,
                        }
                    })
                    .width(Length::Fill)
                    .height(Length::Fill),
            )
        } else {
            column![
                if app.select_mode && matches!(shown, Tab::Library) {
                    container(library::view_header(app))
                        .padding([12, 16])
                        .width(Length::Fill)
                        .style(|_t| container::Style {
                            background: Some(Background::Color(Color::from_rgba8(
                                0x12, 0x12, 0x12, 0.95,
                            ))),
                            border: Border {
                                color: theme::LINE,
                                width: 1.0,
                                radius: 0.0.into(),
                            },
                            ..Default::default()
                        })
                        .into()
                } else {
                    Element::from(container(library::view_header(app)).padding(Padding {
                        top: 14.0,
                        right: 16.0,
                        bottom: 0.0,
                        left: 16.0,
                    }))
                },
                container(body)
                    .padding(Padding {
                        top: 0.0,
                        right: 16.0,
                        bottom: 14.0,
                        left: 16.0,
                    })
                    .width(Length::Fill)
                    .height(Length::Fill),
            ]
            .spacing(14)
            .width(Length::Fill)
            .height(Length::Fill)
            .into()
        },
    ]
    .width(Length::Fill)
    .height(Length::Fill);

    let main_col: Element<'_, Message> =
        iced::widget::row![sidebar::view_sidebar(app, accent), content]
            .spacing(12)
            .padding(Padding {
                top: 0.0,
                right: 12.0,
                bottom: 0.0,
                left: 12.0,
            })
            .width(Length::Fill)
            .height(Length::Fill)
            .into();

    let workspace = container(
        column![
            titlebar::title_strip(app),
            main_col,
            player_bar::view_transport(app, accent),
        ]
        .width(Length::Fill)
        .height(Length::Fill),
    )
    .width(Length::Fill)
    .height(Length::Fill)
    .style(|_t| container::Style {
        background: Some(Background::Color(theme::VOID)),
        border: Border {
            color: theme::LINE,
            width: 1.0,
            radius: 0.0.into(),
        },
        text_color: Some(theme::TEXT),
        ..Default::default()
    });

    let workspace: Element<'_, Message> = fullscreen::view(app, workspace.into(), accent);
    let workspace = popups::overlay(app, workspace, accent);
    // Keep resize handles above the main workspace/popups, but beneath modal dialogs and context menus.
    let mut layers: Vec<Element<'_, Message>> = vec![workspace, titlebar::resize_handles()];
    if app.add_music_menu_open {
        let menu = flyout_menu(
            vec![
                ("SCAN_FOLDER", false, Message::PickFolders),
                ("SCAN_FILES", false, Message::PickFiles),
            ],
            190.0,
        );
        let rail = app.sidebar_width < 150.0;
        let left = if rail { app.sidebar_width + 18.0 } else { 20.0 };
        let top = (if rail { 210.0_f32 } else { 248.0 })
            .clamp(12.0, (app.window_size.height - 90.0).max(12.0));
        layers.push(opaque(
            mouse_area(space::horizontal().width(Length::Fill).height(Length::Fill))
                .on_press(Message::ToggleAddMusicMenu),
        ));
        layers.push(
            container(menu)
                .width(Length::Fill)
                .height(Length::Fill)
                .padding(Padding {
                    top,
                    left,
                    right: 0.0,
                    bottom: 0.0,
                })
                .into(),
        );
    }
    if app.bank_menu_open {
        let menu = flyout_menu(
            vec![
                ("NEW_PLAYLIST", false, Message::ToggleCreatePlaylist(true)),
                ("NEW_SMART_PLAYLIST", false, Message::NewSmartPlaylist),
                ("IMPORT_PLAYLIST", false, Message::ImportPlaylistPick),
            ],
            184.0,
        );
        let rail = app.sidebar_width < 150.0;
        let left = if rail { app.sidebar_width + 18.0 } else { 74.0 };
        let top = (if rail { 290.0_f32 } else { 370.0 })
            .clamp(12.0, (app.window_size.height - 120.0).max(12.0));
        layers.push(opaque(
            mouse_area(space::horizontal().width(Length::Fill).height(Length::Fill))
                .on_press(Message::ToggleBankMenu),
        ));
        layers.push(
            container(menu)
                .width(Length::Fill)
                .height(Length::Fill)
                .padding(Padding {
                    top,
                    left,
                    right: 0.0,
                    bottom: 0.0,
                })
                .into(),
        );
    }
    if app.show_shortcuts_modal {
        layers.push(shortcuts::view_shortcuts_modal());
    }
    if app.egg_open {
        layers.push(egg::view_egg_modal(app));
    }
    if app.create_playlist_open {
        layers.push(dialogs::create_playlist_modal(app));
    }
    if let Some(draft) = &app.smart_draft {
        layers.push(smart_modal::smart_editor_modal(app.window_size, draft));
    }
    if let Some(draft) = &app.tag_draft {
        match &app.cover_picker {
            Some(picker) => layers.push(cover_picker::cover_picker_modal(app, picker)),
            None => layers.push(tag_editor::tag_editor_modal(app, draft)),
        }
    }
    if let Some(d) = &app.confirm {
        layers.push(dialogs::confirm_view(d));
    }
    if app.bg_prompt {
        layers.push(dialogs::background_prompt(app.bg_remember));
    }
    if app.deleting_albums {
        let turn = if app.animations_enabled() {
            (app.anim_phase * 1.4).fract() as f32
        } else {
            0.0
        };
        layers.push(dialogs::deleting_overlay(turn));
    }
    if let Some(menu) = &app.context_menu {
        layers.push(context_menu::view(app, menu));
    }
    if app.hovered_files > 0 {
        layers.push(view_drop_overlay(accent));
    }
    if let Some(t) = toasts::layer(app) {
        layers.push(t);
    }
    stack(layers)
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}
