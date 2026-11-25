use std::fs::{self, File};
use std::io::Cursor;

use super::super::cover_names::is_android_cover_filename;
use super::super::tests_shared::{mk_album, mk_track};
use super::super::zip_export::{
    build_collections_backup_bundle, export_collections_zip_file, export_playlist_zip_file,
};
use super::super::zip_read::{inspect_backup_zip, read_backup_zip};
use super::super::{MAX_ARCHIVE_ACCEPTED_ENTRIES, PlaylistIoError};
use super::*;
use crate::collections::SmartRule;
use crate::covers::{ImageLimits, save_cover_bytes, sha256_hex_prefix};
use crate::test_util::{TempDir, solid_png};
use crate::zip_archive::{
    METHOD_DEFLATED, METHOD_STORED, ZipError, ZipReader, ZipWriter, build_data_descriptor_zip,
};

#[test]
fn zip_export_uses_android_compatible_cover_names_and_roundtrips_with_c_store() {
    let dir = TempDir::new("zip-roundtrip");
    crate::paths::with_test_app_dir(&dir, || {
        let png = solid_png(64, 64, [30, 190, 150]);
        let saved_cover = save_cover_bytes(&png).unwrap();
        let cover_name = saved_cover
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        assert!(cover_name.starts_with("c_"));

        let t_id = "/home/okarin/Music/Ado/Kyogen/01.flac";
        let albums = [mk_album(
            "Kyogen",
            "Ado",
            vec![mk_track(t_id, "Usseewa", "Ado", "Kyogen", 206.0)],
        )];
        let mut col = CollectionsData {
            liked: vec![t_id.to_owned()],
            playlists: vec![PlaylistData {
                id: 1,
                name: "Desktop Mix".to_owned(),
                cover_art: Some(CoverPaths::to_file_url(&saved_cover)),
                track_ids: vec![t_id.to_owned()],
                extra: Map::new(),
            }],
            smart: Some(vec![SmartPlaylistData {
                id: 1,
                name: "High Plays".to_owned(),
                rules: vec![SmartRule {
                    field: "plays".into(),
                    op: "gte".into(),
                    value: "1".into(),
                }],
                ..Default::default()
            }]),
            ..Default::default()
        };

        let zip_path = dir.join("export.zip");
        assert_eq!(export_collections_zip_file(&zip_path, &col).unwrap(), 1);

        let raw_zip = ZipReader::open(File::open(&zip_path).unwrap(), 100).unwrap();
        let expected_zip_cover = format!("native_pl_{cover_name}");
        assert!(is_android_cover_filename(&expected_zip_cover));
        assert!(
            raw_zip
                .entries()
                .iter()
                .any(|e| e.raw_name == format!("covers/{expected_zip_cover}"))
        );

        fs::remove_file(&saved_cover).unwrap();
        let bundle = read_backup_zip(File::open(&zip_path).unwrap()).unwrap();
        let prepared = prepare_zip_import(bundle, &albums, &[]).unwrap();
        let live_refs = Mutex::new(HashSet::new());
        let ingested = prepared.ingest_covers(Some(&live_refs), None).unwrap();
        assert_eq!(ingested.restored_cover_paths, vec![saved_cover.clone()]);
        assert!(live_refs.lock().unwrap().contains(&saved_cover));

        let (report, old_covers, _) = ingested.apply(&mut col, &mut PlayStats::default(), true);
        assert_eq!(
            (
                report.covers_restored,
                report.tracks_exact,
                old_covers.len()
            ),
            (1, 1, 1)
        );
        assert!(saved_cover.exists());
        assert_eq!(
            col.playlists[0].cover_art.as_deref(),
            Some(CoverPaths::to_file_url(&saved_cover).as_str())
        );
    });
}

#[test]
fn zip_import_accepts_android_and_c_covers_skips_album_cache_and_guards_merge_ids() {
    let dir = TempDir::new("zip-compat-merge");
    crate::paths::with_test_app_dir(&dir, || {
        let png_a = solid_png(64, 64, [255, 80, 40]);
        let png_b = solid_png(64, 64, [40, 120, 255]);
        let c_cover_name = format!("c_{}.png", sha256_hex_prefix(&png_b));
        let t_id = "/home/okarin/Music/Ado/Kyogen/01.flac";
        let albums = [mk_album(
            "Kyogen",
            "Ado",
            vec![mk_track(t_id, "Usseewa", "Ado", "Kyogen", 206.0)],
        )];

        let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
        let json = format!(
            r#"{{"liked":["/saf/primary:Music/Ado/Kyogen/01.flac"],"playlists":[
                {{"id":1,"name":"Android Playlist","coverArt":"file:///data/user/0/app/covers/native_pl_1_99_full.png","trackIds":["/storage/emulated/0/Music/Ado/Kyogen/01.flac"]}},
                {{"id":2,"name":"Desktop Match","coverArt":"covers/{c_cover_name}","trackIds":["{t_id}"]}}
            ]}}"#
        );
        writer
            .add_deflated("native-collections.json", json.as_bytes())
            .unwrap();
        writer
            .add_stored("covers/native_pl_1_99_full.png", &png_a)
            .unwrap();
        writer
            .add_stored("covers/native_pl_1_99_thumb.png", &png_b)
            .unwrap();
        writer
            .add_stored(&format!("covers/{c_cover_name}"), &png_b)
            .unwrap();
        for i in 0..600 {
            writer
                .add_stored(&format!("covers/c_{i:024x}.png"), b"ignored")
                .unwrap();
        }
        let zip_bytes = writer.finish().unwrap().into_inner();

        let bundle = read_backup_zip(Cursor::new(zip_bytes)).unwrap();
        assert_eq!(bundle.playlist_covers.len(), 3);
        let ingested = prepare_zip_import(bundle, &albums, &[])
            .unwrap()
            .ingest_covers(None, None)
            .unwrap();
        assert_eq!(ingested.covers_restored, 2);

        let mut local = CollectionsData {
            playlists: vec![
                PlaylistData {
                    id: 1,
                    name: "Local Favorites".into(),
                    track_ids: vec!["/other.flac".into()],
                    ..Default::default()
                },
                PlaylistData {
                    id: 2,
                    name: "desktop match".into(),
                    cover_art: Some("file:///tmp/old.png".into()),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let (report, old_covers, _) = ingested.apply(&mut local, &mut PlayStats::default(), false);
        assert_eq!((report.playlists_imported, report.playlists_total), (2, 3));
        assert_eq!(old_covers, vec!["file:///tmp/old.png"]);
        assert_eq!(local.playlists[0].name, "Local Favorites");
        assert_eq!(local.playlists[1].track_ids, vec![t_id]);
        assert_eq!(local.playlists[2].name, "Android Playlist");
        assert!(local.playlists[2].id > 2);
    });
}

#[test]
fn zip_import_rejects_corrupt_playlist_cover() {
    let albums = [mk_album(
        "Alb",
        "Art",
        vec![mk_track("/lib/t1.flac", "One", "Art", "Alb", 120.0)],
    )];
    let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
    writer
        .add_deflated("native-collections.json", br#"{"liked":[],"playlists":[]}"#)
        .unwrap();
    writer
        .add_stored("covers/native_pl_bad.png", b"not-an-image")
        .unwrap();
    let bundle = read_backup_zip(Cursor::new(writer.finish().unwrap().into_inner())).unwrap();
    assert!(matches!(
        prepare_zip_import(bundle, &albums, &[]),
        Err(PlaylistIoError::InvalidCover {
            reason: "corrupt or unsupported image",
            ..
        })
    ));
}

#[test]
fn zip_reader_rejects_traversal_oversized_covers_and_enforces_entry_limits() {
    let mut w_trav = ZipWriter::new(Cursor::new(Vec::new()));
    w_trav.add_stored("../../etc/passwd", b"x").unwrap();
    assert!(matches!(
        read_backup_zip(Cursor::new(w_trav.finish().unwrap().into_inner())),
        Err(PlaylistIoError::Zip(ZipError::PathTraversal(_)))
    ));

    let mut w_big = ZipWriter::new(Cursor::new(Vec::new()));
    w_big
        .add_deflated("native-collections.json", br#"{"liked":[]}"#)
        .unwrap();
    w_big
        .add_stored(
            "covers/native_pl_1_big.jpg",
            &vec![0u8; MAX_BACKUP_COVER_BYTES + 1],
        )
        .unwrap();
    assert!(matches!(
        read_backup_zip(Cursor::new(w_big.finish().unwrap().into_inner())),
        Err(PlaylistIoError::Zip(ZipError::EntryTooLarge { .. }))
    ));

    let mut w_ok = ZipWriter::new(Cursor::new(Vec::new()));
    w_ok.add_deflated("native-collections.json", br#"{"liked":[]}"#)
        .unwrap();
    for i in 0..(MAX_ARCHIVE_ACCEPTED_ENTRIES - 1) {
        w_ok.add_stored(&format!("covers/native_pl_{i}.jpg"), b"1")
            .unwrap();
    }
    assert_eq!(
        read_backup_zip(Cursor::new(w_ok.finish().unwrap().into_inner()))
            .unwrap()
            .playlist_covers
            .len(),
        MAX_ARCHIVE_ACCEPTED_ENTRIES - 1
    );

    let mut w_many = ZipWriter::new(Cursor::new(Vec::new()));
    w_many
        .add_deflated("native-collections.json", br#"{"liked":[]}"#)
        .unwrap();
    for i in 0..MAX_ARCHIVE_ACCEPTED_ENTRIES {
        w_many
            .add_stored(&format!("covers/native_pl_{i}.jpg"), b"1")
            .unwrap();
    }
    assert!(matches!(
        read_backup_zip(Cursor::new(w_many.finish().unwrap().into_inner())),
        Err(PlaylistIoError::TooLarge { .. })
    ));
}

#[test]
fn single_playlist_zip_export_leaves_liked_and_smart_empty_for_safe_merge() {
    let dir = TempDir::new("single-pl-zip");
    crate::paths::with_test_app_dir(&dir, || {
        let saved = save_cover_bytes(&solid_png(64, 64, [10, 20, 30])).unwrap();
        let pl = PlaylistData {
            id: 5,
            name: "Night Drive".into(),
            cover_art: Some(CoverPaths::to_file_url(&saved)),
            track_ids: vec!["/music/a.flac".into()],
            ..Default::default()
        };
        let zip_path = dir.join("night_drive.zip");
        assert_eq!(export_playlist_zip_file(&zip_path, &pl).unwrap(), 1);
        let summary = inspect_backup_zip(File::open(&zip_path).unwrap()).unwrap();
        assert!(summary.is_single_playlist_export);
        assert_eq!((summary.playlists, summary.liked, summary.smart), (1, 0, 0));
    });
}

#[test]
fn zip_archive_rules_macosx_shallowest_backup_prefix_dupes_and_50mib_cap() {
    let png = solid_png(64, 64, [12, 34, 56]);
    let mut w = ZipWriter::new(Cursor::new(Vec::new()));
    w.add_stored("__MACOSX/._native-collections.json", b"junk")
        .unwrap();
    w.add_deflated(
        "Backup/native-collections.json",
        br#"{"liked":["/deep.flac"]}"#,
    )
    .unwrap();
    w.add_deflated(
        "native-collections.json",
        br#"{"liked":["/shallow.flac"],"playlists":[{"id":1,"name":"Deep","coverArt":"covers/native_pl_1_deep.png","trackIds":[]}]}"#,
    ).unwrap();
    w.add_stored("Backup/covers/native_pl_1_deep.png", &png)
        .unwrap();
    w.add_stored("covers/native_pl_1_deep.png", b"dup-ignored")
        .unwrap();
    let bundle = read_backup_zip(Cursor::new(w.finish().unwrap().into_inner())).unwrap();
    let col: CollectionsData =
        serde_json::from_str(bundle.collections_json.as_deref().unwrap()).unwrap();
    assert_eq!(col.liked, vec!["/shallow.flac"]);
    assert_eq!(bundle.playlist_covers.len(), 1);
    assert_eq!(bundle.playlist_covers[0].bytes, png);

    let mut padded_10m = png;
    padded_10m.resize(MAX_BACKUP_COVER_BYTES, 0);
    assert!(ImageLimits::is_sane_bytes(&padded_10m));
    let mut bundle_50m = BackupBundle {
        collections_json: Some(r#"{"liked":[]}"#.into()),
        playlist_covers: (0..5)
            .map(|i| BackupCoverFile {
                name: format!("native_pl_{i}.png"),
                bytes: padded_10m.clone(),
            })
            .collect(),
        ..Default::default()
    };
    assert!(prepare_zip_import(bundle_50m.clone(), &[], &[]).is_ok());
    bundle_50m.playlist_covers.push(BackupCoverFile {
        name: "native_pl_overflow.png".into(),
        bytes: solid_png(64, 64, [1, 2, 3]),
    });
    assert!(matches!(
        prepare_zip_import(bundle_50m, &[], &[]),
        Err(PlaylistIoError::InvalidCover { .. })
    ));
}

#[test]
fn java_zip_output_stream_fixture_roundtrips_collections_stats_source_roots_and_covers() {
    let dir = TempDir::new("java-fixture");
    crate::paths::with_test_app_dir(&dir, || {
        let png = solid_png(64, 64, [90, 140, 220]);
        let local_id = "/home/okarin/DesktopMusic/FlatMix/01_unique_ado.flac";
        let albums = [mk_album(
            "Kyogen",
            "Ado",
            vec![mk_track(local_id, "Usseewa", "Ado", "Kyogen", 206.0)],
        )];

        let col_json = br#"{"liked":["/saf/primary:CustomSourceRoot/01_unique_ado.flac"],"playlists":[{"id":10,"name":"From Android","coverArt":"file:///data/user/0/app/covers/native_pl_10_1_full.png","trackIds":["/saf/primary:CustomSourceRoot/01_unique_ado.flac"]}],"smart":[],"androidTag":"ok"}"#;
        let stats_json = br#"{"Tracks":{"/saf/primary:CustomSourceRoot/01_unique_ado.flac":{"Count":7,"Last":1700000000000}}}"#;
        let settings_json = br#"{"AndroidBackupSourceRoots":["/saf/primary:CustomSourceRoot"]}"#;

        let fixture = build_data_descriptor_zip(&[
            ("native-collections.json", col_json, METHOD_DEFLATED),
            ("native-stats.json", stats_json, METHOD_DEFLATED),
            ("native-settings.json", settings_json, METHOD_DEFLATED),
            ("covers/native_pl_10_1_full.png", &png, METHOD_STORED),
        ]);

        let summary = inspect_backup_zip(Cursor::new(&fixture)).unwrap();
        assert!(
            !summary.is_single_playlist_export
                && summary.has_collections
                && summary.has_stats
                && summary.has_settings
                && summary.stats_tracks == 1
        );

        let bundle = read_backup_zip(Cursor::new(&fixture)).unwrap();
        let ingested = prepare_zip_import(bundle, &albums, &[])
            .unwrap()
            .ingest_covers(None, None)
            .unwrap();
        let mut local_col = CollectionsData::default();
        let mut local_stats = PlayStats::default();
        let (report, _, stats_changed) = ingested.apply(&mut local_col, &mut local_stats, false);

        assert!(report.collections_imported && report.settings_skipped && stats_changed);
        assert_eq!(local_col.liked, vec![local_id]);
        assert_eq!(
            local_stats.get(local_id).map(|s| (s.count, s.last)),
            Some((7, 1700000000000))
        );
        assert_eq!(
            local_col.extra.get("androidTag").and_then(Value::as_str),
            Some("ok")
        );

        let reexport = dir.join("reexport.zip");
        export_collections_zip_file(&reexport, &local_col).unwrap();
        let r = ZipReader::open(File::open(&reexport).unwrap(), 100).unwrap();
        let cover_e = r
            .entries()
            .iter()
            .find(|e| e.raw_name.starts_with("covers/native_pl_c_"))
            .unwrap();
        assert_eq!((cover_e.method, cover_e.flags & 0x0008), (0, 0));
    });
}

#[test]
fn cover_prepare_created_flags_corrupt_skip_disk_recovery_and_dir_guard() {
    let dir = TempDir::new("cover-lifecycle");
    crate::paths::with_test_app_dir(&dir, || {
        let png = solid_png(64, 64, [40, 200, 90]);
        let prep = prepare_cover_bytes(&png).unwrap();
        let first = write_prepared_cover(&prep).unwrap();
        assert!(first.created);
        let second = write_prepared_cover(&prep).unwrap();
        assert!(!second.created && first.path == second.path);

        // Corrupt cover in bundle does not fail ingest_covers.
        let prep_corrupt = PreparedZipImport {
            collections: Some(PreparedZipCollections {
                liked: Vec::new(),
                playlists: Vec::new(),
                smart: None,
                imported_from: None,
                extra: Map::new(),
            }),
            stats: None,
            has_settings: false,
            referenced_covers: vec![
                BackupCoverFile {
                    name: "native_pl_ok.png".into(),
                    bytes: png.clone(),
                },
                BackupCoverFile {
                    name: "native_pl_bad.png".into(),
                    bytes: b"not-an-image".to_vec(),
                },
            ],
            tracks_exact: 0,
            tracks_remapped: 0,
            tracks_unmatched: 0,
        };
        assert!(prep_corrupt.ingest_covers(None, None).is_ok());

        // Disk cover recovery + live/pending pinning when archive omits cover bytes.
        let first_name = first.path.file_name().unwrap().to_str().unwrap();
        let recover_bundle = BackupBundle {
            collections_json: Some(format!(
                r#"{{"playlists":[{{"id":1,"name":"Recover","coverArt":"covers/native_pl_{first_name}","trackIds":[]}}]}}"#
            )),
            ..Default::default()
        };
        let live_refs = Mutex::new(HashSet::new());
        let pending_refs = Mutex::new(HashSet::new());
        let ingested = prepare_zip_import(recover_bundle, &[], &[])
            .unwrap()
            .ingest_covers(Some(&live_refs), Some(&pending_refs))
            .unwrap();
        assert_eq!(ingested.covers_restored, 0);
        assert!(live_refs.lock().unwrap().contains(&first.path));
        assert!(pending_refs.lock().unwrap().contains(&first.path));
        let mut col = CollectionsData::default();
        ingested.apply(&mut col, &mut PlayStats::default(), false);
        assert_eq!(
            col.playlists[0].cover_art.as_deref(),
            Some(CoverPaths::to_file_url(&first.path).as_str())
        );

        // Boundary guard: files outside `CoverPaths::cover_dir()` must not be exported.
        let outside_img = dir.join("outside_secret.png");
        fs::write(&outside_img, &png).unwrap();
        let col_outside = CollectionsData {
            playlists: vec![PlaylistData {
                id: 1,
                name: "Outside".into(),
                cover_art: Some(CoverPaths::to_file_url(&outside_img)),
                ..Default::default()
            }],
            ..Default::default()
        };
        let out_bundle = build_collections_backup_bundle(&col_outside).unwrap();
        assert!(out_bundle.playlist_covers.is_empty());
    });
}

#[test]
fn zip_replace_preserves_extra_supports_stats_only_and_merges_stats() {
    let dir = TempDir::new("stats-replace");
    crate::paths::with_test_app_dir(&dir, || {
        let albums = [mk_album(
            "Alb",
            "Art",
            vec![mk_track("/lib/t1.flac", "One", "Art", "Alb", 120.0)],
        )];

        let mut w = ZipWriter::new(Cursor::new(Vec::new()));
        let stats_only_json = r#"{"Tracks":{"/lib/t1.flac":{"Count":4,"Last":200}}}"#;
        w.add_deflated("native-stats.json", stats_only_json.as_bytes())
            .unwrap();
        let stats_zip = w.finish().unwrap().into_inner();
        let summary = inspect_backup_zip(Cursor::new(&stats_zip)).unwrap();
        assert!(!summary.has_collections && summary.has_stats && summary.stats_tracks == 1);

        let stats_bundle = read_backup_zip(Cursor::new(stats_zip)).unwrap();
        let ingested_stats = prepare_zip_import(stats_bundle, &albums, &[])
            .unwrap()
            .ingest_covers(None, None)
            .unwrap();
        let mut col = CollectionsData::default();
        col.extra.insert("localKey".into(), Value::Bool(true));
        let mut stats = PlayStats::default();
        stats.tracks.insert(
            "/lib/T1.FLAC".into(),
            PlayStat {
                count: 9,
                last: 100,
            },
        );
        stats.rebuild_index();

        let (rep, _, stats_changed) = ingested_stats.apply(&mut col, &mut stats, false);
        assert!(!rep.collections_imported && stats_changed);
        assert_eq!(
            (
                stats.tracks["/lib/T1.FLAC"].count,
                stats.tracks["/lib/T1.FLAC"].last
            ),
            (9, 200)
        );

        col.playlists.push(PlaylistData {
            id: 1,
            name: "Old".into(),
            cover_art: Some("file:///tmp/old.png".into()),
            ..Default::default()
        });
        let replace_bundle = BackupBundle {
            collections_json: Some(r#"{"liked":[],"playlists":[],"remoteKey":42}"#.into()),
            stats_json: Some(r#"{"Tracks":{"/lib/t1.flac":{"Count":2,"Last":50}}}"#.into()),
            ..Default::default()
        };
        let ingested_rep = prepare_zip_import(replace_bundle, &albums, &[])
            .unwrap()
            .ingest_covers(None, None)
            .unwrap();
        let (rep2, old_gc, _) = ingested_rep.apply(&mut col, &mut stats, true);
        assert!(rep2.collections_imported);
        assert_eq!(old_gc, vec!["file:///tmp/old.png"]);
        assert_eq!(
            col.extra.get("localKey").and_then(Value::as_bool),
            Some(true)
        );
        assert_eq!(col.extra.get("remoteKey").and_then(Value::as_i64), Some(42));
        assert_eq!(stats.tracks["/lib/t1.flac"].count, 2);
    });
}
