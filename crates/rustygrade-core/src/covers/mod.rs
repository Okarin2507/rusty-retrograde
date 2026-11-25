use thiserror::Error;

mod encode;
mod gc;
mod hash;
mod palette;
mod store;

pub use encode::{
    ImageLimits, MAX_COVER_FILE_BYTES, MAX_DIMENSION, MAX_FULL_COVER_EDGE, MAX_PIXELS,
};
pub use gc::{
    CoverDedupResult, collect_playlist_cover_paths, deduplicate_and_sweep_covers,
    delete_unreferenced_covers, delete_unreferenced_covers_with_extra, migrate_playlist_covers,
    referenced_cover_paths,
};
pub use hash::{sha256, sha256_hex_prefix};
pub use palette::{
    CoverPalette, MIN_ACCENT_LUMINANCE, extract_cover_palette, extract_cover_palette_from_rgb,
    relative_luminance,
};
pub use store::{
    PlaylistCovers, PreparedCover, SavedCover, prepare_cover_bytes, save_cover_bytes,
    write_prepared_cover,
};

#[derive(Debug, Error)]
pub enum CoverError {
    #[error("Unsupported image format. Please use JPG, PNG, or WEBP.")]
    UnsupportedFormat,
    #[error("Image could not be read, or is too large.")]
    InsaneImage,
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Image decode/encode error: {0}")]
    Image(String),
}
