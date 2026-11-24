//! The full-screen player: a blurred cover behind a framed cover, the title, a plain seek bar, the transport
//! and a panel with the queue and the lyrics. It opens with a liquid blot that spreads from the full-screen
//! button and folds back into it on close, like the original's reveal.

use std::time::Instant;

use iced::advanced::graphics::geometry::Renderer as _;
use iced::advanced::layout::{self, Layout};
use iced::advanced::renderer::{self, Quad, Renderer as _};
use iced::advanced::widget::{Operation, Tree, tree};
use iced::advanced::{Clipboard, Shell, Widget, overlay};
use iced::widget::canvas::{self, Fill, Path, Stroke};
use iced::widget::{button, column, container, image, responsive, row, scrollable, space, stack, text};
use iced::window::{self, RedrawRequest};
use iced::{
    Alignment, Background, Border, Color, ContentFit, Element, Event, Length, Padding, Point, Rectangle, Renderer, Size,
    Task, Theme, Vector, mouse,
};

use crate::smooth_scroll::smooth;
use crate::icons;
use crate::theme;
use crate::{App, Message, Tab, format_mm_ss};

const OPEN_MS: f32 = 700.0;
const CLOSE_MS: f32 = 420.0;
const BLOT_POINTS: usize = 18;

#[derive(Debug, Clone, Copy)]
pub struct Droplet {
    pub angle: f32,
    pub distance: f32,
    pub size: f32,
    pub phase: f32,
}

/// The reveal in progress: how far it had come when it started and which way it runs.
#[derive(Debug, Clone, Copy)]
pub struct FsAnim {
    pub run_start: Instant,
    pub leg_start: Instant,
    pub opening: bool,
    pub from: f32,
    pub phases: [f32; 6],
    pub drops: [Droplet; 4],
}

fn seeded_blot_params(to_centre: f32) -> ([f32; 6], [Droplet; 4]) {
    let mut s = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0x9E37_79B9_7F4A_7C15, |d| d.as_nanos() as u64)
        | 1;
    let mut next_01 = || -> f32 {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        ((s >> 40) as f32) / ((1u32 << 24) as f32)
    };
    let mut phases = [0.0f32; 6];
    for p in &mut phases {
        *p = next_01() * std::f32::consts::TAU;
    }
    let mut drops = [Droplet {
        angle: 0.0,
        distance: 0.0,
        size: 0.0,
        phase: 0.0,
    }; 4];
    for (i, slot) in drops.iter_mut().enumerate() {
        *slot = Droplet {
            angle: to_centre + (next_01() - 0.5) * 1.6,
            distance: 0.22 + 0.16 * (i as f32) + next_01() * 0.08,
            size: 0.035 + next_01() * 0.03,
            phase: next_01() * std::f32::consts::TAU,
        };
    }
    (phases, drops)
}

impl FsAnim {
    fn progress(&self, now: Instant) -> f32 {
        let ms = now.saturating_duration_since(self.leg_start).as_secs_f32() * 1000.0;
        let span = if self.opening { (1.0 - self.from) * OPEN_MS } else { self.from * CLOSE_MS };
        if span <= 0.0 { 1.0 } else { (ms / span).clamp(0.0, 1.0) }
    }

    /// How much of the window the blot covers at `now`: a pure function of the leg and the clock, so the
    /// widget drawing it and the app reversing it midway agree without a per-frame `update`.
    pub fn share_at(&self, now: Instant) -> f32 {
        let t = self.progress(now);
        if self.opening {
            // ease-out quart: quick initial spill, long soft spread
            self.from + (1.0 - self.from) * (1.0 - (1.0 - t).powi(4))
        } else {
            // ease-in cubic: draws back smoothly into the button
            self.from * (1.0 - t * t * t)
        }
    }

    pub fn finished(&self, now: Instant) -> bool {
        self.progress(now) >= 1.0
    }
}

/// The blot's wobble at a given share: strong while it spills, nearly still once it fills the window.
fn blot_wobble(share: f32) -> f32 {
    0.16 * (1.0 - share).max(0.0).powf(1.3) + 0.025
}

/// From this share on, the blot's narrowest point (ripple amplitudes sum to 1, so `radius * (1 - wobble)`)
/// reaches past the farthest window corner with a 3% margin for the curve dipping between its points.
/// Only then can the player fade in under a quad of the blot's color without uncovered corners.
const COVER_SHARE: f32 = 0.95;
const BLOT_REACH: f32 = 1.12;

/// The player's opacity during the reveal.
fn content_alpha(share: f32) -> f32 {
    ((share - COVER_SHARE) / (1.0 - COVER_SHARE)).clamp(0.0, 1.0)
}

impl App {
    /// Starts the reveal (or its reversal) when the full-screen page was entered or left by the last message.
    pub fn fs_sync(&mut self, was_open: bool) {
        let open = self.active_tab == Tab::NowPlaying;
        if open == was_open {
            return;
        }
        // The bar's popups belong to the window underneath
        self.queue_open = false;
        self.audio_open = false;
        if !self.animations_enabled() {
            self.fs_anim = None;
            return;
        }
        let now = Instant::now();
        if let Some(mut a) = self.fs_anim {
            // Reversal midway: keep run_start, phases, and droplets; flip only the leg.
            if a.opening != open {
                a.from = a.share_at(now);
                a.opening = open;
                a.leg_start = now;
                self.fs_anim = Some(a);
            }
            return;
        }
        let cx = self.window_size.width * 0.5 - (self.window_size.width - 36.0);
        let cy = self.window_size.height * 0.5 - (self.window_size.height - 34.0);
        let (phases, drops) = seeded_blot_params(cy.atan2(cx));
        self.fs_anim = Some(FsAnim {
            run_start: now,
            leg_start: now,
            opening: open,
            from: if was_open { 1.0 } else { 0.0 },
            phases,
            drops,
        });
    }

    /// The reveal widget saw its leg end on a presented frame (a stale leg, reversed since, is ignored).
    pub fn fs_anim_done(&mut self, leg_start: Instant) {
        if self.fs_anim.is_some_and(|a| a.leg_start == leg_start) {
            self.fs_anim = None;
        }
    }

    /// Decodes the current track's cover off the UI thread, once per track (a cover that cannot be read is
    /// not tried again until the track changes): the blurred backdrop, and the framed cover as ready RGBA so
    /// the player's first frame (mid-reveal) uploads it instead of decoding a JPEG on the render thread.
    pub fn fs_refresh_backdrop(&mut self) -> Task<Message> {
        let id = self.current_track.as_ref().map(|t| t.id.clone());
        if self.fs_blur_tried == id {
            return Task::none();
        }
        self.fs_blur_tried = id;
        // The old backdrop stays until the new one is ready, then cross-fades into it (C# BlurredCover).
        let Some(track) = &self.current_track else {
            self.set_fs_blur(None);
            self.fs_cover = None;
            return Task::none();
        };
        let thumb = track.cover_thumb_path.clone();
        let full = track.cover_full_path.clone();
        if thumb.is_none() && full.is_none() {
            self.set_fs_blur(None);
            self.fs_cover = None;
            return Task::none();
        }
        let id = track.id.clone();
        Task::perform(
            async move {
                tokio::task::spawn_blocking(move || {
                    // Never larger than the cached full covers (720 px), whatever file sits behind the path.
                    let rgba = |img: ::image::DynamicImage| {
                        let edge = rustygrade_core::covers::MAX_FULL_COVER_EDGE;
                        let img = if img.width() > edge || img.height() > edge {
                            img.resize(edge, edge, ::image::imageops::FilterType::Triangle)
                        } else {
                            img
                        };
                        let img = img.to_rgba8();
                        let (w, h) = img.dimensions();
                        image::Handle::from_rgba(w, h, img.into_raw())
                    };
                    let thumb_img = thumb.as_deref().and_then(|p| ::image::open(p).ok());
                    let full_img = full.as_deref().and_then(|p| ::image::open(p).ok());
                    let blur = thumb_img.as_ref().or(full_img.as_ref()).map(|img| {
                        let small = img.resize_exact(48, 48, ::image::imageops::FilterType::Triangle).to_rgba8();
                        let blurred = ::image::imageops::blur(&small, 3.0);
                        let (w, h) = blurred.dimensions();
                        image::Handle::from_rgba(w, h, blurred.into_raw())
                    });
                    (blur, full_img.map(rgba))
                })
                .await
                .unwrap_or((None, None))
            },
            move |(blur, cover)| Message::FsBlurReady(id.clone(), blur, cover),
        )
    }
}

/// The blot's edge: an 18-point Catmull-Rom lobe around the full-screen button with 4 leading droplets.
struct Blot {
    share: f32,
    seconds: f32,
    phases: [f32; 6],
    drops: [Droplet; 4],
}

impl Blot {
    fn draw_lobe(
        &self,
        p: &mut canvas::path::Builder,
        c: Point,
        radius: f32,
        wobble: f32,
        phase_offset: f32,
        seed_points: (usize, usize),
    ) {
        let (seed, points) = seed_points;
        let seconds = self.seconds + phase_offset;
        let p0 = self.phases[(seed * 3) % self.phases.len()];
        let p1 = self.phases[(seed * 3 + 1) % self.phases.len()];
        let p2 = self.phases[(seed * 3 + 2) % self.phases.len()];
        let mut pts = [Point::ORIGIN; BLOT_POINTS];
        let n = points.min(BLOT_POINTS);
        for (i, slot) in pts[..n].iter_mut().enumerate() {
            let a = i as f32 * std::f32::consts::TAU / n as f32;
            let ripple = 0.55 * (3.0 * a + seconds * 2.2 + p0).sin()
                + 0.30 * (5.0 * a - seconds * 1.5 + p1).sin()
                + 0.15 * (8.0 * a + seconds * 3.1 + p2).sin();
            let r = radius * (1.0 + wobble * ripple);
            *slot = Point::new(c.x + a.cos() * r, c.y + a.sin() * r);
        }
        p.move_to(pts[0]);
        for i in 0..n {
            let a_pt = pts[(i + n - 1) % n];
            let b = pts[i];
            let cc = pts[(i + 1) % n];
            let d = pts[(i + 2) % n];
            let c1 = Point::new(b.x + (cc.x - a_pt.x) / 6.0, b.y + (cc.y - a_pt.y) / 6.0);
            let c2 = Point::new(cc.x - (d.x - b.x) / 6.0, cc.y - (d.y - b.y) / 6.0);
            p.bezier_curve_to(c1, c2, cc);
        }
        p.close();
    }

    /// The blot's outline in a window of `size`, or `None` before it has grown.
    fn path(&self, size: Size) -> Option<Path> {
        if self.share <= 0.001 {
            return None;
        }
        let origin = Point::new(size.width - 36.0, size.height - 34.0);
        let far = origin.x.max(size.width - origin.x).hypot(origin.y.max(size.height - origin.y));
        let radius = (far * BLOT_REACH * self.share).max(1.0);
        let wobble = blot_wobble(self.share);
        Some(Path::new(|p| {
            self.draw_lobe(p, origin, radius, wobble, 0.0, (0, BLOT_POINTS));
            for d in self.drops {
                let dist = d.distance * far;
                if dist <= 0.0 {
                    continue;
                }
                let lead = (radius / dist - 0.55) / 0.45;
                if lead <= 0.0 || lead >= 1.3 {
                    continue;
                }
                let grow = (lead.min(1.0) * std::f32::consts::FRAC_PI_2).sin();
                let r = d.size * far * grow;
                if r < 1.0 {
                    continue;
                }
                let center = Point::new(origin.x + d.angle.cos() * dist, origin.y + d.angle.sin() * dist);
                self.draw_lobe(p, center, r, 0.12, d.phase, (1, 10));
            }
        }))
    }
}

/// The player plus, while the reveal runs, the blot over the window underneath.
/// Always wraps `workspace` in the same [`Reveal`] as child 0, so entering/leaving full-screen
/// preserves the scroll/focus state of the underlying library/detail tree.
pub fn view<'a>(app: &'a App, workspace: Element<'a, Message>, accent: Color) -> Element<'a, Message> {
    let open = app.active_tab == Tab::NowPlaying;
    let content = (open || app.fs_anim.is_some()).then(|| content(app, accent, 1.0));
    let base = theme::color_from_rgb8([8, 8, 10]);
    let tint = app.detail_tint.filter(|c| *c != crate::pages::NO_COVER_TINT).unwrap_or(app.accent_anim.current_glow);
    let color = Color::from_rgb(
        base.r + (tint.r - base.r) * 0.22,
        base.g + (tint.g - base.g) * 0.22,
        base.b + (tint.b - base.b) * 0.22,
    );
    let mut children = vec![workspace];
    children.extend(content);
    Element::new(Reveal {
        children,
        anim: app.fs_anim,
        open,
        color,
    })
}

/// Runs the reveal inside the widget tree, like the transitions in `crate::motion`: each presented frame
/// (`RedrawRequested`) advances it and asks for the next one at 60 FPS (`crate::motion::FRAME`), so a
/// frame costs a redraw only, with no `update`, `view` or layout of the library and the player.
/// Children: `[workspace, player?]`; the player is present while open or animating.
struct Reveal<'a> {
    children: Vec<Element<'a, Message>>,
    anim: Option<FsAnim>,
    open: bool,
    color: Color,
}

#[derive(Default)]
struct RevealState {
    /// The last presented frame's clock, so a frame draws the moment its `RedrawRequested` reported.
    now: Option<Instant>,
    /// The leg whose end was already reported.
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
    matches!(event, Event::Mouse(mouse::Event::ButtonPressed(_)) | Event::Touch(iced::touch::Event::FingerPressed { .. }))
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

    fn layout(&mut self, tree: &mut Tree, renderer: &Renderer, limits: &layout::Limits) -> layout::Node {
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

    fn operate(&mut self, tree: &mut Tree, layout: Layout<'_>, renderer: &Renderer, operation: &mut dyn Operation) {
        operation.container(None, layout.bounds());
        operation.traverse(&mut |operation| {
            self.children
                .iter_mut()
                .zip(&mut tree.children)
                .zip(layout.children())
                .for_each(|((child, state), layout)| child.as_widget_mut().operate(state, layout, renderer, operation));
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
        if let (Some(child), Some(state), Some(l)) = (player.first_mut(), pl_tree.first_mut(), layouts.next()) {
            // The player takes the pointer only once it shows, and not while it fades out on close.
            let takes_pointer = alpha > 0.5 && self.anim.is_none_or(|a| a.opening);
            if !is_pointer(event) || takes_pointer {
                child.as_widget_mut().update(state, event, l, cursor, renderer, clipboard, shell, viewport);
                if shell.is_event_captured() {
                    return;
                }
            }
        }
        if let (Some(child), Some(state), Some(l)) = (workspace.first_mut(), ws_tree.first_mut(), ws_layout) {
            // Under the player or the blot, the window underneath keeps its frames, keys and releases (its
            // entrance transitions count frames; a drag in progress must see its button come up), but with the
            // cursor levitated, as `opaque` did, so nothing in it hovers; presses are swallowed.
            if has_player {
                if !is_press(event) {
                    child.as_widget_mut().update(state, event, l, cursor.levitate(), renderer, clipboard, shell, viewport);
                }
            } else {
                child.as_widget_mut().update(state, event, l, cursor, renderer, clipboard, shell, viewport);
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
        match (self.children.get(i), tree.children.get(i), layout.children().nth(i)) {
            (Some(child), Some(state), Some(l)) => child.as_widget().mouse_interaction(state, l, cursor, viewport, renderer),
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

        // The window underneath is drawn even when the player covers it: iced drops images that a frame did
        // not draw from its GPU atlas, and its covers would be decoded again on the render thread the moment
        // it shows, a dropped frame in the middle of the close. Under the (opaque) player it goes into a 1 px
        // clip, which keeps its images alive while the GPU fills next to nothing.
        if let (Some(child), Some(state), Some(l)) = (self.children.first(), tree.children.first(), ws_layout) {
            let cursor = if self.children.len() > 1 { mouse::Cursor::Unavailable } else { cursor };
            if alpha <= 0.0 {
                child.as_widget().draw(state, renderer, theme, style, l, cursor, viewport);
            } else {
                let pixel = Rectangle::new(bounds.position(), Size::new(1.0, 1.0));
                renderer.with_layer(pixel, |r| child.as_widget().draw(state, r, theme, style, l, cursor, viewport));
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
                    // No MSAA in this app (see `main`): a thin half-transparent stroke softens the stair-stepped edge.
                    frame.stroke(&path, Stroke::default().with_color(Color { a: 0.5, ..self.color }).with_width(1.25));
                    let geometry = frame.into_geometry();
                    renderer.with_layer(*viewport, |r| {
                        r.with_translation(Vector::new(bounds.x, bounds.y), |r| r.draw_geometry(geometry));
                    });
                }
            }
            return;
        }
        if let (Some(child), Some(state), Some(l)) = (self.children.get(1), tree.children.get(1), pl_layout) {
            renderer.with_layer(*viewport, |r| child.as_widget().draw(state, r, theme, style, l, cursor, viewport));
        }
        if alpha < 1.0 {
            // Fades the player in from the blot's color: exact, since the blot covers the whole window here.
            renderer.with_layer(*viewport, |r| {
                r.fill_quad(Quad { bounds, ..Quad::default() }, Background::Color(Color { a: 1.0 - alpha, ..self.color }));
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
        overlay::from_children(&mut self.children, tree, layout, renderer, viewport, translation)
    }
}

fn fa(c: Color, alpha: f32) -> Color {
    Color { a: c.a * alpha, ..c }
}

fn round_btn<'a>(icon: Element<'a, Message>, msg: Message) -> Element<'a, Message> {
    button(container(icon).width(44).height(44).align_x(Alignment::Center).align_y(Alignment::Center))
        .padding(0)
        .style(|_t, status| button::Style {
            background: Some(Background::Color(if matches!(status, button::Status::Hovered) {
                Color::from_rgba(1.0, 1.0, 1.0, 0.1)
            } else {
                Color::TRANSPARENT
            })),
            border: Border { radius: 22.0.into(), ..Default::default() },
            ..Default::default()
        })
        .on_press(msg)
        .into()
}

fn content<'a>(app: &'a App, accent: Color, alpha: f32) -> Element<'a, Message> {
    // A new track's backdrop cross-fades over the old one (500 ms), on the void when either is missing.
    let fade = app.fs_blur_fade();
    let blur_layer = |h: &iced::widget::image::Handle, a: f32| -> Element<'a, Message> {
        image(h.clone()).content_fit(ContentFit::Cover).width(Length::Fill).height(Length::Fill).opacity(a).into()
    };
    let mut layers: Vec<Element<'a, Message>> = vec![
        container(space::horizontal())
            .width(Length::Fill)
            .height(Length::Fill)
            .style(theme::fill_style(fa(theme::VOID, alpha)))
            .into(),
    ];
    if fade < 1.0
        && let Some(prev) = &app.fs_blur_prev
    {
        let a = if app.fs_blur.is_some() { alpha } else { alpha * (1.0 - fade) };
        layers.push(blur_layer(prev, a));
    }
    if let Some((_, h)) = &app.fs_blur {
        layers.push(blur_layer(h, alpha * fade));
    }
    if app.fs_blur.is_some() || (fade < 1.0 && app.fs_blur_prev.is_some()) {
        let shade = if app.fs_blur.is_some() { 1.0 } else { 1.0 - fade };
        layers.push(
            container(space::horizontal())
                .width(Length::Fill)
                .height(Length::Fill)
                .style(theme::fill_style(Color::from_rgba(0.0, 0.0, 0.0, 0.56 * alpha * shade)))
                .into(),
        );
    }
    let backdrop: Element<'a, Message> = stack(layers).into();

    let track = app.current_track.clone();
    let album_name = track.as_ref().map(|t| t.album.clone()).unwrap_or_default();
    let header = row![
        round_btn(icons::icon("chevrondown", 32.0, fa(theme::TEXT_DIM, alpha)), Message::SelectTab(app.previous_tab)),
        container(
            column![
                text("NOW PLAYING").size(10).color(fa(theme::TEXT_DIM, alpha)),
                text(truncate(&album_name.to_uppercase(), 26)).size(12).font(crate::views::bold()).color(fa(Color::WHITE, alpha)),
            ]
            .spacing(2)
            .align_x(Alignment::Center),
        )
        .width(Length::Fill)
        .align_x(Alignment::Center),
        // Balances the close button so the title stays centered.
        space::horizontal().width(44),
    ]
    .align_y(Alignment::Center)
    .padding(Padding { top: 24.0, right: 48.0, bottom: 0.0, left: 48.0 });

    let body = responsive(move |size| {
        let half = ((size.width.min(1200.0) - 64.0) / 2.0 - 24.0).max(200.0);
        let cover_size = half.min(550.0).min(size.height - 220.0).max(160.0) - 34.0;
        body_view(app, accent, alpha, cover_size.max(120.0), half.min(550.0))
    });

    stack![
        backdrop,
        container(body).width(Length::Fill).height(Length::Fill).padding(Padding { top: 80.0, right: 32.0, bottom: 48.0, left: 32.0 }),
        header,
    ]
    .width(Length::Fill)
    .height(Length::Fill)
    .into()
}

fn truncate(s: &str, max: usize) -> String {
    let mut it = s.chars();
    let head: String = it.by_ref().take(max).collect();
    if it.next().is_some() { format!("{head}…") } else { head }
}

fn corner<'a>(color: Color, top_left: bool) -> Element<'a, Message> {
    let bar = |w: f32, h: f32| container(space::horizontal()).width(w).height(h).style(theme::fill_style(color));
    let l: Element<'a, Message> = if top_left {
        column![bar(24.0, 2.0), bar(2.0, 22.0)].into()
    } else {
        column![container(bar(2.0, 22.0)).width(24).align_x(Alignment::End), container(bar(24.0, 2.0))].into()
    };
    container(l)
        .width(Length::Fill)
        .height(Length::Fill)
        .align_x(if top_left { Alignment::Start } else { Alignment::End })
        .align_y(if top_left { Alignment::Start } else { Alignment::End })
        .into()
}

fn body_view<'a>(app: &'a App, accent: Color, alpha: f32, cover_size: f32, col_w: f32) -> Element<'a, Message> {
    let track = app.current_track.clone();
    let (title, artist) = track
        .as_ref()
        .map(|t| (t.title.clone(), t.artist.clone()))
        .unwrap_or_default();
    // The cover decoded ahead of time for this track (see `fs_refresh_backdrop`), else the shared cache's.
    let cover_handle = track.as_ref().and_then(|t| {
        app.fs_cover
            .as_ref()
            .filter(|(id, _)| *id == t.id)
            .map(|(_, h)| h)
            .or_else(|| app.covers.get_first(&[t.cover_full_path.as_deref(), t.cover_thumb_path.as_deref()]))
    });

    let framed = container(
        stack![
            crate::covers_ui::render_cover(cover_handle, cover_size, 0.0, accent, &title),
            corner(theme::ORANGE, true),
            corner(theme::TEAL, false),
        ]
        .width(cover_size)
        .height(cover_size),
    )
    .padding(16)
    .style(move |_t| container::Style {
        background: Some(Background::Color(Color::from_rgba(0.0, 0.0, 0.0, 0.18 * alpha))),
        border: Border { color: Color::from_rgba(1.0, 1.0, 1.0, 0.05 * alpha), width: 1.0, radius: 12.0.into() },
        ..Default::default()
    });

    // Seek: Canvas seek strip with translucent hover indicator
    let max_dur = app.duration_s.max(1.0);
    let pos = app.seeking_pos.unwrap_or(app.position_s).clamp(0.0, max_dur);
    let seek = crate::views::seek_strip(
        pos,
        max_dur,
        crate::views::SeekStripSpec {
            height: 14.0,
            track_h: 4.0,
            fill: fa(Color::WHITE, alpha),
            track: fa(Color::from_rgba(1.0, 1.0, 1.0, 0.22), alpha),
            knob: Some(fa(Color::WHITE, alpha)),
        },
    );

    let repeating = app.repeat_mode != crate::playback::RepeatMode::Off;
    let mode = |name: &'static str, on: bool, msg: Message| {
        button(icons::icon(name, 24.0, fa(if on { theme::TEAL } else { theme::TEXT_DIM }, alpha)))
            .padding(8)
            .style(theme::bare_button_style())
            .on_press(msg)
    };
    let skip = |name: &'static str, msg: Message| {
        button(icons::icon(name, 36.0, fa(theme::TEXT, alpha)))
            .padding(8)
            .style(theme::bare_button_style())
            .on_press(msg)
    };
    let controls = row![
        mode("shuffle", app.shuffle, Message::ToggleShuffle),
        skip("skipback", Message::Previous),
        crate::motion::hover_scope_slop(crate::motion::HEX_HOVER, 10.0, button(icons::hex_play(app.is_playing, 91.0))
            .padding(0)
            .style(theme::bare_button_style())
            .on_press(Message::TogglePlayPause)),
        skip("skipforward", Message::Next),
        mode("repeat", repeating, Message::CycleRepeat),
    ]
    .spacing(24)
    .align_y(Alignment::Center);

    let head = column![
        text(truncate(&title, 60)).size(44).font(crate::views::bold()).color(fa(Color::WHITE, alpha)),
        text(truncate(&artist, 70)).size(20).color(fa(Color::from_rgb8(0xCF, 0xCC, 0xC4), alpha)),
        space::vertical().height(12),
        seek,
        row![
            text(format_mm_ss(pos)).size(12).color(fa(Color::from_rgba(1.0, 1.0, 1.0, 0.72), alpha)),
            space::horizontal(),
            text(format_mm_ss(app.duration_s)).size(12).color(fa(Color::from_rgba(1.0, 1.0, 1.0, 0.72), alpha)),
        ],
        container(controls).width(Length::Fill).align_x(Alignment::Center),
    ]
    .spacing(8);

    // The cover and the column beside it form one block, centered in the window as the original's
    let right = column![head, panel(app, accent, alpha)].spacing(24).width(Length::Fixed(col_w));
    let left = container(framed).width(Length::Fill).align_x(Alignment::End);

    container(
        row![left, container(right).width(Length::Fill).align_x(Alignment::Start)]
            .spacing(48)
            .width(Length::Fill)
            .align_y(Alignment::Center),
    )
    .width(Length::Fill)
    .height(Length::Fill)
    .align_y(Alignment::Center)
    .into()
}

fn tab_btn<'a>(label: &'static str, on: bool, alpha: f32, msg: Message) -> Element<'a, Message> {
    button(text(label).size(11).font(crate::views::bold()).color(fa(if on { Color::WHITE } else { theme::TEXT_MUTE }, alpha)))
        .padding([2, 0])
        .style(|_t, _s| button::Style { background: None, ..Default::default() })
        .on_press(msg)
        .into()
}

/// The queue / lyrics panel under the controls.
fn panel<'a>(app: &'a App, accent: Color, alpha: f32) -> Element<'a, Message> {
    let upcoming_from = app.queue_index.map_or(0, |i| i + 1).min(app.queue_tracks.len());
    let upcoming_total = app.queue_tracks.len() - upcoming_from;
    // Only the first rows are built: a shuffled library queues every track
    let shown_to = (upcoming_from + crate::popups::QUEUE_ROWS).min(app.queue_tracks.len());
    let upcoming: Vec<(usize, &rustygrade_core::Track)> =
        app.queue_tracks[upcoming_from..shown_to].iter().enumerate().map(|(i, t)| (upcoming_from + i, t)).collect();

    let head = container(
        row![
            tab_btn("NEXT IN QUEUE", !app.fs_lyrics_tab, alpha, Message::SetFsLyricsTab(false)),
            container(tab_btn("LYRICS", app.fs_lyrics_tab, alpha, Message::SetFsLyricsTab(true))).padding(Padding { left: 20.0, ..Padding::ZERO }),
            space::horizontal(),
            text(if app.fs_lyrics_tab {
                app.lyrics.as_ref().map(|d| d.source.to_uppercase()).unwrap_or_default()
            } else {
                format!("{upcoming_total} TRACKS")
            })
            .size(10)
            .color(fa(theme::TEXT_MUTE, alpha)),
        ]
        .align_y(Alignment::Center),
    )
    .padding([12, 16])
    .width(Length::Fill)
    .style(move |_t| container::Style {
        background: Some(Background::Color(Color::from_rgba(0.1, 0.1, 0.11, 0.6 * alpha))),
        border: Border { color: Color::from_rgba(1.0, 1.0, 1.0, 0.05 * alpha), width: 1.0, radius: 0.0.into() },
        ..Default::default()
    });

    let body: Element<'a, Message> = if app.fs_lyrics_tab {
        lyrics_body(app, accent, alpha)
    } else if upcoming.is_empty() {
        container(text("End of queue").size(12).font(iced::Font { style: iced::font::Style::Italic, ..iced::Font::DEFAULT }).color(fa(theme::TEXT_MUTE, alpha)))
            .width(Length::Fill)
            .height(Length::Fill)
            .align_x(Alignment::Center)
            .align_y(Alignment::Center)
            .into()
    } else {
        let rows: Vec<Element<'a, Message>> = upcoming
            .into_iter()
            .enumerate()
            .map(|(rel, (qi, t))| {
                let cover = app.covers.get_first(&[t.cover_thumb_path.as_deref(), t.cover_full_path.as_deref()]);
                row![
                    button(
                        row![
                            container(text(format!("{}", rel + 1)).size(10).color(fa(theme::TEXT_MUTE, alpha))).width(16).align_x(Alignment::Center),
                            crate::covers_ui::render_cover(cover, 32.0, 4.0, accent, &t.title),
                            column![
                                text(truncate(&t.title, 40)).size(12).color(fa(Color::WHITE, alpha)),
                                text(truncate(&t.artist, 44)).size(10).color(fa(Color::from_rgb8(0x6B, 0x69, 0x63), alpha)),
                            ]
                            .spacing(2),
                        ]
                        .spacing(12)
                        .align_y(Alignment::Center),
                    )
                    .width(Length::Fill)
                    .padding([6, 8])
                    .style(theme::nav_item_style(false, accent))
                    .on_press(Message::JumpToQueueIndex(qi)),
                    button(icons::icon("x", 14.0, fa(theme::TEXT_DIM, alpha)))
                        .padding(8)
                        .style(theme::bare_button_style())
                        .on_press(Message::RemoveQueueIndex(qi)),
                ]
                .align_y(Alignment::Center)
                .into()
            })
            .collect();
        let mut list = column(rows).spacing(2).padding(Padding::ZERO.right(10.0));
        if upcoming_total > crate::popups::QUEUE_ROWS {
            list = list.push(
                container(text(format!("+{} MORE", upcoming_total - crate::popups::QUEUE_ROWS)).size(10).color(fa(theme::TEXT_MUTE, alpha)))
                    .width(Length::Fill)
                    .align_x(Alignment::Center)
                    .padding(8),
            );
        }
        smooth(scrollable(list).height(Length::Fill))
    };

    container(column![head, container(body).padding(8).height(Length::Fill)].height(Length::Fill))
        .height(Length::Fixed(258.0))
        .style(move |_t| container::Style {
            background: Some(Background::Color(Color::from_rgba(0.1, 0.1, 0.11, 0.4 * alpha))),
            border: Border { color: Color::from_rgba(1.0, 1.0, 1.0, 0.05 * alpha), width: 1.0, radius: 8.0.into() },
            ..Default::default()
        })
        .into()
}

fn lyrics_body<'a>(app: &'a App, accent: Color, alpha: f32) -> Element<'a, Message> {
    let Some(doc) = &app.lyrics else {
        return container(text("No lyrics for this track").size(12).color(fa(theme::TEXT_MUTE, alpha)))
            .width(Length::Fill)
            .height(Length::Fill)
            .align_x(Alignment::Center)
            .align_y(Alignment::Center)
            .into();
    };
    // Like the original: sung lines dim, coming ones grey, the current one bold white with a 2 px orange
    // marker on its left (no filled block); the time stamp only shows on hover; blank lines are 14 px gaps.
    let active = doc.index_at(app.position_s);
    let _ = accent;
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
                LYRIC_PLAIN
            } else if is_active {
                Color::WHITE
            } else if past {
                theme::TEXT_MUTE
            } else {
                LYRIC_NEXT
            };
            let mut label = text(line.text.clone())
                .size(17)
                .line_height(iced::widget::text::LineHeight::Absolute(24.0.into()))
                .color(fa(col, alpha))
                .width(Length::Fill);
            if is_active && synced {
                label = label.font(crate::views::bold());
            }
            if !synced {
                return container(label).padding(Padding { top: 6.0, right: 16.0, bottom: 6.0, left: 16.0 }).into();
            }
            let stamp_text = format_mm_ss(line.time);
            let stamp = iced::widget::hover(
                text(stamp_text.clone()).size(10).color(Color::TRANSPARENT),
                text(stamp_text).size(10).color(fa(theme::TEXT_MUTE, alpha)),
            );
            button(row![label, stamp].spacing(12).align_y(Alignment::Center))
                .width(Length::Fill)
                .padding(Padding { top: 6.0, right: 16.0, bottom: 6.0, left: 14.0 })
                .style(lyric_style(is_active, alpha))
                .on_press(Message::SeekChanged(line.time))
                .into()
        })
        .collect();
    smooth(
        scrollable(column(lines).padding([16, 8]))
            .id(app.lyrics_scroll_id.clone())
            .on_scroll(|vp| Message::LyricsScrolled(vp.absolute_offset().y, vp.bounds().height, vp.content_bounds().height))
            .direction(scrollable::Direction::Vertical(scrollable::Scrollbar::new().width(4).scroller_width(4).margin(2)))
            .style(thin_scroll_style)
            .height(Length::Fill),
    )
}

const LYRIC_NEXT: Color = Color::from_rgb(0xA3 as f32 / 255.0, 0xA0 as f32 / 255.0, 0x99 as f32 / 255.0);
const LYRIC_PLAIN: Color = Color::from_rgb(0xCF as f32 / 255.0, 0xCC as f32 / 255.0, 0xC4 as f32 / 255.0);
const NOW_ACCENT: Color = Color::from_rgb(1.0, 0x6B as f32 / 255.0, 0x35 as f32 / 255.0);

/// A lyric line: transparent, a faint white wash on hover, and the current line's 2 px orange left marker.
fn lyric_style(current: bool, alpha: f32) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |_t, status| {
        let hovered = matches!(status, button::Status::Hovered | button::Status::Pressed);
        button::Style {
            background: hovered.then(|| Background::Color(Color::from_rgba(1.0, 1.0, 1.0, 0.05 * alpha))),
            border: Border { radius: 4.0.into(), ..Default::default() },
            shadow: if current {
                // iced borders are all-sides; a hard left shadow draws the single 2 px marker.
                iced::Shadow { color: fa(NOW_ACCENT, alpha), offset: iced::Vector::new(-2.0, 0.0), blur_radius: 0.0 }
            } else {
                iced::Shadow::default()
            },
            ..Default::default()
        }
    }
}

/// The original's overlay scrollbar: a thin light thumb, no rail.
fn thin_scroll_style(_t: &Theme, status: scrollable::Status) -> scrollable::Style {
    let hovered = matches!(status, scrollable::Status::Hovered { .. } | scrollable::Status::Dragged { .. });
    let rail = scrollable::Rail {
        background: None,
        border: Border::default(),
        scroller: scrollable::Scroller {
            background: Background::Color(Color::from_rgba(1.0, 1.0, 1.0, if hovered { 0.25 } else { 0.12 })),
            border: Border { radius: 2.0.into(), ..Default::default() },
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

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn anim(opening: bool, from: f32, leg_start: Instant) -> FsAnim {
        FsAnim {
            run_start: leg_start,
            leg_start,
            opening,
            from,
            phases: [0.0; 6],
            drops: [Droplet { angle: 0.0, distance: 0.0, size: 0.0, phase: 0.0 }; 4],
        }
    }

    #[test]
    fn blot_covers_the_window_before_the_player_fades_in() {
        // The narrowest point of the lobe must clear the farthest corner (radius 1.0) with a margin.
        for i in 0..=50 {
            let share = COVER_SHARE + (1.0 - COVER_SHARE) * i as f32 / 50.0;
            let narrowest = BLOT_REACH * share * (1.0 - blot_wobble(share));
            assert!(narrowest >= 1.03, "share {share}: narrowest {narrowest}");
        }
        assert_eq!(content_alpha(COVER_SHARE - 0.01), 0.0);
        assert_eq!(content_alpha(1.0), 1.0);
    }

    #[test]
    fn share_runs_both_legs_and_ends() {
        let t0 = Instant::now();
        let open = anim(true, 0.0, t0);
        assert_eq!(open.share_at(t0), 0.0);
        let mid = open.share_at(t0 + Duration::from_millis(350));
        assert!(mid > 0.5 && mid < 1.0);
        assert!(!open.finished(t0 + Duration::from_millis(600)));
        assert!(open.finished(t0 + Duration::from_millis(700)));
        assert_eq!(open.share_at(t0 + Duration::from_secs(5)), 1.0);

        // A close reversed from 60% runs only 60% of the close time.
        let close = anim(false, 0.6, t0);
        assert!((close.share_at(t0) - 0.6).abs() < 1e-6);
        assert!(close.finished(t0 + Duration::from_millis(253)));
        assert_eq!(close.share_at(t0 + Duration::from_secs(1)), 0.0);
    }
}
