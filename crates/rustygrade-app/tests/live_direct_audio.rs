//! Live hardware integration test for direct ALSA USB DAC switching.
//! Run with:
//! `RETRO_LIVE_DAC=alsa/hw:CARD=Pro,DEV=0 RETRO_LIVE_SHARED=pipewire/<sink> RETRO_LIVE_FILE=<silent.flac> cargo test --test live_direct_audio -- --ignored --nocapture`

use std::time::Duration;

use rustygrade_audio::{AO_INIT_FAILED, AlsaDirect, AlsaMixer, EngineEvent, MpvEngine};
use rustygrade_linux::DeviceReservation;
use tokio::sync::mpsc;

const CARD_FREE_POLL: Duration = Duration::from_millis(50);
const CARD_FREE_POLLS: u32 = 50;
const CARD_FREE_STABLE: u32 = 3;
const LEAVE_DIRECT_SETTLE: Duration = Duration::from_millis(1000);

async fn wait_for_card_free(device: &str) -> Result<(), Vec<String>> {
    let mut closed_for = 0;
    for _ in 0..CARD_FREE_POLLS {
        let dev = device.to_owned();
        let open = tokio::task::spawn_blocking(move || AlsaDirect::pcm_is_open(&dev))
            .await
            .ok()
            .flatten();
        match open {
            Some(true) => closed_for = 0,
            _ => {
                closed_for += 1;
                if closed_for >= CARD_FREE_STABLE {
                    return Ok(());
                }
            }
        }
        tokio::time::sleep(CARD_FREE_POLL).await;
    }
    let dev = device.to_owned();
    let still = tokio::task::spawn_blocking(move || AlsaDirect::pcm_holders(&dev))
        .await
        .unwrap_or_default();
    if still.is_empty() { Ok(()) } else { Err(still) }
}

#[test]
#[ignore = "Needs a free USB DAC and its PipeWire sink; plays digital silence"]
fn live_direct_shared_direct_on_one_track() {
    let direct = std::env::var("RETRO_LIVE_DAC").expect("RETRO_LIVE_DAC");
    let shared = std::env::var("RETRO_LIVE_SHARED").expect("RETRO_LIVE_SHARED");
    let file = std::env::var("RETRO_LIVE_FILE").expect("RETRO_LIVE_FILE");
    let card = AlsaDirect::card_of(&direct).expect("card");
    assert_eq!(
        AlsaDirect::pcm_is_open(&direct),
        Some(false),
        "the DAC is in use"
    );

    struct Restore(i32, Option<(String, f64)>);
    impl Drop for Restore {
        fn drop(&mut self) {
            if let Some((c, lvl)) = &self.1 {
                AlsaMixer::write_level(self.0, c, *lvl);
            }
        }
    }
    let control = AlsaMixer::find_control(card);
    let saved = control
        .as_deref()
        .and_then(|c| AlsaMixer::read_level(card, c).map(|l| (c.to_owned(), l)));
    let _restore = Restore(card, saved);
    if let Some(c) = control.as_deref() {
        AlsaMixer::write_level(card, c, 0.0);
    }

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        async fn watch(rx: &mut mpsc::UnboundedReceiver<EngineEvent>, ms: u64) -> (bool, bool) {
            let (mut failed, mut opened) = (false, false);
            let end = tokio::time::Instant::now() + Duration::from_millis(ms);
            while let Ok(Some(e)) = tokio::time::timeout_at(end, rx.recv()).await {
                match e {
                    EngineEvent::PlaybackFailed { reason, .. } => {
                        eprintln!("  PlaybackFailed: {reason}");
                        failed |= reason == AO_INIT_FAILED;
                    }
                    EngineEvent::AudioPathChanged(ap) => opened |= ap.output.is_some(),
                    EngineEvent::EndOfFile => eprintln!("  EndOfFile"),
                    _ => {}
                }
            }
            (failed, opened)
        }

        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut mpv = MpvEngine::new(tx);
        mpv.start(Some(&shared), Some("no")).expect("libmpv");
        mpv.set_volume(1.0);
        mpv.play(&file, 0.0);
        let (_, opened) = watch(&mut rx, 2500).await;
        assert!(opened, "shared output did not open");

        if std::env::var("RETRO_LIVE_SKIP_NAIVE").is_err() {
            let res = DeviceReservation::acquire(card, "Rustygrade live test")
                .await
                .expect("reserve");
            mpv.set_audio_device(&direct);
            let (naive_failed, _) = watch(&mut rx, 2000).await;
            eprintln!("old switch failed with '{AO_INIT_FAILED}': {naive_failed}");

            mpv.set_audio_device(&shared);
            res.release().await;
            tokio::time::sleep(LEAVE_DIRECT_SETTLE).await;
            mpv.reload_audio();
            if naive_failed {
                mpv.play(&file, 3.0);
            }
            let (_, opened) = watch(&mut rx, 2500).await;
            assert!(opened, "shared output did not reopen");
        }

        let res = DeviceReservation::acquire(card, "Rustygrade live test")
            .await
            .expect("reserve");
        let free = wait_for_card_free(&direct).await;
        mpv.set_audio_device(&direct);
        let (failed, opened) = watch(&mut rx, 2500).await;
        let hw = AlsaDirect::read_hw_params(&direct);
        mpv.shutdown();
        res.release().await;
        assert!(free.is_ok());
        assert!(
            !failed && opened && hw.is_some(),
            "direct output did not open"
        );
    });
}
