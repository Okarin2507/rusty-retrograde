use iced::widget::text::Wrapping;
use iced::widget::{button, column, container, hover, row, space, text};
use iced::{Alignment, Color, Element, Length, Padding};
use rustygrade_core::Track;

use super::{DetailCtx, fit};
use crate::covers_ui::render_cover;
use crate::views::bold;
use crate::{App, Message, context_menu, format_mm_ss, icons, reorder, theme};

fn mini_eq_glyph<'a>(color: Color) -> Element<'a, Message> {
    row![
        container(space::horizontal())
            .width(2)
            .height(7)
            .style(theme::fill_style(color)),
        container(space::horizontal())
            .width(2)
            .height(12)
            .style(theme::fill_style(color)),
        container(space::horizontal())
            .width(2)
            .height(9)
            .style(theme::fill_style(color)),
    ]
    .spacing(2)
    .align_y(Alignment::End)
    .into()
}

pub(super) fn disc_header<'a>(disc: i32) -> Element<'a, Message> {
    row![
        icons::icon("disc", 16.0, theme::TEXT_SUBTLE),
        text(format!("DISC {disc}"))
            .size(13)
            .font(bold())
            .color(theme::TEXT_SOFT),
        container(space::horizontal())
            .width(Length::Fill)
            .height(1)
            .style(theme::fill_style(theme::LINE_SOFT)),
    ]
    .spacing(12)
    .align_y(Alignment::Center)
    .padding(Padding {
        top: 18.0,
        right: 0.0,
        bottom: 8.0,
        left: 0.0,
    })
    .into()
}

#[allow(clippy::too_many_arguments)]
pub(super) fn track_row<'a>(
    app: &'a App,
    track: &Track,
    number: usize,
    ctx: &DetailCtx,
    show_cover: bool,
    removable_playlist: Option<i64>,
    play: Message,
    accent: Color,
) -> Element<'a, Message> {
    let active = app
        .current_track
        .as_ref()
        .is_some_and(|cur| cur.id == track.id);
    let is_liked = app.liked_set.contains(&track.id);
    let (album_artist, album_year) = match ctx {
        DetailCtx::Album(k) => app
            .find_album(k)
            .map(|a| (a.artist.clone(), a.year))
            .unwrap_or_default(),
        _ => (String::new(), 0),
    };

    let number_cell: Element<'a, Message> = if active {
        mini_eq_glyph(accent)
    } else {
        text(format!("{number:02}"))
            .size(15)
            .color(theme::TEXT_MUTE)
            .into()
    };

    let edge = container(space::horizontal())
        .width(2)
        .height(34)
        .style(theme::fill_style(if active {
            accent
        } else {
            Color::TRANSPARENT
        }));

    let mut body = row![
        edge,
        container(number_cell).width(46).align_x(Alignment::Center),
    ]
    .spacing(12)
    .align_y(Alignment::Center);

    if show_cover {
        let cover = app.covers.get_first(&[
            track.cover_thumb_path.as_deref(),
            track.cover_full_path.as_deref(),
        ]);
        body = body.push(render_cover(cover, 32.0, 0.0, accent, &track.title));
    }

    // Reserve a single aligned 54 px action zone (`heart` [24] + spacing [4] + `more` [26])
    // before the 72 px duration column so hover controls appear without shifting the heart.
    let static_heart: Element<'a, Message> = if is_liked {
        container(icons::icon("heart", 14.0, theme::ORANGE))
            .width(24)
            .align_x(Alignment::Center)
            .into()
    } else {
        space::horizontal().width(24).into()
    };

    body = body
        .push(
            column![
                text(fit(&track.title, 760.0, 14.0).into_owned())
                    .size(14)
                    .font(bold())
                    .color(if active { accent } else { theme::TEXT })
                    .wrapping(Wrapping::None),
                text(fit(&track.artist.to_uppercase(), 760.0, 10.0).into_owned())
                    .size(10)
                    .color(theme::TEXT_MUTE)
                    .wrapping(Wrapping::None),
            ]
            .spacing(2)
            .width(Length::Fill),
        )
        .push(container(static_heart).width(54).align_x(Alignment::Start))
        .push(
            container(
                text(format_mm_ss(track.duration))
                    .size(12)
                    .color(theme::TEXT_MUTE),
            )
            .width(72)
            .align_x(Alignment::Center),
        );

    let base_btn = button(container(body).padding([9, 0]))
        .width(Length::Fill)
        .padding(0)
        .style(theme::nav_item_style(active, accent))
        .on_press(play);

    let track_id_like = track.id.clone();
    let track_for_btn = track.clone();
    let aa_for_btn = album_artist.clone();
    let track_for_rclick = track.clone();
    let aa_for_rclick = album_artist;

    let heart_btn = reorder::no_drag(
        button(icons::icon(
            "heart",
            14.0,
            if is_liked {
                theme::ORANGE
            } else {
                theme::TEXT_DIM
            },
        ))
        .padding(5)
        .style(theme::bare_button_style())
        .on_press(Message::ToggleLike(track_id_like)),
    );
    let more_btn = reorder::no_drag(context_menu::left_trigger(
        button(icons::icon("more", 16.0, theme::TEXT_DIM))
            .padding(5)
            .style(theme::bare_button_style())
            .on_press(Message::Noop),
        move |pos| Message::OpenTrackMenu {
            track: track_for_btn.clone(),
            album_artist: aa_for_btn.clone(),
            album_year,
            removable_playlist,
            pos,
        },
    ));

    let hover_controls = container(
        row![heart_btn, more_btn]
            .spacing(4)
            .width(54)
            .align_y(Alignment::Center),
    )
    .width(Length::Fill)
    .height(Length::Fill)
    .align_x(Alignment::End)
    .align_y(Alignment::Center)
    .padding(Padding::ZERO.right(84.0));

    let row_hovered = hover(base_btn, hover_controls);
    context_menu::trigger(row_hovered, move |pos| Message::OpenTrackMenu {
        track: track_for_rclick.clone(),
        album_artist: aa_for_rclick.clone(),
        album_year,
        removable_playlist,
        pos,
    })
}
