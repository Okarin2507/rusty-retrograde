use std::cell::Cell;

use iced::widget::canvas::{self, Canvas};
use iced::widget::text::Wrapping;
use iced::widget::{column, container, row, space, text, tooltip};
use iced::{
    Alignment, Background, Border, Color, Element, Length, Padding, Point, Rectangle, Renderer,
    Size, Theme, mouse, window,
};

use super::bold;
use crate::{Message, theme};

#[derive(Debug, Clone, Copy)]
pub(crate) struct SeekStripSpec {
    pub height: f32,
    pub track_h: f32,
    pub fill: Color,
    pub track: Color,
    pub knob: Option<Color>,
}

#[derive(Default)]
struct SeekState {
    dragging: bool,
    hovered: bool,
}

struct SeekStrip {
    pos: f64,
    max_dur: f64,
    spec: SeekStripSpec,
}

impl SeekStrip {
    fn seek_pos(&self, bounds: Rectangle, cursor_pos: Point) -> f64 {
        let w = bounds.width.max(1.0);
        let frac = ((cursor_pos.x - bounds.x) / w).clamp(0.0, 1.0) as f64;
        frac * self.max_dur.max(0.0)
    }
}

impl canvas::Program<Message> for SeekStrip {
    type State = SeekState;

    fn update(
        &self,
        state: &mut SeekState,
        event: &iced::Event,
        bounds: Rectangle,
        cursor: mouse::Cursor,
    ) -> Option<canvas::Action<Message>> {
        match event {
            iced::Event::Mouse(mouse::Event::CursorMoved { .. }) => {
                let over = cursor.is_over(bounds);
                let prev = state.hovered;
                state.hovered = over;
                if state.dragging
                    && let Some(p) = cursor.position()
                {
                    return Some(
                        canvas::Action::publish(Message::SeekDragging(self.seek_pos(bounds, p)))
                            .and_capture(),
                    );
                }
                if over != prev {
                    return Some(canvas::Action::request_redraw());
                }
                None
            }
            iced::Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) => {
                if !cursor.is_over(bounds) {
                    return None;
                }
                let p = cursor.position()?;
                state.dragging = true;
                Some(
                    canvas::Action::publish(Message::SeekDragging(self.seek_pos(bounds, p)))
                        .and_capture(),
                )
            }
            iced::Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) => {
                if state.dragging {
                    state.dragging = false;
                    return Some(canvas::Action::publish(Message::SeekCommit).and_capture());
                }
                None
            }
            iced::Event::Window(window::Event::Unfocused) => {
                if state.dragging || state.hovered {
                    state.dragging = false;
                    state.hovered = false;
                    return Some(canvas::Action::request_redraw());
                }
                None
            }
            _ => None,
        }
    }

    fn mouse_interaction(
        &self,
        state: &SeekState,
        bounds: Rectangle,
        cursor: mouse::Cursor,
    ) -> mouse::Interaction {
        if state.dragging || cursor.is_over(bounds) {
            mouse::Interaction::Pointer
        } else {
            mouse::Interaction::None
        }
    }

    fn draw(
        &self,
        state: &SeekState,
        renderer: &Renderer,
        _t: &Theme,
        bounds: Rectangle,
        _c: mouse::Cursor,
    ) -> Vec<canvas::Geometry> {
        let mut frame = canvas::Frame::new(renderer, bounds.size());
        let frac = if self.max_dur > 0.0 {
            (self.pos / self.max_dur).clamp(0.0, 1.0) as f32
        } else {
            0.0
        };
        let th = self.spec.track_h;
        let ty = (bounds.height - th) * 0.5;
        frame.fill_rectangle(
            Point::new(0.0, ty),
            Size::new(bounds.width, th),
            self.spec.track,
        );
        let fw = (bounds.width * frac).max(0.0);
        if fw > 0.5 {
            frame.fill_rectangle(Point::new(0.0, ty), Size::new(fw, th), self.spec.fill);
        }
        if (state.hovered || state.dragging)
            && let Some(kc) = self.spec.knob
        {
            let kw = 6.0;
            let kh = (th + 6.0).min(bounds.height);
            let kx = (fw - kw * 0.5).clamp(0.0, (bounds.width - kw).max(0.0));
            let ky = (bounds.height - kh) * 0.5;
            frame.fill_rectangle(Point::new(kx, ky), Size::new(kw, kh), kc);
        }
        vec![frame.into_geometry()]
    }
}

pub(crate) fn seek_strip<'a>(pos: f64, max_dur: f64, spec: SeekStripSpec) -> Element<'a, Message> {
    Canvas::new(SeekStrip { pos, max_dur, spec })
        .width(Length::Fill)
        .height(Length::Fixed(spec.height))
        .into()
}

pub(super) struct VolumeMeterCanvas {
    pub volume: f64,
    pub muted: bool,
}

pub(super) struct VolumeMeterState {
    cache: canvas::Cache,
    last: Cell<(usize, bool)>,
}

impl Default for VolumeMeterState {
    fn default() -> Self {
        Self {
            cache: canvas::Cache::new(),
            last: Cell::new((usize::MAX, false)),
        }
    }
}

impl<M> canvas::Program<M> for VolumeMeterCanvas {
    type State = VolumeMeterState;

    fn draw(
        &self,
        state: &VolumeMeterState,
        renderer: &Renderer,
        _theme: &Theme,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<canvas::Geometry> {
        let lit = (self.volume * 15.0).round() as usize;
        let key = (lit, self.muted);
        if state.last.get() != key {
            state.cache.clear();
            state.last.set(key);
        }
        let geom = state.cache.draw(renderer, bounds.size(), |frame| {
            let count = 15usize;
            let gap = 2.0_f32;
            let bar_w = ((bounds.width - gap * (count as f32 - 1.0)) / count as f32).max(1.0);
            let bar_h = 18.0_f32.min(bounds.height);
            let y = (bounds.height - bar_h) * 0.5;
            for i in 0..count {
                let on = i < lit && !self.muted;
                let col = if !on {
                    theme::LINE
                } else if i < 5 {
                    theme::TEAL
                } else if i < 10 {
                    theme::GOLD
                } else {
                    theme::ORANGE
                };
                frame.fill_rectangle(
                    Point::new(i as f32 * (bar_w + gap), y),
                    Size::new(bar_w, bar_h),
                    col,
                );
            }
        });
        vec![geom]
    }
}

pub(super) fn section_head<'a>(label: &str, color: Color) -> Element<'a, Message> {
    row![
        container(space::horizontal())
            .width(8)
            .height(1)
            .style(theme::fill_style(color)),
        text(theme::tracked(label)).size(10).color(color),
    ]
    .spacing(8)
    .align_y(Alignment::Center)
    .into()
}

pub(super) fn title_block<'a>(label: &str) -> Element<'a, Message> {
    row![
        container(space::horizontal())
            .width(4)
            .height(24)
            .style(theme::fill_style(theme::ORANGE)),
        text(theme::tracked(label))
            .size(20)
            .font(bold())
            .color(Color::WHITE),
    ]
    .spacing(12)
    .align_y(Alignment::Center)
    .into()
}

pub(super) fn orbit_tooltip<'a>(
    content: impl Into<Element<'a, Message>>,
    tip: String,
    accent: Color,
    pos: tooltip::Position,
) -> Element<'a, Message> {
    tooltip(
        content,
        container(
            text(tip)
                .size(11)
                .color(theme::TEXT)
                .wrapping(Wrapping::Word),
        )
        .max_width(360.0)
        .padding([8, 12])
        .style(move |_t| container::Style {
            background: Some(Background::Color(theme::RAISED)),
            border: Border {
                color: theme::with_alpha(accent, 0.55),
                width: 1.0,
                radius: 0.0.into(),
            },
            shadow: iced::Shadow {
                color: Color::from_rgba(0.0, 0.0, 0.0, 0.72),
                offset: iced::Vector::new(0.0, 6.0),
                blur_radius: 16.0,
            },
            ..Default::default()
        }),
        pos,
    )
    .gap(8)
    .delay(std::time::Duration::from_millis(160))
    .snap_within_viewport(true)
    .into()
}

pub(super) fn orbit_section<'a>(
    title: &str,
    dot_color: Color,
    body: Element<'a, Message>,
) -> Element<'a, Message> {
    let head = container(
        row![
            container(space::horizontal())
                .width(8)
                .height(8)
                .style(theme::fill_style(dot_color)),
            text(theme::tracked(title))
                .size(11)
                .font(bold())
                .color(Color::WHITE),
        ]
        .spacing(10)
        .align_y(Alignment::Center),
    )
    .padding([10, 16])
    .width(Length::Fill)
    .style(|_t| container::Style {
        background: Some(Background::Color(theme::LINE_SOFT)),
        border: Border {
            color: theme::LINE,
            width: 1.0,
            radius: 0.0.into(),
        },
        ..Default::default()
    });
    container(column![
        head,
        container(body).padding(16).width(Length::Fill)
    ])
    .width(Length::Fill)
    .style(|_t| container::Style {
        background: Some(Background::Color(theme::PANEL)),
        border: Border {
            color: theme::LINE,
            width: 1.0,
            radius: 0.0.into(),
        },
        text_color: Some(theme::TEXT),
        ..Default::default()
    })
    .into()
}

pub(super) fn tag_chip<'a>(label: &'a str, color: Color) -> Element<'a, Message> {
    container(text(label).size(8).font(bold()).color(color))
        .padding(Padding {
            top: 3.0,
            right: 5.0,
            bottom: 1.0,
            left: 5.0,
        })
        .style(move |_t| container::Style {
            background: Some(Background::Color(theme::with_alpha(color, 0.10))),
            border: Border {
                color,
                width: 1.0,
                radius: 0.0.into(),
            },
            ..Default::default()
        })
        .into()
}
