use super::*;
use rustygrade_core::{Album, build_track_map};
use std::path::PathBuf;

fn mk_track(id: &str, album: &str, duration: f64) -> Track {
    Track {
        id: id.to_owned(),
        title: id.to_owned(),
        artist: "Artist".to_owned(),
        album: album.to_owned(),
        duration,
        file_path: id.to_owned(),
        track_number: 1,
        disc_number: 1,
        codec: "FLAC".to_owned(),
        sample_rate: 44100,
        bits_per_sample: 16,
        channels: 2,
        cover_thumb_path: Some(PathBuf::from("/tmp/a.jpg")),
        cover_full_path: None,
    }
}

fn mk_tracks(prefix: &str, n: usize, album: &str) -> Vec<Track> {
    (1..=n)
        .map(|i| mk_track(&format!("{prefix}{i}"), album, 200.0))
        .collect()
}

fn mk_album(name: &str, ids: &[&str]) -> Album {
    Album {
        name: name.to_owned(),
        artist: "Artist".to_owned(),
        cover_thumb_path: None,
        cover_full_path: None,
        year: 2024,
        added_at: 1,
        tracks: ids.iter().map(|id| mk_track(id, name, 200.0)).collect(),
    }
}

fn library() -> Vec<Album> {
    vec![
        mk_album("A", &["/a/1", "/a/2", "/a/3", "/a/4"]),
        mk_album("B", &["/b/1", "/b/2", "/b/3"]),
    ]
}

fn ids(q: &PlaybackQueue) -> Vec<&str> {
    q.tracks().iter().map(|t| t.id.as_str()).collect()
}

fn upcoming(q: &PlaybackQueue) -> Vec<&str> {
    let cur = q.index().unwrap();
    q.tracks()[cur + 1..]
        .iter()
        .map(|t| t.id.as_str())
        .collect()
}

#[test]
fn shuffle_stays_within_current_queue_and_restores_order() {
    let tracks = mk_tracks("/m/", 20, "Album");
    let original_ids: Vec<String> = tracks.iter().map(|t| t.id.clone()).collect();
    let mut q = PlaybackQueue::default();
    q.set_queue(tracks.clone(), Some(3));
    assert_eq!(q.current().unwrap().id, "/m/4");

    q.set_shuffle(true);
    assert_eq!(q.index(), Some(0));
    assert_eq!(q.current().unwrap().id, "/m/4");
    let mut sorted: Vec<&str> = ids(&q);
    sorted.sort_unstable();
    let mut orig_sorted: Vec<&str> = original_ids.iter().map(String::as_str).collect();
    orig_sorted.sort_unstable();
    assert_eq!(sorted, orig_sorted);
    assert_ne!(
        ids(&q),
        original_ids.iter().map(String::as_str).collect::<Vec<_>>()
    );

    q.set_shuffle(false);
    assert_eq!(q.index(), Some(3));
    assert_eq!(
        ids(&q),
        original_ids.iter().map(String::as_str).collect::<Vec<_>>()
    );

    // `start_index = None` picks a random first track across runs.
    let mut starts = HashSet::new();
    for _ in 0..16 {
        let mut rq = PlaybackQueue::default();
        rq.set_shuffle(true);
        rq.set_queue(tracks.clone(), None);
        assert_eq!(rq.index(), Some(0));
        starts.insert(rq.current().unwrap().id.clone());
    }
    assert!(
        starts.len() > 1,
        "expected random start tracks, got {starts:?}"
    );
}

#[test]
fn restore_session_and_all_track_ids_preserve_shuffled_and_original_orders() {
    let lib = library();
    let map = build_track_map(&lib);

    let no_orig = SessionState {
        current_id: Some("/b/1".to_owned()),
        index: 1,
        position: 0.0,
        shuffle: true,
        repeat_mode: 0,
    };
    let saved = ["/a/3", "/b/1", "/a/1"].map(str::to_owned).to_vec();
    let mut q = PlaybackQueue::default();
    q.restore_session(&map, &no_orig, &saved, None);
    q.set_shuffle(false);
    assert_eq!(ids(&q), vec!["/a/3", "/b/1", "/a/1"]);
    assert_eq!(q.index(), Some(1));

    let state = SessionState {
        current_id: Some("/a/3".to_owned()),
        index: 0,
        position: 12.0,
        shuffle: true,
        repeat_mode: 0,
    };
    let shuf = vec!["/a/3".to_owned(), "/a/1".to_owned(), "/a/2".to_owned()];
    let orig = vec![
        "/a/1".to_owned(),
        "/a/2".to_owned(),
        "/a/3".to_owned(),
        "/b/1".to_owned(),
    ];
    let mut q2 = PlaybackQueue::default();
    q2.restore_session(&map, &state, &shuf, Some(&orig));
    assert_eq!(q2.all_track_ids().len(), 4);
    assert_eq!(q2.to_session(12.0).2, orig);
    q2.set_shuffle(false);
    assert_eq!(ids(&q2), vec!["/a/1", "/a/2", "/a/3", "/b/1"]);
    assert_eq!(q2.index(), Some(2));
    assert!(q2.to_session(0.0).2.is_empty());
}

#[test]
fn repeat_modes_wrap_shuffle_and_preserve_hand_queue() {
    assert_eq!(RepeatMode::Off.icon_name(), "repeat");
    assert!(!RepeatMode::Off.is_active());
    assert_eq!(RepeatMode::All.icon_name(), "repeat");
    assert_eq!(RepeatMode::One.icon_name(), "repeat1");
    assert_eq!(RepeatMode::Off.cycle().cycle().cycle(), RepeatMode::Off);

    let mut q = PlaybackQueue::default();
    q.set_queue(
        vec![mk_track("/m/1", "A", 100.0), mk_track("/m/2", "B", 100.0)],
        Some(1),
    );
    assert_eq!(q.auto_next_index(), None);
    assert!(q.step_next(true).is_none());

    q.set_repeat(RepeatMode::All);
    assert_eq!(q.auto_next_index(), Some(0));
    assert_eq!(q.step_next(true).unwrap().id, "/m/1");

    // Reshuffle after wrap keeps `tracks[0]` playing and shuffles the remaining round.
    q.set_queue(mk_tracks("/s/", 12, "S"), Some(0));
    q.set_shuffle(true);
    let first_id = q.tracks()[0].id.clone();
    let before_tail: Vec<String> = ids(&q)[1..].iter().map(|s| (*s).to_owned()).collect();
    q.reshuffle_after_wrap();
    assert_eq!(q.tracks()[0].id, first_id);
    let after_tail: Vec<String> = ids(&q)[1..].iter().map(|s| (*s).to_owned()).collect();
    assert_ne!(before_tail, after_tail);

    // Repeat One preserves hand queue and blocks crossfade to self.
    q.set_shuffle(false);
    q.enqueue(vec![mk_track("/m/hand", "H", 100.0)]);
    q.set_repeat(RepeatMode::One);
    assert_eq!(q.step_next(true).unwrap().id, first_id);
    assert_eq!(q.hand_len(), 1);
    assert!(
        matches!(q.on_mpv_advanced(&first_id, Some(0)), MpvAdvance::Followed(t) if t.id == first_id)
    );
    assert!(!q.crossfade_applies(0, 5, false));
}

#[test]
fn hand_queue_shuffle_sync_and_shared_arc() {
    let lib = library();
    let mut q = PlaybackQueue::default();
    assert!(q.enqueue(vec![mk_track("/init", "I", 200.0)]));
    assert_eq!(q.hand_len(), 0);

    q.set_queue(lib[0].tracks.clone(), Some(0));
    let shared = q.shared_tracks();
    assert!(!q.enqueue(vec![mk_track("/x/1", "X", 200.0)]));
    assert!(!Arc::ptr_eq(&shared, &q.shared_tracks()));
    q.play_next(vec![mk_track("/y/1", "Y", 200.0)]);
    assert_eq!(upcoming(&q), vec!["/y/1", "/x/1", "/a/2", "/a/3", "/a/4"]);

    let ui = q.shared_tracks();
    q.step_next(false);
    assert_eq!(q.current().unwrap().id, "/y/1");
    assert_eq!(q.hand_len(), 1);
    assert!(Arc::ptr_eq(&ui, &q.shared_tracks()));

    q.set_shuffle(true);
    q.play_next(vec![mk_track("/z/1", "Z", 200.0)]);
    q.set_shuffle(false);
    assert_eq!(ids(&q)[..3], ["/a/1", "/y/1", "/z/1"]);
    q.jump_to(q.tracks().len() - 1);
    assert_eq!(q.hand_len(), 0);
}

#[test]
fn remove_at_and_remove_tracks_by_id_handle_last_track_duplicates_and_hand_line() {
    let mut q = PlaybackQueue::default();
    let dup = mk_track("/d/1", "D", 200.0);
    q.set_queue(
        vec![
            mk_track("/a/1", "A", 200.0),
            dup.clone(),
            mk_track("/a/2", "A", 200.0),
            dup,
        ],
        Some(0),
    );
    q.set_shuffle(true);
    let first_dup = q.tracks().iter().position(|t| t.id == "/d/1").unwrap();
    assert!(q.remove_at(first_dup));
    q.set_shuffle(false);
    assert_eq!(ids(&q), vec!["/a/1", "/d/1", "/a/2"]);

    // Removing the last track while playing stops (`index = None`) instead of rewinding.
    q.jump_to(2);
    assert!(q.remove_at(2));
    assert_eq!(q.index(), None);
    assert!(!q.remove_at(99));

    q.set_queue(mk_tracks("/t/", 4, "X"), Some(1));
    q.enqueue(vec![
        mk_track("/t/3", "Y", 200.0),
        mk_track("/h/2", "Y", 200.0),
    ]);
    let drop: HashSet<String> = ["/t/1", "/t/3"].into_iter().map(str::to_owned).collect();
    let (changed, cur_removed) = q.remove_tracks_by_id(&drop);
    assert!(changed && !cur_removed && q.index() == Some(0) && q.hand_len() == 1);
    let drop_cur: HashSet<String> = ["/t/2"].into_iter().map(str::to_owned).collect();
    assert_eq!(q.remove_tracks_by_id(&drop_cur), (true, true));
    assert_eq!(q.index(), None);
}

#[test]
fn play_tracker_ignores_seeks_supports_reset_at_and_counts_short_tracks() {
    let mut tracker = PlayTracker::default();
    tracker.reset(Some("/m/song".to_owned()));
    assert_eq!(tracker.on_time_pos(0.0, 180.0, true), None);
    assert_eq!(tracker.on_time_pos(150.0, 180.0, true), None);
    let hits: usize = (1..=92)
        .filter(|&s| tracker.on_time_pos(150.0 + s as f64, 180.0, true).is_some())
        .count();
    assert_eq!(hits, 1);

    // Resume mid-track must NOT count 0..60s.
    tracker.reset_at(Some("/m/resume".to_owned()), 60.0, 0.0);
    assert_eq!(tracker.on_time_pos(61.0, 100.0, true), None);
    assert!(tracker.heard_seconds < 2.0);

    // Crossfade carries initial heard credit so short incoming tracks finish honestly.
    tracker.reset_at(Some("/m/short".to_owned()), 5.0, 5.0);
    assert_eq!(tracker.on_time_pos(9.5, 10.0, true), None); // delta 4.5s ignored, still at 5s
    assert_eq!(tracker.on_time_pos(10.0, 10.0, true), None); // +0.5 -> 5.5s, needs >=9.0s

    let mut short = PlayTracker::default();
    short.reset(Some("/m/short2".to_owned()));
    let mut fired = false;
    let mut t = 0.3;
    while t < 10.0 {
        fired |= short.on_time_pos(t, 10.0, true).is_some();
        t += 0.5;
    }
    assert!(fired);
}

#[test]
fn enqueue_racing_mpv_advance_and_jump_near() {
    let mut q = PlaybackQueue::default();
    q.set_queue(
        vec![mk_track("A", "X", 100.0), mk_track("B", "X", 100.0)],
        Some(0),
    );
    q.enqueue(vec![mk_track("C", "Y", 100.0)]);
    match q.on_mpv_advanced("B", q.auto_next_index()) {
        MpvAdvance::Reload(t) => assert_eq!(t.id, "C"),
        other => panic!("expected Reload(C), got {other:?}"),
    }
    // Queue is now [A, C, B]: C took the hand slot and is current, B follows.
    assert_eq!(q.current().unwrap().id, "C");
    assert_eq!(q.hand_len(), 0);
    assert_eq!(q.jump_to_track_near(0, "B"), Some(2));
    assert!(matches!(q.on_mpv_advanced("Z", None), MpvAdvance::Stop));
}
