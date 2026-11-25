use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use rustygrade_audio::alsa_direct::AudioFormat;
use rustygrade_audio::mpv_engine::{EngineEvent, MpvEngine};
use tokio::sync::mpsc;

#[test]
#[ignore = "Requires libmpv and ffmpeg installed on host"]
fn live_silent_flac_has_no_sample_alteration() {
    let path: PathBuf = std::env::temp_dir().join(format!(
        "rustygrade-silent-test-{}.flac",
        std::process::id()
    ));
    let path_str = path.to_str().expect("UTF-8 temp path");
    let ffmpeg_status = Command::new("ffmpeg")
        .args([
            "-y",
            "-f",
            "lavfi",
            "-i",
            "anullsrc=r=44100:cl=stereo",
            "-t",
            "1",
            "-c:a",
            "flac",
            path_str,
        ])
        .status()
        .expect("run ffmpeg");
    assert!(
        ffmpeg_status.success(),
        "ffmpeg failed generating silent FLAC"
    );

    struct Cleanup(PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    let _cleanup = Cleanup(path.clone());

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut engine = MpvEngine::new(tx);
        engine
            .start(Some("null"), Some("no"))
            .expect("start libmpv with ao=null");
        engine.set_volume(1.0);
        engine.play(path_str, 0.0);

        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let mut input_fmt: Option<AudioFormat> = None;
        while tokio::time::Instant::now() < deadline {
            if let Ok(Some(EngineEvent::AudioPathChanged(ap))) =
                tokio::time::timeout(Duration::from_millis(250), rx.recv()).await
                && let Some(inp) = ap.input
            {
                input_fmt = Some(inp);
                break;
            }
        }
        let fmt = input_fmt.expect("expected AudioPathChanged input format for silent FLAC");
        assert_eq!(fmt.rate, 44100);
        assert_eq!(fmt.channels, 2);

        let altered = engine.get_sample_alteration().await;
        assert_eq!(
            altered, None,
            "unexpected mpv sample alteration: {altered:?}"
        );
        engine.shutdown();
    });
}

#[cfg(feature = "pipewire-bridge")]
#[test]
#[ignore = "Requires live PipeWire daemon and wpctl CLI"]
fn live_pipewire_bridge_syncs_two_way_and_cleans_up_on_drop() {
    use rustygrade_audio::pipewire_bridge::{BridgeEvent, PipeWireBridge};
    use std::time::Instant;

    let (bridge, mut rx) =
        PipeWireBridge::connect("Test DAC (Direct)", 0.50, false).expect("connect PipeWire");

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(5);
    let node_id = loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(
            !remaining.is_zero(),
            "timed out waiting for BridgeEvent::Ready"
        );
        let evt = rt
            .block_on(async { tokio::time::timeout(remaining, rx.recv()).await })
            .expect("timeout")
            .expect("channel closed");
        if let BridgeEvent::Ready { node_id, .. } = evt {
            break node_id;
        }
    };

    let mut set_ok = false;
    for _ in 0..10 {
        let status = Command::new("wpctl")
            .args(["set-volume", &node_id.to_string(), "0.70"])
            .status()
            .expect("run wpctl set-volume");
        if status.success() {
            set_ok = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(set_ok, "wpctl set-volume failed for node {node_id}");

    let deadline = Instant::now() + Duration::from_secs(3);
    let got_70 = loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break false;
        }
        if let Ok(Some(evt)) =
            rt.block_on(async { tokio::time::timeout(remaining, rx.recv()).await })
            && let BridgeEvent::VolumeChanged(v) = evt
            && (v - 0.70).abs() < 0.02
        {
            break true;
        }
    };
    assert!(
        got_70,
        "expected VolumeChanged(~0.70) from wpctl set-volume"
    );

    bridge.set_volume(0.35);
    std::thread::sleep(Duration::from_millis(250));
    let inspect = Command::new("wpctl")
        .args(["get-volume", &node_id.to_string()])
        .output()
        .expect("wpctl get-volume");
    let text = String::from_utf8_lossy(&inspect.stdout);
    assert!(
        text.contains("0.35"),
        "expected wpctl get-volume to show ~0.35, got: {text}"
    );

    let inspect_pw = Command::new("pw-dump")
        .arg(node_id.to_string())
        .output()
        .expect("pw-dump");
    let pw_text = String::from_utf8_lossy(&inspect_pw.stdout);
    assert!(
        pw_text.contains("0.0428") || pw_text.contains("0.0429"),
        "expected pw-dump channelVolumes near 0.042875, got: {pw_text}"
    );

    drop(bridge);
    std::thread::sleep(Duration::from_millis(250));
    let status_after = Command::new("wpctl")
        .args(["inspect", &node_id.to_string()])
        .output()
        .expect("wpctl inspect after drop");
    assert!(
        !status_after.status.success(),
        "expected virtual node {node_id} to disappear after drop"
    );
}
