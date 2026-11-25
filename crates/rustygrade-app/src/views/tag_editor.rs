use iced::widget::{button, column, container, image, row, space, text, text_input};
use iced::{Alignment, Background, Border, Element, Length};

use super::bold;
use crate::{App, Message, TagDraft, TagField, dialogs, icons, pages, theme};

fn tag_input<'a>(
    placeholder: &str,
    value: &str,
    field: TagField,
    editable: bool,
) -> Element<'a, Message> {
    let ti = text_input(placeholder, value)
        .size(12)
        .padding([8, 10])
        .style(theme::input_style)
        .width(Length::Fill);
    if editable {
        ti.on_input(move |s| Message::SetTagField(field, s))
            .on_submit(Message::SaveTrackTags)
            .into()
    } else {
        ti.into()
    }
}

pub(super) fn tag_editor_modal<'a>(app: &'a App, draft: &'a TagDraft) -> Element<'a, Message> {
    let field_label =
        |label: &'static str| text(label).size(9).font(bold()).color(theme::TEXT_MUTE);
    let cover_handle = draft
        .cover_preview
        .as_deref()
        .and_then(|p| app.covers.get_first(&[Some(p)]));
    let cover_box: Element<'a, Message> = if let Some(h) = cover_handle {
        image(h.clone())
            .width(Length::Fixed(166.0))
            .height(Length::Fixed(166.0))
            .content_fit(iced::ContentFit::Cover)
            .into()
    } else {
        icons::icon("disc", 48.0, theme::BORDER_STRONG)
    };
    let framed_cover = container(cover_box)
        .width(168)
        .height(168)
        .align_x(Alignment::Center)
        .align_y(Alignment::Center)
        .style(|_t| container::Style {
            background: Some(Background::Color(theme::LINE_SOFT)),
            border: Border {
                color: theme::BORDER_STRONG,
                width: 1.0,
                radius: 0.0.into(),
            },
            ..Default::default()
        });

    let outline_btn = |label: &'static str, msg: Option<Message>| {
        let b = button(text(label).size(10).font(bold()).color(theme::TEXT))
            .padding([6, 8])
            .width(Length::Fill)
            .style(|_t, st| button::Style {
                background: Some(Background::Color(
                    if matches!(st, button::Status::Hovered) {
                        theme::HOVER
                    } else {
                        theme::PANEL_DEEP
                    },
                )),
                border: Border {
                    color: theme::LINE,
                    width: 1.0,
                    radius: 0.0.into(),
                },
                text_color: theme::TEXT,
                ..Default::default()
            });
        match msg {
            Some(m) => b.on_press(m),
            None => b,
        }
    };
    let idle = !draft.saving && draft.fetching.is_none();
    let editable = !draft.saving;
    let cover_btns = row![
        outline_btn("CHANGE", idle.then_some(Message::TagPickCover)),
        outline_btn("REMOVE", idle.then_some(Message::TagRemoveCover)),
    ]
    .spacing(8);
    let fetch_btn = outline_btn(
        if draft.fetching.is_some() {
            "SEARCHING..."
        } else {
            "FETCH ONLINE"
        },
        idle.then_some(Message::TagFetchCover),
    );

    let cover_note = if draft.fetching.is_some() {
        "ITUNES / MUSICBRAINZ".to_owned()
    } else if let (Some(_), Some(src)) = (&draft.new_cover, draft.cover_source) {
        format!("FOUND ON {src}")
    } else if draft.new_cover.is_some() {
        "NEW COVER SELECTED".to_owned()
    } else if draft.remove_cover {
        "COVER WILL BE REMOVED".to_owned()
    } else {
        String::new()
    };
    let mut left_col = column![framed_cover, cover_btns, fetch_btn]
        .spacing(8)
        .width(168);
    if !cover_note.is_empty() {
        left_col = left_col.push(text(cover_note).size(9).color(theme::TEXT_HINT));
    }

    let mut fields = column![].spacing(4).width(Length::Fill);
    if draft.is_track {
        fields = fields
            .push(field_label("TITLE"))
            .push(tag_input("", &draft.title, TagField::Title, editable))
            .push(space::vertical().height(6))
            .push(field_label("ARTIST"))
            .push(tag_input("", &draft.artist, TagField::Artist, editable))
            .push(space::vertical().height(6));
    }
    fields = fields
        .push(field_label("ALBUM"))
        .push(tag_input("", &draft.album, TagField::Album, editable))
        .push(space::vertical().height(6))
        .push(field_label("ALBUM ARTIST"))
        .push(tag_input(
            "EMPTY: THE TRACK ARTISTS DECIDE",
            &draft.album_artist,
            TagField::AlbumArtist,
            editable,
        ))
        .push(space::vertical().height(6));

    let mut num_row = row![
        column![
            field_label("YEAR"),
            tag_input("", &draft.year, TagField::Year, editable),
        ]
        .spacing(4)
        .width(Length::Fill),
    ]
    .spacing(12)
    .width(Length::Fill);
    if draft.is_track {
        num_row = num_row
            .push(
                column![
                    field_label("TRACK #"),
                    tag_input("", &draft.track, TagField::Track, editable),
                ]
                .spacing(4)
                .width(Length::Fill),
            )
            .push(
                column![
                    field_label("DISC #"),
                    tag_input("", &draft.disc, TagField::Disc, editable),
                ]
                .spacing(4)
                .width(Length::Fill),
            );
    }
    fields = fields.push(num_row).push(space::vertical().height(8)).push(
        text("WRITTEN INTO THE FILES. ONLY CHANGED FIELDS ARE WRITTEN.")
            .size(9)
            .color(theme::TEXT_HINT),
    );
    if let Some(status) = &app.tag_status {
        let (msg, col) = match status {
            Ok(s) => (s.as_str(), theme::TEAL),
            Err(e) => (e.as_str(), theme::ORANGE),
        };
        fields = fields
            .push(space::vertical().height(6))
            .push(text(msg).size(10).color(col));
    }

    let body = row![left_col, fields]
        .spacing(24)
        .width(Length::Fill)
        .into();

    let saving_indicator: Element<'a, Message> = if draft.saving {
        text("WRITING_TAGS...")
            .size(10)
            .font(bold())
            .color(theme::TEAL)
            .into()
    } else {
        space::horizontal().into()
    };

    let cancel_btn = button(text("CANCEL").size(12).color(theme::TEXT_DIM))
        .padding([8, 14])
        .style(theme::bare_button_style())
        .on_press_maybe(editable.then_some(Message::CloseTagEditor));
    let save_btn = button(text("SAVE").size(12).font(bold()))
        .padding([8, 16])
        .style(theme::accent_button_style())
        .on_press_maybe(editable.then_some(Message::SaveTrackTags));
    let footer = row![saving_indicator, space::horizontal(), cancel_btn, save_btn]
        .spacing(12)
        .align_y(Alignment::Center)
        .into();

    let header_right = text(pages::fit(&draft.target_label, 340.0, 10.0).into_owned())
        .size(10)
        .color(theme::TEXT_MUTE)
        .into();

    dialogs::modal_frame_sized(
        dialogs::ModalChrome {
            title: if draft.is_track {
                "EDIT_TAGS"
            } else {
                "EDIT_ALBUM_TAGS"
            },
            dot_color: theme::ORANGE,
            corner_color: theme::ORANGE,
            width: (app.window_size.width - 48.0).clamp(360.0, 620.0),
            header_right: Some(header_right),
        },
        body,
        footer,
        editable.then_some(Message::CloseTagEditor),
    )
}
