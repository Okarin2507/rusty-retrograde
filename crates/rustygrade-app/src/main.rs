mod app;
mod context_menu;
mod controller;
mod cover_fetch;
mod covers_ui;
mod crossfade;
mod devshot;
mod dialogs;
mod egg;
mod fullscreen;
mod icon_paths;
mod icons;
mod library_view;
mod motion;
mod pages;
mod playback;
mod popups;
mod reorder;
mod smooth_scroll;
mod theme;
mod toasts;
mod views;

pub use app::*;

fn main() -> iced::Result {
    app::run()
}
