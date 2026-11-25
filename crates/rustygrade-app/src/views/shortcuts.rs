use iced::widget::{column, container, row, space, text};
use iced::{Alignment, Color, Element, Length};

use super::bold;
use crate::{Message, dialogs, theme};

pub(super) fn view_shortcuts_modal<'a>() -> Element<'a, Message> {
    let entries = [
        ("Space", "Play / Pause"),
        ("← / →", "Previous / Next Track"),
        ("Shift + ← / →", "Seek -10s / +10s"),
        ("Ctrl + S", "Toggle Shuffle"),
        ("Ctrl + R", "Cycle Repeat Mode"),
        ("M", "Mute / Unmute"),
        ("K / J or ↑ / ↓", "Volume Up / Down"),
        ("F", "Full-Screen Player"),
        ("/ or Ctrl + F", "Focus Library Search"),
        ("Ctrl + B", "Show / Hide Sidebar"),
        ("F5", "Rescan Library"),
        ("Ctrl + M", "Mini Player"),
        ("?", "This List of Shortcuts"),
        ("Escape", "Close Menus / Go Back"),
    ];
    let mut rows = column![
        column![
            text("KEYBOARD SHORTCUTS")
                .size(10)
                .font(bold())
                .color(theme::ORANGE),
            container(space::horizontal())
                .width(Length::Fill)
                .height(1)
                .style(theme::fill_style(theme::LINE)),
        ]
        .spacing(4),
    ]
    .spacing(8);

    for (key_label, desc) in entries {
        rows = rows.push(
            row![
                container(text(key_label).size(11).color(theme::TEXT_MUTED_LIGHT))
                    .width(Length::Fixed(130.0))
                    .align_x(Alignment::End),
                text(desc).size(12).color(Color::WHITE),
            ]
            .spacing(16)
            .align_y(Alignment::Center),
        );
    }

    let footer = container(
        column![
            text("[PROJECT_LT] RL09 - TERMINAL_V2")
                .size(10)
                .color(theme::TEXT_MUTE),
            text(format!("RUSTYGRADE // VER {}", env!("CARGO_PKG_VERSION")))
                .size(10)
                .color(theme::TEXT_MUTE),
        ]
        .spacing(2)
        .align_x(Alignment::Center),
    )
    .width(Length::Fill)
    .align_x(Alignment::Center)
    .into();

    dialogs::modal_frame(
        "SYSTEM_MANUAL",
        rows.into(),
        footer,
        Some(Message::ToggleShortcutsModal),
    )
}
