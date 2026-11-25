use std::time::Instant;

use iced::advanced::layout::{self, Layout};
use iced::advanced::renderer::{self, Quad};
use iced::advanced::widget::{Tree, tree};
use iced::advanced::{Clipboard, Renderer as _, Shell, Widget};
use iced::window::RedrawRequest;
use iced::{Background, Color, Element, Event, Length, Rectangle, Size, mouse, window};

use super::{FRAME, enabled};

/// A circular indicator dot that pulses at 60 FPS inside widget state when `active` and animations are on.
pub fn pulse_dot<'a, Message: 'a>(
    color: Color,
    diameter: f32,
    active: bool,
) -> Element<'a, Message> {
    Element::new(PulseDot {
        color,
        diameter,
        active,
    })
}

struct PulseDot {
    color: Color,
    diameter: f32,
    active: bool,
}

#[derive(Default)]
struct PulseState {
    phase: f64,
    last_frame: Option<Instant>,
}

impl<Message> Widget<Message, iced::Theme, iced::Renderer> for PulseDot {
    fn size(&self) -> Size<Length> {
        Size::new(Length::Fixed(self.diameter), Length::Fixed(self.diameter))
    }

    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<PulseState>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(PulseState::default())
    }

    fn layout(
        &mut self,
        _tree: &mut Tree,
        _renderer: &iced::Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        layout::atomic(
            limits,
            Length::Fixed(self.diameter),
            Length::Fixed(self.diameter),
        )
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        _layout: Layout<'_>,
        _cursor: mouse::Cursor,
        _renderer: &iced::Renderer,
        _clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        _viewport: &Rectangle,
    ) {
        if let Event::Window(window::Event::RedrawRequested(now)) = event {
            let state = tree.state.downcast_mut::<PulseState>();
            if self.active && enabled() {
                if let Some(prev) = state.last_frame {
                    state.phase += now.saturating_duration_since(prev).as_secs_f64().min(0.1) * 0.7;
                }
                state.last_frame = Some(*now);
                shell.request_redraw_at(RedrawRequest::At(*now + FRAME));
            } else {
                state.last_frame = None;
            }
        }
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut iced::Renderer,
        _theme: &iced::Theme,
        _style: &renderer::Style,
        layout: Layout<'_>,
        _cursor: mouse::Cursor,
        _viewport: &Rectangle,
    ) {
        let state = tree.state.downcast_ref::<PulseState>();
        let alpha = if self.active && enabled() {
            0.4 + 0.6 * ((state.phase * 2.5).sin() as f32 * 0.5 + 0.5)
        } else {
            1.0
        };
        renderer.fill_quad(
            Quad {
                bounds: layout.bounds(),
                border: iced::Border {
                    radius: (self.diameter / 2.0).into(),
                    ..Default::default()
                },
                ..Quad::default()
            },
            Background::Color(Color {
                a: self.color.a * alpha,
                ..self.color
            }),
        );
    }
}
