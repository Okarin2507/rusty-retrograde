use iced::widget::{
    button, column, container, hover, responsive, row, scrollable, space, stack, text,
};
use iced::{Alignment, Background, Border, Color, Element, Length, Padding, Radians, Vector};
use rustygrade_core::Track;
use std::time::Duration;

use super::DetailCtx;
use super::cards::{empty_state, fit, visible_row_range};
use super::rows::{disc_header, track_row};
use crate::covers_ui::render_cover;
use crate::smooth_scroll::smooth;
use crate::views::bold;
use crate::{App, Message, PlaylistView, icons, motion, theme};

/// The header color of a page without a cover.
pub const NO_COVER_TINT: Color = Color::from_rgb(
    0x40 as f32 / 255.0,
    0x40 as f32 / 255.0,
    0x44 as f32 / 255.0,
);

/// Everything the detail page needs besides the track list.
pub struct DetailSpec<'a> {
    pub ctx: DetailCtx,
    pub chip: &'a str,
    pub title: String,
    pub artist: Option<String>,
    pub year: Option<i32>,
    pub quality: Option<String>,
    pub cover: Option<&'a iced::widget::image::Handle>,
    pub tint: Color,
    pub show_cover_col: bool,
    pub extra: Option<Element<'a, Message>>,
    pub artist_link: Option<String>,
    pub body: Option<Element<'a, Message>>,
    pub meta_text: Option<String>,
    pub is_liked_cover: bool,
    pub change_cover_playlist: Option<i64>,
}

pub fn detail_page<'a>(
    app: &'a App,
    spec: DetailSpec<'a>,
    tracks: &[Track],
    accent: Color,
) -> Element<'a, Message> {
    let tint = spec.tint;
    let tint_alpha = if spec.is_liked_cover {
        1.0
    } else {
        app.detail_tint_alpha()
    };
    let name_for_cover = spec.title.clone();

    let frame_offset = container(space::horizontal())
        .width(224)
        .height(224)
        .style(|_t| container::Style {
            border: Border {
                color: theme::BORDER_STRONG,
                width: 1.0,
                radius: 0.0.into(),
            },
            ..Default::default()
        });
    let cover_inner: Element<'a, Message> = if spec.is_liked_cover {
        container(icons::icon("heart", 72.0, Color::WHITE))
            .width(222)
            .height(222)
            .align_x(Alignment::Center)
            .align_y(Alignment::Center)
            .style(|_t| container::Style {
                background: Some(Background::Gradient(
                    iced::gradient::Linear::new(Radians(std::f32::consts::FRAC_PI_4 * 3.0))
                        .add_stop(0.0, Color::from_rgb8(0x45, 0x0A, 0xF5))
                        .add_stop(0.6, Color::from_rgb8(0x8E, 0x8E, 0xE5))
                        .add_stop(1.0, Color::from_rgb8(0xC4, 0xEF, 0xD9))
                        .into(),
                )),
                ..Default::default()
            })
            .into()
    } else if let Some(pid) = spec.change_cover_playlist {
        let base_cov = render_cover(spec.cover, 222.0, 0.0, accent, &name_for_cover);
        let overlay_btn = button(
            container(
                column![
                    icons::icon("image", 32.0, Color::WHITE),
                    text("CHANGE_IMG").size(12).font(bold()).color(Color::WHITE),
                ]
                .spacing(8)
                .align_x(Alignment::Center),
            )
            .width(222)
            .height(222)
            .align_x(Alignment::Center)
            .align_y(Alignment::Center),
        )
        .padding(0)
        .style(|_t, _s| button::Style {
            background: Some(Background::Color(Color::from_rgba(0.0, 0.0, 0.0, 0.6))),
            text_color: Color::WHITE,
            border: Border::default(),
            ..Default::default()
        })
        .on_press(Message::PickPlaylistCover(pid));
        match spec.cover {
            Some(c) => motion::hover_scope(
                Duration::from_millis(200),
                iced::widget::stack![base_cov, overlay_btn, motion::under_image(c.clone(), 222.0)],
            ),
            None => hover(base_cov, overlay_btn),
        }
    } else {
        render_cover(spec.cover, 222.0, 0.0, accent, &name_for_cover)
    };

    let dot4 = |ax: Alignment, ay: Alignment| {
        container(
            container(space::horizontal())
                .width(4)
                .height(4)
                .style(theme::fill_style(Color::from_rgba(1.0, 1.0, 1.0, 0.5))),
        )
        .width(Length::Fill)
        .height(Length::Fill)
        .align_x(ax)
        .align_y(ay)
    };
    let cover_reticle = container(
        container(stack![
            dot4(Alignment::Start, Alignment::Start),
            dot4(Alignment::End, Alignment::Start),
            dot4(Alignment::Start, Alignment::End),
            dot4(Alignment::End, Alignment::End),
        ])
        .padding(4)
        .width(Length::Fill)
        .height(Length::Fill)
        .style(|_t| container::Style {
            border: Border {
                color: Color::from_rgba(1.0, 1.0, 1.0, 0.2),
                width: 0.5,
                radius: 0.0.into(),
            },
            ..Default::default()
        }),
    )
    .padding(8)
    .width(222)
    .height(222);

    let picture =
        container(stack![cover_inner, cover_reticle].width(222).height(222)).style(|_t| {
            container::Style {
                background: Some(Background::Color(theme::LINE_SOFT)),
                border: Border {
                    color: theme::TEXT_SUBTLE,
                    width: 1.0,
                    radius: 0.0.into(),
                },
                ..Default::default()
            }
        });
    let cover = stack![
        container(frame_offset).padding(Padding {
            top: 8.0,
            left: 8.0,
            right: 0.0,
            bottom: 0.0
        }),
        container(picture).padding(Padding {
            top: 0.0,
            left: 0.0,
            right: 8.0,
            bottom: 8.0
        }),
    ]
    .width(232)
    .height(232);

    let title = spec.title.to_uppercase();
    let title_el = responsive(move |size| {
        let n = title.chars().count() as f32;
        let fits = |sz: f32, lines: f32| n * sz * 0.66 <= size.width * lines;
        let (sz, lines) = if fits(60.0, 2.0) {
            (60.0, 2.0)
        } else if fits(46.0, 3.0) {
            (46.0, 3.0)
        } else {
            (36.0, 3.0)
        };
        container(
            container(
                text(title.clone())
                    .size(sz)
                    .font(bold())
                    .color(Color::WHITE)
                    .line_height(iced::widget::text::LineHeight::Absolute((sz * 1.28).into())),
            )
            .max_height(sz * 1.28 * lines + 8.0)
            .clip(true),
        )
        .height(Length::Fill)
        .align_y(Alignment::End)
        .into()
    })
    .height(Length::Fixed(180.0));

    let dot = || {
        container(space::horizontal())
            .width(4)
            .height(4)
            .style(theme::fill_style(theme::TEXT_MUTE))
    };
    let mut meta = row![].spacing(16).align_y(Alignment::Center);
    if let Some(a) = &spec.artist {
        let label = row![
            icons::icon("user", 12.0, Color::WHITE),
            text(fit(a, 420.0, 12.0).into_owned()).size(12),
        ]
        .spacing(6)
        .align_y(Alignment::Center);
        meta = meta.push(match spec.artist_link.clone() {
            Some(key) => Element::from(
                button(label)
                    .padding(0)
                    .style(crate::views::link_style(Color::WHITE))
                    .on_press(Message::OpenArtist(key)),
            ),
            None => Element::from(container(label).style(|_t| container::Style {
                text_color: Some(Color::WHITE),
                ..Default::default()
            })),
        });
    }
    if let Some(y) = spec.year.filter(|y| *y > 0) {
        meta = meta
            .push(dot())
            .push(text(y.to_string()).size(12).color(theme::TEXT_DIM));
    }
    if let Some(q) = &spec.quality {
        meta = meta
            .push(dot())
            .push(text(q.clone()).size(12).color(theme::TEXT_DIM));
    }
    if let Some(m) = &spec.meta_text {
        meta = meta.push(text(m.clone()).size(12).font(bold()).color(Color::WHITE));
    }

    let rule = row![
        container(space::horizontal())
            .width(64)
            .height(3)
            .style(theme::fill_style(theme::ORANGE)),
        container(space::horizontal())
            .width(64)
            .height(3)
            .style(theme::fill_style(theme::GOLD)),
        container(space::horizontal())
            .width(64)
            .height(3)
            .style(theme::fill_style(theme::TEAL)),
    ];
    let chip_row = row![
        container(
            text(spec.chip.to_owned())
                .size(9)
                .font(bold())
                .color(theme::TEAL),
        )
        .padding(Padding {
            top: 3.0,
            right: 8.0,
            bottom: 1.0,
            left: 8.0
        })
        .style(|_t| container::Style {
            background: Some(Background::Color(theme::with_alpha(theme::TEAL, 0.10))),
            border: Border {
                color: theme::TEAL,
                width: 1.0,
                radius: 0.0.into(),
            },
            ..Default::default()
        }),
        container(
            container(space::horizontal())
                .height(1)
                .width(Length::Fill)
                .style(theme::fill_style(theme::LINE)),
        )
        .width(Length::Fill)
        .padding([0, 12]),
        text(format!("{} TRACKS", tracks.len()))
            .size(10)
            .color(theme::GOLD),
    ]
    .align_y(Alignment::Center);
    let head_text = column![chip_row, title_el, rule, meta]
        .spacing(10)
        .width(Length::Fill);

    let has_tracks = !tracks.is_empty();
    let play = button(
        container(icons::icon("play", 20.0, theme::VOID))
            .width(48)
            .height(40)
            .align_x(Alignment::Center)
            .align_y(Alignment::Center),
    )
    .padding(0)
    .style(|_t, status| button::Style {
        background: Some(Background::Color(
            if matches!(status, button::Status::Hovered) {
                Color::WHITE
            } else {
                theme::TEXT
            },
        )),
        text_color: theme::VOID,
        border: Border {
            radius: 0.0.into(),
            ..Default::default()
        },
        ..Default::default()
    })
    .on_press_maybe(has_tracks.then(|| Message::PlayDetail(spec.ctx.clone(), None)));
    let mut actions = row![
        play,
        button(icons::icon("listmusic", 26.0, theme::TEXT_DIM))
            .padding(6)
            .style(theme::bare_button_style())
            .on_press_maybe(has_tracks.then(|| Message::QueueDetail(spec.ctx.clone()))),
    ]
    .spacing(24)
    .align_y(Alignment::Center);
    if let Some(extra) = spec.extra {
        actions = actions.push(extra);
    }

    let back_dim = Color::from_rgba(1.0, 1.0, 1.0, 0.65);
    let back = motion::hover_scope(
        Duration::from_millis(200),
        button(motion::scale_on(
            motion::Trigger::Press,
            0.96,
            100,
            row![
                motion::hover_icon(
                    icons::handle("arrowleft"),
                    14.0,
                    back_dim,
                    Color::WHITE,
                    150,
                    Vector::new(-3.0, 0.0)
                ),
                motion::hover_text_color(
                    back_dim,
                    Color::WHITE,
                    150,
                    text(app.back_label()).size(10).font(bold())
                ),
            ]
            .spacing(8)
            .align_y(Alignment::Center),
        ))
        .padding([4, 8])
        .style(theme::bare_button_style())
        .on_press(Message::Back),
    );

    let header = container(
        column![
            container(back).padding([10, 24]),
            container(row![cover, head_text].spacing(32).align_y(Alignment::End)).padding([8, 32]),
            container(actions).padding([8, 32]),
        ]
        .spacing(8),
    )
    .width(Length::Fill)
    .style(move |_t| container::Style {
        background: Some(Background::Gradient(
            iced::gradient::Linear::new(Radians(std::f32::consts::PI))
                .add_stop(0.0, theme::with_alpha(tint, 0.55 * tint_alpha))
                .add_stop(1.0, theme::PANEL)
                .into(),
        )),
        ..Default::default()
    });

    let mut head_cols = row![
        container(text("#").size(9).color(theme::TEXT_MUTE))
            .width(50)
            .align_x(Alignment::Center)
    ]
    .spacing(16);
    if spec.show_cover_col {
        head_cols = head_cols.push(space::horizontal().width(32));
    }
    head_cols = head_cols
        .push(
            text("OPERATION_LOG")
                .size(9)
                .color(theme::TEXT_MUTE)
                .width(Length::Fill),
        )
        .push(
            container(text("DURATION").size(9).color(theme::TEXT_MUTE))
                .width(80)
                .align_x(Alignment::Center),
        );
    let columns = container(head_cols)
        .padding([8, 0])
        .style(|_t| container::Style {
            border: Border {
                color: theme::LINE,
                width: 1.0,
                radius: 0.0.into(),
            },
            ..Default::default()
        });

    let removable = match &spec.ctx {
        DetailCtx::Playlist(PlaylistView::Custom(id)) => Some(*id),
        _ => None,
    };
    let body: Element<'a, Message> = if let Some(body) = spec.body {
        body
    } else if tracks.is_empty() {
        empty_state("NO DATA FOUND", "ADD TRACKS TO POPULATE")
    } else {
        let multi_disc = matches!(spec.ctx, DetailCtx::Album(_))
            && tracks
                .first()
                .is_some_and(|first| tracks.iter().any(|t| t.disc_number != first.disc_number));
        enum DetailItem {
            Disc(i32),
            Track(usize),
        }
        let mut items: Vec<DetailItem> = Vec::with_capacity(tracks.len() + 4);
        let mut last_disc = -1;
        for (i, t) in tracks.iter().enumerate() {
            if multi_disc && t.disc_number != last_disc {
                last_disc = t.disc_number;
                items.push(DetailItem::Disc(t.disc_number.max(1)));
            }
            items.push(DetailItem::Track(i));
        }

        const ROW_STRIDE: f32 = 54.0;
        let virtualize = tracks.len() > 80;
        let range = if virtualize {
            let list_scroll = (app.detail_scroll_y - 320.0).max(0.0);
            visible_row_range(
                list_scroll,
                (app.window_size.height - 160.0).max(240.0),
                ROW_STRIDE,
                items.len(),
                6,
            )
        } else {
            0..items.len()
        };
        let mut rows: Vec<Element<'a, Message>> = Vec::with_capacity(range.len());
        for item in &items[range.clone()] {
            match *item {
                DetailItem::Disc(d) => rows.push(disc_header(d)),
                DetailItem::Track(i) => {
                    let play = Message::PlayDetail(spec.ctx.clone(), Some(i));
                    rows.push(track_row(
                        app,
                        &tracks[i],
                        i + 1,
                        &spec.ctx,
                        spec.show_cover_col,
                        removable,
                        play,
                        accent,
                    ));
                }
            }
        }
        let mut list_col = column![];
        if virtualize && range.start > 0 {
            list_col = list_col.push(space::vertical().height(range.start as f32 * ROW_STRIDE));
        }
        if !multi_disc && app.can_reorder(&spec.ctx) {
            let ghost = app
                .reorder_lift
                .as_ref()
                .filter(|(ctx, from, _)| *ctx == spec.ctx && *from < tracks.len())
                .map(|(_, from, _)| {
                    track_row(
                        app,
                        &tracks[*from],
                        from + 1,
                        &spec.ctx,
                        spec.show_cover_col,
                        removable,
                        Message::Noop,
                        accent,
                    )
                });
            let lift_ctx = spec.ctx.clone();
            list_col = list_col.push(crate::reorder::list(
                rows,
                range.start,
                items.len(),
                ghost,
                accent,
                crate::reorder::Callbacks {
                    on_lift: Box::new(move |i| Message::ReorderLift(lift_ctx.clone(), i)),
                    on_drop: Box::new(Message::ReorderDrop),
                    on_edge: Box::new(Message::ReorderEdge),
                },
            ));
        } else {
            list_col = list_col.push(column(rows).spacing(crate::reorder::SPACING));
        }
        if virtualize && range.end < items.len() {
            list_col = list_col.push(
                space::vertical()
                    .height(((items.len() - range.end) as f32 * ROW_STRIDE - 2.0).max(0.0)),
            );
        }
        column![columns, list_col].spacing(8).into()
    };

    let scroll = scrollable(
        column![
            header,
            container(body).padding(Padding {
                top: 16.0,
                right: 32.0,
                bottom: 40.0,
                left: 32.0,
            }),
        ]
        .width(Length::Fill),
    )
    .id(app.detail_scroll_id.clone())
    .on_scroll(|vp| Message::DetailScrolled(vp.absolute_offset().y));

    container(motion::enter(
        "detail",
        motion::EnterSpec::new(
            200,
            motion::Ease::Linear,
            0.0,
            motion::Fade::Cover(theme::PANEL),
        ),
        smooth(scroll),
    ))
    .width(Length::Fill)
    .height(Length::Fill)
    .style(|_t| container::Style {
        background: Some(Background::Color(theme::PANEL)),
        border: Border {
            color: theme::LINE_SOFT,
            width: 1.0,
            radius: 0.0.into(),
        },
        ..Default::default()
    })
    .into()
}
