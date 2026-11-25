use std::time::Instant;

use iced::advanced::graphics::geometry::Renderer as _;
use iced::advanced::layout::{self, Layout};
use iced::advanced::renderer::{self, Quad, Renderer as _};
use iced::advanced::widget::{Operation, Tree, tree};
use iced::advanced::{Clipboard, Shell, Widget, overlay};
use iced::widget::canvas::{self, Fill, Stroke};
use iced::window::{self, RedrawRequest};
use iced::{
    Background, Color, Element, Event, Length, Rectangle, Renderer, Size, Theme, Vector, mouse,
};

use super::blot::{Blot, FsAnim, content_alpha};
use super::view::content;
use crate::{App, Message, Tab, theme};

/// Renders the full-screen player over `workspace`, preserving workspace widget state as child 0.
pub fn view<'a>(
    app: &'a App,
    workspace: Element<'a, Message>,
    accent: Color,
) -> Element<'a, Message> {
    let open = app.active_tab == Tab::NowPlaying;
    let overlay_player = (open || app.fs_anim.is_some()).then(|| content(app, accent));
    let color = crate::motion::lerp_color(theme::VOID, app.accent_anim.current_glow, 0.22);
    let mut children = vec![workspace];
    children.extend(overlay_player);
    Element::new(Reveal {
        children,
        anim: app.fs_anim,
        open,
        color,
    })
}

struct Reveal<'a> {
    children: Vec<Element<'a, Message>>,
    anim: Option<FsAnim>,
    open: bool,
    color: Color,
}

#[derive(Default)]
struct RevealState {
    now: Option<Instant>,
    done: Option<Instant>,
}

impl Reveal<'_> {
    fn share(&self, now: Instant) -> f32 {
        match self.anim {
            Some(a) => a.share_at(now),
            None => {
                if self.open {
                    1.0
                } else {
                    0.0
                }
            }
        }
    }

    fn player_alpha(&self, state: &RevealState) -> f32 {
        if self.children.len() < 2 {
            return 0.0;
        }
        content_alpha(self.share(state.now.unwrap_or_else(Instant::now)))
    }
}

fn is_pointer(event: &Event) -> bool {
    matches!(event, Event::Mouse(_) | Event::Touch(_))
}

fn is_press(event: &Event) -> bool {
    matches!(
        event,
        Event::Mouse(mouse::Event::ButtonPressed(_))
            | Event::Touch(iced::touch::Event::FingerPressed { .. })
    )
}

impl Widget<Message, Theme, Renderer> for Reveal<'_> {
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<RevealState>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(RevealState::default())
    }

    fn children(&self) -> Vec<Tree> {
        self.children.iter().map(Tree::new).collect()
    }

    fn diff(&self, tree: &mut Tree) {
        tree.diff_children(&self.children);
    }

    fn size(&self) -> Size<Length> {
        Size::new(Length::Fill, Length::Fill)
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        let size = limits.max();
        let limits = layout::Limits::new(Size::ZERO, size);
        let nodes = self
            .children
            .iter_mut()
            .zip(&mut tree.children)
            .map(|(child, tree)| child.as_widget_mut().layout(tree, renderer, &limits))
            .collect();
        layout::Node::with_children(size, nodes)
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        operation: &mut dyn Operation,
    ) {
        operation.container(None, layout.bounds());
        operation.traverse(&mut |operation| {
            self.children
                .iter_mut()
                .zip(&mut tree.children)
                .zip(layout.children())
                .for_each(|((child, state), layout)| {
                    child
                        .as_widget_mut()
                        .operate(state, layout, renderer, operation);
                });
        });
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) {
        if let Event::Window(window::Event::RedrawRequested(now)) = event {
            let state = tree.state.downcast_mut::<RevealState>();
            state.now = Some(*now);
            if let Some(a) = self.anim {
                if crate::motion::enabled() && !a.finished(*now) {
                    crate::motion::dev::log_frame(*now);
                    shell.request_redraw_at(RedrawRequest::At(*now + crate::motion::FRAME));
                } else if state.done != Some(a.leg_start) {
                    state.done = Some(a.leg_start);
                    shell.publish(Message::FsAnimDone(a.leg_start));
                }
            }
        }
        let alpha = self.player_alpha(tree.state.downcast_ref::<RevealState>());
        let has_player = self.children.len() > 1;
        let n = self.children.len().min(1);
        let (workspace, player) = self.children.split_at_mut(n);
        let n = tree.children.len().min(1);
        let (ws_tree, pl_tree) = tree.children.split_at_mut(n);
        let mut layouts = layout.children();
        let ws_layout = layouts.next();
        if let (Some(child), Some(state), Some(l)) =
            (player.first_mut(), pl_tree.first_mut(), layouts.next())
        {
            let takes_pointer = alpha > 0.5 && self.anim.is_none_or(|a| a.opening);
            if !is_pointer(event) || takes_pointer {
                child.as_widget_mut().update(
                    state, event, l, cursor, renderer, clipboard, shell, viewport,
                );
                if shell.is_event_captured() {
                    return;
                }
            }
        }
        if let (Some(child), Some(state), Some(l)) =
            (workspace.first_mut(), ws_tree.first_mut(), ws_layout)
        {
            if has_player {
                let fully_occluded = self.open && self.anim.is_none();
                let is_redraw = matches!(event, Event::Window(window::Event::RedrawRequested(_)));
                if !is_press(event) && !(fully_occluded && is_redraw) {
                    child.as_widget_mut().update(
                        state,
                        event,
                        l,
                        cursor.levitate(),
                        renderer,
                        clipboard,
                        shell,
                        viewport,
                    );
                }
            } else {
                child.as_widget_mut().update(
                    state, event, l, cursor, renderer, clipboard, shell, viewport,
                );
            }
        }
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &Renderer,
    ) -> mouse::Interaction {
        let alpha = self.player_alpha(tree.state.downcast_ref::<RevealState>());
        let i = if self.children.len() > 1 {
            if alpha <= 0.5 {
                return mouse::Interaction::default();
            }
            1
        } else {
            0
        };
        match (
            self.children.get(i),
            tree.children.get(i),
            layout.children().nth(i),
        ) {
            (Some(child), Some(state), Some(l)) => child
                .as_widget()
                .mouse_interaction(state, l, cursor, viewport, renderer),
            _ => mouse::Interaction::default(),
        }
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut Renderer,
        theme: &Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        let state = tree.state.downcast_ref::<RevealState>();
        let now = state.now.unwrap_or_else(Instant::now);
        let share = self.share(now);
        let alpha = self.player_alpha(state);
        let bounds = layout.bounds();
        let mut layouts = layout.children();
        let ws_layout = layouts.next();
        let pl_layout = layouts.next();

        // Keep workspace textures alive inside Iced's GPU atlas via a 1 px clip under full screen.
        if let (Some(child), Some(state), Some(l)) =
            (self.children.first(), tree.children.first(), ws_layout)
        {
            let cursor = if self.children.len() > 1 {
                mouse::Cursor::Unavailable
            } else {
                cursor
            };
            if alpha <= 0.0 {
                child
                    .as_widget()
                    .draw(state, renderer, theme, style, l, cursor, viewport);
            } else {
                let pixel = Rectangle::new(bounds.position(), Size::new(1.0, 1.0));
                renderer.with_layer(pixel, |r| {
                    child
                        .as_widget()
                        .draw(state, r, theme, style, l, cursor, viewport);
                });
            }
        }
        if alpha <= 0.0 {
            if let Some(a) = self.anim {
                let blot = Blot {
                    share,
                    seconds: now.saturating_duration_since(a.run_start).as_secs_f32(),
                    phases: a.phases,
                    drops: a.drops,
                };
                if let Some(path) = blot.path(bounds.size()) {
                    let mut frame = canvas::Frame::new(renderer, bounds.size());
                    frame.fill(&path, Fill::from(self.color));
                    frame.stroke(
                        &path,
                        Stroke::default()
                            .with_color(Color {
                                a: 0.5,
                                ..self.color
                            })
                            .with_width(1.25),
                    );
                    let geometry = frame.into_geometry();
                    renderer.with_layer(*viewport, |r| {
                        r.with_translation(Vector::new(bounds.x, bounds.y), |r| {
                            r.draw_geometry(geometry);
                        });
                    });
                }
            }
            return;
        }
        if let (Some(child), Some(state), Some(l)) =
            (self.children.get(1), tree.children.get(1), pl_layout)
        {
            renderer.with_layer(*viewport, |r| {
                child
                    .as_widget()
                    .draw(state, r, theme, style, l, cursor, viewport);
            });
        }
        if alpha < 1.0 {
            renderer.with_layer(*viewport, |r| {
                r.fill_quad(
                    Quad {
                        bounds,
                        ..Quad::default()
                    },
                    Background::Color(Color {
                        a: 1.0 - alpha,
                        ..self.color
                    }),
                );
            });
        }
    }

    fn overlay<'b>(
        &'b mut self,
        tree: &'b mut Tree,
        layout: Layout<'b>,
        renderer: &Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'b, Message, Theme, Renderer>> {
        overlay::from_children(
            &mut self.children,
            tree,
            layout,
            renderer,
            viewport,
            translation,
        )
    }
}
