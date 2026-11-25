use std::cell::RefCell;
use std::time::{Duration, Instant};

use iced::advanced::layout::Layout;
use iced::advanced::renderer;
use iced::advanced::widget::{Tree, tree};
use iced::advanced::{Clipboard, Shell, Widget};
use iced::window::RedrawRequest;
use iced::{Element, Event, Point, Rectangle, mouse, window};

use super::{FRAME, delegate_single_child_core, dev, enabled};

#[derive(Debug, Clone, Copy)]
pub(super) struct ScopeCtx {
    pub hovered: bool,
    pub pressed: bool,
    pub now: Instant,
}

thread_local! {
    static SCOPES: RefCell<Vec<ScopeCtx>> = const { RefCell::new(Vec::new()) };
}

struct ScopeGuard;

impl ScopeGuard {
    fn push(ctx: ScopeCtx) -> Self {
        SCOPES.with(|s| s.borrow_mut().push(ctx));
        ScopeGuard
    }
}

impl Drop for ScopeGuard {
    fn drop(&mut self) {
        SCOPES.with(|s| {
            s.borrow_mut().pop();
        });
    }
}

pub(super) fn scope() -> Option<ScopeCtx> {
    SCOPES.with(|s| s.borrow().last().copied())
}

pub(super) fn hovered_in_scope() -> bool {
    scope().is_some_and(|c| c.hovered)
}

/// Makes `content` a hover target for animated leaves inside it.
pub fn hover_scope<'a, Message: 'a>(
    hold: Duration,
    content: impl Into<Element<'a, Message>>,
) -> Element<'a, Message> {
    hover_scope_slop(hold, 0.0, content)
}

/// Like [`hover_scope`], expanding hit/hover bounds by `slop` px on each side.
pub fn hover_scope_slop<'a, Message: 'a>(
    hold: Duration,
    slop: f32,
    content: impl Into<Element<'a, Message>>,
) -> Element<'a, Message> {
    Element::new(HoverScope {
        hold: hold + FRAME * 2,
        slop,
        content: content.into(),
    })
}

struct HoverScope<'a, Message> {
    hold: Duration,
    slop: f32,
    content: Element<'a, Message>,
}

impl<Message> HoverScope<'_, Message> {
    fn content_cursor(&self, cursor: mouse::Cursor, bounds: Rectangle) -> mouse::Cursor {
        match cursor.position() {
            Some(p)
                if self.slop > 0.0
                    && !bounds.contains(p)
                    && bounds.expand(self.slop).contains(p) =>
            {
                let inset = bounds.shrink(1.0);
                mouse::Cursor::Available(Point::new(
                    p.x.clamp(inset.x, inset.x + inset.width),
                    p.y.clamp(inset.y, inset.y + inset.height),
                ))
            }
            _ => cursor,
        }
    }
}

#[derive(Debug, Default)]
struct ScopeState {
    hovered: bool,
    pressed: bool,
    changed: Option<Instant>,
}

impl<Message> Widget<Message, iced::Theme, iced::Renderer> for HoverScope<'_, Message> {
    delegate_single_child_core!();

    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<ScopeState>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(ScopeState::default())
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
        let pointer = dev::cursor().map_or(cursor, mouse::Cursor::Available);
        let state = tree.state.downcast_mut::<ScopeState>();
        let mut hovered = pointer.is_over(layout.bounds().expand(self.slop));
        let mut pressed = state.pressed;
        let unfocused = matches!(event, Event::Window(window::Event::Unfocused));
        match event {
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) if hovered => {
                pressed = true
            }
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) => pressed = false,
            Event::Window(window::Event::Unfocused) => {
                hovered = false;
                pressed = false;
            }
            _ => {}
        }
        let tracked = unfocused
            || matches!(
                event,
                Event::Mouse(_) | Event::Window(window::Event::RedrawRequested(_))
            );
        if tracked && (hovered != state.hovered || pressed != state.pressed) {
            state.hovered = hovered;
            state.pressed = pressed;
            state.changed = enabled().then(Instant::now);
            shell.request_redraw();
        }
        if let Event::Window(window::Event::RedrawRequested(now)) = event
            && enabled()
            && state
                .changed
                .is_some_and(|c| now.saturating_duration_since(c) < self.hold)
        {
            dev::log_frame(*now);
            shell.request_redraw_at(RedrawRequest::At(*now + FRAME));
        }
        let cursor = self.content_cursor(cursor, layout.bounds());
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
        let cursor = self.content_cursor(cursor, layout.bounds());
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
        let state = tree.state.downcast_ref::<ScopeState>();
        let _guard = ScopeGuard::push(ScopeCtx {
            hovered: state.hovered,
            pressed: state.pressed,
            now: Instant::now(),
        });
        let cursor = self.content_cursor(cursor, layout.bounds());
        self.content.as_widget().draw(
            &tree.children[0],
            renderer,
            theme,
            style,
            layout,
            cursor,
            viewport,
        );
    }
}
