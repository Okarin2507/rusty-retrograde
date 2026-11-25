use std::borrow::Cow;
use std::time::Duration;

use iced::widget::text::Wrapping;
use iced::widget::{Id, button, column, container, grid, row, scrollable, space, stack, text};
use iced::{Alignment, Background, Border, Color, Element, Length, Padding, Vector};

use crate::covers_ui::render_cover;
use crate::smooth_scroll::smooth;
use crate::views::bold;
use crate::{App, Message, icons, library_view::artist_key, motion, theme};

/// Estimated character advance in pixels at `size`, counting CJK glyphs wide.
pub fn char_width_px(c: char, size: f32) -> f32 {
    if (c as u32) >= 0x2E80 {
        size
    } else if c.is_uppercase() || c.is_ascii_digit() {
        size * 0.68
    } else if matches!(c, ' ' | 'i' | 'l' | 'j' | '.' | ',') {
        size * 0.32
    } else {
        size * 0.58
    }
}

/// Width-aware shortening: borrows `s` when it fits, or appends an ellipsis within `max_px`.
pub fn fit<'a>(s: &'a str, max_px: f32, size: f32) -> Cow<'a, str> {
    let total: f32 = s.chars().map(|c| char_width_px(c, size)).sum();
    if total <= max_px {
        return Cow::Borrowed(s);
    }
    let mut out = String::new();
    let mut acc = 0.0;
    let ell = size * 0.6;
    for c in s.chars() {
        let w = char_width_px(c, size);
        if acc + w + ell > max_px {
            break;
        }
        acc += w;
        out.push(c);
    }
    out.push('…');
    Cow::Owned(out)
}

fn badge<'a>(label: &'a str) -> Element<'a, Message> {
    container(text(label).size(8).color(theme::TEXT_DIM))
        .padding(Padding {
            top: 3.0,
            right: 4.0,
            bottom: 1.0,
            left: 4.0,
        })
        .style(theme::fill_style(theme::LINE_SOFT))
        .into()
}

/// Height of a card's padding and its two text lines under the cover.
const CARD_TEXT_H: f32 = 70.0;

#[derive(Debug, Clone, Copy)]
pub enum GridArea {
    Library,
    Search,
    Detail,
}

impl App {
    /// Returns `(columns, card_w, card_h)` for a card grid in `area`.
    pub fn grid_layout(&self, area: GridArea) -> (usize, f32, f32) {
        let w = self.window_size.width;
        let expanded = self.sidebar_width >= 150.0;
        let sidebar_footprint = self.sidebar_width + 12.0;
        let content = w - 24.0 - sidebar_footprint;
        let mut columns = if content >= 1000.0 {
            5
        } else if content >= 744.0 {
            4
        } else if content >= 488.0 {
            3
        } else {
            2
        };
        if !expanded {
            columns = if content >= 1512.0 {
                7
            } else if content >= 1256.0 {
                6
            } else {
                columns
            };
        }
        let around = match area {
            GridArea::Library | GridArea::Search => 32.0 + 14.0 + 10.0,
            GridArea::Detail => 64.0 + 10.0,
        };
        let grid_w = w - 24.0 - sidebar_footprint - around;
        let card_w = ((grid_w - 12.0 * (columns as f32 - 1.0)) / columns as f32).max(80.0);
        (columns, card_w, (card_w - 24.0 + CARD_TEXT_H).floor())
    }
}

pub struct CardMotion {
    pub key: u64,
    pub play: Option<Message>,
}

fn kind_static(k: &str) -> &'static str {
    match k {
        "ALB" => "ALB",
        "ART" => "ART",
        "TRK" => "TRK",
        "SMRT" => "SMRT",
        _ => "PL",
    }
}

#[allow(clippy::too_many_arguments)]
pub fn media_card<'a>(
    kind: &'a str,
    title: String,
    caption: String,
    cover: Option<&'a iced::widget::image::Handle>,
    mono: &str,
    card_w: f32,
    card_h: f32,
    accent: Color,
    msg: Message,
) -> Element<'a, Message> {
    media_card_opts(
        kind, title, caption, cover, mono, card_w, card_h, accent, msg, None, None,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn media_card_opts<'a>(
    kind: &'a str,
    title: String,
    caption: String,
    cover: Option<&'a iced::widget::image::Handle>,
    mono: &str,
    card_w: f32,
    card_h: f32,
    accent: Color,
    msg: Message,
    selectable_state: Option<bool>,
    motion_card: Option<CardMotion>,
) -> Element<'a, Message> {
    let selected = selectable_state.unwrap_or(false);
    let side = (card_w - 24.0).min(card_h - CARD_TEXT_H).max(60.0);
    let name = fit(&title, side - 40.0, 13.0).into_owned();
    let cap = fit(&caption, side, 10.0).into_owned();

    let cover_el: Element<'_, Message> = match &motion_card {
        Some(m) => {
            let pic = match cover {
                Some(h) => motion::zoom_cover(m.key, h.clone(), side),
                None => render_cover(None, side, 0.0, accent, mono),
            };
            let mut layers: Vec<Element<'_, Message>> = vec![pic];
            if let Some(play) = &m.play {
                layers.push(
                    container(motion::card_play(m.key, play.clone(), 8.0))
                        .width(Length::Fixed(side))
                        .height(Length::Fixed(side))
                        .padding(8)
                        .align_x(Alignment::End)
                        .align_y(Alignment::End)
                        .into(),
                );
            }
            stack(layers).into()
        }
        None => render_cover(cover, side, 0.0, accent, mono),
    };
    let card_col = column![
        cover_el,
        row![
            badge(kind_static(kind)),
            text(name)
                .size(13)
                .font(bold())
                .color(theme::TEXT)
                .wrapping(Wrapping::None),
        ]
        .spacing(8)
        .align_y(Alignment::Center),
        text(cap)
            .size(10)
            .color(theme::TEXT_MUTE)
            .wrapping(Wrapping::None),
    ]
    .spacing(8)
    .padding(12);

    let corner_tl = motion::corner_tick(
        if selected { theme::ORANGE } else { theme::LINE },
        if selected { theme::ORANGE } else { theme::GOLD },
    );

    let mut layers: Vec<Element<'_, Message>> = vec![card_col.into(), corner_tl];
    if let Some(is_sel) = selectable_state {
        let check = container(if is_sel {
            icons::icon("checksquare", 18.0, theme::ORANGE)
        } else {
            motion::hover_icon(
                icons::handle("square"),
                18.0,
                theme::TEXT_MUTE,
                theme::TEXT_DIM,
                0,
                Vector::ZERO,
            )
        })
        .width(Length::Fill)
        .align_x(Alignment::End)
        .padding(8);
        layers.push(check.into());
    }
    let inner = stack(layers).width(Length::Fill).height(Length::Fill);

    motion::hover_scope(
        Duration::from_millis(500),
        button(inner)
            .style(theme::album_card_style(selected))
            .width(Length::Fill)
            .height(Length::Fixed(card_h))
            .padding(0)
            .on_press(msg),
    )
}

pub fn card_grid<'a>(cards: Vec<Element<'a, Message>>, columns: usize) -> Element<'a, Message> {
    grid(cards)
        .spacing(12)
        .columns(columns.max(1))
        .height(Length::Shrink)
        .into()
}

/// Computes `[start..end)` visible row indices for a uniformly spaced vertical list or grid.
pub fn visible_row_range(
    scroll_y: f32,
    viewport_h: f32,
    row_h: f32,
    total_rows: usize,
    overscan: usize,
) -> std::ops::Range<usize> {
    if total_rows == 0 || row_h <= 0.0 {
        return 0..0;
    }
    let y0 = scroll_y.max(0.0);
    let y1 = y0 + viewport_h.max(0.0);
    let first = ((y0 / row_h).floor() as usize).saturating_sub(overscan);
    let last = ((y1 / row_h).ceil() as usize)
        .saturating_add(overscan)
        .min(total_rows);
    first.min(last)..last
}

fn grid_window(
    scroll_y: f32,
    viewport_h: f32,
    card_h: f32,
    columns: usize,
    total: usize,
) -> (std::ops::Range<usize>, f32, f32) {
    if total == 0 {
        return (0..0, 0.0, 0.0);
    }
    let cols = columns.max(1);
    let row_stride = card_h + 12.0;
    let total_rows = total.div_ceil(cols);
    let total_h = (total_rows as f32 * row_stride - 12.0).max(0.0);
    let clamped_scroll = scroll_y.clamp(0.0, (total_h - viewport_h).max(0.0));
    let range = visible_row_range(clamped_scroll, viewport_h, row_stride, total_rows, 2);
    let start_idx = range.start * cols;
    let end_idx = (range.end * cols).min(total);
    let top_h = range.start as f32 * row_stride;
    let bot_h = total_rows.saturating_sub(range.end) as f32 * row_stride;
    (start_idx..end_idx, top_h, bot_h)
}

/// Shared virtualized grid scrollable for library albums and artists.
pub fn virtual_card_grid<'a>(
    scroll_id: Id,
    scroll_y: f32,
    window_h: f32,
    columns: usize,
    card_h: f32,
    total: usize,
    build_card: impl FnMut(usize) -> Option<Element<'a, Message>>,
) -> Element<'a, Message> {
    let cols = columns.max(1);
    let viewport_h = (window_h - 160.0).max(200.0);
    let (item_range, top_h, bot_h) = grid_window(scroll_y, viewport_h, card_h, cols, total);
    let cards: Vec<Element<'a, Message>> = item_range.filter_map(build_card).collect();

    let mut stack_col = column![];
    if top_h > 0.0 {
        stack_col = stack_col.push(space::vertical().height(top_h));
    }
    stack_col = stack_col.push(card_grid(cards, cols));
    if bot_h > 0.0 {
        stack_col = stack_col.push(space::vertical().height(bot_h));
    }
    motion::enter(
        "library",
        motion::EnterSpec::new(
            300,
            motion::Ease::CubicOut,
            8.0,
            motion::Fade::Cover(theme::VOID),
        ),
        smooth(
            scrollable(container(stack_col).padding(Padding::ZERO.right(14.0)))
                .id(scroll_id)
                .on_scroll(|vp| Message::LibScrolled(vp.absolute_offset().y))
                .height(Length::Fill),
        ),
    )
}

/// "N ALBUM(S) · M TRACKS" under an artist card.
pub fn artist_caption(ar: &rustygrade_core::ArtistSummary) -> String {
    let n = ar.album_indices.len();
    if n == 1 {
        format!("1 ALBUM · {} TRACKS", ar.track_count)
    } else {
        format!("{n} ALBUMS · {} TRACKS", ar.track_count)
    }
}

/// Library artists displayed as virtualized cards.
pub fn artists_grid<'a>(app: &'a App, accent: Color) -> Element<'a, Message> {
    let total = app.library_artists.len();
    if total == 0 {
        return empty_state("DATABASE_EMPTY", "USE ADD MUSIC TO SCAN A FOLDER");
    }
    let (columns, card_w, card_h) = app.grid_layout(GridArea::Library);
    virtual_card_grid(
        app.lib_scroll_id.clone(),
        app.lib_scroll_y,
        app.window_size.height,
        columns,
        card_h,
        total,
        |pos| {
            let idx = *app.library_artists.get(pos)?;
            let ar = app.artists.get(idx)?;
            let cover = ar
                .album_indices
                .iter()
                .filter_map(|&ai| app.albums.get(ai))
                .find_map(|a| {
                    app.covers
                        .get_first(&[a.cover_thumb_path.as_deref(), a.cover_full_path.as_deref()])
                });
            Some(media_card(
                "ART",
                ar.name.clone(),
                artist_caption(ar),
                cover,
                &ar.name,
                card_w,
                card_h,
                accent,
                Message::OpenArtist(artist_key(&ar.name)),
            ))
        },
    )
}

pub fn empty_state<'a>(title: &'a str, sub: &'a str) -> Element<'a, Message> {
    container(
        column![
            text(title).size(24).font(bold()).color(Color::WHITE),
            text(sub).size(12).color(theme::TEXT_MUTE),
        ]
        .spacing(8)
        .align_x(Alignment::Center),
    )
    .width(Length::Fill)
    .height(Length::Fill)
    .align_x(Alignment::Center)
    .align_y(Alignment::Center)
    .into()
}

pub fn empty_library_box(hint: &str) -> Element<'_, Message> {
    let box_inner = column![
        icons::icon("folderplus", 48.0, theme::LINE),
        text("DATABASE_EMPTY")
            .size(18)
            .font(bold())
            .color(theme::TEXT_MUTE),
        text(hint.to_owned()).size(11).color(theme::TEXT_MUTE),
        button(text("INITIATE_SCAN").size(10).font(bold()))
            .padding([8, 20])
            .style(|_t, s| {
                let hover = matches!(s, button::Status::Hovered | button::Status::Pressed);
                button::Style {
                    background: Some(Background::Color(if hover {
                        theme::ORANGE
                    } else {
                        theme::with_alpha(theme::ORANGE, 0.14)
                    })),
                    text_color: if hover { Color::BLACK } else { theme::ORANGE },
                    border: Border {
                        color: theme::ORANGE,
                        width: 1.0,
                        radius: 0.0.into(),
                    },
                    ..Default::default()
                }
            })
            .on_press(Message::PickFolders),
    ]
    .spacing(14)
    .align_x(Alignment::Center);

    container(
        container(box_inner)
            .width(Length::Fill)
            .height(256)
            .align_x(Alignment::Center)
            .align_y(Alignment::Center)
            .style(|_t| container::Style {
                background: Some(Background::Color(theme::with_alpha(theme::PANEL, 0.5))),
                border: Border {
                    color: theme::LINE,
                    width: 1.0,
                    radius: 8.0.into(),
                },
                ..Default::default()
            }),
    )
    .width(Length::Fill)
    .height(Length::Fill)
    .padding(Padding {
        top: 48.0,
        right: 24.0,
        bottom: 0.0,
        left: 24.0,
    })
    .align_y(Alignment::Start)
    .into()
}

#[cfg(test)]
mod tests {
    use super::{grid_window, visible_row_range};

    #[test]
    fn visible_row_range_handles_empty_middle_bottom_and_overscan() {
        assert_eq!(visible_row_range(0.0, 600.0, 200.0, 0, 2), 0..0);
        assert_eq!(visible_row_range(0.0, 600.0, 0.0, 10, 2), 0..0);
        assert_eq!(visible_row_range(0.0, 600.0, 200.0, 20, 1), 0..4);
        assert_eq!(visible_row_range(1000.0, 600.0, 200.0, 20, 1), 4..9);
        assert_eq!(visible_row_range(3800.0, 600.0, 200.0, 20, 2), 17..20);
    }

    #[test]
    fn grid_window_preserves_exact_total_height_with_and_without_bottom_spacer() {
        let cols = 5;
        let card_h = 200.0_f32;
        let row_stride = card_h + 12.0;
        let total: usize = 53;
        let total_rows = total.div_ceil(cols);
        let expected_total_h = total_rows as f32 * row_stride - 12.0;

        for scroll_y in [0.0_f32, 400.0, 1400.0, 99999.0] {
            let (item_range, top_h, bot_h) = grid_window(scroll_y, 600.0, card_h, cols, total);
            let start_row = item_range.start / cols;
            let end_row = item_range.end.div_ceil(cols);
            let vis_rows = end_row - start_row;
            let visible_h = vis_rows as f32 * card_h + (vis_rows - 1) as f32 * 12.0;
            assert!(
                (top_h + visible_h + bot_h - expected_total_h).abs() < 0.01,
                "scroll_y={scroll_y}: {top_h} + {visible_h} + {bot_h} != {expected_total_h}"
            );
        }
    }
}
