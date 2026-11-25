use iced::widget::{
    button, column, container, image, responsive, row, scrollable, space, stack, text,
};
use iced::{Alignment, Background, Border, Color, ContentFit, Element, Length, Padding};

use super::lyrics::lyrics_body;
use crate::smooth_scroll::smooth;
use crate::{App, Message, format_mm_ss, icons, theme};

fn round_btn<'a>(icon: Element<'a, Message>, msg: Message) -> Element<'a, Message> {
    button(
        container(icon)
            .width(44)
            .height(44)
            .align_x(Alignment::Center)
            .align_y(Alignment::Center),
    )
    .padding(0)
    .style(|_t, status| button::Style {
        background: Some(Background::Color(
            if matches!(status, button::Status::Hovered) {
                Color::from_rgba(1.0, 1.0, 1.0, 0.1)
            } else {
                Color::TRANSPARENT
            },
        )),
        border: Border {
            radius: 22.0.into(),
            ..Default::default()
        },
        ..Default::default()
    })
    .on_press(msg)
    .into()
}

pub(super) fn content<'a>(app: &'a App, accent: Color) -> Element<'a, Message> {
    let fade = app.fs_blur_fade();
    let blur_layer = |h: &image::Handle, a: f32| -> Element<'a, Message> {
        image(h.clone())
            .content_fit(ContentFit::Cover)
            .width(Length::Fill)
            .height(Length::Fill)
            .opacity(a)
            .into()
    };
    let mut layers: Vec<Element<'a, Message>> = vec![
        container(space::horizontal())
            .width(Length::Fill)
            .height(Length::Fill)
            .style(theme::fill_style(theme::VOID))
            .into(),
    ];
    if fade < 1.0
        && let Some(prev) = &app.fs_blur_prev
    {
        let a = if app.fs_blur.is_some() {
            1.0
        } else {
            1.0 - fade
        };
        layers.push(blur_layer(prev, a));
    }
    if let Some((_, h)) = &app.fs_blur {
        layers.push(blur_layer(h, fade));
    }
    if app.fs_blur.is_some() || (fade < 1.0 && app.fs_blur_prev.is_some()) {
        let shade = if app.fs_blur.is_some() {
            1.0
        } else {
            1.0 - fade
        };
        layers.push(
            container(space::horizontal())
                .width(Length::Fill)
                .height(Length::Fill)
                .style(theme::fill_style(Color::from_rgba(
                    0.0,
                    0.0,
                    0.0,
                    0.56 * shade,
                )))
                .into(),
        );
    }
    let backdrop: Element<'a, Message> = stack(layers).into();

    let album_upper = app
        .current_track
        .as_ref()
        .map(|t| t.album.to_uppercase())
        .unwrap_or_default();
    let header = row![
        round_btn(
            icons::icon("chevrondown", 32.0, theme::TEXT_DIM),
            Message::SelectTab(app.previous_tab),
        ),
        container(
            column![
                text("NOW PLAYING").size(10).color(theme::TEXT_DIM),
                text(crate::pages::fit(&album_upper, 240.0, 12.0).into_owned())
                    .size(12)
                    .font(crate::views::bold())
                    .color(Color::WHITE),
            ]
            .spacing(2)
            .align_x(Alignment::Center),
        )
        .width(Length::Fill)
        .align_x(Alignment::Center),
        space::horizontal().width(44),
    ]
    .align_y(Alignment::Center)
    .padding(Padding {
        top: 24.0,
        right: 48.0,
        bottom: 0.0,
        left: 48.0,
    });

    let body = responsive(move |size| {
        let half = ((size.width.min(1200.0) - 64.0) / 2.0 - 24.0).max(200.0);
        let cover_size = half.min(550.0).min(size.height - 220.0).max(160.0) - 34.0;
        let panel_h = (size.height - 460.0).clamp(180.0, 320.0);
        body_view(app, accent, cover_size.max(120.0), half.min(550.0), panel_h)
    });

    stack![
        backdrop,
        container(body)
            .width(Length::Fill)
            .height(Length::Fill)
            .padding(Padding {
                top: 80.0,
                right: 32.0,
                bottom: 48.0,
                left: 32.0,
            }),
        header,
    ]
    .width(Length::Fill)
    .height(Length::Fill)
    .into()
}

fn corner<'a>(color: Color, top_left: bool) -> Element<'a, Message> {
    let bar = |w: f32, h: f32| {
        container(space::horizontal())
            .width(w)
            .height(h)
            .style(theme::fill_style(color))
    };
    let l: Element<'a, Message> = if top_left {
        column![bar(24.0, 2.0), bar(2.0, 22.0)].into()
    } else {
        column![
            container(bar(2.0, 22.0)).width(24).align_x(Alignment::End),
            container(bar(24.0, 2.0)),
        ]
        .into()
    };
    container(l)
        .width(Length::Fill)
        .height(Length::Fill)
        .align_x(if top_left {
            Alignment::Start
        } else {
            Alignment::End
        })
        .align_y(if top_left {
            Alignment::Start
        } else {
            Alignment::End
        })
        .into()
}

fn body_view<'a>(
    app: &'a App,
    accent: Color,
    cover_size: f32,
    col_w: f32,
    panel_h: f32,
) -> Element<'a, Message> {
    let (title, artist) = app
        .current_track
        .as_ref()
        .map(|t| (t.title.as_str(), t.artist.as_str()))
        .unwrap_or(("", ""));
    let cover_handle = app.current_track.as_ref().and_then(|t| {
        app.fs_cover
            .as_ref()
            .filter(|(id, _)| *id == t.id)
            .map(|(_, h)| h)
            .or_else(|| {
                app.covers
                    .get_first(&[t.cover_full_path.as_deref(), t.cover_thumb_path.as_deref()])
            })
    });

    let framed = container(
        stack![
            crate::covers_ui::render_cover(cover_handle, cover_size, 0.0, accent, title),
            corner(theme::ORANGE, true),
            corner(theme::TEAL, false),
        ]
        .width(cover_size)
        .height(cover_size),
    )
    .padding(16)
    .style(|_t| container::Style {
        background: Some(Background::Color(Color::from_rgba(0.0, 0.0, 0.0, 0.18))),
        border: Border {
            color: Color::from_rgba(1.0, 1.0, 1.0, 0.05),
            width: 1.0,
            radius: 12.0.into(),
        },
        ..Default::default()
    });

    let max_dur = app.duration_s.max(1.0);
    let pos = app
        .seeking_pos
        .unwrap_or(app.position_s)
        .clamp(0.0, max_dur);
    let seek = crate::views::seek_strip(
        pos,
        max_dur,
        crate::views::SeekStripSpec {
            height: 14.0,
            track_h: 4.0,
            fill: Color::WHITE,
            track: Color::from_rgba(1.0, 1.0, 1.0, 0.22),
            knob: Some(Color::WHITE),
        },
    );

    let mode = |name: &'static str, on: bool, msg: Message| {
        button(icons::icon(
            name,
            24.0,
            if on { theme::TEAL } else { theme::TEXT_DIM },
        ))
        .padding(8)
        .style(theme::bare_button_style())
        .on_press(msg)
    };
    let skip = |name: &'static str, msg: Message| {
        button(icons::icon(name, 36.0, theme::TEXT))
            .padding(8)
            .style(theme::bare_button_style())
            .on_press(msg)
    };
    let controls = row![
        mode("shuffle", app.shuffle, Message::ToggleShuffle),
        skip("skipback", Message::Previous),
        crate::motion::hover_scope_slop(
            crate::motion::HEX_HOVER,
            10.0,
            button(icons::hex_play(app.is_playing, 91.0))
                .padding(0)
                .style(theme::bare_button_style())
                .on_press(Message::TogglePlayPause),
        ),
        skip("skipforward", Message::Next),
        mode(
            app.repeat_mode.icon_name(),
            app.repeat_mode.is_active(),
            Message::CycleRepeat,
        ),
    ]
    .spacing(24)
    .align_y(Alignment::Center);

    let time_color = Color::from_rgba(1.0, 1.0, 1.0, 0.72);
    let head = column![
        text(crate::pages::fit(title, col_w, 44.0).into_owned())
            .size(44)
            .font(crate::views::bold())
            .color(Color::WHITE),
        text(crate::pages::fit(artist, col_w, 20.0).into_owned())
            .size(20)
            .color(theme::TEXT_SOFT),
        space::vertical().height(12),
        seek,
        row![
            text(format_mm_ss(pos)).size(12).color(time_color),
            space::horizontal(),
            text(format_mm_ss(app.duration_s))
                .size(12)
                .color(time_color),
        ],
        container(controls)
            .width(Length::Fill)
            .align_x(Alignment::Center),
    ]
    .spacing(8);

    let right = column![head, panel(app, accent, panel_h)]
        .spacing(24)
        .width(Length::Fixed(col_w));
    let left = container(framed)
        .width(Length::Fill)
        .align_x(Alignment::End);

    container(
        row![
            left,
            container(right)
                .width(Length::Fill)
                .align_x(Alignment::Start),
        ]
        .spacing(48)
        .width(Length::Fill)
        .align_y(Alignment::Center),
    )
    .width(Length::Fill)
    .height(Length::Fill)
    .align_y(Alignment::Center)
    .into()
}

fn tab_btn<'a>(label: &'static str, on: bool, msg: Message) -> Element<'a, Message> {
    button(
        text(label)
            .size(11)
            .font(crate::views::bold())
            .color(if on { Color::WHITE } else { theme::TEXT_MUTE }),
    )
    .padding([2, 0])
    .style(|_t, _s| button::Style {
        background: None,
        ..Default::default()
    })
    .on_press(msg)
    .into()
}

fn panel<'a>(app: &'a App, accent: Color, panel_h: f32) -> Element<'a, Message> {
    let upcoming_from = app
        .queue_index
        .map_or(0, |i| i + 1)
        .min(app.queue_tracks.len());
    let upcoming_total = app.queue_tracks.len() - upcoming_from;
    let shown_to = (upcoming_from + crate::popups::QUEUE_ROWS).min(app.queue_tracks.len());

    let head = container(
        row![
            tab_btn(
                "NEXT IN QUEUE",
                !app.fs_lyrics_tab,
                Message::SetFsLyricsTab(false)
            ),
            container(tab_btn(
                "LYRICS",
                app.fs_lyrics_tab,
                Message::SetFsLyricsTab(true)
            ))
            .padding(Padding {
                left: 20.0,
                ..Padding::ZERO
            }),
            space::horizontal(),
            text(if app.fs_lyrics_tab {
                app.lyrics
                    .as_ref()
                    .map(|d| d.source.to_uppercase())
                    .unwrap_or_default()
            } else {
                format!("{upcoming_total} TRACKS")
            })
            .size(10)
            .color(theme::TEXT_MUTE),
        ]
        .align_y(Alignment::Center),
    )
    .padding([12, 16])
    .width(Length::Fill)
    .style(|_t| container::Style {
        background: Some(Background::Color(Color::from_rgba(0.1, 0.1, 0.11, 0.6))),
        border: Border {
            color: Color::from_rgba(1.0, 1.0, 1.0, 0.05),
            width: 1.0,
            radius: 0.0.into(),
        },
        ..Default::default()
    });

    let body: Element<'a, Message> = if app.fs_lyrics_tab {
        lyrics_body(app)
    } else if upcoming_from >= shown_to {
        container(
            text("End of queue")
                .size(12)
                .font(iced::Font {
                    style: iced::font::Style::Italic,
                    ..iced::Font::DEFAULT
                })
                .color(theme::TEXT_MUTE),
        )
        .width(Length::Fill)
        .height(Length::Fill)
        .align_x(Alignment::Center)
        .align_y(Alignment::Center)
        .into()
    } else {
        let rows: Vec<Element<'a, Message>> = app.queue_tracks[upcoming_from..shown_to]
            .iter()
            .enumerate()
            .map(|(rel, t)| {
                let qi = upcoming_from + rel;
                let cover = app
                    .covers
                    .get_first(&[t.cover_thumb_path.as_deref(), t.cover_full_path.as_deref()]);
                row![
                    button(
                        row![
                            container(
                                text(format!("{}", rel + 1))
                                    .size(10)
                                    .color(theme::TEXT_MUTE)
                            )
                            .width(16)
                            .align_x(Alignment::Center),
                            crate::covers_ui::render_cover(cover, 32.0, 4.0, accent, &t.title),
                            column![
                                text(crate::pages::fit(&t.title, 340.0, 12.0).into_owned())
                                    .size(12)
                                    .color(Color::WHITE),
                                text(crate::pages::fit(&t.artist, 340.0, 10.0).into_owned())
                                    .size(10)
                                    .color(theme::TEXT_SUBTLE),
                            ]
                            .spacing(2),
                        ]
                        .spacing(12)
                        .align_y(Alignment::Center),
                    )
                    .width(Length::Fill)
                    .padding([6, 8])
                    .style(theme::nav_item_style(false, accent))
                    .on_press(Message::JumpToQueueIndex(qi)),
                    button(icons::icon("x", 14.0, theme::TEXT_DIM))
                        .padding(8)
                        .style(theme::bare_button_style())
                        .on_press(Message::RemoveQueueIndex(qi)),
                ]
                .align_y(Alignment::Center)
                .into()
            })
            .collect();
        let mut list = column(rows).spacing(2).padding(Padding::ZERO.right(10.0));
        if upcoming_total > crate::popups::QUEUE_ROWS {
            list = list.push(
                container(
                    text(format!(
                        "+{} MORE",
                        upcoming_total - crate::popups::QUEUE_ROWS
                    ))
                    .size(10)
                    .color(theme::TEXT_MUTE),
                )
                .width(Length::Fill)
                .align_x(Alignment::Center)
                .padding(8),
            );
        }
        smooth(scrollable(list).height(Length::Fill))
    };

    container(column![head, container(body).padding(8).height(Length::Fill)].height(Length::Fill))
        .height(Length::Fixed(panel_h))
        .style(|_t| container::Style {
            background: Some(Background::Color(Color::from_rgba(0.1, 0.1, 0.11, 0.4))),
            border: Border {
                color: Color::from_rgba(1.0, 1.0, 1.0, 0.05),
                width: 1.0,
                radius: 8.0.into(),
            },
            ..Default::default()
        })
        .into()
}
