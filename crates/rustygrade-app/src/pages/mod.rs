//! Shared library/detail pages, virtualized card grids, track rows, and search views.

mod cards;
mod detail;
mod detail_views;
mod nav;
mod rows;
mod search;

pub use cards::{
    CardMotion, GridArea, artists_grid, char_width_px, empty_library_box, fit, media_card_opts,
    virtual_card_grid, visible_row_range,
};
pub use detail::NO_COVER_TINT;
pub use detail_views::{album_detail, artist_detail, playlist_page};
pub use search::search_page;

use crate::{AlbumKey, PlaylistView};

/// What a detail page lists. Messages carry this instead of tracks to avoid copying lists per row.
#[derive(Debug, Clone, PartialEq)]
pub enum DetailCtx {
    Album(AlbumKey),
    Artist(String),
    Playlist(PlaylistView),
}

/// A detail page remembered by navigation history (`BACK`).
#[derive(Debug, Clone, PartialEq)]
pub enum Page {
    Album(AlbumKey),
    Artist(String),
    Playlist(PlaylistView),
}
