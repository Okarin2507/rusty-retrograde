use std::cell::Cell;
use std::time::{Duration, Instant};

use iced::advanced::layout::{self, Layout};
use iced::advanced::renderer::{self, Quad, Renderer as _};
use iced::advanced::svg::{Renderer as _, Svg};
use iced::advanced::widget::{Tree, tree};
use iced::advanced::{Clipboard, Shell, Widget};
use iced::widget::svg;
use iced::{
    Background, Color, ContentFit, Element, Event, Length, Radians, Rectangle, Size, Vector, mouse,
    window,
};

use super::{
    Ease, Tween, lerp, lerp_color,
    scope::{hovered_in_scope, scope},
};

enum Drawing<Message> {
    ZoomCover {
        handle: iced::widget::image::Handle,
    },
    Icon {
        handle: svg::Handle,
        from: Color,
        to: Color,
        color_ms: u64,
        shift: Vector,
    },
    Bar {
        color: Color,
    },
    CardPlay {
        normal: svg::Handle,
        hover: svg::Handle,
        on_press: Message,
        clip_below: f32,
    },
    Hex {
        normal: svg::Handle,
        hover: svg::Handle,
    },
    UnderImage {
        handle: iced::widget::image::Handle,
    },
    Corner {
        rest: Color,
        hover: Color,
    },
}

struct Drawn<Message> {
    key: u64,
    width: Length,
    height: Length,
    dur: Duration,
    ease: Ease,
    drawing: Drawing<Message>,
}

#[derive(Default)]
struct DrawnState {
    tween: Cell<Option<Tween>>,
    tween2: Cell<Option<Tween>>,
    /// Tracks press state and last rendered visibility for `CardPlay`.
    card_play_pressed: bool,
    card_play_visible: Cell<bool>,
}

impl<Message: Clone> Widget<Message, iced::Theme, iced::Renderer> for Drawn<Message> {
    fn size(&self) -> Size<Length> {
        Size::new(self.width, self.height)
    }

    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<DrawnState>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(DrawnState::default())
    }

    fn layout(
        &mut self,
        _tree: &mut Tree,
        _renderer: &iced::Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        layout::atomic(limits, self.width, self.height)
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        _renderer: &iced::Renderer,
        _clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        _viewport: &Rectangle,
    ) {
        let Drawing::CardPlay { on_press, .. } = &self.drawing else {
            return;
        };
        let state = tree.state.downcast_mut::<DrawnState>();
        match event {
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left))
                if state.card_play_visible.get() && cursor.is_over(layout.bounds()) =>
            {
                state.card_play_pressed = true;
                shell.capture_event();
            }
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left))
                if state.card_play_pressed =>
            {
                state.card_play_pressed = false;
                if cursor.is_over(layout.bounds()) {
                    shell.publish(on_press.clone());
                }
                shell.capture_event();
            }
            Event::Window(window::Event::Unfocused) => {
                state.card_play_pressed = false;
            }
            _ => {}
        }
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        _viewport: &Rectangle,
        _renderer: &iced::Renderer,
    ) -> mouse::Interaction {
        let state = tree.state.downcast_ref::<DrawnState>();
        if matches!(self.drawing, Drawing::CardPlay { .. })
            && (state.card_play_pressed || state.card_play_visible.get())
            && cursor.is_over(layout.bounds())
        {
            mouse::Interaction::Pointer
        } else {
            mouse::Interaction::None
        }
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut iced::Renderer,
        _theme: &iced::Theme,
        _style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        let state = tree.state.downcast_ref::<DrawnState>();
        let ctx = scope();
        let now = ctx.map_or_else(Instant::now, |c| c.now);
        let target = if ctx.is_some_and(|c| c.hovered) {
            1.0
        } else {
            0.0
        };
        let v = Tween::step(&state.tween, self.key, target, now, self.dur, self.ease);
        let bounds = layout.bounds();
        match &self.drawing {
            Drawing::ZoomCover { handle } => {
                iced::widget::image::draw(
                    renderer,
                    layout,
                    handle,
                    None,
                    iced::border::Radius::default(),
                    ContentFit::Cover,
                    iced::widget::image::FilterMethod::Linear,
                    iced::Rotation::default(),
                    1.0,
                    lerp(1.0, 1.05, v),
                );
            }
            Drawing::Icon {
                handle,
                from,
                to,
                color_ms,
                shift,
            } => {
                let c = Tween::step(
                    &state.tween2,
                    self.key,
                    target,
                    now,
                    Duration::from_millis(*color_ms),
                    Ease::Linear,
                );
                let at = Rectangle {
                    x: bounds.x + shift.x * v,
                    y: bounds.y + shift.y * v,
                    ..bounds
                };
                renderer.draw_svg(
                    Svg {
                        handle: handle.clone(),
                        color: Some(lerp_color(*from, *to, c)),
                        rotation: Radians(0.0),
                        opacity: 1.0,
                    },
                    at,
                    at,
                );
            }
            Drawing::Bar { color } => {
                if v > 0.001 {
                    renderer.fill_quad(
                        Quad {
                            bounds: Rectangle {
                                width: bounds.width * v,
                                ..bounds
                            },
                            ..Quad::default()
                        },
                        Background::Color(*color),
                    );
                }
            }
            Drawing::CardPlay {
                normal,
                hover,
                clip_below,
                ..
            } => {
                state.card_play_visible.set(v >= 0.1);
                if v > 0.001 {
                    let at = Rectangle {
                        y: bounds.y + 32.0 * (1.0 - v),
                        ..bounds
                    };
                    let Some(clip) = Rectangle {
                        height: bounds.height + clip_below,
                        ..bounds
                    }
                    .intersection(viewport) else {
                        return;
                    };
                    let own_hover = cursor.is_over(bounds);
                    renderer.with_layer(clip, |r| {
                        r.draw_svg(
                            Svg {
                                handle: if own_hover {
                                    hover.clone()
                                } else {
                                    normal.clone()
                                },
                                color: None,
                                rotation: Radians(0.0),
                                opacity: v,
                            },
                            at,
                            clip,
                        );
                    });
                }
            }
            Drawing::Hex { normal, hover } => {
                let pressed = if ctx.is_some_and(|c| c.pressed) {
                    1.0
                } else {
                    0.0
                };
                let p = Tween::step(
                    &state.tween2,
                    self.key,
                    pressed,
                    now,
                    HEX_PRESS,
                    Ease::CubicOut,
                );
                let s = lerp(1.0, 1.06, v) * lerp(1.0, 0.96, p);
                let at = Rectangle {
                    x: bounds.center_x() - bounds.width * s / 2.0,
                    y: bounds.center_y() - bounds.height * s / 2.0,
                    width: bounds.width * s,
                    height: bounds.height * s,
                };
                let svg = |handle: &svg::Handle, opacity: f32| Svg {
                    handle: handle.clone(),
                    color: None,
                    rotation: Radians(0.0),
                    opacity,
                };
                renderer.draw_svg(svg(normal, 1.0), at, at);
                if v > 0.001 {
                    renderer.draw_svg(svg(hover, v), at, at);
                }
            }
            Drawing::UnderImage { handle } => {
                let cover = 1.0 - v;
                if cover > 0.001
                    && let Some(clip) = bounds.intersection(viewport)
                {
                    renderer.with_layer(clip, |r| {
                        iced::widget::image::draw(
                            r,
                            layout,
                            handle,
                            None,
                            iced::border::Radius::default(),
                            ContentFit::Cover,
                            iced::widget::image::FilterMethod::Linear,
                            iced::Rotation::default(),
                            cover,
                            1.0,
                        );
                    });
                }
            }
            Drawing::Corner { rest, hover } => {
                let color = if hovered_in_scope() { *hover } else { *rest };
                for b in [
                    Rectangle {
                        width: 8.0,
                        height: 1.0,
                        ..bounds
                    },
                    Rectangle {
                        width: 1.0,
                        height: 8.0,
                        ..bounds
                    },
                ] {
                    renderer.fill_quad(
                        Quad {
                            bounds: b,
                            ..Quad::default()
                        },
                        Background::Color(color),
                    );
                }
            }
        }
    }
}

fn drawn<'a, Message: Clone + 'a>(
    key: u64,
    width: Length,
    height: Length,
    dur_ms: u64,
    ease: Ease,
    drawing: Drawing<Message>,
) -> Element<'a, Message> {
    Element::new(Drawn {
        key,
        width,
        height,
        dur: Duration::from_millis(dur_ms),
        ease,
        drawing,
    })
}

/// A card cover that zooms to 1.05 while its card is hovered (500 ms cubic-out).
pub fn zoom_cover<'a, Message: Clone + 'a>(
    key: u64,
    handle: iced::widget::image::Handle,
    side: f32,
) -> Element<'a, Message> {
    drawn(
        key,
        Length::Fixed(side),
        Length::Fixed(side),
        500,
        Ease::CubicOut,
        Drawing::ZoomCover { handle },
    )
}

/// An icon that changes tint over `color_ms` and moves by `shift` (200 ms cubic-out) on hover.
pub fn hover_icon<'a, Message: Clone + 'a>(
    handle: svg::Handle,
    size: f32,
    from: Color,
    to: Color,
    color_ms: u64,
    shift: Vector,
) -> Element<'a, Message> {
    drawn(
        0,
        Length::Fixed(size),
        Length::Fixed(size),
        200,
        Ease::CubicOut,
        Drawing::Icon {
            handle,
            from,
            to,
            color_ms,
            shift,
        },
    )
}

/// A `height` px bar that grows from the left across its width on hover (500 ms cubic-in-out).
pub fn hover_bar<'a, Message: Clone + 'a>(color: Color, height: f32) -> Element<'a, Message> {
    drawn(
        0,
        Length::Fill,
        Length::Fixed(height),
        500,
        Ease::CubicInOut,
        Drawing::Bar { color },
    )
}

/// The 40 px play button of an album card (300 ms cubic-out rise and fade), clipped `clip_below` px below.
pub fn card_play<'a, Message: Clone + 'a>(
    key: u64,
    on_press: Message,
    clip_below: f32,
) -> Element<'a, Message> {
    drawn(
        key,
        Length::Fixed(40.0),
        Length::Fixed(40.0),
        300,
        Ease::CubicOut,
        Drawing::CardPlay {
            normal: crate::icons::card_play_handle(false),
            hover: crate::icons::card_play_handle(true),
            on_press,
            clip_below,
        },
    )
}

/// How long the hexagonal button takes to grow and whiten on hover.
pub const HEX_HOVER: Duration = Duration::from_millis(180);
const HEX_PRESS: Duration = Duration::from_millis(90);

/// The hexagonal play / pause button.
pub fn hex<'a, Message: Clone + 'a>(playing: bool, width: f32) -> Element<'a, Message> {
    drawn(
        0,
        Length::Fixed(width),
        Length::Fixed(width * 56.0 / 64.0),
        HEX_HOVER.as_millis() as u64,
        Ease::CubicOut,
        Drawing::Hex {
            normal: crate::icons::hex_play_handle(playing, false),
            hover: crate::icons::hex_play_handle(playing, true),
        },
    )
}

/// Covers an overlay that should fade in on hover over 200 ms with its underlying image.
pub fn under_image<'a, Message: Clone + 'a>(
    handle: iced::widget::image::Handle,
    side: f32,
) -> Element<'a, Message> {
    drawn(
        0,
        Length::Fixed(side),
        Length::Fixed(side),
        200,
        Ease::Linear,
        Drawing::UnderImage { handle },
    )
}

/// The 8 px corner tick of a card.
pub fn corner_tick<'a, Message: Clone + 'a>(rest: Color, hover: Color) -> Element<'a, Message> {
    drawn(
        0,
        Length::Fixed(8.0),
        Length::Fixed(8.0),
        0,
        Ease::Linear,
        Drawing::Corner { rest, hover },
    )
}
