//! Hover, press and entrance transitions (Avalonia `Transitions` and one-shot `Animation`s),
//! done inside widgets so moving values only redraw the window without rebuilding `App::view`.

mod drawn;
mod enter;
mod leaf;
mod pulse;
mod scope;

pub use drawn::{
    HEX_HOVER, card_play, corner_tick, hex, hover_bar, hover_icon, under_image, zoom_cover,
};
pub use enter::{EnterSpec, Fade, enter};
pub use leaf::{Trigger, hover_text_color, scale_on};
pub use pulse::pulse_dot;
pub use scope::{hover_scope, hover_scope_slop};

use std::cell::Cell;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use iced::Color;

/// Fixed frame rate of every animation in the app when enabled.
pub const FPS: u32 = rustygrade_linux::RenderTier::ANIMATION_FPS;
/// Fixed frame spacing for 60 FPS animations.
pub const FRAME: Duration = Duration::from_micros(1_000_000 / FPS as u64);

static ENABLED: AtomicBool = AtomicBool::new(true);
/// Windows presented so far (counted from the raw `RedrawRequested` listener).
pub(super) static FRAME_SEQ: AtomicU64 = AtomicU64::new(0);

/// Follows the resolved animation state (Power Saver profile and manual Settings toggle): when off,
/// transitions jump to their end.
pub fn set_enabled(on: bool) {
    ENABLED.store(on, Ordering::Relaxed);
}

pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// Called once per presented frame of the main window.
pub fn frame_presented() {
    FRAME_SEQ.fetch_add(1, Ordering::Relaxed);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ease {
    Linear,
    CubicOut,
    CubicInOut,
}

impl Ease {
    pub fn apply(self, t: f32) -> f32 {
        let t = t.clamp(0.0, 1.0);
        match self {
            Ease::Linear => t,
            Ease::CubicOut => 1.0 - (1.0 - t).powi(3),
            Ease::CubicInOut => {
                if t < 0.5 {
                    4.0 * t * t * t
                } else {
                    1.0 - (-2.0 * t + 2.0).powi(3) / 2.0
                }
            }
        }
    }
}

/// Progress (0..1, eased) of a one-shot animation started at `start`.
pub fn progress(start: Instant, now: Instant, dur: Duration, ease: Ease) -> f32 {
    if !enabled() || dur.is_zero() {
        return 1.0;
    }
    ease.apply(now.saturating_duration_since(start).as_secs_f32() / dur.as_secs_f32())
}

/// A stable key for a leaf's tween (e.g. the album a card shows).
pub fn key_of(v: &impl std::hash::Hash) -> u64 {
    use std::hash::Hasher;
    let mut h = std::collections::hash_map::DefaultHasher::new();
    v.hash(&mut h);
    h.finish()
}

pub fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

pub fn lerp_color(a: Color, b: Color, t: f32) -> Color {
    Color {
        r: lerp(a.r, b.r, t),
        g: lerp(a.g, b.g, t),
        b: lerp(a.b, b.b, t),
        a: lerp(a.a, b.a, t),
    }
}

/// A transition toward a target that can change midway (Avalonia `Transition` semantics).
#[derive(Debug, Clone, Copy)]
pub(super) struct Tween {
    key: u64,
    from: f32,
    to: f32,
    start: Instant,
}

impl Tween {
    fn at(&self, now: Instant, dur: Duration, ease: Ease) -> f32 {
        lerp(self.from, self.to, progress(self.start, now, dur, ease))
    }

    /// Returns the value at `now`, retargeting first. A new `key` starts settled at `target`.
    pub(super) fn step(
        cell: &Cell<Option<Tween>>,
        key: u64,
        target: f32,
        now: Instant,
        dur: Duration,
        ease: Ease,
    ) -> f32 {
        let t = match cell.get() {
            Some(t) if t.key == key && enabled() => {
                if t.to == target {
                    t
                } else {
                    Tween {
                        key,
                        from: t.at(now, dur, ease),
                        to: target,
                        start: now,
                    }
                }
            }
            _ => Tween {
                key,
                from: target,
                to: target,
                start: now,
            },
        };
        cell.set(Some(t));
        t.at(now, dur, ease)
    }
}

/// Generates standard single-child delegation methods (`size`, `size_hint`, `children`, `diff`,
/// `layout`, `operate`, `overlay`) inside a `Widget` impl that holds `self.content`.
macro_rules! delegate_single_child_core {
    () => {
        fn size(&self) -> iced::Size<iced::Length> {
            self.content.as_widget().size()
        }

        fn size_hint(&self) -> iced::Size<iced::Length> {
            self.content.as_widget().size_hint()
        }

        fn children(&self) -> Vec<iced::advanced::widget::Tree> {
            vec![iced::advanced::widget::Tree::new(&self.content)]
        }

        fn diff(&self, tree: &mut iced::advanced::widget::Tree) {
            tree.diff_children(std::slice::from_ref(&self.content));
        }

        fn layout(
            &mut self,
            tree: &mut iced::advanced::widget::Tree,
            renderer: &iced::Renderer,
            limits: &iced::advanced::layout::Limits,
        ) -> iced::advanced::layout::Node {
            self.content
                .as_widget_mut()
                .layout(&mut tree.children[0], renderer, limits)
        }

        fn operate(
            &mut self,
            tree: &mut iced::advanced::widget::Tree,
            layout: iced::advanced::layout::Layout<'_>,
            renderer: &iced::Renderer,
            operation: &mut dyn iced::advanced::widget::Operation,
        ) {
            self.content.as_widget_mut().operate(
                &mut tree.children[0],
                layout,
                renderer,
                operation,
            );
        }

        fn overlay<'b>(
            &'b mut self,
            tree: &'b mut iced::advanced::widget::Tree,
            layout: iced::advanced::layout::Layout<'b>,
            renderer: &iced::Renderer,
            viewport: &iced::Rectangle,
            translation: iced::Vector,
        ) -> Option<iced::advanced::overlay::Element<'b, Message, iced::Theme, iced::Renderer>> {
            self.content.as_widget_mut().overlay(
                &mut tree.children[0],
                layout,
                renderer,
                viewport,
                translation,
            )
        }
    };
}

pub(super) use delegate_single_child_core;

/// Developer hooks: a fake pointer for `RETRO_SHOT`, and `RETRO_MOTION_LOG=<file>`.
pub mod dev {
    use std::io::Write;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Mutex, OnceLock};
    use std::time::Instant;

    use iced::Point;

    static ACTIVE: AtomicBool = AtomicBool::new(false);
    static CURSOR: Mutex<Option<Point>> = Mutex::new(None);

    /// Sets (or clears) the fake pointer, in window coordinates.
    pub fn set_cursor(p: Option<Point>) {
        if let Ok(mut c) = CURSOR.lock() {
            *c = p;
            ACTIVE.store(p.is_some(), Ordering::Relaxed);
        }
    }

    pub(super) fn cursor() -> Option<Point> {
        if !ACTIVE.load(Ordering::Relaxed) {
            return None;
        }
        CURSOR.lock().ok().and_then(|c| *c)
    }

    pub(crate) fn log_frame(now: Instant) {
        static FILE: OnceLock<Option<(Instant, Mutex<std::fs::File>)>> = OnceLock::new();
        let file = FILE.get_or_init(|| {
            let path = std::env::var_os("RETRO_MOTION_LOG")?;
            let f = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .ok()?;
            Some((Instant::now(), Mutex::new(f)))
        });
        if let Some((t0, f)) = file
            && let Ok(mut f) = f.lock()
        {
            let _ = writeln!(
                f,
                "{:.1}",
                now.saturating_duration_since(*t0).as_secs_f32() * 1000.0
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static ENABLED_TEST_LOCK: Mutex<()> = Mutex::new(());

    struct EnabledGuard(bool);

    impl Drop for EnabledGuard {
        fn drop(&mut self) {
            set_enabled(self.0);
        }
    }

    fn with_enabled(on: bool) -> EnabledGuard {
        let prev = enabled();
        set_enabled(on);
        EnabledGuard(prev)
    }

    #[test]
    fn eases_hit_their_ends_and_midpoints() {
        for e in [Ease::Linear, Ease::CubicOut, Ease::CubicInOut] {
            assert_eq!(e.apply(0.0), 0.0);
            assert_eq!(e.apply(1.0), 1.0);
        }
        assert!((Ease::CubicOut.apply(0.5) - 0.875).abs() < 1e-6);
        assert!((Ease::CubicInOut.apply(0.5) - 0.5).abs() < 1e-6);
    }

    #[test]
    fn tween_retargets_from_current_value_and_resets_on_new_key() {
        let _lock = ENABLED_TEST_LOCK.lock().unwrap();
        let _guard = with_enabled(true);
        let cell = Cell::new(None);
        let t0 = Instant::now();
        let dur = Duration::from_millis(100);
        assert_eq!(Tween::step(&cell, 1, 0.0, t0, dur, Ease::Linear), 0.0);
        assert_eq!(Tween::step(&cell, 1, 1.0, t0, dur, Ease::Linear), 0.0);
        let mid = Tween::step(
            &cell,
            1,
            1.0,
            t0 + Duration::from_millis(50),
            dur,
            Ease::Linear,
        );
        assert!((mid - 0.5).abs() < 1e-3);
        let back = Tween::step(
            &cell,
            1,
            0.0,
            t0 + Duration::from_millis(50),
            dur,
            Ease::Linear,
        );
        assert!((back - 0.5).abs() < 1e-3);
        let later = Tween::step(
            &cell,
            1,
            0.0,
            t0 + Duration::from_millis(100),
            dur,
            Ease::Linear,
        );
        assert!((later - 0.25).abs() < 1e-3);
        assert_eq!(
            Tween::step(
                &cell,
                2,
                1.0,
                t0 + Duration::from_millis(100),
                dur,
                Ease::Linear
            ),
            1.0
        );
    }

    #[test]
    fn disabled_animations_jump_straight_to_target() {
        let _lock = ENABLED_TEST_LOCK.lock().unwrap();
        let _guard = with_enabled(false);
        let t0 = Instant::now();
        let dur = Duration::from_millis(200);
        assert_eq!(progress(t0, t0, dur, Ease::CubicOut), 1.0);
        let cell = Cell::new(None);
        assert_eq!(Tween::step(&cell, 1, 0.0, t0, dur, Ease::Linear), 0.0);
        assert_eq!(Tween::step(&cell, 1, 1.0, t0, dur, Ease::Linear), 1.0);
        assert_eq!(Tween::step(&cell, 1, 0.0, t0, dur, Ease::Linear), 0.0);
    }
}
