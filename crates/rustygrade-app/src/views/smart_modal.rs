use iced::widget::{
    button, column, container, pick_list, row, scrollable, space, text, text_input,
};
use iced::{Alignment, Background, Border, Color, Element, Length, Size};
use rustygrade_core::SmartPlaylists;

use super::{bold, outline_button_style};
use crate::smooth_scroll::smooth;
use crate::{
    Message, SmartDraft, SmartFieldOption, SmartOpOption, SmartSortOption, dialogs, icons, theme,
};

pub(super) fn smart_editor_modal(window_size: Size, draft: &SmartDraft) -> Element<'_, Message> {
    let field_opts: Vec<SmartFieldOption> = SmartPlaylists::FIELDS
        .iter()
        .map(|f| SmartFieldOption {
            key: f.key,
            label: f.label,
        })
        .collect();
    let sort_opts: Vec<SmartSortOption> = SmartPlaylists::SORTS
        .iter()
        .map(|&(key, label)| SmartSortOption { key, label })
        .collect();
    let selected_sort = sort_opts.iter().find(|s| s.key == draft.sort).copied();
    let can_save = !draft.name.trim().is_empty();

    let field_label_style = |_t: &iced::Theme| text::Style {
        color: Some(theme::TEXT_MUTE),
    };
    let chip = |label: &'static str, on: bool, msg: Message| {
        button(text(label).size(10))
            .padding([5, 10])
            .style(theme::chip_style(on))
            .on_press(msg)
    };

    let mut rule_rows = column![].spacing(8);
    let can_remove = draft.rules.len() > 1;
    for (i, rule) in draft.rules.iter().enumerate() {
        let cur_field = SmartPlaylists::field(&rule.field);
        let op_opts: Vec<SmartOpOption> = cur_field
            .ops
            .iter()
            .map(|&op| SmartOpOption {
                op,
                label: SmartPlaylists::op_label(op),
            })
            .collect();
        let selected_field = field_opts.iter().find(|f| f.key == rule.field).copied();
        let selected_op = op_opts.iter().find(|o| o.op == rule.op).copied();
        let needs_val = SmartPlaylists::needs_value(&rule.op);

        let mut r = row![
            pick_list(field_opts.clone(), selected_field, move |f| {
                Message::SmartRuleFieldChanged(i, f)
            })
            .text_size(11)
            .width(Length::Fixed(146.0))
            .style(theme::pick_style)
            .menu_style(theme::pick_menu_style),
            pick_list(op_opts, selected_op, move |o| Message::SmartRuleOpChanged(
                i, o
            ))
            .text_size(11)
            .width(Length::Fixed(166.0))
            .style(theme::pick_style)
            .menu_style(theme::pick_menu_style),
        ]
        .spacing(8)
        .align_y(Alignment::Center);

        if needs_val {
            let mut val_input = text_input(cur_field.hint, &rule.value)
                .size(12)
                .padding([8, 10])
                .style(theme::input_style)
                .on_input(move |v| Message::SmartRuleValueChanged(i, v))
                .width(Length::Fill);
            if can_save {
                val_input = val_input.on_submit(Message::SaveSmartPlaylist);
            }
            r = r.push(val_input);
        } else {
            r = r.push(space::horizontal().width(Length::Fill));
        }

        if can_remove {
            r = r.push(
                button(icons::icon("x", 14.0, theme::TEXT_DIM))
                    .padding(6)
                    .style(theme::bare_button_style())
                    .on_press(Message::RemoveSmartRule(i)),
            );
        } else {
            r = r.push(space::horizontal().width(26));
        }
        rule_rows = rule_rows.push(r);
    }

    let match_line = row![
        text("MATCH").size(9).style(field_label_style),
        chip(
            "ALL",
            draft.match_all,
            Message::SmartDraftMatchAllChanged(true)
        ),
        chip(
            "ANY",
            !draft.match_all,
            Message::SmartDraftMatchAllChanged(false)
        ),
        text("OF THESE RULES").size(9).style(field_label_style),
    ]
    .spacing(8)
    .align_y(Alignment::Center);

    let add_rule_btn = button(
        row![
            icons::icon("plus", 12.0, theme::TEXT_DIM),
            text("ADD_RULE").size(10).color(theme::TEXT),
        ]
        .spacing(6)
        .align_y(Alignment::Center),
    )
    .padding([6, 12])
    .style(outline_button_style(false))
    .on_press(Message::AddSmartRule);

    let mut limit_input = text_input("NONE", &draft.limit)
        .size(12)
        .padding([8, 10])
        .style(theme::input_style)
        .on_input(Message::SmartDraftLimitChanged)
        .width(Length::Fixed(110.0));
    if can_save {
        limit_input = limit_input.on_submit(Message::SaveSmartPlaylist);
    }

    let order_row = row![
        text("ORDER").size(9).style(field_label_style),
        pick_list(sort_opts, selected_sort, Message::SmartDraftSortChanged)
            .text_size(11)
            .width(Length::Fixed(190.0))
            .style(theme::pick_style)
            .menu_style(theme::pick_menu_style),
        space::horizontal().width(16),
        text("LIMIT").size(9).style(field_label_style),
        limit_input,
    ]
    .spacing(8)
    .align_y(Alignment::Center);

    let preview = if draft.match_count == 1 {
        "1 TRACK MATCHES".to_owned()
    } else {
        format!("{} TRACKS MATCH", draft.match_count)
    };

    let mut name_input = text_input("ENTER_DESIGNATION...", &draft.name)
        .size(13)
        .padding([10, 10])
        .style(theme::input_style)
        .on_input(Message::SmartDraftNameChanged)
        .width(Length::Fill);
    if can_save {
        name_input = name_input.on_submit(Message::SaveSmartPlaylist);
    }

    let body = column![
        name_input,
        match_line,
        container(smooth(scrollable(rule_rows))).max_height(220.0),
        add_rule_btn,
        order_row,
        text(preview).size(11).font(bold()).color(theme::TEAL),
    ]
    .spacing(14)
    .into();

    let save_label = if draft.id > 0 { "SAVE" } else { "CREATE" };
    let footer = row![
        space::horizontal(),
        button(text("CANCEL").size(10).font(bold()).color(theme::TEXT_DIM))
            .padding([8, 16])
            .style(theme::bare_button_style())
            .on_press(Message::CancelSmartPlaylist),
        button(text(save_label).size(10).font(bold()))
            .padding([8, 24])
            .style(move |_t, s| {
                let bg = if !can_save {
                    theme::LINE
                } else if matches!(s, button::Status::Hovered) {
                    Color::WHITE
                } else {
                    theme::GOLD
                };
                button::Style {
                    background: Some(Background::Color(bg)),
                    text_color: if can_save {
                        Color::BLACK
                    } else {
                        theme::TEXT_MUTE
                    },
                    border: Border::default(),
                    ..Default::default()
                }
            })
            .on_press_maybe(can_save.then_some(Message::SaveSmartPlaylist)),
    ]
    .spacing(12)
    .align_y(Alignment::Center)
    .into();

    let heading = if draft.id > 0 {
        "EDIT_SMART_PLAYLIST"
    } else {
        "NEW_SMART_PLAYLIST"
    };
    dialogs::modal_frame_sized(
        dialogs::ModalChrome {
            title: heading,
            dot_color: theme::TEAL,
            corner_color: theme::TEAL,
            width: (window_size.width - 48.0).clamp(360.0, 640.0),
            header_right: None,
        },
        body,
        footer,
        Some(Message::CancelSmartPlaylist),
    )
}
