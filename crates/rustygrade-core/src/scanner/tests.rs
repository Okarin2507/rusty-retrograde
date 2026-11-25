use super::*;
use crate::tag_writer::{TagEdit, TagWriter};
use crate::test_util::{TempDir, minimal_flac_bytes, solid_png};

fn write_tagged_flac(path: &Path, title: &str, artist: &str, album: &str, year: u32) {
    fs::write(path, minimal_flac_bytes()).unwrap();
    TagWriter::write(
        path,
        &TagEdit {
            title: Some(title.into()),
            artist: Some(artist.into()),
            album: Some(album.into()),
            year: Some(year),
            ..Default::default()
        },
    )
    .unwrap();
}

fn scan_opts(prune: PrunePolicy, resync_covers: bool) -> ScanOptions {
    ScanOptions {
        prune,
        max_threads: Some(1),
        force: false,
        resync_covers,
    }
}

#[test]
fn scan_deduplicates_identical_covers_and_refcounts_on_prune() {
    let base = TempDir::new("scan-dedup");
    let app_dir = base.join("app");
    let music_dir = base.join("music");
    let alb1_dir = music_dir.join("Alb1");
    let alb2_dir = music_dir.join("Alb2");
    fs::create_dir_all(&alb1_dir).unwrap();
    fs::create_dir_all(&alb2_dir).unwrap();

    write_tagged_flac(
        &alb1_dir.join("01.flac"),
        "Song 1",
        "Artist",
        "Album One",
        2020,
    );
    write_tagged_flac(
        &alb2_dir.join("01.flac"),
        "Song 2",
        "Artist",
        "Album Two",
        2020,
    );
    let cover_bytes = solid_png(128, 128, [30, 60, 90]);
    fs::write(alb1_dir.join("cover.png"), &cover_bytes).unwrap();
    fs::write(alb2_dir.join("cover.png"), &cover_bytes).unwrap();

    crate::paths::with_test_app_dir(&app_dir, || {
        let res = Scanner::scan(
            std::slice::from_ref(&music_dir),
            scan_opts(PrunePolicy::Safe, false),
            None,
            None,
        )
        .unwrap();
        assert_eq!((res.added_tracks, res.new_albums, res.covers), (2, 2, 2));

        let lib = LibraryStore::load_raw().unwrap();
        let c1 = lib["Album One"].cover_art.clone().unwrap();
        assert_eq!(c1, lib["Album Two"].cover_art.clone().unwrap());
        let cover_file_path = CoverPaths::to_local_path(Some(&c1)).unwrap();
        assert!(cover_file_path.exists());

        fs::remove_dir_all(&alb1_dir).unwrap();
        let res2 = Scanner::scan(
            std::slice::from_ref(&music_dir),
            scan_opts(PrunePolicy::Safe, false),
            None,
            None,
        )
        .unwrap();
        assert_eq!((res2.removed_tracks, res2.removed_albums), (1, 1));
        assert!(
            cover_file_path.exists(),
            "Album Two keeps shared cover alive"
        );

        fs::remove_dir_all(&alb2_dir).unwrap();
        let res3 = Scanner::scan(
            std::slice::from_ref(&music_dir),
            scan_opts(PrunePolicy::Safe, false),
            None,
            None,
        )
        .unwrap();
        assert_eq!((res3.removed_tracks, res3.removed_albums), (1, 1));
        assert!(LibraryStore::load_raw().unwrap().is_empty());
        assert!(!cover_file_path.exists());
    });
}

#[test]
fn scan_resync_covers_preserves_unextractable_and_refcounts_replacements() {
    let base = TempDir::new("scan-resync");
    let app_dir = base.join("app");
    let music_dir = base.join("music");
    let d1 = music_dir.join("Alb1");
    let d2 = music_dir.join("Alb2");
    let d3 = music_dir.join("Alb3");
    for d in [&d1, &d2, &d3] {
        fs::create_dir_all(d).unwrap();
    }
    write_tagged_flac(&d1.join("01.flac"), "S1", "Artist", "Resync One", 2021);
    write_tagged_flac(&d2.join("01.flac"), "S2", "Artist", "Resync Two", 2021);
    write_tagged_flac(&d3.join("01.flac"), "S3", "Artist", "Resync Three", 2021);

    let shared_png = solid_png(64, 64, [200, 20, 20]);
    let unique_png = solid_png(64, 64, [20, 200, 20]);
    let replacement_png = solid_png(64, 64, [20, 20, 200]);
    fs::write(d1.join("cover.png"), &shared_png).unwrap();
    fs::write(d2.join("cover.png"), &shared_png).unwrap();
    fs::write(d3.join("cover.png"), &unique_png).unwrap();

    crate::paths::with_test_app_dir(&app_dir, || {
        Scanner::scan(
            std::slice::from_ref(&music_dir),
            scan_opts(PrunePolicy::Safe, false),
            None,
            None,
        )
        .unwrap();
        let lib_before = LibraryStore::load_raw().unwrap();
        let shared_url = lib_before["Resync One"].cover_art.clone().unwrap();
        let unique_url = lib_before["Resync Three"].cover_art.clone().unwrap();
        let shared_file = CoverPaths::to_local_path(Some(&shared_url)).unwrap();
        let unique_file = CoverPaths::to_local_path(Some(&unique_url)).unwrap();

        fs::write(d1.join("cover.png"), &replacement_png).unwrap();
        fs::remove_file(d3.join("cover.png")).unwrap();

        let cancelled = AtomicBool::new(true);
        Scanner::scan(
            std::slice::from_ref(&music_dir),
            scan_opts(PrunePolicy::Never, true),
            Some(&cancelled),
            None,
        )
        .unwrap();
        assert_eq!(
            LibraryStore::load_raw().unwrap()["Resync One"]
                .cover_art
                .as_ref(),
            Some(&shared_url)
        );

        let res = Scanner::scan(
            std::slice::from_ref(&music_dir),
            scan_opts(PrunePolicy::Never, true),
            None,
            None,
        )
        .unwrap();
        assert_eq!(res.covers, 2);
        let lib_after = LibraryStore::load_raw().unwrap();
        assert_ne!(
            lib_after["Resync One"].cover_art.as_ref(),
            Some(&shared_url)
        );
        assert!(shared_file.exists());
        assert_eq!(
            lib_after["Resync Three"].cover_art.as_ref(),
            Some(&unique_url)
        );
        assert!(unique_file.exists());

        fs::write(d2.join("cover.png"), &replacement_png).unwrap();
        Scanner::scan(
            std::slice::from_ref(&music_dir),
            scan_opts(PrunePolicy::Never, true),
            None,
            None,
        )
        .unwrap();
        assert!(!shared_file.exists());
    });
}

#[test]
fn tag_edits_update_album_year_artist_and_album_artist_on_force_rescan() {
    let base = TempDir::new("scan-tag-edit");
    let app_dir = base.join("app");
    let music_dir = base.join("music");
    fs::create_dir_all(&music_dir).unwrap();
    let t1 = music_dir.join("01.flac");
    let t2 = music_dir.join("02.flac");
    fs::write(&t1, minimal_flac_bytes()).unwrap();
    fs::write(&t2, minimal_flac_bytes()).unwrap();

    let seed = TagEdit {
        title: Some("Track".into()),
        artist: Some("Old Artist".into()),
        album: Some("Edited Album".into()),
        album_artist: Some("Old AA".into()),
        year: Some(2020),
        ..Default::default()
    };
    let (written, errs) = TagWriter::write_batch(&[t1.clone(), t2.clone()], &seed);
    assert_eq!((written, errs.is_empty()), (2, true));

    crate::paths::with_test_app_dir(&app_dir, || {
        let force_opts = ScanOptions {
            force: true,
            max_threads: Some(1),
            ..Default::default()
        };
        Scanner::scan(std::slice::from_ref(&music_dir), force_opts, None, None).unwrap();

        let (written, errs) = TagWriter::write_batch(
            &[t1.clone(), t2.clone()],
            &TagEdit {
                artist: Some("New Artist".into()),
                album_artist: Some("New AA".into()),
                year: Some(2024),
                ..Default::default()
            },
        );
        assert_eq!((written, errs.is_empty()), (2, true));
        Scanner::scan(std::slice::from_ref(&music_dir), force_opts, None, None).unwrap();
        let lib = LibraryStore::load_raw().unwrap();
        let alb = &lib["Edited Album"];
        assert_eq!(alb.year, 2024);
        assert_eq!(alb.album_artist.as_deref(), Some("New AA"));
        assert_eq!(alb.artist.as_deref(), Some("New AA"));

        // Clearing album_artist falls back to the track artist; a cleared year keeps the stored
        // value instead of zeroing it.
        let (written, errs) = TagWriter::write_batch(
            &[t1, t2],
            &TagEdit {
                album_artist: Some(String::new()),
                year: Some(0),
                ..Default::default()
            },
        );
        assert_eq!((written, errs.is_empty()), (2, true));
        Scanner::scan(std::slice::from_ref(&music_dir), force_opts, None, None).unwrap();
        let lib2 = LibraryStore::load_raw().unwrap();
        let alb2 = &lib2["Edited Album"];
        assert_eq!(alb2.year, 2024);
        assert_eq!(alb2.album_artist, None);
        assert_eq!(alb2.artist.as_deref(), Some("New Artist"));
    });
}

#[test]
fn rescan_without_year_tags_keeps_stored_album_year() {
    let base = TempDir::new("scan-year-keep");
    let app_dir = base.join("app");
    let music_dir = base.join("music");
    fs::create_dir_all(&music_dir).unwrap();
    let t1 = music_dir.join("01.flac");
    let t2 = music_dir.join("02.flac");
    write_tagged_flac(&t1, "One", "Artist", "Year Album", 2020);
    write_tagged_flac(&t2, "Two", "Artist", "Year Album", 2020);

    crate::paths::with_test_app_dir(&app_dir, || {
        Scanner::scan(
            std::slice::from_ref(&music_dir),
            scan_opts(PrunePolicy::Never, false),
            None,
            None,
        )
        .unwrap();
        assert_eq!(LibraryStore::load_raw().unwrap()["Year Album"].year, 2020);

        let (written, errs) = TagWriter::write_batch(
            &[t1, t2],
            &TagEdit {
                year: Some(0),
                ..Default::default()
            },
        );
        assert_eq!((written, errs.is_empty()), (2, true));
        Scanner::scan(
            std::slice::from_ref(&music_dir),
            ScanOptions {
                force: true,
                max_threads: Some(1),
                ..Default::default()
            },
            None,
            None,
        )
        .unwrap();

        let lib = LibraryStore::load_raw().unwrap();
        let album = &lib["Year Album"];
        assert_eq!(album.year, 2020);
        assert_eq!(album.tracks.as_ref().unwrap().len(), 2);
    });
}

#[test]
fn partial_album_artist_edit_turns_album_into_common_artist() {
    let base = TempDir::new("scan-mixed-aa");
    let app_dir = base.join("app");
    let music_dir = base.join("music");
    fs::create_dir_all(&music_dir).unwrap();
    let t1 = music_dir.join("01.flac");
    let t2 = music_dir.join("02.flac");

    for (path, title, artist) in [(&t1, "One", "Artist One"), (&t2, "Two", "Artist Two")] {
        fs::write(path, minimal_flac_bytes()).unwrap();
        TagWriter::write(
            path,
            &TagEdit {
                title: Some(title.into()),
                artist: Some(artist.into()),
                album: Some("Mixed AA Album".into()),
                album_artist: Some("Shared AA".into()),
                year: Some(2020),
                ..Default::default()
            },
        )
        .unwrap();
    }

    crate::paths::with_test_app_dir(&app_dir, || {
        Scanner::scan(
            std::slice::from_ref(&music_dir),
            scan_opts(PrunePolicy::Never, false),
            None,
            None,
        )
        .unwrap();
        let lib = LibraryStore::load_raw().unwrap();
        let album = &lib["Mixed AA Album"];
        assert_eq!(album.album_artist.as_deref(), Some("Shared AA"));
        assert_eq!(album.artist.as_deref(), Some("Shared AA"));

        // Re-read only the edited track: its AlbumArtist now disagrees with the stored album.
        TagWriter::write(
            &t1,
            &TagEdit {
                album_artist: Some("Other AA".into()),
                ..Default::default()
            },
        )
        .unwrap();
        Scanner::scan(
            std::slice::from_ref(&t1),
            ScanOptions {
                force: true,
                max_threads: Some(1),
                ..Default::default()
            },
            None,
            None,
        )
        .unwrap();

        let lib = LibraryStore::load_raw().unwrap();
        let album = &lib["Mixed AA Album"];
        assert_eq!(album.album_artist, None);
        assert_eq!(album.artist.as_deref(), Some("Various Artists"));
        assert_eq!(album.tracks.as_ref().unwrap().len(), 2);
    });
}
