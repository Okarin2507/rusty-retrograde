use iced::widget::canvas::Canvas;
use iced::widget::text::Wrapping;
use iced::widget::{button, column, container, row, slider, space, stack, text};
use iced::{Alignment, Background, Border, Color, Element, Length};

use super::widgets::{SeekStripSpec, VolumeMeterCanvas, seek_strip};
use super::{bold, link_style};
use crate::covers_ui::render_cover;
use crate::{App, Message, Tab, format_mm_ss, icons, motion, pages, theme};

fn icon_btn<'a>(
    name: &str,
    size: f32,
    on: bool,
    accent: Color,
    msg: Message,
) -> Element<'a, Message> {
    button(icons::icon(
        name,
        size,
        if on { accent } else { theme::TEXT_DIM },
    ))
    .padding(6)
    .style(theme::bare_button_style())
    .on_press(msg)
    .into()
}

pub(super) fn view_transport(app: &App, accent: Color) -> Element<'_, Message> {
    let (title, artist, cover_path, liked_id) = match &app.current_track {
        Some(t) => (
            t.title.clone(),
            t.artist.clone(),
            app.covers
                .get_first(&[t.cover_thumb_path.as_deref(), t.cover_full_path.as_deref()]),
            Some(t.id.clone()),
        ),
        None => (
            "NO TRACK LOADED".to_owned(),
            "Select an album to begin".to_owned(),
            None,
            None,
        ),
    };

    let max_dur = app.duration_s.max(1.0);
    let pos = app
        .seeking_pos
        .unwrap_or(app.position_s)
        .clamp(0.0, max_dur);

    let line = seek_strip(
        pos,
        max_dur,
        SeekStripSpec {
            height: 12.0,
            track_h: 3.0,
            fill: accent,
            track: theme::LINE_SOFT,
            knob: Some(Color::WHITE),
        },
    );

    let cover_btn = button(render_cover(cover_path, 56.0, 0.0, accent, &title))
        .padding(0)
        .style(theme::bare_button_style())
        .on_press(Message::SelectTab(if app.active_tab == Tab::NowPlaying {
            app.previous_tab
        } else {
            Tab::NowPlaying
        }));
    let left = row![
        cover_btn,
        column![
            text(pages::fit(&title.to_uppercase(), 300.0, 14.0).into_owned())
                .size(14)
                .font(bold())
                .color(theme::TEXT)
                .wrapping(Wrapping::None),
            button(
                text(pages::fit(&artist, 300.0, 12.0).into_owned())
                    .size(12)
                    .wrapping(Wrapping::None),
            )
            .padding(0)
            .style(link_style(theme::TEXT_DIM))
            .on_press_maybe(
                app.current_track
                    .as_ref()
                    .map(|_| Message::OpenCurrentArtist)
            ),
        ]
        .spacing(4),
    ]
    .spacing(16)
    .align_y(Alignment::Center)
    .width(Length::FillPortion(1));

    let center = column![
        row![
            icon_btn(
                "shuffle",
                18.0,
                app.shuffle,
                theme::TEAL,
                Message::ToggleShuffle
            ),
            icon_btn("skipback", 22.0, false, accent, Message::Previous),
            motion::hover_scope_slop(
                motion::HEX_HOVER,
                5.0,
                button(icons::hex_play(app.is_playing, 62.0))
                    .padding(0)
                    .style(theme::bare_button_style())
                    .on_press(Message::TogglePlayPause),
            ),
            icon_btn("skipforward", 22.0, false, accent, Message::Next),
            icon_btn(
                app.repeat_mode.icon_name(),
                18.0,
                app.repeat_mode.is_active(),
                theme::TEAL,
                Message::CycleRepeat,
            ),
        ]
        .spacing(12)
        .align_y(Alignment::Center),
        row![
            text(format_mm_ss(pos)).size(9).color(theme::TEXT_MUTE),
            space::horizontal(),
            text(format_mm_ss(app.duration_s))
                .size(9)
                .color(theme::TEXT_MUTE),
        ]
        .width(Length::Fixed(220.0)),
    ]
    .spacing(2)
    .align_x(Alignment::Center);

    let meter = stack![
        Canvas::new(VolumeMeterCanvas {
            volume: app.volume,
            muted: app.muted,
        })
        .width(Length::Fill)
        .height(Length::Fixed(24.0)),
        slider(0.0..=1.0, app.volume, Message::VolumeDragging)
            .on_release(Message::VolumeCommit)
            .step(1.0 / 15.0)
            .height(24)
            .style(theme::invisible_slider_style),
    ]
    .width(Length::Fixed(98.0))
    .height(24);
    let vol_icon = if app.muted || app.volume <= 0.001 {
        "volumex"
    } else if app.volume < 0.5 {
        "volume1"
    } else {
        "volume2"
    };

    let liked = liked_id
        .as_ref()
        .is_some_and(|id| app.liked_set.contains(id));
    let heart: Element<'_, Message> = match liked_id {
        Some(id) => icon_btn("heart", 18.0, liked, theme::ORANGE, Message::ToggleLike(id)),
        None => button(icons::icon("heart", 18.0, theme::TEXT_MUTE))
            .padding(6)
            .style(theme::bare_button_style())
            .into(),
    };
    let toggle = |tab: Tab| {
        Message::SelectTab(if app.active_tab == tab {
            app.previous_tab
        } else {
            tab
        })
    };
    let right = row![
        space::horizontal(),
        button(icons::icon(vol_icon, 18.0, theme::TEXT_DIM))
            .padding(6)
            .style(theme::bare_button_style())
            .on_press(Message::ToggleMute),
        meter,
        container(space::horizontal())
            .width(1)
            .height(24)
            .style(theme::fill_style(theme::LINE)),
        heart,
        icon_btn(
            "listmusic",
            18.0,
            app.queue_open,
            accent,
            Message::ToggleQueuePanel
        ),
        icon_btn(
            "sliders",
            18.0,
            app.audio_open,
            accent,
            Message::ToggleAudioPanel
        ),
        icon_btn("miniplayer", 18.0, false, accent, Message::ToggleMiniPlayer),
        icon_btn(
            "maximize2",
            18.0,
            app.active_tab == Tab::NowPlaying,
            accent,
            toggle(Tab::NowPlaying),
        ),
    ]
    .spacing(6)
    .align_y(Alignment::Center)
    .width(Length::FillPortion(1));

    container(column![
        line,
        row![left, center, right]
            .spacing(16)
            .padding([6, 20])
            .align_y(Alignment::Center),
    ])
    .width(Length::Fill)
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
