pub mod collections;
pub mod covers;
pub mod electron_import;
pub mod error_log;
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
pub mod watcher;
pub mod zip_archive;

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
pub use library::{AlbumDto, LibraryStore, TrackDto, reorder_by_ids, with_album_track_order};
pub use lyrics::{LyricLine, LyricsDoc, LyricsLoader};
pub use models::{Album, ArtistSummary, Track};
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
pub use smart::{Codecs, SmartField, SmartPlaylists};
pub use stats::{PlayStat, PlayStats};
pub use tag_writer::{TagEdit, TagWriter};
pub use watcher::{FolderWatcher, nearest_watched_ancestor};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "Run explicitly with RETRO_APPDIR pointing to a scratch copy of ~/.rustygrade"]
    fn real_data_compatibility_from_retro_appdir() {
        let app_dir = CoverPaths::app_dir();
        assert!(app_dir.exists(), "app_dir missing: {}", app_dir.display());

        let settings = AppSettings::load();
        assert!(settings.volume >= 0.0 && settings.volume <= 1.0);

        let albums = LibraryStore::load().expect("load library.json");
        assert!(!albums.is_empty(), "expected non-empty library.json");

        let collections = CollectionsStore::load();
        let (_session_state, session_queue) = SessionStore::load().unwrap_or_default();
        let stats = PlayStats::load();

        let _all_tracks = library::build_track_index(&albums);
        let liked_set: std::collections::HashSet<&str> =
            collections.liked.iter().map(String::as_str).collect();
        let smart_list = collections.smart.as_deref().unwrap_or(&[]);
        let smart_matches = smart_list
            .first()
            .map(|sp| {
                SmartPlaylists::evaluate(
                    sp,
                    albums
                        .iter()
                        .flat_map(|a| a.tracks.iter().map(move |t| (t, a))),
                    |id| stats.get(id),
                    |id| liked_set.contains(id),
                    0,
                )
            })
            .unwrap_or_default();

        // Check cover files on disk exist when referenced.
        let mut matched_covers = 0usize;
        for album in &albums {
            if album.cover_thumb_path.as_deref().is_some_and(std::path::Path::exists)
                || album.cover_full_path.as_deref().is_some_and(std::path::Path::exists)
            {
                matched_covers += 1;
            }
        }
        assert!(matched_covers > 0, "expected cover files matching hash_name");

        println!(
            "Loaded: albums={}, playlists={}, smart_playlists={}, smart_first_matches={}, queue={}, stats={}, matched_covers={}",
            albums.len(),
            collections.playlists.len(),
            smart_list.len(),
            smart_matches.len(),
            session_queue.len(),
            stats.tracks.len(),
            matched_covers,
        );
    }
}
