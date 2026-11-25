use std::time::{Duration, Instant};

use iced::widget::{button, column, container, operation, row, scrollable, space, text};
use iced::{Alignment, Background, Border, Color, Element, Length, Padding, Task, Theme};

use crate::smooth_scroll::smooth;
use crate::{App, Message, Tab, format_mm_ss, motion, theme};

impl App {
    /// Keeps the current lyric line in view in the full-screen panel.
    pub(crate) fn fs_lyric_scroll(&mut self) -> Task<Message> {
        if !(self.fs_lyrics_tab && self.active_tab == Tab::NowPlaying) {
            return Task::none();
        }
        let Some(doc) = &self.lyrics else {
            return Task::none();
        };
        if !doc.is_synced || doc.lines.len() < 2 {
            return Task::none();
        }
        let idx = doc.index_at(self.position_s);
        if idx == self.fs_last_lyric {
            return Task::none();
        }
        self.fs_last_lyric = idx;
        if let Some((offset, viewport_h, content_h)) = self.lyrics_vp
            && content_h > viewport_h
        {
            let n = doc.lines.len() as f32;
            let row_h = (content_h - 32.0).max(0.0) / n;
            let center = idx.map_or(0.0, |i| 16.0 + (i as f32 + 0.5) * row_h);
            let target = (center - viewport_h * 0.35).clamp(0.0, content_h - viewport_h);
            if self.animations_enabled() {
                self.lyric_anim = Some((offset, target, Instant::now()));
                return Task::none();
            }
            return operation::scroll_to(
                self.lyrics_scroll_id.clone(),
                iced::widget::scrollable::AbsoluteOffset { x: 0.0, y: target },
            );
        }
        let y = idx.map_or(0.0, |i| {
            (i as f32 / (doc.lines.len() - 1) as f32).clamp(0.0, 1.0)
        });
        operation::snap_to(
            self.lyrics_scroll_id.clone(),
            iced::widget::scrollable::RelativeOffset {
                x: Some(0.0),
                y: Some(y),
            },
        )
    }
}

pub(super) fn lyrics_body<'a>(app: &'a App) -> Element<'a, Message> {
    let Some(doc) = &app.lyrics else {
        return container(
            text("No lyrics for this track")
                .size(12)
                .color(theme::TEXT_MUTE),
        )
        .width(Length::Fill)
        .height(Length::Fill)
        .align_x(Alignment::Center)
        .align_y(Alignment::Center)
        .into();
    };
    let active = doc.index_at(app.seeking_pos.unwrap_or(app.position_s));
    let lines: Vec<Element<'a, Message>> = doc
        .lines
        .iter()
        .enumerate()
        .map(|(i, line)| {
            if line.text.trim().is_empty() {
                return space::vertical().height(14).into();
            }
            let is_active = active == Some(i);
            let past = active.is_some_and(|a| i < a);
            let synced = doc.is_synced && line.time >= 0.0;
            let col = if !synced {
                theme::TEXT_SOFT
            } else if is_active {
                Color::WHITE
            } else if past {
                theme::TEXT_MUTE
            } else {
                theme::TEXT_MUTED_LIGHT
            };
            let mut label = text(line.text.as_str())
                .size(17)
                .line_height(iced::widget::text::LineHeight::Absolute(24.0.into()))
                .color(col)
                .width(Length::Fill);
            if is_active && synced {
                label = label.font(crate::views::bold());
            }
            if !synced {
                return container(label)
                    .padding(Padding {
                        top: 6.0,
                        right: 16.0,
                        bottom: 6.0,
                        left: 16.0,
                    })
                    .into();
            }
            let stamp = motion::hover_text_color(
                Color::TRANSPARENT,
                theme::TEXT_MUTE,
                0,
                text(format_mm_ss(line.time)).size(10),
            );
            motion::hover_scope(
                Duration::ZERO,
                button(row![label, stamp].spacing(12).align_y(Alignment::Center))
                    .width(Length::Fill)
                    .padding(Padding {
                        top: 6.0,
                        right: 16.0,
                        bottom: 6.0,
                        left: 14.0,
                    })
                    .style(lyric_style(is_active))
                    .on_press(Message::SeekChanged(line.time)),
            )
        })
        .collect();
    smooth(
        scrollable(column(lines).padding([16, 8]))
            .id(app.lyrics_scroll_id.clone())
            .on_scroll(|vp| {
                Message::LyricsScrolled(
                    vp.absolute_offset().y,
                    vp.bounds().height,
                    vp.content_bounds().height,
                )
            })
            .direction(scrollable::Direction::Vertical(
                scrollable::Scrollbar::new()
                    .width(4)
                    .scroller_width(4)
                    .margin(2),
            ))
            .style(thin_scroll_style)
            .height(Length::Fill),
    )
}

fn lyric_style(current: bool) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |_t, status| {
        let hovered = matches!(status, button::Status::Hovered | button::Status::Pressed);
        button::Style {
            background: hovered.then(|| Background::Color(Color::from_rgba(1.0, 1.0, 1.0, 0.05))),
            border: Border {
                radius: 4.0.into(),
                ..Default::default()
            },
            shadow: if current {
                iced::Shadow {
                    color: theme::ORANGE,
                    offset: iced::Vector::new(-2.0, 0.0),
                    blur_radius: 0.0,
                }
            } else {
                iced::Shadow::default()
            },
            ..Default::default()
        }
    }
}

fn thin_scroll_style(_t: &Theme, status: scrollable::Status) -> scrollable::Style {
    let hovered = matches!(
        status,
        scrollable::Status::Hovered { .. } | scrollable::Status::Dragged { .. }
    );
    let rail = scrollable::Rail {
        background: None,
        border: Border::default(),
        scroller: scrollable::Scroller {
            background: Background::Color(Color::from_rgba(
                1.0,
                1.0,
                1.0,
                if hovered { 0.25 } else { 0.12 },
            )),
            border: Border {
                radius: 2.0.into(),
                ..Default::default()
            },
        },
    };
    scrollable::Style {
        container: container::Style::default(),
        vertical_rail: rail,
        horizontal_rail: rail,
        gap: None,
        auto_scroll: scrollable::AutoScroll {
            background: Background::Color(Color::TRANSPARENT),
            border: Border::default(),
            shadow: iced::Shadow::default(),
            icon: Color::TRANSPARENT,
        },
    }
}
