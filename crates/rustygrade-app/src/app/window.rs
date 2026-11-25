use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use iced::event;
use iced::{Event, Point, Size, Task, window};
use rustygrade_linux::RenderTier;

use super::{
    App, MAIN_WINDOW_ID, Message, PRESENT_SEQ, WAKE_ON_PRESENT, batch_tasks, set_main_window_id,
};
use crate::controller::{ControllerCommand, PlaybackTickMode};
use crate::motion;

impl App {
    pub(super) fn handle_window_message(&mut self, msg: Message) -> Task<Message> {
        match msg {
            Message::WindowOpened(id) => {
                if self.main_window != Some(id) {
                    // Stale open from a fast mini/main toggle: the app no longer expects it.
                    return window::close(id);
                }
                self.window_visible = true;
                self.main_window_focused = true;
                self.main_window_minimized = false;
                if self.shot.is_some() && !self.shot_started {
                    self.shot_started = true;
                    return self.shot_begin();
                }
                Task::none()
            }
            Message::MiniWindowOpened(id) => {
                if self.mini_window != Some(id) {
                    return window::close(id);
                }
                Task::none()
            }
            Message::WindowCloseRequested(id) => {
                if self.mini_window == Some(id) {
                    if self.settings.minimize_to_tray == Some(true) && self.tray_available {
                        self.mini_window = None;
                        self.window_visible = false;
                        self.push_settings();
                        window::close(id)
                    } else {
                        // WM close on the mini-player means "back to the full window", not quit.
                        self.restore_main_from_mini()
                    }
                } else {
                    self.request_close_main()
                }
            }
            Message::BackgroundRemember(on) => {
                self.bg_remember = on;
                Task::none()
            }
            Message::BackgroundChoice(minimize) => {
                self.bg_prompt = false;
                if self.bg_remember {
                    self.settings.minimize_to_tray = Some(minimize);
                    self.push_settings();
                }
                if minimize && self.tray_available {
                    self.hide_main_window()
                } else {
                    self.begin_shutdown()
                }
            }
            Message::WindowResized(id, size) => {
                if self.main_window == Some(id) {
                    self.window_size = size;
                    self.bank_viewport_h = None;
                }
                Task::none()
            }
            Message::WindowClosed(id) => {
                if self.main_window == Some(id) {
                    self.main_window = None;
                    set_main_window_id(None);
                    self.window_visible = false;
                    self.clear_transient_window_flags();
                }
                if self.mini_window == Some(id) {
                    self.mini_window = None;
                    self.push_settings();
                }
                Task::none()
            }
            Message::WindowFocused(id, focused) => {
                if self.main_window == Some(id) {
                    self.main_window_focused = focused;
                    if focused {
                        self.main_window_minimized = false;
                    } else {
                        self.watchdog
                            .on_unfocus(PRESENT_SEQ.load(Ordering::SeqCst), Instant::now());
                        return window::is_minimized(id)
                            .map(move |m| Message::MainWindowMinimized(id, m.unwrap_or(false)));
                    }
                }
                Task::none()
            }
            Message::WindowPresented(id) => {
                if self.main_window == Some(id) {
                    tracing::debug!(?id, "main window presented after compositor suspend");
                    self.watchdog
                        .on_presented(PRESENT_SEQ.load(Ordering::SeqCst), Instant::now());
                } else if self.watchdog.is_suspended() {
                    let seq = PRESENT_SEQ.load(Ordering::SeqCst);
                    WAKE_ON_PRESENT.store(true, Ordering::SeqCst);
                    let after = PRESENT_SEQ.load(Ordering::SeqCst);
                    if after != seq {
                        WAKE_ON_PRESENT.store(false, Ordering::SeqCst);
                        self.watchdog.on_presented(after, Instant::now());
                    }
                }
                Task::none()
            }
            Message::MainWindowMinimized(id, minimized) => {
                if self.main_window == Some(id) && !self.main_window_focused {
                    self.main_window_minimized = minimized;
                    if minimized {
                        self.last_frame_at = None;
                    }
                }
                Task::none()
            }
            Message::WindowMoved(id, pt) => {
                if self.mini_window == Some(id) {
                    let x = Some(pt.x.round() as i32);
                    let y = Some(pt.y.round() as i32);
                    if self.settings.mini_x != x || self.settings.mini_y != y {
                        self.settings.mini_x = x;
                        self.settings.mini_y = y;
                        self.mini_pos_dirty = true;
                    }
                }
                Task::none()
            }
            Message::FileHovered => {
                self.hovered_files = self.hovered_files.saturating_add(1);
                Task::none()
            }
            Message::FilesHoveredLeft => {
                self.hovered_files = 0;
                Task::none()
            }
            Message::FileDropped(path) => {
                self.hovered_files = 0;
                let first = self.dropped_files.is_empty();
                self.dropped_files.push(path);
                if first {
                    Task::perform(tokio::time::sleep(Duration::from_millis(150)), |_| {
                        Message::FlushDrops
                    })
                } else {
                    Task::none()
                }
            }
            Message::FlushDrops => {
                let paths = std::mem::take(&mut self.dropped_files);
                if !paths.is_empty() {
                    let _ = self.cmd_tx.send(ControllerCommand::ImportPaths(paths));
                }
                Task::none()
            }
            Message::ToggleMiniPlayer => self.toggle_mini_player(),
            Message::DragMiniWindow => {
                if let Some(mid) = self.mini_window {
                    window::drag(mid)
                } else {
                    Task::none()
                }
            }
            Message::CloseMiniPlayerToMain => self.restore_main_from_mini(),
            Message::MiniHover(hovered) => {
                self.mini_hovered = hovered;
                if !hovered {
                    self.mini_marquee_phase = 0.0;
                }
                Task::none()
            }
            Message::DragMainWindow => {
                if let Some(id) = self.main_window {
                    let now = Instant::now();
                    if self
                        .last_titlebar_press
                        .is_some_and(|t| now.duration_since(t) <= Duration::from_millis(350))
                    {
                        self.last_titlebar_press = None;
                        return window::toggle_maximize(id);
                    }
                    self.last_titlebar_press = Some(now);
                    return window::drag(id);
                }
                Task::none()
            }
            Message::MinimizeMainWindow => {
                if let Some(id) = self.main_window {
                    window::minimize(id, true)
                } else {
                    Task::none()
                }
            }
            Message::ToggleMaximizeMainWindow => {
                if let Some(id) = self.main_window {
                    window::toggle_maximize(id)
                } else {
                    Task::none()
                }
            }
            Message::RequestCloseMainWindow => self.request_close_main(),
            Message::ResizeMainWindow(dir) => {
                if let Some(id) = self.main_window {
                    window::drag_resize(id, dir)
                } else {
                    Task::none()
                }
            }
            Message::ForceExit => iced::exit(),
            _ => Task::none(),
        }
    }

    pub(super) fn clear_transient_window_flags(&mut self) {
        self.hovered_files = 0;
        self.bg_prompt = false;
        self.confirm = None;
        self.context_menu = None;
        self.sort_menu_open = false;
        self.bank_menu_open = false;
        self.add_music_menu_open = false;
        self.queue_open = false;
        self.audio_open = false;
    }

    pub(super) fn is_unfocused_open_main(&self) -> bool {
        self.mini_window.is_none()
            && self.window_visible
            && self.main_window.is_some()
            && !self.main_window_minimized
            && !self.main_window_focused
    }

    pub(super) fn sync_playback_tick_mode(&mut self) {
        if !self.is_unfocused_open_main() {
            self.watchdog.reset();
            if WAKE_ON_PRESENT.load(Ordering::Relaxed) {
                WAKE_ON_PRESENT.store(false, Ordering::SeqCst);
            }
        }
        let next = resolve_playback_tick_mode(
            self.mini_window.is_some(),
            self.window_visible,
            self.main_window.is_some(),
            self.main_window_minimized,
            self.main_window_focused,
            self.watchdog.is_suspended(),
        );
        if next != self.playback_tick_mode {
            tracing::debug!(
                prev = ?self.playback_tick_mode,
                ?next,
                suspended = self.watchdog.is_suspended(),
                "playback tick mode changed"
            );
            self.playback_tick_mode = next;
            let _ = self
                .cmd_tx
                .send(ControllerCommand::SetPlaybackTickMode(next));
        }
    }

    pub(super) fn arm_watchdog_on_tick(&mut self) {
        let seq = PRESENT_SEQ.load(Ordering::SeqCst);
        if let super::WatchdogAction::ArmAndSuspend { expected_seq } =
            self.watchdog.on_tick(seq, Instant::now())
        {
            // Dekker store-load check with `map_present_event` so a frame arriving right
            // as we arm `WAKE_ON_PRESENT` can never cause a lost wakeup.
            WAKE_ON_PRESENT.store(true, Ordering::SeqCst);
            let after = PRESENT_SEQ.load(Ordering::SeqCst);
            if !self.watchdog.confirm_suspend(after, expected_seq) {
                WAKE_ON_PRESENT.store(false, Ordering::SeqCst);
            }
        }
    }

    pub(super) fn request_close_main(&mut self) -> Task<Message> {
        match self.settings.minimize_to_tray {
            None if self.tray_available => {
                self.bg_remember = false;
                self.bg_prompt = true;
                Task::none()
            }
            Some(true) if self.tray_available => self.hide_main_window(),
            _ => self.begin_shutdown(),
        }
    }

    /// Closes the main window to the tray (playback goes on).
    pub(super) fn hide_main_window(&mut self) -> Task<Message> {
        self.window_visible = false;
        self.clear_transient_window_flags();
        self.last_frame_at = None;
        match self.main_window.take() {
            Some(id) => {
                set_main_window_id(None);
                window::close(id)
            }
            None => Task::none(),
        }
    }

    pub(super) fn open_or_focus_main_window(&mut self) -> Task<Message> {
        if let Some(id) = self.main_window {
            self.window_visible = true;
            self.main_window_minimized = false;
            Task::batch([window::minimize(id, false), window::gain_focus(id)])
        } else {
            // Virtualized scroll state does not survive closing and reopening the main window.
            self.lib_scroll_y = 0.0;
            self.lib_scroll_raw = 0.0;
            self.detail_scroll_y = 0.0;
            self.bank_scroll_y = 0.0;
            self.lib_restore = None;
            self.window_size = Size::new(1200.0, 800.0);
            self.bank_viewport_h = None;
            let (id, open_task) = open_main_window();
            self.main_window = Some(id);
            set_main_window_id(Some(id));
            self.window_visible = true;
            self.main_window_minimized = false;
            open_task.map(Message::WindowOpened)
        }
    }

    pub(super) fn toggle_mini_player(&mut self) -> Task<Message> {
        if self.mini_window.is_some() {
            return self.restore_main_from_mini();
        }
        self.mini_hovered = false;
        self.mini_marquee_phase = 0.0;
        let (mid, open_mini) = open_mini_window(self.settings.mini_x, self.settings.mini_y);
        self.mini_window = Some(mid);
        let mut tasks = vec![open_mini.map(Message::MiniWindowOpened)];
        let hide_task = self.hide_main_window();
        tasks.push(hide_task);
        batch_tasks(tasks)
    }

    pub(super) fn restore_main_from_mini(&mut self) -> Task<Message> {
        self.mini_hovered = false;
        self.mini_marquee_phase = 0.0;
        self.push_settings();
        let mut tasks = Vec::new();
        if let Some(mid) = self.mini_window.take() {
            tasks.push(window::close(mid));
        }
        tasks.push(self.open_or_focus_main_window());
        batch_tasks(tasks)
    }

    pub(super) fn begin_shutdown(&mut self) -> Task<Message> {
        if self.shutting_down {
            return Task::none();
        }
        self.shutting_down = true;
        if self.mini_pos_dirty || self.discogs_token_dirty {
            self.push_settings();
        }
        let _ = self.cmd_tx.send(ControllerCommand::Shutdown);
        Task::perform(tokio::time::sleep(Duration::from_millis(4000)), |_| {
            Message::ForceExit
        })
    }
}

/// Receives raw `RedrawRequested` events (filtered out by `event::listen_with` in iced 0.14).
/// Emits `WindowPresented` once when `WAKE_ON_PRESENT` was armed during compositor suspension.
pub(super) fn map_present_event(
    event: Event,
    _status: event::Status,
    window_id: window::Id,
) -> Option<Message> {
    let Event::Window(window::Event::RedrawRequested(_)) = event else {
        return None;
    };
    let is_main = MAIN_WINDOW_ID
        .try_lock()
        .ok()
        .is_some_and(|g| *g == Some(window_id));
    if !is_main {
        return None;
    }
    PRESENT_SEQ.fetch_add(1, Ordering::SeqCst);
    motion::frame_presented();
    if WAKE_ON_PRESENT.load(Ordering::SeqCst) && WAKE_ON_PRESENT.swap(false, Ordering::SeqCst) {
        Some(Message::WindowPresented(window_id))
    } else {
        None
    }
}

pub(super) fn open_main_window() -> (window::Id, Task<window::Id>) {
    window::open(window::Settings {
        size: Size::new(1200.0, 800.0),
        min_size: Some(Size::new(900.0, 600.0)),
        decorations: false,
        resizable: true,
        exit_on_close_request: false,
        platform_specific: window::settings::PlatformSpecific {
            application_id: "rustygrade".to_owned(),
            ..Default::default()
        },
        ..Default::default()
    })
}

fn open_mini_window(mini_x: Option<i32>, mini_y: Option<i32>) -> (window::Id, Task<window::Id>) {
    let position = match (mini_x, mini_y) {
        (Some(x), Some(y)) if (-4000..=16000).contains(&x) && (-4000..=16000).contains(&y) => {
            window::Position::Specific(Point::new(x as f32, y as f32))
        }
        _ => window::Position::Default,
    };
    window::open(window::Settings {
        size: Size::new(400.0, 122.0),
        position,
        resizable: false,
        decorations: false,
        level: window::Level::AlwaysOnTop,
        exit_on_close_request: false,
        platform_specific: window::settings::PlatformSpecific {
            application_id: "rustygrade-mini".to_owned(),
            ..Default::default()
        },
        ..Default::default()
    })
}

/// Canonicalizes a local CLI path so a secondary instance launched from another
/// working directory still forwards an absolute path; `file://` URIs pass through.
pub(super) fn normalize_open_arg(arg: &str) -> String {
    if arg.len() >= 7 && arg[..7].eq_ignore_ascii_case("file://") {
        return arg.to_owned();
    }
    std::fs::canonicalize(arg)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| arg.to_owned())
}

pub(super) fn animations_active(setting_enabled: bool, tier: RenderTier) -> bool {
    setting_enabled && tier.enable_animations
}

fn resolve_playback_tick_mode(
    has_mini_window: bool,
    window_visible: bool,
    has_main_window: bool,
    main_window_minimized: bool,
    main_window_focused: bool,
    compositor_suspended: bool,
) -> PlaybackTickMode {
    if has_mini_window {
        PlaybackTickMode::Active
    } else if !window_visible || !has_main_window || main_window_minimized {
        PlaybackTickMode::Paused
    } else if main_window_focused {
        PlaybackTickMode::Active
    } else if compositor_suspended {
        PlaybackTickMode::Paused
    } else {
        PlaybackTickMode::Unfocused
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::super::{PresentWatchdog, WatchdogAction};
    use super::{PlaybackTickMode, resolve_playback_tick_mode};

    #[test]
    fn resolves_playback_tick_mode_for_all_window_states() {
        assert_eq!(
            resolve_playback_tick_mode(false, true, true, false, true, false),
            PlaybackTickMode::Active
        );
        assert_eq!(
            resolve_playback_tick_mode(true, false, false, true, false, true),
            PlaybackTickMode::Active
        );
        assert_eq!(
            resolve_playback_tick_mode(false, true, true, false, false, false),
            PlaybackTickMode::Unfocused
        );
        assert_eq!(
            resolve_playback_tick_mode(false, true, true, false, false, true),
            PlaybackTickMode::Paused
        );
        assert_eq!(
            resolve_playback_tick_mode(false, true, true, true, false, false),
            PlaybackTickMode::Paused
        );
        assert_eq!(
            resolve_playback_tick_mode(false, false, false, false, false, false),
            PlaybackTickMode::Paused
        );
    }

    #[test]
    fn watchdog_tracks_present_streak_and_suspend_races() {
        let t0 = Instant::now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        let mut wd = PresentWatchdog::default();
        wd.on_unfocus(10, t0);

        // Bursts under 500 ms and advancing frame sequences do not suspend.
        assert_eq!(wd.on_tick(10, at(100)), WatchdogAction::None);
        assert_eq!(wd.on_tick(10, at(499)), WatchdogAction::None);
        assert_eq!(wd.on_tick(11, at(1000)), WatchdogAction::None);
        assert!(!wd.is_suspended());

        // Two stale ticks >= 500 ms apart suspend when no frame raced the arm.
        assert_eq!(wd.on_tick(11, at(1600)), WatchdogAction::None);
        assert_eq!(
            wd.on_tick(11, at(2200)),
            WatchdogAction::ArmAndSuspend { expected_seq: 11 }
        );
        assert!(wd.confirm_suspend(11, 11));
        assert!(wd.is_suspended());
        assert_eq!(wd.on_tick(11, at(5000)), WatchdogAction::None);

        // Presenting wakes the watchdog and resets check baseline.
        wd.on_presented(12, at(5100));
        assert!(!wd.is_suspended());

        // A frame racing confirm_suspend cancels suspension and advances seq cleanly.
        assert_eq!(wd.on_tick(12, at(5700)), WatchdogAction::None);
        assert_eq!(
            wd.on_tick(12, at(6300)),
            WatchdogAction::ArmAndSuspend { expected_seq: 12 }
        );
        assert!(!wd.confirm_suspend(13, 12));
        assert!(!wd.is_suspended());
        assert_eq!(wd.on_tick(13, at(6900)), WatchdogAction::None);
        assert!(!wd.is_suspended());
    }
}
