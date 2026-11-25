use std::path::PathBuf;

use super::super::suggest_export_filename;
use super::super::tests_shared::{mk_album, mk_track};
use super::*;
use crate::test_util::TempDir;

#[test]
fn m3u8_export_and_import_roundtrip_strips_newlines_and_handles_missing_artist() {
    let tracks = vec![
        mk_track(
            "/music/Ado/Kyogen/01.flac",
            "Usseewa\n",
            "Ado\r\n",
            "Kyogen\n",
            206.4,
        ),
        mk_track("/music/NoArtist/02.flac", "Ambient Intro", "", "", 0.0),
    ];
    let m3u = format_m3u8("My\r\nMix", &tracks);
    assert!(!m3u.contains('\r'));
    assert!(m3u.starts_with("#EXTM3U\n#PLAYLIST:MyMix\n"));
    assert!(m3u.contains("#EXTINF:206,Ado - Usseewa\n#EXTALB:Kyogen\n/music/Ado/Kyogen/01.flac\n"));
    assert!(m3u.contains("#EXTINF:-1,Ambient Intro\n/music/NoArtist/02.flac\n"));

    let clean = [mk_album(
        "Kyogen",
        "Ado",
        vec![
            mk_track(
                "/music/Ado/Kyogen/01.flac",
                "Usseewa",
                "Ado",
                "Kyogen",
                206.4,
            ),
            mk_track("/music/NoArtist/02.flac", "Ambient Intro", "", "", 0.0),
        ],
    )];
    let res = import_m3u_str(&m3u, None, &clean, &[]).unwrap();
    assert_eq!(
        (
            res.name.as_str(),
            res.exact,
            res.remapped,
            res.metadata,
            res.unmatched
        ),
        ("MyMix", 2, 0, 0, 0)
    );
}

#[test]
fn m3u_import_handles_bom_crlf_file_uri_relative_android_windows_disc_guard_and_metadata() {
    let dir = TempDir::new("m3u-tiers");
    let albums = [mk_album(
        "Alpha",
        "Artist A",
        vec![
            mk_track(
                "/home/okarin/Music/Rock/Alpha/01 Track.flac",
                "Track One",
                "Artist A",
                "Alpha",
                200.0,
            ),
            mk_track(
                "/home/okarin/Music/Jazz/Beta/02.flac",
                "Blue Note",
                "Artist B",
                "Beta",
                180.0,
            ),
            mk_track(
                "/home/okarin/Music/Re:Zero/01.flac",
                "Styx Helix",
                "MYTH & ROID",
                "Re:Zero",
                260.0,
            ),
            mk_track(
                "/home/okarin/Music/AlbumW/CD1/01.flac",
                "Disc Track",
                "Artist W",
                "AlbumW",
                150.0,
            ),
            mk_track(
                "/home/okarin/Music/Singles/flat_unique.flac",
                "Flat Unique",
                "Solo",
                "Singles",
                140.0,
            ),
            mk_track(
                "/home/okarin/Music/Live/Echo.flac",
                "Echo",
                "Band",
                "Live 2024",
                300.0,
            ),
            mk_track(
                "/home/okarin/Music/Studio/Echo.flac",
                "Echo",
                "Band",
                "Studio",
                190.0,
            ),
        ],
    )];

    let playlist_path = dir.join("Sub/My Export.m3u");
    let m3u = concat!(
        "\u{FEFF}#EXTM3U\r\n",
        "#UNKNOWN_DIRECTIVE:foo\r\n\r\n",
        "file:///home/okarin/Music/Rock/Alpha/01%20Track.flac\r\n",
        "/saf/primary:Music/Jazz/Beta/02.flac\r\n",
        "/storage/emulated/0/Music/Re:Zero/01.flac\r\n",
        "D:\\PhoneMusic\\flat_unique.flac\r\n",
        "/storage/emulated/0/Music/AlbumY/Disc 1/01.flac\r\n",
        "D:\\Backup\\AlbumW\\CD1\\01.flac\r\n",
        "#EXTINF:190,  Band  -  echo \r\n#EXTALB:  studio \r\n/android/unknown1.flac\r\n",
        "#EXTINF:302,Band - Echo\r\n/android/unknown2.flac\r\n",
        "#EXTINF:-1,Band - Echo\r\n/android/unknown3.flac\r\n",
        "#EXTINF:190,Band - Echo\r\n#EXTALB:Wrong Album\r\n/android/unknown4.flac\r\n",
        "/home/okarin/Music/Rock/Alpha/01 Track.flac\r\n",
    );

    let res = import_m3u_str(
        m3u,
        Some(&playlist_path),
        &albums,
        &["D:\\PhoneMusic".to_owned()],
    )
    .unwrap();
    assert_eq!(res.name, "My Export");
    assert_eq!(
        (
            res.exact,
            res.remapped,
            res.metadata,
            res.unmatched,
            res.total_entries
        ),
        (1, 4, 2, 3, 11)
    );
    assert_eq!(
        res.track_ids,
        vec![
            "/home/okarin/Music/Rock/Alpha/01 Track.flac",
            "/home/okarin/Music/Jazz/Beta/02.flac",
            "/home/okarin/Music/Re:Zero/01.flac",
            "/home/okarin/Music/Singles/flat_unique.flac",
            "/home/okarin/Music/AlbumW/CD1/01.flac",
            "/home/okarin/Music/Studio/Echo.flac",
            "/home/okarin/Music/Live/Echo.flac",
        ]
    );

    let rel_m3u_path = PathBuf::from("/home/okarin/Music/Playlists/fav.m3u8");
    let rel_res = import_m3u_str(
        "#EXTM3U\n../Rock/Alpha/01 Track.flac\n",
        Some(&rel_m3u_path),
        &albums,
        &[],
    )
    .unwrap();
    assert_eq!(
        rel_res.track_ids,
        vec!["/home/okarin/Music/Rock/Alpha/01 Track.flac"]
    );
}

#[test]
fn m3u_import_rejects_empty_zero_matches_and_oversized_files() {
    assert!(matches!(
        import_m3u_str("#EXTM3U\n#PLAYLIST:Empty\n", None, &[], &[]),
        Err(PlaylistIoError::EmptyPlaylist)
    ));
    assert!(matches!(
        import_m3u_str("#EXTM3U\n/does/not/exist/01.flac\n", None, &[], &[]),
        Err(PlaylistIoError::NoMatchingTracks { total: 1 })
    ));
    assert!(matches!(
        import_m3u_str(&"a".repeat(MAX_M3U_BYTES + 1), None, &[], &[]),
        Err(PlaylistIoError::TooLarge { .. })
    ));
}

#[test]
fn m3u_edge_cases_uris_cwd_guard_empty_artist_extalb_lone_cr_and_latin1() {
    let dir = TempDir::new("m3u-edge");
    let albums = [mk_album(
        "Alb",
        "Art",
        vec![
            mk_track(
                "/home/okarin/Music/Track One.flac",
                "Track One",
                "Art",
                "Alb",
                200.0,
            ),
            mk_track(
                "C:\\Music\\Win Song.flac",
                "Win Song",
                "WinArt",
                "WinAlb",
                180.0,
            ),
            mk_track(
                "/home/okarin/Music/Ambient/Intro.flac",
                "Intro",
                "",
                "Ambient",
                150.0,
            ),
            mk_track(
                "/home/okarin/Music/Live/Stream.flac",
                "Stream",
                "Band",
                "Live",
                210.0,
            ),
            mk_track(
                "/home/okarin/Music/Cafe.flac",
                "Café – Noël",
                "Chanson",
                "Nights",
                195.0,
            ),
        ],
    )];

    let m3u = concat!(
        "#EXTM3U\r",
        "file://localhost/home/okarin/Music/Track%20One.flac?token=1#t=10\r",
        "file:///C:/Music/Win%20Song.flac?v=2#frag\r",
        "https://example.com/Live/Stream.flac\r",
        "#EXTINF:150,Intro\r/missing/intro1.flac\r",
        "#EXTINF:150,Intro\r#EXTALB:Ambient\r/missing/intro2.flac\r",
        "#EXTALB:WrongAlbum\r#EXTINF:210,Band - Stream\r/missing/stream.flac\r",
    );
    let res = import_m3u_str(m3u, None, &albums, &[]).unwrap();
    assert_eq!(
        (res.exact, res.remapped, res.metadata, res.unmatched),
        (1, 1, 2, 2)
    );

    let cwd_cargo = fs::canonicalize("Cargo.toml").unwrap();
    let cwd_album = [mk_album(
        "Cwd",
        "Dev",
        vec![mk_track(
            cwd_cargo.to_str().unwrap(),
            "Cargo",
            "Dev",
            "Cwd",
            100.0,
        )],
    )];
    assert!(matches!(
        import_m3u_str(
            "#EXTM3U\nCargo.toml\n",
            Some(&dir.join("other/mix.m3u8")),
            &cwd_album,
            &[]
        ),
        Err(PlaylistIoError::NoMatchingTracks { total: 1 })
    ));

    let cp1252 = b"#EXTM3U\n#EXTINF:195,Chanson - Caf\xE9 \x96 No\xEBl\n/other/missing.flac\n";
    let cp1252_m3u = dir.join("legacy.m3u");
    fs::write(&cp1252_m3u, cp1252).unwrap();
    assert_eq!(
        import_m3u_file(&cp1252_m3u, &albums, &[])
            .unwrap()
            .track_ids,
        vec!["/home/okarin/Music/Cafe.flac"]
    );

    let bad_m3u8 = dir.join("bad.m3u8");
    fs::write(&bad_m3u8, cp1252).unwrap();
    assert!(matches!(
        import_m3u_file(&bad_m3u8, &albums, &[]),
        Err(PlaylistIoError::InvalidUtf8)
    ));

    assert_eq!(
        suggest_export_filename("///***???", "m3u8"),
        "playlist.m3u8"
    );
    assert_eq!(suggest_export_filename(".hidden.", "m3u8"), "hidden.m3u8");
    assert_eq!(
        format_m3u8("Clean", &[mk_track("", "Ghost", "Nobody", "Void", 100.0)]),
        "#EXTM3U\n#PLAYLIST:Clean\n"
    );

    let partial = dir.join("partial_fail.m3u8");
    assert!(
        write_file_direct_or_cleanup(&partial, |mut f| {
            f.write_all(b"partial")?;
            Err(PlaylistIoError::Io(std::io::Error::other("disk full")))
        })
        .is_err()
    );
    assert!(!partial.exists());
}
