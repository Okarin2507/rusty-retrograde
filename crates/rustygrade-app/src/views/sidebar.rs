use std::cell::Cell;

use iced::widget::canvas::{self, Canvas};
use iced::widget::text::Wrapping;
use iced::widget::{button, column, container, row, scrollable, space, stack, text, tooltip};
use iced::{
    Alignment, Background, Border, Color, Element, Length, Padding, Point, Rectangle, Renderer,
    Size, Theme, mouse,
};
use rustygrade_core::{CoverPaths, PlaylistCovers};

use super::bold;
use super::widgets::section_head;
use crate::covers_ui::render_cover;
use crate::smooth_scroll::smooth;
use crate::{
    AlbumKey, App, Message, PlaylistView, Tab, context_menu, egg, icons, motion, pages, theme,
};

const SIDEBAR_ALBUM_GAP: f32 = 2.0;

struct VisualizerState {
    phase: f64,
    last_frame: Option<std::time::Instant>,
    cache: canvas::Cache,
    last_static_bars: Cell<Option<[bool; 12]>>,
}

impl Default for VisualizerState {
    fn default() -> Self {
        Self {
            phase: 0.0,
            last_frame: None,
            cache: canvas::Cache::new(),
            last_static_bars: Cell::new(None),
        }
    }
}

struct VisualizerCanvas {
    playing: bool,
    animate: bool,
    active_bars: [bool; 12],
}

impl VisualizerCanvas {
    fn draw_bars(&self, frame: &mut canvas::Frame, phase: f32, bounds: Rectangle) {
        let count = egg::BAR_HEIGHT_PCTS.len();
        let gap = 4.0f32;
        let bar_w = ((bounds.width - gap * (count as f32 - 1.0)) / count as f32).max(2.0);
        for (i, pct) in egg::BAR_HEIGHT_PCTS.iter().enumerate() {
            let lit = self.active_bars[i];
            let h = if lit {
                bounds.height * 0.8
            } else if self.playing {
                4.0 + (bounds.height - 6.0) * ((phase * 1.7 + i as f32 * 0.9).sin().abs())
            } else {
                (bounds.height * (*pct / 100.0)).max(2.0)
            };
            let x = i as f32 * (bar_w + gap);
            let y = (bounds.height - h).max(0.0);
            let color = if lit {
                egg::bar_active_color(i)
            } else if i % 5 == 2 {
                theme::TEXT_MUTE
            } else {
                theme::LINE
            };
            frame.fill_rectangle(Point::new(x, y), Size::new(bar_w, h), color);
        }
    }
}

impl canvas::Program<Message> for VisualizerCanvas {
    type State = VisualizerState;

    fn update(
        &self,
        state: &mut VisualizerState,
        event: &iced::Event,
        bounds: Rectangle,
        cursor: mouse::Cursor,
    ) -> Option<canvas::Action<Message>> {
        if let iced::Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) = event
            && let Some(pos) = cursor.position_in(bounds)
            && bounds.width > 0.0
        {
            let count = egg::BAR_HEIGHT_PCTS.len();
            let idx = ((pos.x / bounds.width) * count as f32).floor() as usize;
            let idx = idx.min(count - 1);
            return Some(canvas::Action::publish(Message::VisualizerBarPressed(idx)).and_capture());
        }
        if let iced::Event::Window(iced::window::Event::RedrawRequested(now)) = event {
            if self.animate {
                if let Some(prev) = state.last_frame {
                    let dt = now.saturating_duration_since(prev).as_secs_f64().min(0.1);
                    state.phase += dt * 0.7;
                }
                state.last_frame = Some(*now);
                return Some(canvas::Action::request_redraw_at(*now + motion::FRAME));
            }
            state.last_frame = None;
        }
        None
    }

    fn mouse_interaction(
        &self,
        _state: &VisualizerState,
        bounds: Rectangle,
        cursor: mouse::Cursor,
    ) -> mouse::Interaction {
        if cursor.is_over(bounds) {
            mouse::Interaction::Pointer
        } else {
            mouse::Interaction::default()
        }
    }

    fn draw(
        &self,
        state: &VisualizerState,
        renderer: &Renderer,
        _theme: &Theme,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<canvas::Geometry> {
        if !self.playing {
            if state.last_static_bars.get() != Some(self.active_bars) {
                state.cache.clear();
                state.last_static_bars.set(Some(self.active_bars));
            }
            let geom = state.cache.draw(renderer, bounds.size(), |frame| {
                self.draw_bars(frame, 0.0, bounds);
            });
            return vec![geom];
        }
        state.last_static_bars.set(None);
        let mut frame = canvas::Frame::new(renderer, bounds.size());
        self.draw_bars(&mut frame, state.phase as f32, bounds);
        vec![frame.into_geometry()]
    }
}

fn rail_tip<'a>(
    content: impl Into<Element<'a, Message>>,
    tip: &str,
    rail: bool,
) -> Element<'a, Message> {
    if !rail {
        return content.into();
    }
    tooltip(
        content,
        container(text(tip.to_owned()).size(10).color(theme::TEXT))
            .padding([5, 9])
            .style(|_t| container::Style {
                background: Some(Background::Color(theme::RAISED)),
                border: Border {
                    color: theme::LINE,
                    width: 1.0,
                    radius: 0.0.into(),
                },
                ..Default::default()
            }),
        tooltip::Position::Right,
    )
    .gap(8)
    .into()
}

fn nav_item<'a>(
    icon: &str,
    label: &'a str,
    active: bool,
    accent: Color,
    rail: bool,
    msg: Message,
) -> Element<'a, Message> {
    let inner: Element<'a, Message> = if rail {
        container(icons::icon(
            icon,
            16.0,
            if active { theme::TEXT } else { theme::TEXT_DIM },
        ))
        .width(Length::Fill)
        .align_x(Alignment::Center)
        .into()
    } else {
        row![
            icons::icon(
                icon,
                16.0,
                if active { theme::TEXT } else { theme::TEXT_DIM },
            ),
            text(label).size(13).font(bold()).wrapping(Wrapping::None),
        ]
        .spacing(12)
        .align_y(Alignment::Center)
        .into()
    };
    let btn = button(inner)
        .width(Length::Fill)
        .padding(if rail { [9, 6] } else { [9, 16] })
        .style(theme::nav_item_style(active, accent))
        .on_press(msg);
    rail_tip(btn, label, rail)
}

fn bank_row_icon<'a>(
    glyph: Element<'a, Message>,
    name: &str,
    sub: String,
    active: bool,
    accent: Color,
    rail: bool,
    msg: Message,
) -> Element<'a, Message> {
    let thumb = container(glyph)
        .width(32)
        .height(32)
        .align_x(Alignment::Center)
        .align_y(Alignment::Center)
        .style(|_t| container::Style {
            background: Some(Background::Color(theme::RAISED)),
            border: Border {
                color: theme::LINE,
                width: 1.0,
                radius: 0.0.into(),
            },
            ..Default::default()
        })
        .into();
    bank_row_with_thumb(thumb, name, sub, active, accent, rail, msg)
}

fn bank_row_with_thumb<'a>(
    thumb: Element<'a, Message>,
    name: &str,
    sub: String,
    active: bool,
    accent: Color,
    rail: bool,
    msg: Message,
) -> Element<'a, Message> {
    let inner: Element<'a, Message> = if rail {
        container(thumb)
            .width(Length::Fill)
            .align_x(Alignment::Center)
            .into()
    } else {
        row![
            thumb,
            column![
                text(pages::fit(name, 152.0, 11.0).into_owned())
                    .size(11)
                    .font(bold())
                    .wrapping(Wrapping::None),
                text(sub)
                    .size(9)
                    .color(theme::TEXT_MUTE)
                    .wrapping(Wrapping::None),
            ]
            .spacing(2),
        ]
        .spacing(12)
        .align_y(Alignment::Center)
        .into()
    };
    let btn = button(inner)
        .width(Length::Fill)
        .padding(if rail { [4, 4] } else { [6, 8] })
        .style(theme::nav_item_style(active, accent))
        .on_press(msg);
    rail_tip(btn, name, rail)
}

/// Computes `(range, row_h, top_h, bot_h)` for the virtualized sidebar `DATA_BANK` album list.
fn sidebar_album_window(
    scroll_y: f32,
    window_h: f32,
    measured_viewport_h: Option<f32>,
    rail: bool,
    total: usize,
) -> (std::ops::Range<usize>, f32, f32, f32) {
    let row_h = if rail { 40.0 } else { 42.0 };
    if total == 0 {
        return (0..0, row_h, 0.0, 0.0);
    }
    let row_stride = row_h + SIDEBAR_ALBUM_GAP;
    let viewport_h = measured_viewport_h
        .filter(|&h| h > 0.0)
        .unwrap_or_else(|| (window_h - if rail { 320.0 } else { 400.0 }).max(240.0));
    let total_h = (total as f32 * row_stride - SIDEBAR_ALBUM_GAP).max(0.0);
    let max_scroll = (total_h - viewport_h).max(0.0);
    let clamped_y = scroll_y.clamp(0.0, max_scroll);
    let range = pages::visible_row_range(clamped_y, viewport_h, row_stride, total, 4);
    let top_h = range.start as f32 * row_stride;
    let bot_h = if range.is_empty() {
        0.0
    } else {
        total.saturating_sub(range.end) as f32 * row_stride
    };
    (range, row_h, top_h, bot_h)
}

pub(super) fn view_sidebar(app: &App, accent: Color) -> Element<'_, Message> {
    let rail = app.sidebar_width < 150.0;
    let dot_active = app.is_playing
        && app.animations_enabled()
        && app.window_visible
        && !app.main_window_minimized
        && app.main_window_focused
        && !app.watchdog.is_suspended()
        && app.active_tab != Tab::NowPlaying;
    let logo_badge = container(icons::icon("terminal", 16.0, theme::ORANGE))
        .padding(6)
        .style(|_t| container::Style {
            border: Border {
                color: theme::ORANGE,
                width: 1.0,
                radius: 1.0.into(),
            },
            ..Default::default()
        });
    let logo_row: Element<'_, Message> = if rail {
        container(logo_badge)
            .width(Length::Fill)
            .height(Length::Fill)
            .align_x(Alignment::Center)
            .align_y(Alignment::Center)
            .into()
    } else {
        container(
            row![
                logo_badge,
                column![
                    text("[PROJECT_LT] RL09")
                        .size(12)
                        .font(bold())
                        .color(Color::WHITE)
                        .wrapping(Wrapping::None),
                    text("TERMINAL")
                        .size(8)
                        .color(theme::TEAL)
                        .wrapping(Wrapping::None),
                ]
                .spacing(2),
            ]
            .spacing(12)
            .align_y(Alignment::Center),
        )
        .width(Length::Fill)
        .height(Length::Fill)
        .align_y(Alignment::Center)
        .padding([0, 12])
        .into()
    };
    let brand_content = stack![
        logo_row,
        container(motion::pulse_dot(theme::TEAL, 6.0, dot_active))
            .width(Length::Fill)
            .height(Length::Fill)
            .align_x(Alignment::End)
            .align_y(Alignment::Start)
            .padding(Padding {
                top: 6.0,
                right: 6.0,
                bottom: 0.0,
                left: 0.0,
            }),
    ];
    let brand_with_hover = stack![
        brand_content,
        container(motion::hover_bar(theme::ORANGE, 2.0))
            .width(Length::Fill)
            .height(Length::Fill)
            .align_y(Alignment::End),
    ];
    let brand = rail_tip(
        motion::hover_scope(
            std::time::Duration::from_millis(500),
            button(brand_with_hover)
                .padding(0)
                .width(Length::Fill)
                .height(64)
                .style(|_t, status| button::Style {
                    background: Some(Background::Color(
                        if matches!(status, button::Status::Hovered) {
                            theme::RAISED
                        } else {
                            theme::CARD
                        },
                    )),
                    text_color: theme::TEXT,
                    border: Border {
                        color: theme::LINE,
                        width: 1.0,
                        radius: 0.0.into(),
                    },
                    ..Default::default()
                })
                .on_press(Message::ToggleShortcutsModal),
        ),
        "SYSTEM_MANUAL (?)",
        rail,
    );

    let tab = app.shown_tab();
    let fav_active = tab == Tab::Playlists && matches!(app.playlist_view, PlaylistView::Liked);
    let mut nav = column![].padding(if rail { [10, 0] } else { [16, 0] });
    if !rail {
        nav = nav
            .push(section_head("SYSTEM_NAV", theme::TEAL))
            .push(space::vertical().height(8));
    }
    nav = nav
        .push(nav_item(
            "home",
            "HOME",
            tab == Tab::Library,
            accent,
            rail,
            Message::GoHome,
        ))
        .push(nav_item(
            "search",
            "SEARCH",
            tab == Tab::Search,
            accent,
            rail,
            Message::FocusSearch,
        ))
        .push(nav_item(
            "folderplus",
            "ADD MUSIC",
            app.add_music_menu_open,
            accent,
            rail,
            Message::ToggleAddMusicMenu,
        ))
        .push(nav_item(
            "heart",
            "FAVORITES",
            fav_active,
            accent,
            rail,
            Message::OpenPlaylistPage(PlaylistView::Liked),
        ))
        .push(nav_item(
            "grip",
            "SETTINGS",
            tab == Tab::Settings,
            accent,
            rail,
            Message::SelectTab(Tab::Settings),
        ));

    let bank_chip = |label: &'static str, on: bool, msg: Message| {
        button(text(label).size(9))
            .padding(Padding {
                top: 5.0,
                right: 8.0,
                bottom: 3.0,
                left: 8.0,
            })
            .style(theme::chip_style(on))
            .on_press(msg)
    };
    let list_pad = if rail {
        Padding::ZERO
    } else {
        Padding::ZERO.right(10.0)
    };
    let list_content: Element<'_, Message> = if app.bank_playlists {
        let fav_item = bank_row_icon(
            icons::icon("heart", 12.0, theme::GOLD),
            "FAVORITES",
            format!("{} UNITS", app.collections.liked.len()),
            tab == Tab::Playlists && matches!(app.playlist_view, PlaylistView::Liked),
            accent,
            rail,
            Message::OpenPlaylistPage(PlaylistView::Liked),
        );
        let mut v: Vec<Element<'_, Message>> = vec![context_menu::trigger(fav_item, |pos| {
            Message::OpenPlaylistMenu(PlaylistView::Liked, pos)
        })];
        for sp in app.collections.smart.iter().flatten() {
            let sid = sp.id;
            let cnt = app.smart_counts.get(&sid).copied().unwrap_or(0);
            let smart_item = bank_row_icon(
                icons::icon("filter", 13.0, theme::TEAL),
                &sp.name,
                format!("{cnt} UNITS"),
                tab == Tab::Playlists && app.playlist_view == PlaylistView::Smart(sid),
                accent,
                rail,
                Message::OpenPlaylistPage(PlaylistView::Smart(sid)),
            );
            v.push(context_menu::trigger(smart_item, move |pos| {
                Message::OpenPlaylistMenu(PlaylistView::Smart(sid), pos)
            }));
        }
        for p in &app.collections.playlists {
            let pid = p.id;
            let cov = CoverPaths::to_local_path(p.cover_art.as_deref())
                .and_then(|full| {
                    let thumb = PlaylistCovers::thumb_for(&full);
                    app.covers
                        .get_first(&[Some(thumb.as_path()), Some(full.as_path())])
                })
                .or_else(|| {
                    p.track_ids
                        .first()
                        .and_then(|tid| app.lookup_track(tid))
                        .and_then(|t| {
                            app.covers.get_first(&[
                                t.cover_thumb_path.as_deref(),
                                t.cover_full_path.as_deref(),
                            ])
                        })
                });
            let thumb: Element<'_, Message> = if cov.is_some() {
                render_cover(cov, 32.0, 0.0, accent, &p.name)
            } else {
                container(icons::icon("listmusic", 13.0, theme::TEXT_MUTE))
                    .width(32)
                    .height(32)
                    .align_x(Alignment::Center)
                    .align_y(Alignment::Center)
                    .style(|_t| container::Style {
                        background: Some(Background::Color(theme::RAISED)),
                        border: Border {
                            color: theme::LINE,
                            width: 1.0,
                            radius: 0.0.into(),
                        },
                        ..Default::default()
                    })
                    .into()
            };
            let item = bank_row_with_thumb(
                thumb,
                &p.name,
                format!("{} UNITS", p.track_ids.len()),
                tab == Tab::Playlists && app.playlist_view == PlaylistView::Custom(pid),
                accent,
                rail,
                Message::OpenPlaylistPage(PlaylistView::Custom(pid)),
            );
            v.push(context_menu::trigger(item, move |pos| {
                Message::OpenPlaylistMenu(PlaylistView::Custom(pid), pos)
            }));
        }
        column(v)
            .spacing(SIDEBAR_ALBUM_GAP)
            .padding(list_pad)
            .into()
    } else {
        let (range, row_h, top_h, bot_h) = sidebar_album_window(
            app.bank_scroll_y,
            app.window_size.height,
            app.bank_viewport_h,
            rail,
            app.sidebar_albums.len(),
        );
        let rows: Vec<Element<'_, Message>> = app.sidebar_albums[range]
            .iter()
            .filter_map(|&idx| app.albums.get(idx))
            .map(|a| {
                let key = AlbumKey::from_album(a);
                let key_menu = key.clone();
                let sel = app.selected_album.as_ref() == Some(&key)
                    && app.album_detail_open
                    && tab == Tab::Library;
                let cover = app
                    .covers
                    .get_first(&[a.cover_thumb_path.as_deref(), a.cover_full_path.as_deref()]);
                let btn_inner: Element<'_, Message> = if rail {
                    container(render_cover(cover, 32.0, 0.0, accent, &a.name))
                        .width(Length::Fill)
                        .height(Length::Fill)
                        .align_x(Alignment::Center)
                        .align_y(Alignment::Center)
                        .into()
                } else {
                    row![
                        render_cover(cover, 32.0, 0.0, accent, &a.name),
                        column![
                            text(pages::fit(&a.name, 152.0, 11.0).into_owned())
                                .size(11)
                                .font(bold())
                                .wrapping(Wrapping::None),
                            text(pages::fit(&a.artist, 152.0, 9.0).into_owned())
                                .size(9)
                                .color(theme::TEXT_MUTE)
                                .wrapping(Wrapping::None),
                        ]
                        .spacing(2),
                    ]
                    .spacing(12)
                    .height(Length::Fill)
                    .align_y(Alignment::Center)
                    .into()
                };
                let btn = rail_tip(
                    button(btn_inner)
                        .width(Length::Fill)
                        .height(Length::Fixed(row_h))
                        .padding(if rail { [4, 4] } else { [5, 8] })
                        .style(theme::nav_item_style(sel, accent))
                        .on_press(Message::SelectAlbum(key)),
                    &a.name,
                    rail,
                );
                context_menu::trigger(btn, move |pos| {
                    Message::OpenAlbumMenu(key_menu.clone(), pos)
                })
            })
            .collect();

        let mut stack_col = column![];
        if top_h > 0.0 {
            stack_col = stack_col.push(space::vertical().height(top_h));
        }
        if !rows.is_empty() {
            stack_col = stack_col.push(column(rows).spacing(SIDEBAR_ALBUM_GAP));
        }
        if bot_h > 0.0 {
            stack_col = stack_col.push(space::vertical().height(bot_h));
        }
        stack_col.padding(list_pad).into()
    };

    let plus_btn = rail_tip(
        button(icons::icon(
            "plus",
            14.0,
            if app.bank_menu_open {
                theme::TEXT
            } else {
                theme::TEXT_DIM
            },
        ))
        .padding(4)
        .style(theme::bare_button_style())
        .on_press(Message::ToggleBankMenu),
        "CREATE_LIST",
        rail,
    );
    let bank_head: Element<'_, Message> = if rail {
        container(plus_btn)
            .width(Length::Fill)
            .align_x(Alignment::Center)
            .into()
    } else {
        row![
            section_head("DATA_BANK", theme::GOLD),
            space::horizontal(),
            plus_btn
        ]
        .align_y(Alignment::Center)
        .into()
    };
    let mut list = scrollable(list_content)
        .id(app.bank_scroll_id.clone())
        .height(Length::Fill);
    if !app.bank_playlists {
        list =
            list.on_scroll(|vp| Message::BankScrolled(vp.absolute_offset().y, vp.bounds().height));
    }
    let list = smooth(list);

    let mut bank_col = column![bank_head]
        .spacing(10)
        .padding(if rail { [8, 4] } else { [14, 8] })
        .height(Length::Fill);
    if !rail {
        bank_col = bank_col.push(
            row![
                bank_chip(
                    "PLAYLISTS",
                    app.bank_playlists,
                    Message::SetBankPlaylists(true)
                ),
                bank_chip(
                    "ALBUMS",
                    !app.bank_playlists,
                    Message::SetBankPlaylists(false)
                ),
            ]
            .spacing(4),
        );
    }
    bank_col = bank_col.push(list);

    let mut inner_col = column![
        container(nav).style(|_t| container::Style {
            border: Border {
                color: theme::LINE_SOFT,
                width: 1.0,
                radius: 0.0.into(),
            },
            ..Default::default()
        }),
        bank_col,
    ];
    if !rail {
        let spectrum = container(
            Canvas::new(VisualizerCanvas {
                playing: app.is_playing && app.animations_enabled(),
                animate: dot_active,
                active_bars: app.egg_morse.active_bars(),
            })
            .width(Length::Fill)
            .height(Length::Fixed(32.0)),
        )
        .padding(Padding {
            top: 4.0,
            right: 12.0,
            bottom: 8.0,
            left: 12.0,
        })
        .height(40)
        .align_y(Alignment::End)
        .width(Length::Fill)
        .style(|_t| container::Style {
            background: Some(Background::Color(Color::from_rgb8(0x0C, 0x0C, 0x0E))),
            border: Border {
                color: theme::LINE,
                width: 1.0,
                radius: 0.0.into(),
            },
            ..Default::default()
        });
        inner_col = inner_col.push(spectrum);
    }

    let panel = container(inner_col)
        .style(|_t| container::Style {
            background: Some(Background::Color(theme::PANEL)),
            border: Border {
                color: theme::LINE,
                width: 1.0,
                radius: 0.0.into(),
            },
            ..Default::default()
        })
        .height(Length::Fill);

    container(
        column![brand, panel]
            .spacing(12)
            .width(Length::Fill)
            .height(Length::Fill)
            .padding(Padding::ZERO.bottom(8.0)),
    )
    .width(Length::Fixed(app.sidebar_width))
    .height(Length::Fill)
    .clip(true)
    .into()
}

#[cfg(test)]
mod tests {
    use super::{SIDEBAR_ALBUM_GAP, sidebar_album_window};

    #[test]
    fn sidebar_album_window_handles_empty() {
        let (range, row_h, top_h, bot_h) = sidebar_album_window(500.0, 1026.0, None, false, 0);
        assert_eq!(range, 0..0);
        assert_eq!(row_h, 42.0);
        assert_eq!(top_h, 0.0);
        assert_eq!(bot_h, 0.0);
    }

    #[test]
    fn sidebar_album_window_preserves_total_height_caps_rows_and_covers_viewport() {
        for rail in [false, true] {
            for measured in [None, Some(520.0_f32)] {
                let window_h = 1026.0_f32;
                let expected_vp_h = measured
                    .unwrap_or_else(|| (window_h - if rail { 320.0 } else { 400.0 }).max(240.0));
                for total in [5_usize, 115_usize] {
                    for scroll_y in [0.0_f32, 1600.0, 4400.0, 99999.0] {
                        let (range, row_h, top_h, bot_h) =
                            sidebar_album_window(scroll_y, window_h, measured, rail, total);
                        assert!(!range.is_empty());
                        assert!(range.end <= total);
                        let row_stride = row_h + SIDEBAR_ALBUM_GAP;
                        let visible_len = range.len();
                        let visible_h = visible_len as f32 * row_h
                            + (visible_len - 1) as f32 * SIDEBAR_ALBUM_GAP;
                        let expected_total_h =
                            total as f32 * row_h + (total - 1) as f32 * SIDEBAR_ALBUM_GAP;
                        assert!(
                            (top_h + visible_h + bot_h - expected_total_h).abs() < 0.01,
                            "rail={rail} total={total} scroll_y={scroll_y}: {top_h} + {visible_h} + {bot_h} != {expected_total_h}"
                        );
                        let max_scroll = (expected_total_h - expected_vp_h).max(0.0);
                        let clamped_y = scroll_y.clamp(0.0, max_scroll);
                        let range_start_y = range.start as f32 * row_stride;
                        let range_end_y = range.end as f32 * row_stride;
                        assert!(range_start_y <= clamped_y + 0.01);
                        assert!(
                            range_end_y >= (clamped_y + expected_vp_h).min(expected_total_h) - 0.01
                        );
                        if total == 115 {
                            assert!(visible_len < 30);
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn sidebar_album_window_handles_library_shrink_with_stale_scroll() {
        let (range, row_h, top_h, bot_h) =
            sidebar_album_window(4400.0, 1026.0, Some(520.0), false, 5);
        assert_eq!(row_h, 42.0);
        assert_eq!(range, 0..5);
        assert_eq!(top_h, 0.0);
        assert_eq!(bot_h, 0.0);
    }
}
