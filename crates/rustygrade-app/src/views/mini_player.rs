use iced::widget::canvas::{self, Canvas};
use iced::widget::text::Wrapping;
use iced::widget::{button, column, container, mouse_area, row, space, text};
use iced::{
    Alignment, Background, Border, Color, Element, Length, Point, Rectangle, Renderer, Theme, mouse,
};

use super::bold;
use super::widgets::{SeekStripSpec, seek_strip};
use crate::covers_ui::render_cover;
use crate::{App, Message, format_mm_ss, icons, motion, pages, theme};

struct MiniDiscCanvas {
    phase: f32,
    playing: bool,
    accent: Color,
}

impl<M> canvas::Program<M> for MiniDiscCanvas {
    type State = ();

    fn draw(
        &self,
        _s: &(),
        renderer: &Renderer,
        _t: &Theme,
        bounds: Rectangle,
        _c: mouse::Cursor,
    ) -> Vec<canvas::Geometry> {
        let mut frame = canvas::Frame::new(renderer, bounds.size());
        let c = Point::new(bounds.width * 0.5, bounds.height * 0.5);
        let r = bounds.width.min(bounds.height) * 0.45;
        frame.fill(
            &canvas::Path::circle(c, r),
            canvas::Fill::from(Color::from_rgb8(0x14, 0x14, 0x18)),
        );
        frame.stroke(
            &canvas::Path::circle(c, r),
            canvas::Stroke::default()
                .with_width(1.2)
                .with_color(theme::LINE),
        );
        frame.stroke(
            &canvas::Path::circle(c, r * 0.68),
            canvas::Stroke::default()
                .with_width(0.8)
                .with_color(Color::from_rgba(1.0, 1.0, 1.0, 0.1)),
        );
        frame.stroke(
            &canvas::Path::circle(c, r * 0.48),
            canvas::Stroke::default()
                .with_width(0.8)
                .with_color(Color::from_rgba(1.0, 1.0, 1.0, 0.08)),
        );
        let angle = if self.playing { self.phase * 2.4 } else { 0.35 };
        let marker_end = Point::new(
            c.x + angle.cos() * (r * 0.84),
            c.y + angle.sin() * (r * 0.84),
        );
        let marker_start = Point::new(
            c.x + angle.cos() * (r * 0.42),
            c.y + angle.sin() * (r * 0.42),
        );
        frame.stroke(
            &canvas::Path::line(marker_start, marker_end),
            canvas::Stroke::default()
                .with_width(1.6)
                .with_color(theme::with_alpha(self.accent, 0.7)),
        );
        frame.fill(
            &canvas::Path::circle(c, r * 0.24),
            canvas::Fill::from(theme::with_alpha(self.accent, 0.85)),
        );
        frame.fill(
            &canvas::Path::circle(c, 3.0),
            canvas::Fill::from(theme::VOID),
        );
        vec![frame.into_geometry()]
    }
}

fn mini_marquee(s: &str, max_w: f32, font_px: f32, hovered: bool, phase: f32) -> String {
    let total_w: f32 = s.chars().map(|c| pages::char_width_px(c, font_px)).sum();
    if total_w <= max_w || !hovered {
        return pages::fit(s, max_w, font_px).into_owned();
    }
    let chars: Vec<char> = s.chars().collect();
    let mut suffix_w = 0.0;
    let mut suffix_start = chars.len();
    while suffix_start > 0 {
        let w = pages::char_width_px(chars[suffix_start - 1], font_px);
        if suffix_w + w > max_w {
            break;
        }
        suffix_w += w;
        suffix_start -= 1;
    }
    let overflow = suffix_start.max(1);
    let pause_s = 0.8_f32;
    let pan_s = (overflow as f32) * 0.22;
    let cycle = pause_s + pan_s + pause_s;
    let t = phase % cycle;
    let offset = if t < pause_s {
        0
    } else if t < pause_s + pan_s {
        (((t - pause_s) / pan_s) * overflow as f32).round() as usize
    } else {
        overflow
    };
    let mut acc = 0.0;
    chars
        .into_iter()
        .skip(offset.min(overflow))
        .take_while(|&ch| {
            let w = pages::char_width_px(ch, font_px);
            if acc + w <= max_w || acc == 0.0 {
                acc += w;
                true
            } else {
                false
            }
        })
        .collect()
}

pub fn render_mini_player(app: &App) -> Element<'_, Message> {
    let accent = app.accent_anim.current_accent;
    let max_dur = app.duration_s.max(1.0);
    let pos = app
        .seeking_pos
        .unwrap_or(app.position_s)
        .clamp(0.0, max_dur);

    let (title, artist, cover_path, is_liked) = match &app.current_track {
        Some(t) => (
            t.title.clone(),
            t.artist.clone(),
            app.covers
                .get_first(&[t.cover_thumb_path.as_deref(), t.cover_full_path.as_deref()]),
            Some(t.id.clone()),
        ),
        None => (
            "No track loaded".to_owned(),
            "Rustygrade Mini".to_owned(),
            None,
            None,
        ),
    };

    let liked = is_liked
        .as_ref()
        .is_some_and(|tid| app.liked_set.contains(tid));
    let mini_icon_btn =
        |icon_name: &'static str, icon_sz: f32, col: Color, msg: Option<Message>| {
            let b = button(
                container(icons::icon(icon_name, icon_sz, col))
                    .width(Length::Fill)
                    .height(Length::Fill)
                    .align_x(Alignment::Center)
                    .align_y(Alignment::Center),
            )
            .width(26)
            .height(26)
            .padding(0)
            .style(|_t, st| button::Style {
                background: if matches!(st, button::Status::Hovered) {
                    Some(Background::Color(theme::HOVER))
                } else {
                    None
                },
                border: Border {
                    color: Color::TRANSPARENT,
                    width: 0.0,
                    radius: 3.0.into(),
                },
                ..Default::default()
            });
            match msg {
                Some(m) => b.on_press(m),
                None => b,
            }
        };

    let cover_inner: Element<'_, Message> = if cover_path.is_some() {
        render_cover(cover_path, 82.0, 0.0, accent, &title)
    } else {
        Canvas::new(MiniDiscCanvas {
            phase: app.anim_phase as f32,
            playing: app.is_playing && app.animations_enabled(),
            accent,
        })
        .width(82)
        .height(82)
        .into()
    };
    let cover_el = container(cover_inner)
        .width(88)
        .height(88)
        .padding(2)
        .align_x(Alignment::Center)
        .align_y(Alignment::Center)
        .style(|_t| container::Style {
            background: Some(Background::Color(theme::RAISED)),
            border: Border {
                color: theme::BORDER_STRONG,
                width: 1.0,
                radius: 0.0.into(),
            },
            ..Default::default()
        });

    let marquee_on = app.mini_hovered && app.animations_enabled();
    let top_row = row![
        column![
            text(mini_marquee(
                &title,
                194.0,
                13.0,
                marquee_on,
                app.mini_marquee_phase
            ))
            .size(13)
            .font(bold())
            .color(theme::TEXT)
            .wrapping(Wrapping::None),
            text(mini_marquee(
                &artist,
                194.0,
                10.0,
                marquee_on,
                app.mini_marquee_phase
            ))
            .size(10)
            .color(theme::TEXT_SUBTLE)
            .wrapping(Wrapping::None),
        ]
        .spacing(2)
        .width(Length::Fill),
        row![
            mini_icon_btn(
                "heart",
                14.0,
                if liked {
                    theme::ORANGE
                } else {
                    theme::TEXT_MUTE
                },
                is_liked.map(Message::ToggleLike),
            ),
            mini_icon_btn(
                "maximize2",
                13.0,
                theme::TEXT_MUTE,
                Some(Message::CloseMiniPlayerToMain)
            ),
        ]
        .spacing(2),
    ]
    .spacing(8)
    .align_y(Alignment::Start);

    let shuffle_col = if app.shuffle {
        theme::TEAL
    } else {
        theme::TEXT_MUTE
    };
    let repeat_col = if app.repeat_mode.is_active() {
        theme::TEAL
    } else {
        theme::TEXT_MUTE
    };
    let controls_row = row![
        space::horizontal(),
        button(icons::icon("shuffle", 15.0, shuffle_col))
            .style(theme::bare_button_style())
            .padding(4)
            .on_press(Message::ToggleShuffle),
        button(icons::icon("skipback", 17.0, theme::TEXT_DIM))
            .style(theme::bare_button_style())
            .padding(4)
            .on_press(Message::Previous),
        motion::hover_scope_slop(
            motion::HEX_HOVER,
            6.0,
            button(icons::hex_play(app.is_playing, 40.0))
                .style(theme::bare_button_style())
                .padding(0)
                .on_press(Message::TogglePlayPause),
        ),
        button(icons::icon("skipforward", 17.0, theme::TEXT_DIM))
            .style(theme::bare_button_style())
            .padding(4)
            .on_press(Message::Next),
        button(icons::icon(app.repeat_mode.icon_name(), 15.0, repeat_col))
            .style(theme::bare_button_style())
            .padding(4)
            .on_press(Message::CycleRepeat),
        space::horizontal(),
    ]
    .spacing(16)
    .align_y(Alignment::Center);

    let seek_row = row![
        container(text(format_mm_ss(pos)).size(9).color(theme::TEXT_SUBTLE)).width(34),
        seek_strip(
            pos,
            max_dur,
            SeekStripSpec {
                height: 12.0,
                track_h: 3.0,
                fill: theme::TEAL,
                track: theme::LINE,
                knob: Some(theme::TEXT),
            },
        ),
        container(
            text(format_mm_ss(app.duration_s))
                .size(9)
                .color(theme::TEXT_SUBTLE)
        )
        .width(34)
        .align_x(Alignment::End),
    ]
    .spacing(4)
    .align_y(Alignment::Center);

    let card = container(
        row![
            cover_el,
            column![top_row, controls_row, seek_row]
                .spacing(4)
                .width(Length::Fill)
                .height(Length::Fill),
        ]
        .spacing(16)
        .align_y(Alignment::Center),
    )
    .padding(16)
    .width(Length::Fill)
    .height(Length::Fill)
    .style(|_t| container::Style {
        background: Some(Background::Color(theme::PANEL_DEEP)),
        border: Border {
            color: theme::LINE,
            width: 1.0,
            radius: 0.0.into(),
        },
        text_color: Some(theme::TEXT),
        ..Default::default()
    });

    mouse_area(card)
        .on_press(Message::DragMiniWindow)
        .on_enter(Message::MiniHover(true))
        .on_exit(Message::MiniHover(false))
        .into()
}
