//! Hold-and-drag reordering of the track rows on a playlist or an unnumbered album page.
//!
//! [`list`] lays rows out like a column with [`SPACING`]. Holding the left button on a row for
//! [`HOLD`] without moving lifts it: the row follows the pointer as a raised copy (the `ghost`
//! element the page builds for the lifted row, so it survives virtualization), the other rows slide
//! apart to show where it will land, and the list scrolls when the pointer nears its top or bottom
//! edge. Releasing drops it there; Escape (or a release on the same slot) puts it back.
//!
//! A press that moves before the hold time is an ordinary press (the row still plays on release),
//! so a slightly shaky click never turns into a drag. Once a row is lifted, the row's button gets a
//! release with no cursor, which resets it without playing the track.
//!
//! Rows are expected to be the same height (track rows), and `offset` is the index of the first
//! row given (the page virtualizes long lists).
//!
//! With animations on, rows slide aside (150 ms cubic-out) and the edge scroll runs at 60 FPS;
//! the wheel still glides through [`crate::smooth_scroll`] while a row is held, the lifted row
//! staying under the pointer. When animations are off (Power Saver or disabled in Settings), rows
//! jump to their place, the wheel scrolls in plain notches and the edge scroll steps ten times a
//! second, so a held row costs no continuous frames.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::motion;

use iced::advanced::layout::{self, Layout};
use iced::advanced::renderer::{self, Quad};
use iced::advanced::widget::{Operation, Tree, tree};
use iced::advanced::{Clipboard, Shell, Widget, overlay};
use iced::{
    Background, Border, Color, Element, Event, Length, Point, Rectangle, Size, Vector, keyboard,
    mouse, window,
};

/// Gap between rows (the page's track column spacing).
pub const SPACING: f32 = 2.0;
/// How long a row must be held still before it lifts.
pub const HOLD: Duration = Duration::from_millis(300);
/// Movement that turns a pending hold back into an ordinary press.
const SLOP: f32 = 6.0;
/// Band at the top and bottom of the visible list where a lifted row scrolls it.
const EDGE: f32 = 56.0;
/// Scroll speed at the very edge, px/s.
const EDGE_SPEED: f32 = 900.0;
/// How long a row takes to slide aside.
const SLIDE: Duration = Duration::from_millis(150);
/// Edge-scroll step interval in power-saver mode.
const ECO_STEP: Duration = Duration::from_millis(100);

/// A row is lifted somewhere: the smooth-scroll wrapper then keeps the real pointer flowing to
/// the list while it glides, so the lifted row follows the hand.
static ACTIVE: AtomicBool = AtomicBool::new(false);
/// Set on left press over an inner non-draggable control (`heart`, `more`) so holding it down
/// does not arm drag-reorder.
static PRESS_ON_ACTION: AtomicBool = AtomicBool::new(false);

pub fn dragging() -> bool {
    ACTIVE.load(Ordering::Relaxed)
}

/// Wraps an inner clickable control (`heart`, overflow button) inside a reorderable track row so
/// holding down on it does not start a drag-reorder hold timer.
pub(crate) fn no_drag<'a, Message: 'a>(
    content: impl Into<Element<'a, Message>>,
) -> Element<'a, Message> {
    Element::new(NoDrag {
        content: content.into(),
    })
}

struct NoDrag<'a, Message> {
    content: Element<'a, Message>,
}

impl<Message> Widget<Message, iced::Theme, iced::Renderer> for NoDrag<'_, Message> {
    fn size(&self) -> Size<Length> {
        self.content.as_widget().size()
    }

    fn size_hint(&self) -> Size<Length> {
        self.content.as_widget().size_hint()
    }

    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(&self.content)]
    }

    fn diff(&self, tree: &mut Tree) {
        tree.diff_children(std::slice::from_ref(&self.content));
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &iced::Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        self.content
            .as_widget_mut()
            .layout(&mut tree.children[0], renderer, limits)
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &iced::Renderer,
        operation: &mut dyn Operation,
    ) {
        self.content
            .as_widget_mut()
            .operate(&mut tree.children[0], layout, renderer, operation);
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
        if matches!(
            event,
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left))
        ) && cursor.is_over(layout.bounds())
        {
            PRESS_ON_ACTION.store(true, Ordering::Relaxed);
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

    fn overlay<'b>(
        &'b mut self,
        tree: &'b mut Tree,
        layout: Layout<'b>,
        renderer: &iced::Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'b, Message, iced::Theme, iced::Renderer>> {
        self.content.as_widget_mut().overlay(
            &mut tree.children[0],
            layout,
            renderer,
            viewport,
            translation,
        )
    }
}

/// A drop: `Some((from, to))`, or `None` when the row went back to its place.
pub type Moved = Option<(usize, usize)>;

pub struct Callbacks<'a, Message> {
    /// A row was lifted (absolute index): the page then supplies its ghost.
    pub on_lift: Box<dyn Fn(usize) -> Message + 'a>,
    /// The lifted row was dropped: `Some((from, to))`, or `None` when nothing moved.
    pub on_drop: Box<dyn Fn(Moved) -> Message + 'a>,
    /// Scroll the list by this many pixels (the pointer is at an edge).
    pub on_edge: Box<dyn Fn(f32) -> Message + 'a>,
}

/// `rows` are rows `offset..offset + rows.len()` of a list of `total`; `ghost` is the lifted row's
/// copy while the page knows of a lift.
pub fn list<'a, Message: 'a>(
    rows: Vec<Element<'a, Message>>,
    offset: usize,
    total: usize,
    ghost: Option<Element<'a, Message>>,
    accent: Color,
    callbacks: Callbacks<'a, Message>,
) -> Element<'a, Message> {
    Element::new(Reorder {
        rows,
        offset,
        total,
        ghost,
        accent,
        callbacks,
    })
}

struct Reorder<'a, Message> {
    rows: Vec<Element<'a, Message>>,
    offset: usize,
    total: usize,
    ghost: Option<Element<'a, Message>>,
    accent: Color,
    callbacks: Callbacks<'a, Message>,
}

#[derive(Debug, Clone, Copy)]
struct Press {
    index: usize,
    origin: Point,
    at: Instant,
    /// Pointer y minus the row's top.
    grab: f32,
}

#[derive(Debug, Clone, Copy)]
struct Drag {
    from: usize,
    grab: f32,
    /// Pointer y (list coordinates) on the last event.
    y: f32,
    /// Pointer y relative to the top of the visible list, for events that carry no pointer.
    rel_y: f32,
    target: usize,
    last_frame: Option<Instant>,
}

/// A row sliding from one offset to another.
#[derive(Debug, Clone, Copy)]
struct Slide {
    from: f32,
    to: f32,
    start: Instant,
}

#[derive(Debug, Default)]
struct State {
    press: Option<Press>,
    drag: Option<Drag>,
    /// Rows still sliding, by absolute index.
    slides: HashMap<usize, Slide>,
    /// RETRO_SHOT harness: the fake pointer (list coordinates) that replaces the real one.
    dev_cursor: Option<Point>,
}

impl Drop for State {
    fn drop(&mut self) {
        if self.drag.is_some() {
            ACTIVE.store(false, Ordering::Relaxed);
        }
    }
}

/// Developer hook for the RETRO_SHOT harness (no input device needed): press a row, move the
/// pointer, release, as a hand would.
pub mod dev {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[derive(Debug, Clone, Copy)]
    pub enum Act {
        /// Press the middle of row `n` (absolute index) of the list.
        Press(usize),
        /// Move the pointer by this many pixels down.
        Move(f32),
        Release,
    }

    pub(super) static PENDING: AtomicBool = AtomicBool::new(false);
    static ACT: Mutex<Option<Act>> = Mutex::new(None);

    pub fn act(a: Act) {
        if let Ok(mut slot) = ACT.lock() {
            *slot = Some(a);
            PENDING.store(true, Ordering::Relaxed);
        }
    }

    pub(super) fn take() -> Option<Act> {
        let a = ACT.lock().ok()?.take();
        PENDING.store(false, Ordering::Relaxed);
        a
    }
}

impl<Message> Reorder<'_, Message> {
    fn row_height(&self, layout: Layout<'_>) -> f32 {
        layout.children().next().map_or(0.0, |r| r.bounds().height)
    }

    /// Top of row 0, which may be scrolled away (virtualized).
    fn origin_y(&self, layout: Layout<'_>, stride: f32) -> f32 {
        layout.bounds().y - self.offset as f32 * stride
    }

    fn target_for(&self, layout: Layout<'_>, drag: &Drag) -> usize {
        let h = self.row_height(layout);
        let stride = h + SPACING;
        if stride <= 0.0 || self.total == 0 {
            return drag.from;
        }
        let center = drag.y - drag.grab + h / 2.0;
        let slot = ((center - self.origin_y(layout, stride)) / stride).floor();
        slot.clamp(0.0, (self.total - 1) as f32) as usize
    }

    /// Where row `index` is drawn now: its slide, or its resting place for `drag`.
    fn offset_now(
        slides: &HashMap<usize, Slide>,
        drag: &Drag,
        index: usize,
        stride: f32,
        now: Instant,
    ) -> f32 {
        match slides.get(&index) {
            Some(s) => motion::lerp(
                s.from,
                s.to,
                motion::progress(s.start, now, SLIDE, motion::Ease::CubicOut),
            ),
            None => Self::shift(drag, index, stride),
        }
    }

    /// The drop slot moved: rows between the old and new slot start sliding from where they are
    /// drawn now (or jump, with animations off).
    fn retarget(
        slides: &mut HashMap<usize, Slide>,
        drag: &mut Drag,
        target: usize,
        stride: f32,
        now: Instant,
    ) {
        if target == drag.target {
            return;
        }
        let old = *drag;
        drag.target = target;
        if !motion::enabled() {
            slides.clear();
            return;
        }
        let lo = old.from.min(old.target).min(target);
        let hi = old.from.max(old.target).max(target);
        for index in lo..=hi {
            if index == old.from {
                continue;
            }
            let from = Self::offset_now(slides, &old, index, stride, now);
            let to = Self::shift(drag, index, stride);
            if (from - to).abs() > 0.01 {
                slides.insert(
                    index,
                    Slide {
                        from,
                        to,
                        start: now,
                    },
                );
            } else {
                slides.remove(&index);
            }
        }
    }

    /// How far row `index` is pushed aside while `drag` is held over its target.
    fn shift(drag: &Drag, index: usize, stride: f32) -> f32 {
        let (from, to) = (drag.from, drag.target);
        if from < to && index > from && index <= to {
            -stride
        } else if to < from && index >= to && index < from {
            stride
        } else {
            0.0
        }
    }
}

impl<Message> Widget<Message, iced::Theme, iced::Renderer> for Reorder<'_, Message> {
    fn size(&self) -> Size<Length> {
        Size::new(Length::Fill, Length::Shrink)
    }

    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<State>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(State::default())
    }

    fn children(&self) -> Vec<Tree> {
        self.rows
            .iter()
            .chain(self.ghost.iter())
            .map(Tree::new)
            .collect()
    }

    fn diff(&self, tree: &mut Tree) {
        let all: Vec<&Element<'_, Message>> = self.rows.iter().chain(self.ghost.iter()).collect();
        tree.diff_children(&all);
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &iced::Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        let width = limits.width(Length::Fill).max().width;
        let child_limits = layout::Limits::new(Size::ZERO, Size::new(width, f32::INFINITY));
        let mut nodes = Vec::with_capacity(self.rows.len() + 1);
        let mut y = 0.0;
        let n = self.rows.len();
        for (row, state) in self.rows.iter_mut().zip(&mut tree.children) {
            let node = row.as_widget_mut().layout(state, renderer, &child_limits);
            let h = node.size().height;
            nodes.push(node.move_to(Point::new(0.0, y)));
            y += h + SPACING;
        }
        if let Some(ghost) = &mut self.ghost
            && let Some(state) = tree.children.get_mut(n)
        {
            nodes.push(ghost.as_widget_mut().layout(state, renderer, &child_limits));
        }
        let height = if n == 0 { 0.0 } else { y - SPACING };
        layout::Node::with_children(Size::new(width, height), nodes)
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &iced::Renderer,
        operation: &mut dyn Operation,
    ) {
        operation.container(None, layout.bounds());
        operation.traverse(&mut |operation| {
            for ((row, state), layout) in self
                .rows
                .iter_mut()
                .zip(&mut tree.children)
                .zip(layout.children())
            {
                row.as_widget_mut()
                    .operate(state, layout, renderer, operation);
            }
        });
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
        if dev::PENDING.load(std::sync::atomic::Ordering::Relaxed)
            && let Some(act) = dev::take()
        {
            let h = self.row_height(layout);
            let stride = h + SPACING;
            let st = tree.state.downcast_mut::<State>();
            let (point, synthetic) = match act {
                dev::Act::Press(i) => (
                    Point::new(
                        layout.bounds().center_x(),
                        self.origin_y(layout, stride) + i as f32 * stride + h / 2.0,
                    ),
                    Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
                ),
                dev::Act::Move(dy) => {
                    let p = st.dev_cursor.unwrap_or_default();
                    (
                        Point::new(p.x, p.y + dy),
                        Event::Mouse(mouse::Event::CursorMoved {
                            position: Point::new(p.x, p.y + dy),
                        }),
                    )
                }
                dev::Act::Release => (
                    st.dev_cursor.unwrap_or_default(),
                    Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)),
                ),
            };
            st.dev_cursor = Some(point);
            let release = matches!(act, dev::Act::Release);
            self.update(
                tree,
                &synthetic,
                layout,
                mouse::Cursor::Available(point),
                renderer,
                clipboard,
                shell,
                viewport,
            );
            if release {
                tree.state.downcast_mut::<State>().dev_cursor = None;
            }
            shell.request_redraw();
        }
        let n = self.rows.len();
        let state = tree.state.downcast_mut::<State>();
        let cursor = state.dev_cursor.map_or(cursor, mouse::Cursor::Available);

        // A lifted row owns the pointer until it is dropped.
        if let Some(mut drag) = state.drag {
            // Outside the list the pointer only levitates; it still steers the row. Events without
            // a pointer (a glide's frames) keep it where it was on screen.
            match cursor.land().position() {
                Some(p) => {
                    drag.y = p.y;
                    drag.rel_y = p.y - viewport.y;
                }
                None => drag.y = viewport.y + drag.rel_y,
            }
            let stride = self.row_height(layout) + SPACING;
            let target = self.target_for(layout, &drag);
            Self::retarget(&mut state.slides, &mut drag, target, stride, Instant::now());
            match event {
                Event::Mouse(mouse::Event::CursorMoved { .. }) => {
                    shell.request_redraw();
                    shell.capture_event();
                }
                Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left))
                | Event::Mouse(mouse::Event::ButtonPressed(_)) => {
                    state.drag = None;
                    state.slides.clear();
                    ACTIVE.store(false, Ordering::Relaxed);
                    let moved = (drag.target != drag.from).then_some((drag.from, drag.target));
                    shell.publish((self.callbacks.on_drop)(moved));
                    shell.request_redraw();
                    shell.capture_event();
                    return;
                }
                Event::Keyboard(keyboard::Event::KeyPressed {
                    key: keyboard::Key::Named(keyboard::key::Named::Escape),
                    ..
                }) => {
                    state.drag = None;
                    state.slides.clear();
                    ACTIVE.store(false, Ordering::Relaxed);
                    shell.publish((self.callbacks.on_drop)(None));
                    shell.request_redraw();
                    shell.capture_event();
                    return;
                }
                Event::Window(window::Event::Unfocused) => {
                    state.drag = None;
                    state.slides.clear();
                    ACTIVE.store(false, Ordering::Relaxed);
                    shell.publish((self.callbacks.on_drop)(None));
                    shell.request_redraw();
                    return;
                }
                Event::Window(window::Event::RedrawRequested(now)) => {
                    let top = viewport.y + EDGE;
                    let bottom = viewport.y + viewport.height - EDGE;
                    let can_up = layout.bounds().y < viewport.y - 0.5;
                    let can_down = layout.bounds().y + layout.bounds().height
                        > viewport.y + viewport.height + 0.5;
                    let raw_push = if drag.y < top {
                        -((top - drag.y) / EDGE).min(1.0)
                    } else if drag.y > bottom {
                        ((drag.y - bottom) / EDGE).min(1.0)
                    } else {
                        0.0
                    };
                    let push = if (raw_push < 0.0 && !can_up) || (raw_push > 0.0 && !can_down) {
                        0.0
                    } else {
                        raw_push
                    };
                    if push == 0.0 {
                        drag.last_frame = None;
                    } else if motion::enabled() {
                        let dt = drag.last_frame.map_or(1.0 / 60.0, |t| {
                            now.saturating_duration_since(t).as_secs_f32().min(0.05)
                        });
                        drag.last_frame = Some(*now);
                        shell.publish((self.callbacks.on_edge)(push * EDGE_SPEED * dt));
                        shell.request_redraw_at(*now + motion::FRAME);
                    } else {
                        // Animations off: a step every ECO_STEP instead of a 60 FPS frame stream.
                        let due = drag
                            .last_frame
                            .is_none_or(|t| now.saturating_duration_since(t) >= ECO_STEP);
                        if due {
                            drag.last_frame = Some(*now);
                            shell.publish((self.callbacks.on_edge)(
                                push * EDGE_SPEED * ECO_STEP.as_secs_f32(),
                            ));
                        }
                        shell.request_redraw_at(drag.last_frame.unwrap_or(*now) + ECO_STEP);
                    }
                    if motion::enabled() {
                        state
                            .slides
                            .retain(|_, s| now.saturating_duration_since(s.start) < SLIDE);
                        if !state.slides.is_empty() {
                            shell.request_redraw_at(*now + motion::FRAME);
                        }
                    } else {
                        state.slides.clear();
                    }
                }
                _ => {}
            }
            state.drag = Some(drag);
            return;
        }

        if let Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) = event {
            let captured_before = shell.is_event_captured();
            PRESS_ON_ACTION.store(false, Ordering::Relaxed);
            for ((row, st), l) in self
                .rows
                .iter_mut()
                .zip(&mut tree.children)
                .zip(layout.children())
            {
                row.as_widget_mut()
                    .update(st, event, l, cursor, renderer, clipboard, shell, viewport);
            }
            let on_action = PRESS_ON_ACTION.swap(false, Ordering::Relaxed);
            state.press = None;
            if !captured_before
                && !on_action
                && let Some(p) = cursor.position()
                && let Some((i, row)) = layout
                    .children()
                    .take(n)
                    .enumerate()
                    .find(|(_, r)| r.bounds().contains(p))
            {
                let at = Instant::now();
                state.press = Some(Press {
                    index: self.offset + i,
                    origin: p,
                    at,
                    grab: p.y - row.bounds().y,
                });
                shell.request_redraw_at(at + HOLD);
            }
            return;
        }

        match event {
            Event::Mouse(mouse::Event::ButtonReleased(_))
            | Event::Window(window::Event::Unfocused) => state.press = None,
            Event::Mouse(mouse::Event::CursorMoved { .. }) => {
                if let Some(press) = state.press
                    && cursor
                        .position()
                        .is_none_or(|p| p.distance(press.origin) > SLOP)
                {
                    state.press = None;
                }
            }
            Event::Window(window::Event::RedrawRequested(now)) => {
                if let Some(press) = state.press {
                    if now.saturating_duration_since(press.at) >= HOLD {
                        state.press = None;
                        let y = cursor.position().map_or(press.origin.y, |p| p.y);
                        let mut drag = Drag {
                            from: press.index,
                            grab: press.grab,
                            y,
                            rel_y: y - viewport.y,
                            target: press.index,
                            last_frame: None,
                        };
                        drag.target = self.target_for(layout, &drag);
                        state.drag = Some(drag);
                        state.slides.clear();
                        ACTIVE.store(true, Ordering::Relaxed);
                        // Reset the pressed row without playing it.
                        let release =
                            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left));
                        for ((row, st), l) in self
                            .rows
                            .iter_mut()
                            .zip(&mut tree.children)
                            .zip(layout.children())
                        {
                            row.as_widget_mut().update(
                                st,
                                &release,
                                l,
                                mouse::Cursor::Unavailable,
                                renderer,
                                clipboard,
                                shell,
                                viewport,
                            );
                        }
                        shell.publish((self.callbacks.on_lift)(press.index));
                        shell.request_redraw();
                        return;
                    }
                    shell.request_redraw_at(press.at + HOLD);
                }
            }
            _ => {}
        }

        for ((row, st), l) in self
            .rows
            .iter_mut()
            .zip(&mut tree.children)
            .zip(layout.children())
        {
            row.as_widget_mut()
                .update(st, event, l, cursor, renderer, clipboard, shell, viewport);
        }
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &iced::Renderer,
    ) -> mouse::Interaction {
        if tree.state.downcast_ref::<State>().drag.is_some() {
            return mouse::Interaction::Grabbing;
        }
        self.rows
            .iter()
            .zip(&tree.children)
            .zip(layout.children())
            .map(|((row, st), l)| {
                row.as_widget()
                    .mouse_interaction(st, l, cursor, viewport, renderer)
            })
            .max()
            .unwrap_or_default()
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
        use iced::advanced::Renderer as _;
        let n = self.rows.len();
        let state = tree.state.downcast_ref::<State>();
        let Some(drag) = state.drag else {
            for ((row, st), l) in self.rows.iter().zip(&tree.children).zip(layout.children()) {
                if l.bounds().intersects(viewport) {
                    row.as_widget()
                        .draw(st, renderer, theme, style, l, cursor, viewport);
                }
            }
            return;
        };

        let h = self.row_height(layout);
        let stride = h + SPACING;
        let now = Instant::now();
        for (i, ((row, st), l)) in self
            .rows
            .iter()
            .zip(&tree.children)
            .zip(layout.children())
            .enumerate()
        {
            let index = self.offset + i;
            if index == drag.from {
                continue;
            }
            let dy = Self::offset_now(&state.slides, &drag, index, stride, now);
            let moved = l.bounds() + Vector::new(0.0, dy);
            if !moved.intersects(viewport) {
                continue;
            }
            renderer.with_translation(Vector::new(0.0, dy), |r| {
                row.as_widget()
                    .draw(st, r, theme, style, l, mouse::Cursor::Unavailable, viewport);
            });
        }

        // The slot the row will land in.
        let slot_y = self.origin_y(layout, stride) + drag.target as f32 * stride;
        let bounds = layout.bounds();
        renderer.fill_quad(
            Quad {
                bounds: Rectangle {
                    x: bounds.x,
                    y: slot_y,
                    width: bounds.width,
                    height: h,
                },
                border: Border {
                    color: Color {
                        a: 0.35,
                        ..self.accent
                    },
                    width: 1.0,
                    radius: 0.0.into(),
                },
                ..Quad::default()
            },
            Background::Color(Color {
                a: 0.06,
                ..self.accent
            }),
        );

        // The lifted copy, above everything in the list.
        if let (Some(ghost), Some(st), Some(gl)) =
            (&self.ghost, tree.children.get(n), layout.children().nth(n))
        {
            let top = drag.y - drag.grab;
            let offset = Vector::new(bounds.x - gl.bounds().x, top - gl.bounds().y);
            renderer.with_layer(*viewport, |r| {
                r.with_translation(offset, |r| {
                    let b = gl.bounds();
                    r.fill_quad(
                        Quad {
                            bounds: b,
                            border: Border {
                                color: self.accent,
                                width: 1.0,
                                radius: 0.0.into(),
                            },
                            shadow: iced::Shadow {
                                color: Color::from_rgba(0.0, 0.0, 0.0, 0.5),
                                offset: Vector::new(0.0, 6.0),
                                blur_radius: 18.0,
                            },
                            ..Quad::default()
                        },
                        Background::Color(crate::theme::HOVER),
                    );
                    ghost.as_widget().draw(
                        st,
                        r,
                        theme,
                        style,
                        gl,
                        mouse::Cursor::Unavailable,
                        viewport,
                    );
                });
            });
        }
    }

    fn overlay<'b>(
        &'b mut self,
        tree: &'b mut Tree,
        layout: Layout<'b>,
        renderer: &iced::Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'b, Message, iced::Theme, iced::Renderer>> {
        overlay::from_children(
            &mut self.rows,
            tree,
            layout,
            renderer,
            viewport,
            translation,
        )
    }
}

/// Moves `items[from]` so it ends up at index `to` (as the drop slot shows).
pub fn move_item<T>(items: &mut Vec<T>, from: usize, to: usize) {
    if from >= items.len() {
        return;
    }
    let item = items.remove(from);
    let to = to.min(items.len());
    items.insert(to, item);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drag(from: usize, target: usize) -> Drag {
        Drag {
            from,
            grab: 0.0,
            y: 0.0,
            rel_y: 0.0,
            target,
            last_frame: None,
        }
    }

    #[test]
    fn rows_between_slide_toward_the_lifted_row() {
        let down = drag(1, 3);
        assert_eq!(Reorder::<()>::shift(&down, 0, 54.0), 0.0);
        assert_eq!(Reorder::<()>::shift(&down, 2, 54.0), -54.0);
        assert_eq!(Reorder::<()>::shift(&down, 3, 54.0), -54.0);
        assert_eq!(Reorder::<()>::shift(&down, 4, 54.0), 0.0);
        let up = drag(3, 1);
        assert_eq!(Reorder::<()>::shift(&up, 1, 54.0), 54.0);
        assert_eq!(Reorder::<()>::shift(&up, 2, 54.0), 54.0);
        assert_eq!(Reorder::<()>::shift(&up, 3, 54.0), 0.0);
        assert_eq!(Reorder::<()>::shift(&up, 0, 54.0), 0.0);
    }

    #[test]
    fn move_index_matches_the_drop_slot() {
        let mut v = vec!["a", "b", "c", "d"];
        move_item(&mut v, 0, 2);
        assert_eq!(v, ["b", "c", "a", "d"]);
        move_item(&mut v, 3, 0);
        assert_eq!(v, ["d", "b", "c", "a"]);
        move_item(&mut v, 1, 9);
        assert_eq!(v, ["d", "c", "a", "b"]);
    }
}
