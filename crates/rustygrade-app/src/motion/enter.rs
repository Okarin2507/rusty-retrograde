use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use iced::advanced::layout::Layout;
use iced::advanced::renderer::{self, Quad, Renderer as _};
use iced::advanced::widget::Tree;
use iced::advanced::{Clipboard, Shell, Widget};
use iced::window::RedrawRequest;
use iced::{Background, Color, Element, Event, Rectangle, Vector, mouse, window};

use super::{Ease, FRAME, FRAME_SEQ, delegate_single_child_core, dev, enabled, progress};

/// How an entrance fades in.
#[derive(Debug, Clone, Copy)]
pub enum Fade {
    /// Covered by this (opaque) color with falling alpha: an exact fade over that solid background.
    Cover(Color),
    /// A dimming layer of this color drawn under the content, its alpha rising from 0.
    Scrim(Color),
}

#[derive(Debug, Clone, Copy)]
pub struct EnterSpec {
    pub dur_ms: u64,
    pub ease: Ease,
    /// Starts this many px lower and slides up.
    pub dy: f32,
    pub fade: Fade,
}

impl EnterSpec {
    pub const fn new(dur_ms: u64, ease: Ease, dy: f32, fade: Fade) -> Self {
        Self {
            dur_ms,
            ease,
            dy,
            fade,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct Entry {
    start: Instant,
    last_seen: u64,
    last_seen_at: Instant,
}

const ABSENCE: Duration = Duration::from_millis(100);

thread_local! {
    static ENTRANCES: RefCell<HashMap<&'static str, Entry>> = RefCell::new(HashMap::new());
}

/// Plays `spec` each time `channel` comes on screen after being absent.
pub fn enter<'a, Message: 'a>(
    channel: &'static str,
    spec: EnterSpec,
    content: impl Into<Element<'a, Message>>,
) -> Element<'a, Message> {
    Element::new(Enter {
        channel,
        spec,
        content: content.into(),
    })
}

struct Enter<'a, Message> {
    channel: &'static str,
    spec: EnterSpec,
    content: Element<'a, Message>,
}

impl<Message> Enter<'_, Message> {
    fn value(&self, now: Instant) -> f32 {
        let start = ENTRANCES.with(|e| e.borrow().get(self.channel).map(|e| e.start));
        start.map_or(0.0, |s| {
            progress(
                s,
                now,
                Duration::from_millis(self.spec.dur_ms),
                self.spec.ease,
            )
        })
    }
}

impl<Message> Widget<Message, iced::Theme, iced::Renderer> for Enter<'_, Message> {
    delegate_single_child_core!();

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
        if let Event::Window(window::Event::RedrawRequested(now)) = event {
            let seq = FRAME_SEQ.load(Ordering::Relaxed);
            let start = ENTRANCES.with(|e| {
                let mut e = e.borrow_mut();
                let entry = e.entry(self.channel).or_insert(Entry {
                    start: *now,
                    last_seen: seq,
                    last_seen_at: *now,
                });
                if seq > entry.last_seen + 2
                    && now.saturating_duration_since(entry.last_seen_at) >= ABSENCE
                {
                    entry.start = *now;
                }
                entry.last_seen = seq;
                entry.last_seen_at = *now;
                entry.start
            });
            if enabled()
                && now.saturating_duration_since(start) < Duration::from_millis(self.spec.dur_ms)
            {
                dev::log_frame(*now);
                shell.request_redraw_at(RedrawRequest::At(*now + FRAME));
            }
        }
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
        let v = self.value(Instant::now());
        let child = &tree.children[0];
        if let Fade::Scrim(color) = self.spec.fade {
            renderer.fill_quad(
                Quad {
                    bounds: layout.bounds(),
                    ..Quad::default()
                },
                Background::Color(Color {
                    a: color.a * v,
                    ..color
                }),
            );
        }
        if v >= 1.0 {
            self.content
                .as_widget()
                .draw(child, renderer, theme, style, layout, cursor, viewport);
            return;
        }
        let dy = self.spec.dy * (1.0 - v);
        renderer.with_translation(Vector::new(0.0, dy), |r| {
            self.content
                .as_widget()
                .draw(child, r, theme, style, layout, cursor, viewport);
        });
        if let Fade::Cover(color) = self.spec.fade {
            let moved = layout.bounds() + Vector::new(0.0, dy);
            if let Some(clip) = moved.intersection(viewport) {
                renderer.with_layer(clip, |r| {
                    r.fill_quad(
                        Quad {
                            bounds: moved,
                            ..Quad::default()
                        },
                        Background::Color(Color {
                            a: 1.0 - v,
                            ..color
                        }),
                    );
                });
            }
        }
    }
}
