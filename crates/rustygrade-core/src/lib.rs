pub mod collections;
pub mod covers;
pub(crate) mod electron_import;
pub mod error_log;
pub(crate) mod json_store;
pub mod library;
pub mod lyrics;
pub mod models;
pub mod paths;
pub mod playlist_io;
pub mod scanner;
pub mod session;
pub mod settings;
pub mod smart;
pub mod stats;
pub mod tag_writer;
#[cfg(test)]
pub(crate) mod test_util;
pub mod watcher;
pub(crate) mod zip_archive;

pub use collections::{
    CollectionsData, CollectionsStore, ExtraMap, PlaylistData, SmartPlaylistData, SmartRule,
};
pub use covers::{
    CoverDedupResult, CoverPalette, ImageLimits, MIN_ACCENT_LUMINANCE, PlaylistCovers,
    PreparedCover, SavedCover, collect_playlist_cover_paths, deduplicate_and_sweep_covers,
    delete_unreferenced_covers, delete_unreferenced_covers_with_extra, extract_cover_palette,
    extract_cover_palette_from_rgb, migrate_playlist_covers, prepare_cover_bytes,
    relative_luminance, save_cover_bytes, write_prepared_cover,
};
pub use library::{
    AlbumDto, LibraryStore, TrackDto, build_track_map, reorder_by_ids, with_album_track_order,
};
pub use lyrics::{LyricLine, LyricsDoc, LyricsLoader};
pub use models::{Album, ArtistSummary, Track, normalize_codec};
pub use paths::CoverPaths;
pub use playlist_io::{
    BackupBundle, BackupCoverFile, IngestedZipImport, M3uEntry, M3uImportResult, MapOutcome,
    ParsedM3u, PlaylistIoError, PreparedZipImport, TrackPathMapper, ZipArchiveSummary,
    ZipImportReport, android_export_cover_name, build_collections_backup_bundle,
    build_single_playlist_backup_bundle, claim_next_id, export_collections_zip_file,
    export_m3u8_file, export_playlist_zip_file, format_m3u8, import_m3u_file, import_m3u_str,
    inspect_backup_zip, inspect_backup_zip_file, is_android_cover_filename,
    is_content_cover_filename, normalize_path_segments, parse_m3u, prepare_zip_import,
    read_backup_zip, suggest_export_filename, write_backup_zip,
};
pub use scanner::{PrunePolicy, ScanOptions, ScanProgress, ScanResult, Scanner};
pub use session::{SessionState, SessionStore};
pub use settings::{
    AppSettings, DirectVolumeBridge, DisplayBackend, GpuPreference, LIBRARY_SORT_KEYS,
    PerformanceMode, canonical_library_sort,
};
pub use smart::{SmartField, SmartPlaylists};
pub use stats::{PlayStat, PlayStats};
pub use tag_writer::{TagEdit, TagWriter};
pub use watcher::{FolderWatcher, nearest_watched_ancestor};
pub use zip_archive::ZipError;

pub(crate) fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
