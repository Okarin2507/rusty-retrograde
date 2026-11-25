//! Full-screen player overlay with liquid-blot reveal transitions, dynamic cover backdrop,
//! queue view, and synced lyrics panel.

mod blot;
mod lyrics;
mod reveal;
mod view;

pub use blot::FsAnim;
pub use reveal::view;

use std::time::Instant;

use iced::Task;
use iced::widget::image;

use crate::{App, Message, Tab};

impl App {
    /// Starts the reveal (or reverses it) when entering or leaving the full-screen player.
    pub fn fs_sync(&mut self, was_open: bool) {
        let open = self.active_tab == Tab::NowPlaying;
        if open == was_open {
            return;
        }
        self.queue_open = false;
        self.audio_open = false;
        if !self.animations_enabled() {
            self.fs_anim = None;
            return;
        }
        let now = Instant::now();
        if let Some(mut a) = self.fs_anim {
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
        let (phases, drops) = blot::seeded_blot_params(cy.atan2(cx));
        self.fs_anim = Some(FsAnim {
            run_start: now,
            leg_start: now,
            opening: open,
            from: if was_open { 1.0 } else { 0.0 },
            phases,
            drops,
        });
    }

    /// Marks the reveal leg ending at `leg_start` complete once presented.
    pub fn fs_anim_done(&mut self, leg_start: Instant) {
        if self.fs_anim.is_some_and(|a| a.leg_start == leg_start) {
            self.fs_anim = None;
        }
    }

    /// Decodes the current track's cover off the UI thread (once per track).
    pub fn fs_refresh_backdrop(&mut self) -> Task<Message> {
        let id = self.current_track.as_ref().map(|t| t.id.clone());
        if self.fs_blur_tried == id {
            return Task::none();
        }
        self.fs_blur_tried = id;
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
                    let separate_thumb = thumb.as_deref().filter(|t| full.as_deref() != Some(*t));
                    let full_img = full
                        .as_deref()
                        .or(thumb.as_deref())
                        .and_then(|p| ::image::open(p).ok());
                    let thumb_img = separate_thumb.and_then(|p| ::image::open(p).ok());
                    let blur = thumb_img.as_ref().or(full_img.as_ref()).map(|img| {
                        let small = img
                            .resize_exact(48, 48, ::image::imageops::FilterType::Triangle)
                            .to_rgba8();
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
