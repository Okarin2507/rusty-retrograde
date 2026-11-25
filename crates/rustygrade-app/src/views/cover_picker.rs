use iced::widget::text::Wrapping;
use iced::widget::{button, column, container, image, row, scrollable, space, text, text_input};
use iced::{Alignment, Background, Border, Color, Element, Length, Padding};

use super::bold;
use super::widgets::tag_chip;
use crate::cover_fetch::Picker;
use crate::smooth_scroll::smooth;
use crate::{App, Message, dialogs, pages, theme};

pub(super) fn cover_picker_modal<'a>(app: &'a App, p: &'a Picker) -> Element<'a, Message> {
    const COLS: usize = 5;
    const THUMB: f32 = 116.0;
    let field_label =
        |label: &'static str| text(label).size(9).font(bold()).color(theme::TEXT_MUTE);
    let busy = p.downloading;
    let input = |value: &str, on: fn(String) -> Message| {
        let t = text_input("", value)
            .size(12)
            .padding([8, 10])
            .style(theme::input_style)
            .width(Length::Fill);
        if busy {
            t
        } else {
            t.on_input(on).on_submit(Message::CoverPickerSearch)
        }
    };
    let search_btn = button(text("SEARCH").size(10).font(bold()).color(theme::TEXT))
        .padding(Padding {
            top: 9.5,
            right: 14.0,
            bottom: 8.5,
            left: 14.0,
        })
        .style(|_t, st| button::Style {
            background: Some(Background::Color(
                if matches!(st, button::Status::Hovered) {
                    theme::HOVER
                } else {
                    theme::PANEL_DEEP
                },
            )),
            border: Border {
                color: theme::LINE,
                width: 1.0,
                radius: 0.0.into(),
            },
            text_color: theme::TEXT,
            ..Default::default()
        })
        .on_press_maybe((!busy && !p.searching()).then_some(Message::CoverPickerSearch));

    let query_row = row![
        column![
            field_label("ARTIST"),
            input(&p.artist, Message::CoverPickerArtist)
        ]
        .spacing(4)
        .width(Length::FillPortion(2)),
        column![
            field_label("ALBUM"),
            input(&p.album, Message::CoverPickerAlbum)
        ]
        .spacing(4)
        .width(Length::FillPortion(3)),
        search_btn,
    ]
    .spacing(12)
    .align_y(Alignment::End);

    let service_state = |pending: bool, n: usize| {
        if pending {
            "SEARCHING...".to_owned()
        } else {
            format!("{n} FOUND")
        }
    };
    let mut status_items: Vec<Element<'_, Message>> = Vec::new();
    let mut push_status =
        |prefix: &'static str, label: &'static str, color: Color, state: String| {
            let tag = if status_items.is_empty() {
                format!("> {label}")
            } else {
                format!("{prefix} {label}")
            };
            status_items.push(text(tag).size(9).font(bold()).color(color).into());
            status_items.push(text(state).size(9).color(theme::TEXT_DIM).into());
        };
    if app.settings.artwork_itunes {
        push_status(
            "//",
            "ITUNES",
            theme::TEAL,
            service_state(p.itunes_pending, p.itunes.len()),
        );
    }
    if app.settings.artwork_deezer {
        push_status(
            "//",
            "DEEZER",
            theme::ORANGE,
            service_state(p.deezer_pending, p.deezer.len()),
        );
    }
    if app.settings.artwork_musicbrainz {
        let state = if !p.musicbrainz_pending && p.album.trim().is_empty() {
            "NEEDS AN ALBUM".to_owned()
        } else {
            service_state(p.musicbrainz_pending, p.musicbrainz.len())
        };
        push_status("//", "MUSICBRAINZ", theme::GOLD, state);
    }
    if app.settings.artwork_discogs {
        let state = if app.settings.discogs_token.trim().is_empty() {
            "NEEDS TOKEN".to_owned()
        } else {
            service_state(p.discogs_pending, p.discogs.len())
        };
        push_status("//", "DISCOGS", theme::STARLIGHT, state);
    }
    if status_items.is_empty() {
        status_items.push(
            text("> NO ONLINE APIS ENABLED IN SETTINGS")
                .size(9)
                .color(theme::ORANGE)
                .into(),
        );
    }
    let status = row(status_items)
        .spacing(6)
        .align_y(Alignment::Center)
        .wrap()
        .vertical_spacing(4);

    let cards: Vec<Element<'_, Message>> = p
        .items()
        .map(|f| {
            let c = &f.cand;
            let selected = p.selected.as_deref() == Some(c.thumb_url.as_str());
            let (chip_label, chip_color) = match c.source {
                "ITUNES" => ("ITUNES", theme::TEAL),
                "DEEZER" => ("DEEZER", theme::ORANGE),
                "DISCOGS" => ("DISCOGS", theme::STARLIGHT),
                _ => ("CAA", theme::GOLD),
            };
            let mut chips = row![tag_chip(chip_label, chip_color)].spacing(4);
            if c.best {
                chips = chips.push(tag_chip("BEST MATCH", theme::ORANGE));
            }
            let meta = match &c.year {
                Some(y) => format!("{} · {y}", c.artist.to_uppercase()),
                None => c.artist.to_uppercase(),
            };
            let body = column![
                container(
                    image(f.thumb.clone())
                        .width(Length::Fill)
                        .height(Length::Fixed(THUMB))
                        .content_fit(iced::ContentFit::Cover),
                )
                .width(Length::Fill)
                .style(|_t| container::Style {
                    background: Some(Background::Color(theme::LINE_SOFT)),
                    ..Default::default()
                }),
                text(pages::fit(&c.title, THUMB, 10.0).into_owned())
                    .size(10)
                    .font(bold())
                    .color(theme::TEXT)
                    .wrapping(Wrapping::None),
                text(pages::fit(&meta, THUMB, 9.0).into_owned())
                    .size(9)
                    .color(theme::TEXT_MUTE)
                    .wrapping(Wrapping::None),
                chips,
            ]
            .spacing(5);
            button(body)
                .padding(5)
                .width(Length::Fill)
                .style(theme::album_card_style(selected))
                .on_press_maybe((!busy).then(|| Message::CoverPickerSelect(c.thumb_url.clone())))
                .into()
        })
        .collect();

    let mut active_names = Vec::new();
    if app.settings.artwork_itunes {
        active_names.push("ITUNES");
    }
    if app.settings.artwork_deezer {
        active_names.push("DEEZER");
    }
    if app.settings.artwork_musicbrainz {
        active_names.push("MUSICBRAINZ");
    }
    if app.settings.artwork_discogs && !app.settings.discogs_token.trim().is_empty() {
        active_names.push("DISCOGS");
    }
    let active_summary = active_names.join(" / ");

    let grid: Element<'_, Message> = if cards.is_empty() {
        let (title, sub): (&str, String) = if p.searching() {
            ("SCANNING_REMOTE_INDEX...", active_summary)
        } else if let Some(e) = &p.error {
            (e.as_str(), "EDIT THE QUERY AND SEARCH AGAIN".to_owned())
        } else {
            (
                "NO COVERS FOUND",
                "EDIT THE QUERY AND SEARCH AGAIN".to_owned(),
            )
        };
        container(
            column![
                text(title).size(12).font(bold()).color(if p.searching() {
                    theme::TEAL
                } else {
                    theme::TEXT_DIM
                }),
                text(sub).size(9).color(theme::TEXT_MUTE),
            ]
            .spacing(6)
            .align_x(Alignment::Center),
        )
        .width(Length::Fill)
        .height(Length::Fill)
        .align_x(Alignment::Center)
        .align_y(Alignment::Center)
        .into()
    } else {
        let mut rows = column![].spacing(10);
        let mut cards = cards.into_iter().peekable();
        while cards.peek().is_some() {
            let mut r = row![].spacing(10);
            for _ in 0..COLS {
                match cards.next() {
                    Some(c) => r = r.push(c),
                    None => r = r.push(space::horizontal().width(Length::Fill)),
                }
            }
            rows = rows.push(r);
        }
        smooth(
            scrollable(rows)
                .direction(scrollable::Direction::Vertical(
                    scrollable::Scrollbar::new()
                        .width(6)
                        .scroller_width(6)
                        .spacing(6),
                ))
                .height(Length::Fill),
        )
    };
    let grid_h = (app.window_size.height - 260.0).clamp(220.0, 392.0);
    let grid_box = container(grid)
        .height(Length::Fixed(grid_h))
        .width(Length::Fill)
        .padding(10)
        .style(|_t| container::Style {
            background: Some(Background::Color(theme::PANEL_DEEP)),
            border: Border {
                color: theme::LINE,
                width: 1.0,
                radius: 0.0.into(),
            },
            ..Default::default()
        });

    let body = column![query_row, status, grid_box]
        .spacing(12)
        .width(Length::Fill)
        .into();

    let left: Element<'_, Message> = if busy {
        text("DOWNLOADING_FULL_RES...")
            .size(10)
            .font(bold())
            .color(theme::TEAL)
            .into()
    } else if let (Some(e), false) = (&p.error, p.items().next().is_none()) {
        text(e.as_str()).size(10).color(theme::ORANGE).into()
    } else if let Some(c) = p.selected_candidate() {
        text(
            pages::fit(
                &format!("SELECTED // {}", c.title.to_uppercase()),
                330.0,
                10.0,
            )
            .into_owned(),
        )
        .size(10)
        .color(theme::TEXT_DIM)
        .into()
    } else {
        space::horizontal().into()
    };
    let cancel_btn = button(text("CANCEL").size(12).color(theme::TEXT_DIM))
        .padding([8, 14])
        .style(theme::bare_button_style())
        .on_press(Message::CoverPickerClose);
    let use_btn = button(text("USE COVER").size(12).font(bold()))
        .padding([8, 16])
        .style(theme::accent_button_style())
        .on_press_maybe((!busy && p.selected.is_some()).then_some(Message::CoverPickerUse));
    let footer = row![left, space::horizontal(), cancel_btn, use_btn]
        .spacing(12)
        .align_y(Alignment::Center)
        .into();

    let total = p.total_found();
    dialogs::modal_frame_sized(
        dialogs::ModalChrome {
            title: "COVER_SEARCH",
            dot_color: theme::TEAL,
            corner_color: theme::ORANGE,
            width: (app.window_size.width - 48.0).clamp(360.0, 760.0),
            header_right: Some(
                text(format!("{total} RESULTS"))
                    .size(10)
                    .color(theme::TEXT_MUTE)
                    .into(),
            ),
        },
        body,
        footer,
        Some(Message::CoverPickerClose),
    )
}
