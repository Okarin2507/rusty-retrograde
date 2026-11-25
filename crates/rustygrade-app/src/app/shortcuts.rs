use iced::event;
use iced::keyboard::{self, Key, Modifiers};
use iced::widget::{Id, operation};
use iced::{Event, Task, window};

use super::{App, Message, PlaylistView, Tab};
use crate::controller::ControllerCommand;

impl App {
    pub(super) fn handle_shortcut_message(&mut self, msg: Message) -> Task<Message> {
        match msg {
            Message::KeyShortcut {
                key,
                modifiers,
                ignored,
            } => self.handle_key_shortcut(key, modifiers, ignored),
            Message::FocusCheckedShortcut {
                key,
                modifiers,
                text_input_focused,
            } => {
                if text_input_focused
                    || self.tag_draft.is_some()
                    || self.smart_draft.is_some()
                    || self.shutting_down
                {
                    Task::none()
                } else {
                    self.dispatch_shortcut(key, modifiers)
                }
            }
            _ => Task::none(),
        }
    }

    fn has_mounted_text_input(&self) -> bool {
        self.active_tab == Tab::Search
            || (self.active_tab == Tab::Playlists
                && matches!(self.playlist_view, PlaylistView::Custom(_)))
            || self.active_tab == Tab::Settings
            || self.create_playlist_open
            || self.smart_draft.is_some()
            || self.tag_draft.is_some()
            || self.cover_picker.is_some()
    }

    fn focus_search(&mut self) -> Task<Message> {
        self.page_stack.clear();
        if self.active_tab != Tab::Search && self.active_tab != Tab::NowPlaying {
            self.previous_tab = self.active_tab;
        }
        self.active_tab = Tab::Search;
        operation::focus(self.search_input_id.clone())
    }

    fn handle_key_shortcut(
        &mut self,
        key: Key,
        modifiers: Modifiers,
        ignored: bool,
    ) -> Task<Message> {
        if matches!(key.as_ref(), Key::Named(keyboard::key::Named::Escape)) {
            // Exactly one layer closes per Escape press.
            if self.bg_prompt {
                self.bg_prompt = false;
            } else if self.context_menu.is_some() {
                self.context_menu = None;
            } else if self.confirm.is_some() {
                self.confirm = None;
            } else if self.sort_menu_open || self.bank_menu_open || self.add_music_menu_open {
                self.sort_menu_open = false;
                self.bank_menu_open = false;
                self.add_music_menu_open = false;
            } else if self.show_shortcuts_modal {
                self.show_shortcuts_modal = false;
            } else if self.egg_open {
                self.close_egg();
            } else if self.create_playlist_open {
                self.create_playlist_open = false;
            } else if self.cover_picker.is_some() {
                self.close_cover_picker();
            } else if !ignored {
                // A focused text_input consumed this Escape to unfocus itself; stop here.
            } else if self.queue_open || self.audio_open {
                self.queue_open = false;
                self.audio_open = false;
            } else if self.select_mode {
                self.select_mode = false;
                self.selected_albums.clear();
            } else if self.tag_draft.is_some() {
                self.close_tag_editor();
            } else if self.smart_draft.is_some() {
                self.smart_draft = None;
            } else if self.active_tab == Tab::NowPlaying {
                self.active_tab = self.previous_tab;
            } else if self.current_page().is_some() {
                return self.go_back();
            } else if self.active_tab == Tab::Search {
                self.active_tab = self.previous_tab;
            } else if self.mini_window.is_some() {
                return self.restore_main_from_mini();
            }
            return Task::none();
        }

        // Captured keys, modal prompts, context menus, and editors block global shortcuts.
        if !ignored
            || self.confirm.is_some()
            || self.bg_prompt
            || self.context_menu.is_some()
            || self.create_playlist_open
            || self.egg_open
            || self.tag_draft.is_some()
            || self.smart_draft.is_some()
            || self.shutting_down
        {
            return Task::none();
        }

        if !self.has_mounted_text_input() {
            return self.dispatch_shortcut(key, modifiers);
        }

        // Confirm no text field currently owns focus before acting on uncaptured keys.
        iced::advanced::widget::operate(AnyTextInputFocused::default()).map(move |focused| {
            Message::FocusCheckedShortcut {
                key: key.clone(),
                modifiers,
                text_input_focused: focused,
            }
        })
    }

    fn dispatch_shortcut(&mut self, key: Key, modifiers: Modifiers) -> Task<Message> {
        if self.egg_open {
            return Task::none();
        }
        let mini_only = self.main_window.is_none();
        // While the shortcuts card is open only "?" (close) works; the player underneath stays put.
        if self.show_shortcuts_modal && !matches!(key.as_ref(), Key::Character("?")) {
            return Task::none();
        }
        if modifiers.control() {
            match key.as_ref() {
                Key::Character("m") | Key::Character("M") => return self.toggle_mini_player(),
                Key::Character("s") | Key::Character("S") => {
                    let _ = self.cmd_tx.send(ControllerCommand::ToggleShuffle);
                }
                Key::Character("r") | Key::Character("R") => {
                    let _ = self.cmd_tx.send(ControllerCommand::CycleRepeat);
                }
                Key::Character("b") | Key::Character("B") if !mini_only => {
                    self.toggle_sidebar();
                }
                Key::Character("f") | Key::Character("F") if !mini_only => {
                    return self.focus_search();
                }
                _ => {}
            }
            return Task::none();
        }

        match key.as_ref() {
            Key::Named(keyboard::key::Named::Space) if modifiers.is_empty() => {
                let _ = self.cmd_tx.send(ControllerCommand::TogglePlayPause);
            }
            Key::Named(keyboard::key::Named::ArrowRight) if modifiers.shift() => {
                let target = (self.position_s + 10.0).min(self.duration_s.max(0.0));
                let _ = self.cmd_tx.send(ControllerCommand::SeekSeconds(target));
            }
            Key::Named(keyboard::key::Named::ArrowLeft) if modifiers.shift() => {
                let target = (self.position_s - 10.0).max(0.0);
                let _ = self.cmd_tx.send(ControllerCommand::SeekSeconds(target));
            }
            Key::Named(keyboard::key::Named::ArrowRight) if modifiers.is_empty() => {
                let _ = self.cmd_tx.send(ControllerCommand::Next);
            }
            Key::Named(keyboard::key::Named::ArrowLeft) if modifiers.is_empty() => {
                let _ = self.cmd_tx.send(ControllerCommand::Previous);
            }
            Key::Named(keyboard::key::Named::ArrowUp) if modifiers.is_empty() => {
                self.step_volume(1.0);
            }
            Key::Named(keyboard::key::Named::ArrowDown) if modifiers.is_empty() => {
                self.step_volume(-1.0);
            }
            Key::Named(keyboard::key::Named::F5) if !mini_only => {
                let _ = self.cmd_tx.send(ControllerCommand::RescanLibrary);
            }
            Key::Character("/") if !modifiers.alt() && !mini_only => {
                return self.focus_search();
            }
            Key::Character("?") if !mini_only => {
                self.show_shortcuts_modal = !self.show_shortcuts_modal;
            }
            Key::Character("m") | Key::Character("M") if modifiers.is_empty() => {
                let _ = self.cmd_tx.send(ControllerCommand::ToggleMute);
            }
            Key::Character("f") | Key::Character("F") if modifiers.is_empty() && !mini_only => {
                if self.active_tab == Tab::NowPlaying {
                    self.active_tab = self.previous_tab;
                } else {
                    self.previous_tab = self.active_tab;
                    self.active_tab = Tab::NowPlaying;
                }
            }
            Key::Character("k") | Key::Character("K") if modifiers.is_empty() => {
                self.step_volume(1.0);
            }
            Key::Character("j") | Key::Character("J") if modifiers.is_empty() => {
                self.step_volume(-1.0);
            }
            _ => {}
        }
        Task::none()
    }
}

pub(super) fn map_runtime_event(
    event: Event,
    status: event::Status,
    window_id: window::Id,
) -> Option<Message> {
    match event {
        Event::Window(window::Event::Focused) => Some(Message::WindowFocused(window_id, true)),
        Event::Window(window::Event::Unfocused) => Some(Message::WindowFocused(window_id, false)),
        Event::Window(window::Event::Moved(pt)) => Some(Message::WindowMoved(window_id, pt)),
        Event::Window(window::Event::Resized(size)) => {
            Some(Message::WindowResized(window_id, size))
        }
        Event::Window(window::Event::FileHovered(_)) => Some(Message::FileHovered),
        Event::Window(window::Event::FilesHoveredLeft) => Some(Message::FilesHoveredLeft),
        Event::Window(window::Event::FileDropped(path)) => Some(Message::FileDropped(path)),
        Event::Keyboard(keyboard::Event::KeyPressed {
            key,
            modified_key,
            modifiers,
            repeat,
            ..
        }) => {
            let key = match (&key, &modified_key) {
                (Key::Character(_), Key::Character(_))
                    if !modifiers.control() && !modifiers.alt() && !modifiers.logo() =>
                {
                    modified_key
                }
                _ => key,
            };
            // Held keys repeat only for continuous actions: volume and shift-seek.
            let repeatable = match key.as_ref() {
                Key::Named(keyboard::key::Named::ArrowUp | keyboard::key::Named::ArrowDown) => {
                    modifiers.is_empty()
                }
                Key::Named(keyboard::key::Named::ArrowLeft | keyboard::key::Named::ArrowRight) => {
                    modifiers.shift()
                }
                Key::Character(c) => modifiers.is_empty() && matches!(c, "j" | "J" | "k" | "K"),
                _ => false,
            };
            if repeat && !repeatable {
                return None;
            }
            Some(Message::KeyShortcut {
                key,
                modifiers,
                ignored: status == event::Status::Ignored,
            })
        }
        _ => None,
    }
}

#[derive(Default)]
struct AnyTextInputFocused {
    focused: bool,
}

impl iced::advanced::widget::Operation<bool> for AnyTextInputFocused {
    fn traverse(
        &mut self,
        operate: &mut dyn FnMut(&mut dyn iced::advanced::widget::Operation<bool>),
    ) {
        if !self.focused {
            operate(self);
        }
    }

    fn focusable(
        &mut self,
        _id: Option<&Id>,
        _bounds: iced::Rectangle,
        state: &mut dyn iced::advanced::widget::operation::Focusable,
    ) {
        if state.is_focused() {
            self.focused = true;
        }
    }

    fn finish(&self) -> iced::advanced::widget::operation::Outcome<bool> {
        iced::advanced::widget::operation::Outcome::Some(self.focused)
    }
}
