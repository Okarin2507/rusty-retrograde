use iced::widget::{button, container, mouse_area, row, space, stack, text};
use iced::{Alignment, Background, Border, Color, Element, Length, Padding, mouse, window};

use crate::{App, Message, icons, theme};

pub(super) fn title_strip(app: &App) -> Element<'_, Message> {
    let win_btn = |icon_name: &str, size: f32, is_close: bool, msg: Message| {
        button(
            container(icons::icon(icon_name, size, theme::TEXT_DIM))
                .width(Length::Fill)
                .height(Length::Fill)
                .align_x(Alignment::Center)
                .align_y(Alignment::Center),
        )
        .width(30)
        .height(24)
        .padding(0)
        .style(move |_t, status| button::Style {
            background: if matches!(status, button::Status::Hovered) {
                Some(Background::Color(if is_close {
                    theme::ORANGE
                } else {
                    theme::RAISED
                }))
            } else {
                None
            },
            text_color: if matches!(status, button::Status::Hovered) {
                Color::WHITE
            } else {
                theme::TEXT_DIM
            },
            border: Border::default(),
            ..Default::default()
        })
        .on_press(msg)
    };

    let sidebar_toggle_icon = if app.settings.sidebar_visible {
        "panelleftclose"
    } else {
        "panelleftopen"
    };
    let sidebar_toggle_color = if app.settings.sidebar_visible {
        theme::TEXT_DIM
    } else {
        theme::TEAL
    };
    container(
        row![
            button(icons::icon(sidebar_toggle_icon, 14.0, sidebar_toggle_color))
                .padding([4, 6])
                .style(theme::bare_button_style())
                .on_press(Message::ToggleSidebar),
            icons::icon("terminal", 12.0, theme::ORANGE),
            text(format!("RUSTYGRADE // VER {}", env!("CARGO_PKG_VERSION")))
                .size(10)
                .color(theme::TEXT_MUTE),
            mouse_area(space::horizontal().width(Length::Fill).height(Length::Fill))
                .interaction(mouse::Interaction::Grab)
                .on_press(Message::DragMainWindow),
            row![
                win_btn("minus", 14.0, false, Message::MinimizeMainWindow),
                win_btn("square", 11.0, false, Message::ToggleMaximizeMainWindow),
                win_btn("x", 14.0, true, Message::RequestCloseMainWindow),
            ]
            .spacing(2)
            .align_y(Alignment::Center),
        ]
        .spacing(8)
        .padding(Padding {
            top: 0.0,
            right: 6.0,
            bottom: 0.0,
            left: 8.0,
        })
        .align_y(Alignment::Center)
        .height(Length::Fill),
    )
    .width(Length::Fill)
    .height(32)
    .into()
}

/// Thin non-blocking 4px edge and corner resize strips for the borderless main window.
pub(super) fn resize_handles<'a>() -> Element<'a, Message> {
    use window::Direction;
    let strip = |w: Length, h: Length, dir: Direction, cursor: mouse::Interaction| {
        mouse_area(space::horizontal().width(w).height(h))
            .interaction(cursor)
            .on_press(Message::ResizeMainWindow(dir))
    };

    // Keep top edge clear of the right-hand min/max/close buttons (`98 px`).
    let top = container(
        row![
            strip(
                Length::Fixed(8.0),
                Length::Fixed(8.0),
                Direction::NorthWest,
                mouse::Interaction::ResizingDiagonallyDown,
            ),
            strip(
                Length::Fill,
                Length::Fixed(4.0),
                Direction::North,
                mouse::Interaction::ResizingVertically,
            ),
        ]
        .width(Length::Fill)
        .align_y(Alignment::Start),
    )
    .padding(Padding::ZERO.right(98.0))
    .width(Length::Fill)
    .align_y(Alignment::Start);

    let sides = row![
        container(strip(
            Length::Fixed(4.0),
            Length::Fill,
            Direction::West,
            mouse::Interaction::ResizingHorizontally,
        ))
        .padding(Padding::ZERO.top(8.0))
        .height(Length::Fill),
        space::horizontal().width(Length::Fill),
        container(strip(
            Length::Fixed(4.0),
            Length::Fill,
            Direction::East,
            mouse::Interaction::ResizingHorizontally,
        ))
        .padding(Padding::ZERO.top(32.0))
        .height(Length::Fill),
    ]
    .width(Length::Fill)
    .height(Length::Fill);

    let bottom = row![
        strip(
            Length::Fixed(10.0),
            Length::Fixed(10.0),
            Direction::SouthWest,
            mouse::Interaction::ResizingDiagonallyUp,
        ),
        strip(
            Length::Fill,
            Length::Fixed(4.0),
            Direction::South,
            mouse::Interaction::ResizingVertically,
        ),
        strip(
            Length::Fixed(10.0),
            Length::Fixed(10.0),
            Direction::SouthEast,
            mouse::Interaction::ResizingDiagonallyDown,
        ),
    ]
    .width(Length::Fill)
    .align_y(Alignment::End);

    stack![
        top,
        sides,
        container(bottom)
            .width(Length::Fill)
            .height(Length::Fill)
            .align_y(Alignment::End),
    ]
    .width(Length::Fill)
    .height(Length::Fill)
    .into()
}
