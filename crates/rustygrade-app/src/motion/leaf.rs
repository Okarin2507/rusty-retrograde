use std::cell::Cell;
use std::time::{Duration, Instant};

use iced::advanced::layout::Layout;
use iced::advanced::renderer;
use iced::advanced::widget::{Tree, tree};
use iced::advanced::{Clipboard, Renderer as _, Shell, Widget};
use iced::{Color, Element, Event, Rectangle, Transformation, mouse};

use super::{Ease, Tween, delegate_single_child_core, lerp, lerp_color, scope::scope};

/// What a leaf reacts to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    Hover,
    Press,
}

/// How far (0..1) a leaf's transition is, given its spec; 0 outside any scope.
fn leaf_value(tree: &Tree, key: u64, trigger: Trigger, dur: Duration, ease: Ease) -> f32 {
    let cell = tree.state.downcast_ref::<Cell<Option<Tween>>>();
    let ctx = scope();
    let now = ctx.map_or_else(Instant::now, |c| c.now);
    let on = ctx.is_some_and(|c| match trigger {
        Trigger::Hover => c.hovered,
        Trigger::Press => c.pressed,
    });
    Tween::step(cell, key, if on { 1.0 } else { 0.0 }, now, dur, ease)
}

struct Leaf<'a, Message> {
    key: u64,
    trigger: Trigger,
    dur: Duration,
    ease: Ease,
    content: Element<'a, Message>,
    paint: Paint,
}

#[derive(Clone, Copy)]
enum Paint {
    Scale(f32),
    TextColor(Color, Color),
}

impl<Message> Widget<Message, iced::Theme, iced::Renderer> for Leaf<'_, Message> {
    delegate_single_child_core!();

    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<Cell<Option<Tween>>>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(Cell::<Option<Tween>>::new(None))
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &iced::Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) {
        self.content.as_widget_mut().update(
            &mut tree.children[0],
            event,
            layout,
            cursor,
            renderer,
            clipboard,
            shell,
            viewport,
        );
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &iced::Renderer,
    ) -> mouse::Interaction {
        self.content.as_widget().mouse_interaction(
            &tree.children[0],
            layout,
            cursor,
            viewport,
            renderer,
        )
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut iced::Renderer,
        theme: &iced::Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        let v = leaf_value(tree, self.key, self.trigger, self.dur, self.ease);
        let child = &tree.children[0];
        match self.paint {
            Paint::Scale(to) => {
                let s = lerp(1.0, to, v);
                if (s - 1.0).abs() < 1e-4 {
                    self.content
                        .as_widget()
                        .draw(child, renderer, theme, style, layout, cursor, viewport);
                } else {
                    let c = layout.bounds().center();
                    let t = Transformation::translate(c.x, c.y)
                        * Transformation::scale(s)
                        * Transformation::translate(-c.x, -c.y);
                    renderer.with_transformation(t, |r| {
                        self.content
                            .as_widget()
                            .draw(child, r, theme, style, layout, cursor, viewport);
                    });
                }
            }
            Paint::TextColor(from, to) => {
                let style = renderer::Style {
                    text_color: lerp_color(from, to, v),
                };
                self.content
                    .as_widget()
                    .draw(child, renderer, theme, &style, layout, cursor, viewport);
            }
        }
    }
}

/// Scales `content` around its center to `to` while its scope is hovered or pressed.
pub fn scale_on<'a, Message: 'a>(
    trigger: Trigger,
    to: f32,
    dur_ms: u64,
    content: impl Into<Element<'a, Message>>,
) -> Element<'a, Message> {
    Element::new(Leaf {
        key: 0,
        trigger,
        dur: Duration::from_millis(dur_ms),
        ease: Ease::Linear,
        content: content.into(),
        paint: Paint::Scale(to),
    })
}

/// Fades the text color of `content` from `from` to `to` on hover.
pub fn hover_text_color<'a, Message: 'a>(
    from: Color,
    to: Color,
    dur_ms: u64,
    content: impl Into<Element<'a, Message>>,
) -> Element<'a, Message> {
    Element::new(Leaf {
        key: 0,
        trigger: Trigger::Hover,
        dur: Duration::from_millis(dur_ms),
        ease: Ease::Linear,
        content: content.into(),
        paint: Paint::TextColor(from, to),
    })
}
