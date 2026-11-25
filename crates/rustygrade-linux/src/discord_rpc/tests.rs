use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::AsyncReadExt;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, oneshot};
use tokio::time::{Instant, timeout};

use crate::artwork_lookup::{ArtworkProviders, ArtworkQuery, ArtworkResolver, LookupOutcome};

use super::ipc::{
    MAX_FRAME_LEN, OP_CLOSE, OP_FRAME, OP_HANDSHAKE, OP_PING, OP_PONG, try_pop_frame,
    write_json_frame, write_raw_frame,
};
use super::{
    DEFAULT_DISCORD_CLIENT_ID, DiscordPlaybackStatus, DiscordPresence, DiscordRpcService,
    DiscordRpcStatus, adjusted_presence, build_set_activity_payload, clamp_discord_str,
    is_valid_client_id, resolve_client_id_with_env, should_send_presence,
};

static TEST_SOCK_SEQ: AtomicU64 = AtomicU64::new(1);

struct MockDiscordServer {
    sock_path: PathBuf,
    listener: UnixListener,
}

impl MockDiscordServer {
    fn bind() -> Self {
        let seq = TEST_SOCK_SEQ.fetch_add(1, Ordering::Relaxed);
        let sock_path =
            std::env::temp_dir().join(format!("rg-drpc-{}-{seq}.sock", std::process::id()));
        let _ = std::fs::remove_file(&sock_path);
        let listener = UnixListener::bind(&sock_path).unwrap();
        Self {
            sock_path,
            listener,
        }
    }

    fn path(&self) -> PathBuf {
        self.sock_path.clone()
    }
}

impl Drop for MockDiscordServer {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.sock_path);
    }
}

fn sample_presence(status: DiscordPlaybackStatus) -> DiscordPresence {
    DiscordPresence {
        status,
        title: "Control's Wishes".to_owned(),
        artist: "Robert del Naja".to_owned(),
        album: "Lone Trail OST".to_owned(),
        position_s: 42.2,
        duration_s: 215.4,
        artwork_url: None,
    }
}

async fn read_mock_frame(sock: &mut UnixStream) -> Option<(u32, Vec<u8>)> {
    let mut hdr = [0u8; 8];
    sock.read_exact(&mut hdr).await.ok()?;
    let op = u32::from_le_bytes(hdr[0..4].try_into().unwrap());
    let len = u32::from_le_bytes(hdr[4..8].try_into().unwrap()) as usize;
    let mut body = vec![0u8; len];
    sock.read_exact(&mut body).await.ok()?;
    Some((op, body))
}

async fn accept_ready(listener: &UnixListener) -> UnixStream {
    let (mut srv, _) = listener.accept().await.unwrap();
    let (op, _) = read_mock_frame(&mut srv).await.unwrap();
    assert_eq!(op, OP_HANDSHAKE);
    assert!(
        write_json_frame(
            &mut srv,
            OP_FRAME,
            &json!({ "cmd": "DISPATCH", "evt": "READY", "data": {} })
        )
        .await
    );
    srv
}

struct MockArtworkResolver {
    delay: Duration,
    calls: AtomicUsize,
    handler: Box<dyn Fn(&ArtworkQuery) -> LookupOutcome + Send + Sync>,
}

impl ArtworkResolver for MockArtworkResolver {
    fn resolve(&self, query: &ArtworkQuery, cancel: &AtomicBool) -> LookupOutcome {
        if cancel.load(Ordering::Relaxed) {
            return LookupOutcome::TransientError;
        }
        self.calls.fetch_add(1, Ordering::Relaxed);
        if !self.delay.is_zero() {
            std::thread::sleep(self.delay);
        }
        (self.handler)(query)
    }
}

#[test]
fn payload_clamping_dedup_adjustment_client_id_and_frame_limits() {
    let url = "https://is1-ssl.mzstatic.com/image/thumb/a/512x512bb.jpg".to_owned();
    let playing = build_set_activity_payload(
        99,
        1,
        &DiscordPresence {
            artwork_url: Some(url.clone()),
            ..sample_presence(DiscordPlaybackStatus::Playing)
        },
        1_700_000_100,
    );
    let act = &playing["args"]["activity"];
    assert_eq!(act["type"], 2);
    assert_eq!(act["timestamps"]["start"], 1_700_000_058);
    assert_eq!(act["timestamps"]["end"], 1_700_000_273);
    assert_eq!(act["assets"]["large_image"], url);
    assert_eq!(act["assets"]["small_image"], "retrograde");

    let untrusted = build_set_activity_payload(
        99,
        2,
        &DiscordPresence {
            artwork_url: Some("https://evil.example.com/bad.jpg".to_owned()),
            ..sample_presence(DiscordPlaybackStatus::Playing)
        },
        1_700_000_100,
    );
    assert_eq!(
        untrusted["args"]["activity"]["assets"]["large_image"],
        "retrograde"
    );
    assert!(
        untrusted["args"]["activity"]["assets"]
            .get("small_image")
            .is_none()
    );

    let paused = build_set_activity_payload(
        99,
        3,
        &sample_presence(DiscordPlaybackStatus::Paused),
        1_700_000_100,
    );
    assert_eq!(
        paused["args"]["activity"]["state"],
        "Robert del Naja • Paused"
    );
    assert!(paused["args"]["activity"].get("timestamps").is_none());

    let stopped = build_set_activity_payload(
        99,
        4,
        &sample_presence(DiscordPlaybackStatus::Stopped),
        1_700_000_100,
    );
    assert!(stopped["args"]["activity"].is_null());

    // UTF-16 clamp & 2-char floor
    assert_eq!(clamp_discord_str("A", "Fallback", 128).chars().count(), 2);
    assert_eq!(
        clamp_discord_str("   ", "Unknown Track", 128),
        "Unknown Track"
    );
    let emojis = "🎵".repeat(80);
    let truncated = clamp_discord_str(&emojis, "Fallback", 128);
    assert!(truncated.encode_utf16().count() <= 128 && truncated.ends_with('…'));

    // Dedup + duplicate Stopped suppression
    let base = Instant::now();
    let p0 = sample_presence(DiscordPlaybackStatus::Playing);
    assert!(should_send_presence(None, &p0, base));
    let last = (p0.clone(), base);
    assert!(!should_send_presence(
        Some(&last),
        &DiscordPresence {
            position_s: p0.position_s + 2.2,
            ..p0.clone()
        },
        base + Duration::from_secs(2)
    ));
    assert!(should_send_presence(
        Some(&last),
        &DiscordPresence {
            position_s: p0.position_s + 10.0,
            ..p0.clone()
        },
        base + Duration::from_secs(1)
    ));
    assert!(!should_send_presence(
        Some(&(sample_presence(DiscordPlaybackStatus::Stopped), base)),
        &DiscordPresence {
            title: "Other".into(),
            ..sample_presence(DiscordPlaybackStatus::Stopped)
        },
        base + Duration::from_secs(1),
    ));

    // Adjusted presence & snowflake client ID & frame limit
    let adv = adjusted_presence(&p0, base, base + Duration::from_secs(60));
    assert!((adv.position_s - 102.2).abs() < 0.01);
    assert!(!is_valid_client_id("1234567890123456") && is_valid_client_id("12345678901234567"));
    assert_eq!(
        resolve_client_id_with_env("  ", None),
        DEFAULT_DISCORD_CLIENT_ID
    );
    assert_eq!(
        resolve_client_id_with_env("987654321012345678", Some("111111111111111111")),
        "111111111111111111"
    );

    let mut buf = Vec::new();
    buf.extend_from_slice(&OP_FRAME.to_le_bytes());
    buf.extend_from_slice(&((MAX_FRAME_LEN as u32) + 1).to_le_bytes());
    assert!(try_pop_frame(&mut buf).is_err());
}

#[tokio::test]
async fn roundtrip_ipc_pipelined_pong_activity_and_shutdown() {
    let mock_srv = MockDiscordServer::bind();
    let path = mock_srv.path();
    let (pong_tx, pong_rx) = oneshot::channel();

    let server = tokio::spawn(async move {
        let (mut srv, _) = mock_srv.listener.accept().await.unwrap();
        let (op, body) = read_mock_frame(&mut srv).await.unwrap();
        assert_eq!(op, OP_HANDSHAKE);
        let hs: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(hs["client_id"], DEFAULT_DISCORD_CLIENT_ID);

        // Send READY and a pipelined PING immediately in sequence to verify buffer preservation.
        assert!(
            write_json_frame(
                &mut srv,
                OP_FRAME,
                &json!({ "cmd": "DISPATCH", "evt": "READY", "data": {} })
            )
            .await
        );
        assert!(write_raw_frame(&mut srv, OP_PING, b"ping-early").await);
        let (op, body) = read_mock_frame(&mut srv).await.unwrap();
        assert_eq!((op, body.as_slice()), (OP_PONG, b"ping-early".as_slice()));

        let (op, body) = read_mock_frame(&mut srv).await.unwrap();
        assert_eq!(op, OP_FRAME);
        let act: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(act["args"]["activity"]["details"], "Control's Wishes");
        let _ = pong_tx.send(());

        let (op, body) = read_mock_frame(&mut srv).await.unwrap();
        assert_eq!(op, OP_FRAME);
        assert!(serde_json::from_slice::<Value>(&body).unwrap()["args"]["activity"].is_null());
        let (op, _) = read_mock_frame(&mut srv).await.unwrap();
        assert_eq!(op, OP_CLOSE);
    });

    let (status_tx, mut status_rx) = mpsc::unbounded_channel();
    let rpc = DiscordRpcService::start_with_sockets(
        DEFAULT_DISCORD_CLIENT_ID.to_owned(),
        status_tx,
        Some(vec![path]),
    );
    assert_eq!(
        timeout(Duration::from_secs(2), status_rx.recv())
            .await
            .unwrap()
            .unwrap(),
        DiscordRpcStatus::Connected
    );

    rpc.update_presence(sample_presence(DiscordPlaybackStatus::Playing));
    timeout(Duration::from_secs(2), pong_rx)
        .await
        .unwrap()
        .unwrap();
    rpc.shutdown().await;
    timeout(Duration::from_secs(2), server)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn handshake_rejections_emit_rejected_and_recover_on_new_id() {
    let mock_srv = MockDiscordServer::bind();
    let path = mock_srv.path();
    let (s2_done_tx, s2_done_rx) = oneshot::channel();

    let server = tokio::spawn(async move {
        // Attempt 1: ERROR frame
        let (mut s1, _) = mock_srv.listener.accept().await.unwrap();
        let _ = read_mock_frame(&mut s1).await.unwrap();
        assert!(write_json_frame(&mut s1, OP_FRAME, &json!({ "cmd": "DISPATCH", "evt": "ERROR", "data": { "code": 4002, "message": "Invalid Client ID" } })).await);
        drop(s1);

        // Attempt 2: OP_CLOSE 4000
        let (mut s2, _) = mock_srv.listener.accept().await.unwrap();
        let _ = read_mock_frame(&mut s2).await.unwrap();
        assert!(
            write_json_frame(
                &mut s2,
                OP_CLOSE,
                &json!({ "code": 4000, "message": "Invalid Client ID" })
            )
            .await
        );
        drop(s2);
        let _ = s2_done_tx.send(());

        // Attempt 3: valid client_id -> READY
        let mut s3 = accept_ready(&mock_srv.listener).await;
        let _ = read_mock_frame(&mut s3).await;
    });

    let (status_tx, mut status_rx) = mpsc::unbounded_channel();
    let rpc = DiscordRpcService::start_with_sockets(
        "88888888888888888".to_owned(),
        status_tx,
        Some(vec![path]),
    );
    assert_eq!(
        timeout(Duration::from_secs(2), status_rx.recv())
            .await
            .unwrap()
            .unwrap(),
        DiscordRpcStatus::Rejected
    );

    rpc.set_client_id("99999999999999999".to_owned());
    timeout(Duration::from_secs(2), s2_done_rx)
        .await
        .unwrap()
        .unwrap();
    // Same Rejected state stays deduplicated on Attempt 2; changing to a valid ID emits Connected.
    rpc.set_client_id("12345678901234567".to_owned());
    assert_eq!(
        timeout(Duration::from_secs(2), status_rx.recv())
            .await
            .unwrap()
            .unwrap(),
        DiscordRpcStatus::Connected
    );
    rpc.shutdown().await;
    timeout(Duration::from_secs(2), server)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn async_artwork_caches_ignores_stale_toggles_and_prunes_providers() {
    let mock_srv = MockDiscordServer::bind();
    let path = mock_srv.path();
    let it_url = "https://is1-ssl.mzstatic.com/image/thumb/x/512x512bb.jpg".to_owned();
    let dz_url =
        "https://cdn-images.dzcdn.net/images/cover/dz123/500x500-000000-80-0-0.jpg".to_owned();
    let expected_it = it_url.clone();
    let expected_dz = dz_url.clone();
    let mode = Arc::new(AtomicUsize::new(0));
    let mode_cb = Arc::clone(&mode);

    let mock = Arc::new(MockArtworkResolver {
        delay: Duration::from_millis(15),
        calls: AtomicUsize::new(0),
        handler: Box::new(move |q| match mode_cb.load(Ordering::Relaxed) {
            1 => LookupOutcome::Found(dz_url.clone()),
            _ if q.album == "Album B" => LookupOutcome::Found(
                "https://is1-ssl.mzstatic.com/image/thumb/b/512x512bb.jpg".into(),
            ),
            _ => LookupOutcome::Found(it_url.clone()),
        }),
    });

    let (phase1_tx, phase1_rx) = oneshot::channel();
    let (phase2_tx, phase2_rx) = oneshot::channel();
    let (phase3_tx, phase3_rx) = oneshot::channel();

    let server = tokio::spawn(async move {
        let mut srv = accept_ready(&mock_srv.listener).await;
        let mut phase1_tx = Some(phase1_tx);
        let mut phase2_tx = Some(phase2_tx);
        let mut phase3_tx = Some(phase3_tx);
        let mut stage = 0u8;
        while let Some((_, body)) = read_mock_frame(&mut srv).await {
            let act: Value = serde_json::from_slice(&body).unwrap();
            let details = act["args"]["activity"]["details"].as_str().unwrap_or("");
            let large = act["args"]["activity"]["assets"]["large_image"]
                .as_str()
                .unwrap_or("");
            if stage == 0 && details == "Control's Wishes" && large == expected_it {
                stage = 1;
                if let Some(tx) = phase1_tx.take() {
                    let _ = tx.send(());
                }
            } else if stage == 1 && details == "Second Track" && large == expected_it {
                stage = 2;
                if let Some(tx) = phase2_tx.take() {
                    let _ = tx.send(());
                }
            } else if stage == 2 && large == expected_dz {
                if let Some(tx) = phase3_tx.take() {
                    let _ = tx.send(());
                }
                break;
            }
        }
        while read_mock_frame(&mut srv).await.is_some() {}
    });

    let (status_tx, mut status_rx) = mpsc::unbounded_channel();
    let rpc = DiscordRpcService::start_with_artwork_for_test(
        DEFAULT_DISCORD_CLIENT_ID.to_owned(),
        true,
        None,
        mock.clone(),
        status_tx,
        Some(vec![path]),
    );
    assert_eq!(
        timeout(Duration::from_secs(2), status_rx.recv())
            .await
            .unwrap()
            .unwrap(),
        DiscordRpcStatus::Connected
    );

    rpc.update_presence(sample_presence(DiscordPlaybackStatus::Playing));
    timeout(Duration::from_secs(2), phase1_rx)
        .await
        .unwrap()
        .unwrap();

    // Wait just past the 10 ms test coalesce window, then play track 2 on same album (cache hit).
    tokio::time::sleep(Duration::from_millis(20)).await;
    rpc.update_presence(DiscordPresence {
        title: "Second Track".into(),
        position_s: 0.0,
        ..sample_presence(DiscordPlaybackStatus::Playing)
    });
    timeout(Duration::from_secs(2), phase2_rx)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(mock.calls.load(Ordering::Relaxed), 1);

    // Switch provider to Deezer -> drops cached iTunes URL & refetches via mock mode=1.
    mode.store(1, Ordering::Relaxed);
    tokio::time::sleep(Duration::from_millis(20)).await;
    rpc.set_artwork_providers(ArtworkProviders {
        itunes: false,
        deezer: true,
        musicbrainz: false,
        discogs: false,
        discogs_token: String::new(),
    });
    timeout(Duration::from_secs(2), phase3_rx)
        .await
        .unwrap()
        .unwrap();

    rpc.shutdown().await;
    timeout(Duration::from_secs(2), server)
        .await
        .unwrap()
        .unwrap();
}
