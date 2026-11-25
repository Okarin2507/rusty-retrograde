use std::time::{Duration, Instant};

use rustygrade_audio::{
    AlsaDirect, AlsaMixer, AudioDevice, BRIDGE_NODE_NAME, BridgeEvent, DirectDevice,
    EqualizerConfig, MpvEngine, PipeWireBridge, evaluate_bit_perfect,
};
use rustygrade_core::DirectVolumeBridge;
use rustygrade_linux::{DeviceReservation, ReservationError};
use tracing::{info, warn};

use super::Coordinator;
use super::types::{ControllerEvent, DeviceOption, NoticeKind};

const CARD_FREE_POLL: Duration = Duration::from_millis(50);
const CARD_FREE_POLLS: u32 = 50;
const CARD_FREE_STABLE: u32 = 3;
const LEAVE_DIRECT_SETTLE: Duration = Duration::from_millis(1000);
pub(super) const OUTPUT_RETRIES: u32 = 3;
pub(super) const OUTPUT_RETRY_BASE: Duration = Duration::from_millis(150);

pub(super) fn saved_device_replacement(saved: &str) -> Option<String> {
    let bridge = saved.split_once('/').is_some_and(|(backend, node)| {
        matches!(backend, "pipewire" | "pulse")
            && (node == BRIDGE_NODE_NAME || node == "retrograde-direct-bridge")
    });
    if bridge {
        return Some("auto".to_owned());
    }
    if AlsaDirect::is_direct(saved)
        && let Some(card) = AlsaDirect::card_of(saved)
        && !AlsaDirect::is_usb_card(card)
    {
        return Some("auto".to_owned());
    }
    None
}

pub(super) fn direct_failure_message(holders: &[String]) -> String {
    if holders.is_empty() {
        "DIRECT_OUTPUT // THE CARD COULD NOT BE OPENED. PRESS PLAY TO RETRY".to_owned()
    } else {
        format!(
            "DIRECT_OUTPUT // CARD BUSY: IN USE BY {}. PRESS PLAY TO RETRY",
            holders.join(", ").to_uppercase()
        )
    }
}

/// Builds deduplicated OUTPUT menu entries: System Default (`auto`), shared sinks, then direct DACs.
pub(crate) fn build_device_options(
    mpv_devs: &[AudioDevice],
    direct_devs: &[DirectDevice],
    selected: &str,
) -> Vec<DeviceOption> {
    let mut opts = vec![DeviceOption {
        id: "auto".to_owned(),
        label: "System Default".to_owned(),
        is_direct: false,
    }];
    let mut shared: Vec<(String, u8, DeviceOption)> = Vec::new();
    for d in mpv_devs {
        let Some((backend, node)) = d.name.split_once('/') else {
            continue;
        };
        if !matches!(backend, "pipewire" | "pulse")
            || node.is_empty()
            || node == BRIDGE_NODE_NAME
            || node == "retrograde-direct-bridge"
        {
            continue;
        }
        let rank = if d.name == selected {
            0
        } else if backend == "pipewire" {
            1
        } else {
            2
        };
        let opt = DeviceOption {
            id: d.name.clone(),
            label: d.description.clone(),
            is_direct: false,
        };
        match shared.iter_mut().find(|(n, _, _)| n == node) {
            Some(entry) if rank < entry.1 => *entry = (node.to_owned(), rank, opt),
            Some(_) => {}
            None => shared.push((node.to_owned(), rank, opt)),
        }
    }
    for (_, _, opt) in shared {
        let prefix = format!("{} #", opt.label);
        let same = opts
            .iter()
            .filter(|o| o.label == opt.label || o.label.starts_with(&prefix))
            .count();
        let label = if same > 0 {
            format!("{} #{}", opt.label, same + 1)
        } else {
            opt.label
        };
        opts.push(DeviceOption { label, ..opt });
    }
    for d in direct_devs {
        if !opts.iter().any(|o| o.id == d.name) {
            opts.push(DeviceOption::direct(d.clone()));
        }
    }
    opts
}

impl Coordinator {
    pub(super) async fn initialize_audio_output(&mut self) {
        let saved = self.settings.audio_device.clone();
        let replacement = tokio::task::spawn_blocking(move || saved_device_replacement(&saved))
            .await
            .ok()
            .flatten();
        if let Some(next) = replacement {
            info!(
                next,
                "Saved output is not offered any more; using system default"
            );
            self.settings.audio_device = next;
            self.save_settings_async();
            let _ = self
                .ui_tx
                .send(ControllerEvent::SettingsSaved(self.settings.clone()));
        }
        let target = self.settings.audio_device.clone();
        if AlsaDirect::is_direct(&target) && !self.enter_direct_mode(&target, false, false).await {
            self.output_override = Some("auto".to_owned());
        }
        self.start_mpv_engine();
    }

    pub(super) fn ensure_mpv_running(&mut self) -> bool {
        if self.mpv.is_running() {
            return true;
        }
        info!("libmpv is not running; starting it again");
        self.start_mpv_engine()
    }

    pub(super) fn mpv_device(&self) -> String {
        self.output_override
            .clone()
            .unwrap_or_else(|| self.settings.audio_device.clone())
    }

    fn start_mpv_engine(&mut self) -> bool {
        let target = self.mpv_device();
        let rg = self.settings.replay_gain.clone();
        if let Err(err) = self.mpv.start(Some(&target), Some(&rg)) {
            warn!(error = %err, "Failed to start libmpv");
            self.notice(NoticeKind::Error, format!("AUDIO_ENGINE_FAILED // {err}"));
            return false;
        }
        self.apply_mpv_filters_and_gain();
        if self.direct_card.is_some() {
            self.mpv.set_volume(1.0);
            self.mpv
                .set_mute(self.direct_control.is_none() && self.current_muted);
        } else {
            self.mpv.set_volume(self.current_vol);
            self.mpv.set_mute(self.current_muted);
        }
        self.emit_volume();
        self.emit_direct_status();
        true
    }

    pub(super) fn is_direct_output(&self) -> bool {
        self.direct_card.is_some()
            || (self.output_override.is_none()
                && AlsaDirect::is_direct(&self.settings.audio_device))
    }

    pub(super) fn apply_mpv_filters_and_gain(&mut self) {
        let direct = self.is_direct_output();
        let filters = EqualizerConfig::build_filters(
            self.settings.eq_enabled,
            direct,
            &self.settings.eq_gains,
        );
        self.mpv.set_audio_filters(&filters);
        self.mpv.set_replay_gain(if direct {
            "no"
        } else {
            &self.settings.replay_gain
        });
        let shared_filters = EqualizerConfig::build_filters(
            self.settings.eq_enabled,
            false,
            &self.settings.eq_gains,
        );
        let spare_device = if direct {
            "auto".to_owned()
        } else {
            self.mpv_device()
        };
        self.crossfade
            .sync_config(&spare_device, &self.settings.replay_gain, &shared_filters);
    }

    pub(super) async fn wait_for_card_free(device: &str) -> Result<(), Vec<String>> {
        let probe = |dev: String| async move {
            tokio::task::spawn_blocking(move || AlsaDirect::pcm_is_open(&dev))
                .await
                .ok()
                .flatten()
        };
        let holders = |dev: String| async move {
            tokio::task::spawn_blocking(move || AlsaDirect::pcm_holders(&dev))
                .await
                .unwrap_or_default()
        };
        let mut closed_for = 0;
        for _ in 0..CARD_FREE_POLLS {
            match probe(device.to_owned()).await {
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
        let still = holders(device.to_owned()).await;
        if still.is_empty() { Ok(()) } else { Err(still) }
    }

    /// Acquires `device` exclusively (preserving any previously active direct card if the new
    /// reservation is refused) and moves mpv onto it when `switch_live_mpv` is set.
    async fn enter_direct_mode(
        &mut self,
        device: &str,
        switch_live_mpv: bool,
        persist_setting: bool,
    ) -> bool {
        let Some(card) = AlsaDirect::card_of(device) else {
            warn!(
                device,
                "Could not resolve ALSA card index for direct device"
            );
            self.notice(NoticeKind::Warn, "DIRECT_OUTPUT // DEVICE NOT FOUND");
            self.emit_direct_status();
            return false;
        };
        if self.direct_card == Some(card) {
            if switch_live_mpv {
                self.mpv.set_audio_device(device);
            }
            if persist_setting && self.settings.audio_device != device {
                self.settings.audio_device = device.to_owned();
                self.save_settings_async();
                let _ = self
                    .ui_tx
                    .send(ControllerEvent::SettingsSaved(self.settings.clone()));
            }
            self.emit_direct_status();
            return true;
        }

        let desc = tokio::task::spawn_blocking({
            let dev = device.to_owned();
            move || {
                AlsaDirect::list_devices()
                    .into_iter()
                    .find(|d| d.name == dev)
                    .map(|d| d.description)
            }
        })
        .await
        .ok()
        .flatten()
        .unwrap_or_else(|| format!("ALSA Card {card}"));

        // Acquire the new card reservation BEFORE tearing down any current direct card.
        let new_reservation = match DeviceReservation::acquire(card, &desc).await {
            Ok(res) => Some(res),
            Err(ReservationError::NoBus(err)) => {
                warn!(card, error = %err, "No session bus for ReserveDevice1; trying the card anyway");
                None
            }
            Err(err) => {
                warn!(card, error = %err, "Could not reserve the card");
                let who = match &err {
                    ReservationError::Refused { holder: Some(h) } => h.to_uppercase(),
                    _ => "ANOTHER PROGRAM".to_owned(),
                };
                self.notice(
                    NoticeKind::Warn,
                    format!("DIRECT_OUTPUT // CARD IN USE BY {who}"),
                );
                self.emit_direct_status();
                return false;
            }
        };

        self.crossfade
            .cancel(&self.mpv, self.shared_vol, self.shared_muted);
        self.crossfade.shutdown_async().await;
        if switch_live_mpv && self.direct_card.is_some() {
            // Release the previous card's PCM handle immediately before tearing down its reservation.
            self.mpv.set_audio_device("null");
        }
        self.teardown_direct_resources().await;
        self.reservation = new_reservation;

        if let Err(holders) = Self::wait_for_card_free(device).await {
            warn!(
                card,
                ?holders,
                "Card still open by another program after the reservation"
            );
        }

        let (control, hw_lvl, hw_mute) = tokio::task::spawn_blocking(move || {
            let control = AlsaMixer::find_control(card);
            let hw_lvl = control
                .as_deref()
                .and_then(|c| AlsaMixer::read_level(card, c));
            let hw_mute = control
                .as_deref()
                .and_then(|c| AlsaMixer::read_muted(card, c));
            (control, hw_lvl, hw_mute)
        })
        .await
        .unwrap_or((None, None, None));

        self.direct_card = Some(card);
        self.direct_control = control.clone();
        self.output_override = None;
        self.last_hw_params = None;
        self.output_retries = 0;

        if persist_setting {
            self.settings.audio_device = device.to_owned();
            self.save_settings_async();
            let _ = self
                .ui_tx
                .send(ControllerEvent::SettingsSaved(self.settings.clone()));
        }

        if control.is_some() {
            if let Some(lvl) = hw_lvl {
                self.current_vol = lvl;
            }
            self.current_muted = hw_mute.unwrap_or(false);
        } else {
            // Mixerless DAC: mpv plays at 100%, mute is applied in mpv. Keep the shared volume and
            // mute, so the UI keeps the user's setting and a muted user stays muted.
            self.current_vol = self.shared_vol;
            self.current_muted = self.shared_muted;
        }

        self.apply_mpv_filters_and_gain();
        self.mpv.set_volume(1.0);
        self.mpv.set_mute(control.is_none() && self.current_muted);
        if switch_live_mpv {
            self.mpv.set_audio_device(device);
        }

        if self.settings.direct_volume_bridge == DirectVolumeBridge::VirtualSink {
            self.start_pw_bridge(&desc).await;
        }

        self.sync_mpv_next();
        self.emit_volume();
        self.emit_direct_status();
        self.sync_mpris().await;
        true
    }

    pub(super) async fn start_pw_bridge(&mut self, desc: &str) {
        let label = format!("{desc} (Direct)");
        let vol = self.current_vol;
        let muted = self.current_muted;
        let res = tokio::task::spawn_blocking(move || PipeWireBridge::connect(label, vol, muted))
            .await
            .unwrap_or_else(|e| Err(format!("PipeWire connect task failed: {e}")));

        match res {
            Ok((bridge, rx)) => {
                self.pw_bridge = Some(bridge);
                self.pw_rx = Some(rx);
            }
            Err(err) => {
                warn!(error = %err, "PipeWire bridge unavailable; staying hardware-only");
                self.pw_bridge = None;
                self.pw_rx = None;
                self.pw_bridge_node_id = None;
            }
        }
    }

    pub(super) async fn teardown_direct_resources(&mut self) {
        self.pw_rx = None;
        self.pw_bridge_node_id = None;
        if let Some(mut bridge) = self.pw_bridge.take() {
            let _ = tokio::task::spawn_blocking(move || bridge.shutdown()).await;
        }
        if let Some(res) = self.reservation.take() {
            res.release().await;
        }
        self.direct_card = None;
        self.direct_control = None;
        self.last_hw_params = None;
        self.volume_lock_told = false;
    }

    pub(super) async fn switch_output_device(&mut self, device: String) {
        if AlsaDirect::is_direct(&device) {
            let usb = tokio::task::spawn_blocking({
                let dev = device.clone();
                move || AlsaDirect::card_of(&dev).is_some_and(AlsaDirect::is_usb_card)
            })
            .await
            .unwrap_or(false);
            if !usb {
                self.notice(
                    NoticeKind::Warn,
                    "DIRECT_OUTPUT // ONLY USB DACS CAN BE OPENED DIRECTLY",
                );
                self.emit_direct_status();
                return;
            }
            if !self.enter_direct_mode(&device, true, true).await {
                let _ = self
                    .ui_tx
                    .send(ControllerEvent::SettingsSaved(self.settings.clone()));
                self.emit_direct_status();
                return;
            }
            self.last_sample_alteration = self.mpv.get_sample_alteration().await;
            self.emit_direct_status();
            return;
        }

        self.settings.audio_device = device.clone();
        self.save_settings_async();
        let _ = self
            .ui_tx
            .send(ControllerEvent::SettingsSaved(self.settings.clone()));
        self.output_override = None;
        self.output_retries = 0;
        let was_direct = self.direct_card.is_some() || self.reservation.is_some();
        self.current_vol = self.shared_vol;
        self.current_muted = self.shared_muted;
        self.last_sample_alteration = None;

        if was_direct {
            let was_playing = self.is_playing;
            if was_playing {
                self.mpv.set_paused(true);
            }
            // Drop the `hw:` handle and release reservation so PipeWire can reclaim the card.
            self.mpv.set_audio_device("null");
            self.teardown_direct_resources().await;
            self.apply_mpv_filters_and_gain();
            self.emit_direct_status();
            tokio::time::sleep(LEAVE_DIRECT_SETTLE).await;
            self.mpv.set_audio_device(&device);
            self.mpv.reload_audio();
            self.mpv.set_volume(self.shared_vol);
            self.mpv.set_mute(self.shared_muted);
            if was_playing {
                self.mpv.set_paused(false);
            }
        } else {
            self.mpv.set_audio_device(&device);
            self.mpv.set_volume(self.shared_vol);
            self.mpv.set_mute(self.shared_muted);
            self.apply_mpv_filters_and_gain();
        }
        self.sync_mpv_next();
        self.emit_volume();
        self.sync_mpris().await;
        self.emit_direct_status();
    }

    pub(super) async fn refresh_devices(&mut self) {
        let mpv_devs: Vec<AudioDevice> = self.mpv.get_audio_devices().await;
        let direct_devs = tokio::task::spawn_blocking(AlsaDirect::list_devices)
            .await
            .unwrap_or_default();
        let opts = build_device_options(&mpv_devs, &direct_devs, &self.settings.audio_device);
        let _ = self.ui_tx.send(ControllerEvent::DevicesUpdated(opts));
    }

    pub(super) async fn apply_volume(&mut self, vol: f64, echo_pw: bool) {
        let v = vol.clamp(0.0, 1.0);
        if self.direct_card.is_some() && self.direct_control.is_none() {
            // Mixerless direct output: the samples stay at 100 %, the shown volume is left as is.
            self.tell_volume_is_locked();
            if !echo_pw && let Some(pw) = &self.pw_bridge {
                pw.set_volume(self.current_vol);
            }
            self.emit_volume();
            return;
        }
        self.current_vol = v;
        if let (Some(card), Some(ctrl)) = (self.direct_card, self.direct_control.as_deref()) {
            self.hw_writer.push_level(card, ctrl, v);
            if echo_pw && let Some(pw) = &self.pw_bridge {
                pw.set_volume(v);
            }
        } else {
            self.shared_vol = v;
            self.settings.volume = v;
            self.settings_dirty_since = Some(Instant::now());
            if !self.crossfade.is_active() {
                self.mpv.set_volume(v);
            }
        }
        self.emit_volume();
        self.sync_mpris().await;
    }

    pub(super) fn apply_mute(&mut self, muted: bool, echo_pw: bool) {
        self.current_muted = muted;
        if let Some(card) = self.direct_card {
            if let Some(ctrl) = self.direct_control.as_deref() {
                self.hw_writer
                    .push_mute(card, ctrl, muted, self.current_vol);
            } else {
                // Mixerless direct DAC mutes safely inside mpv without touching shared_muted.
                self.mpv.set_mute(muted);
            }
            if echo_pw && let Some(pw) = &self.pw_bridge {
                pw.set_mute(muted);
            }
        } else {
            self.shared_muted = muted;
            self.mpv.set_mute(muted);
            self.crossfade.set_mute(muted);
        }
        self.emit_volume();
    }

    fn tell_volume_is_locked(&mut self) {
        if self.volume_lock_told {
            return;
        }
        self.volume_lock_told = true;
        self.notice(
            NoticeKind::Info,
            "DIRECT_OUTPUT // THIS CARD HAS NO VOLUME CONTROL: VOLUME STAYS AT 100%, USE THE DAC'S OWN VOLUME",
        );
    }

    pub(super) async fn set_eq_enabled(&mut self, enabled: bool) {
        self.settings.eq_enabled = enabled;
        self.ensure_eq_gains_len();
        self.finish_dsp_update().await;
    }

    pub(super) async fn select_eq_preset(&mut self, name: String) {
        if let Some((_, bands)) = EqualizerConfig::PRESETS.iter().find(|(p, _)| *p == name) {
            self.settings.eq_preset = name;
            self.settings.eq_gains = bands.to_vec();
            if !self.settings.eq_enabled {
                self.settings.eq_enabled = true;
                self.finish_dsp_update().await;
                return;
            }
            if !self.is_direct_output() {
                self.mpv.update_eq_all_live(&self.settings.eq_gains);
                self.crossfade.update_eq_all_live(&self.settings.eq_gains);
            }
            self.settings_dirty_since = Some(Instant::now());
            let _ = self
                .ui_tx
                .send(ControllerEvent::SettingsSaved(self.settings.clone()));
        }
    }

    pub(super) fn set_eq_band(&mut self, band: usize, gain: f64) {
        if band < 10 {
            self.ensure_eq_gains_len();
            self.settings.eq_gains[band] =
                gain.clamp(-EqualizerConfig::MAX_GAIN, EqualizerConfig::MAX_GAIN);
            self.settings.eq_preset = "CUSTOM".to_owned();
            if self.settings.eq_enabled && !self.is_direct_output() {
                self.mpv.update_eq_band_live(band, &self.settings.eq_gains);
                self.crossfade
                    .update_eq_band_live(band, &self.settings.eq_gains);
            }
            self.settings_dirty_since = Some(Instant::now());
        }
    }

    pub(super) async fn set_replay_gain(&mut self, mode: String) {
        self.settings.replay_gain = MpvEngine::normalize_replay_gain(&mode).to_owned();
        self.finish_dsp_update().await;
    }

    fn ensure_eq_gains_len(&mut self) {
        if self.settings.eq_gains.len() < 10 {
            self.settings.eq_gains.resize(10, 0.0);
        }
    }

    async fn finish_dsp_update(&mut self) {
        self.apply_mpv_filters_and_gain();
        if self.direct_card.is_some() {
            self.last_sample_alteration = self.mpv.get_sample_alteration().await;
        }
        self.emit_direct_status();
        self.settings_dirty_since = Some(Instant::now());
        let _ = self
            .ui_tx
            .send(ControllerEvent::SettingsSaved(self.settings.clone()));
    }

    pub(super) async fn handle_pw_bridge_event(&mut self, evt: BridgeEvent) {
        match evt {
            BridgeEvent::Ready { node_id, .. } => {
                self.pw_bridge_node_id = Some(node_id);
                self.emit_direct_status();
            }
            BridgeEvent::VolumeChanged(v) => self.apply_volume(v, false).await,
            BridgeEvent::MuteChanged(muted) => self.apply_mute(muted, false),
            BridgeEvent::Disconnected(_) => {
                self.pw_rx = None;
                self.pw_bridge = None;
                self.pw_bridge_node_id = None;
                self.emit_direct_status();
            }
        }
    }

    pub(super) async fn poll_external_mixer_and_save(&mut self) {
        if let Some(card) = self.direct_card {
            let poll_mixer = !self.hw_writer.recently_wrote(Duration::from_millis(500))
                && self.direct_control.is_some();
            let ctrl = self.direct_control.clone();
            let dev = self.settings.audio_device.clone();

            let (hw_lvl, hw_mute, hw_params) = tokio::task::spawn_blocking(move || {
                let hw_params = AlsaDirect::read_hw_params(&dev);
                if poll_mixer && let Some(ctrl_name) = ctrl.as_deref() {
                    (
                        AlsaMixer::read_level(card, ctrl_name),
                        AlsaMixer::read_muted(card, ctrl_name),
                        hw_params,
                    )
                } else {
                    (None, None, hw_params)
                }
            })
            .await
            .unwrap_or((None, None, None));

            let params_changed = self.last_hw_params != hw_params;
            self.last_hw_params = hw_params;

            let mut vol_changed = false;
            if let Some(lvl) = hw_lvl
                && (lvl - self.current_vol).abs() >= 0.008
            {
                self.current_vol = lvl;
                if let Some(pw) = &self.pw_bridge {
                    pw.set_volume(lvl);
                }
                vol_changed = true;
            }
            if let Some(muted) = hw_mute
                && muted != self.current_muted
            {
                self.current_muted = muted;
                if let Some(pw) = &self.pw_bridge {
                    pw.set_mute(muted);
                }
                vol_changed = true;
            }
            if vol_changed {
                self.emit_volume();
                self.sync_mpris().await;
            }
            if params_changed {
                self.emit_direct_status();
            }
        }

        if let Some(since) = self.settings_dirty_since
            && since.elapsed() >= Duration::from_millis(800)
        {
            self.settings_dirty_since = None;
            self.save_settings_async();
        }

        if self.is_playing && self.session_last_saved.elapsed() >= Duration::from_secs(2) {
            self.save_session_state_async();
        }
    }

    pub(super) fn emit_direct_status(&self) {
        let active = self.direct_card.is_some();
        let status = if active {
            let bits = self.queue.current().map(|t| t.bits_per_sample).unwrap_or(0);
            evaluate_bit_perfect(
                self.last_audio_path.input.as_ref(),
                self.last_audio_path.output.is_some(),
                self.last_hw_params.as_ref(),
                bits,
                self.last_sample_alteration.as_deref(),
            )
        } else if AlsaDirect::is_direct(&self.settings.audio_device) {
            "NOT ACTIVE. PLAYING THROUGH PIPEWIRE.".to_owned()
        } else {
            String::new()
        };
        let direct_volume_hint = match (&self.direct_control, active) {
            (_, false) => String::new(),
            (Some(control), true) => {
                let system = if self.pw_bridge.is_some() {
                    "THE SYSTEM VOLUME REACHES IT THROUGH THE (DIRECT) BRIDGE SINK."
                } else {
                    "THE SYSTEM VOLUME DOES NOT REACH THIS CARD."
                };
                format!(
                    "VOLUME SETS THE CARD'S OWN {} CONTROL, THE FILE'S SAMPLES ARE NOT TOUCHED. {system}",
                    control.to_uppercase()
                )
            }
            (None, true) => "THIS CARD HAS NO VOLUME CONTROL OF ITS OWN: THE VOLUME STAYS AT 100%. USE THE DAC'S OWN VOLUME.".to_owned(),
        };
        let _ = self.ui_tx.send(ControllerEvent::DirectStatus {
            audio_device: self.settings.audio_device.clone(),
            bit_perfect_status: status,
            direct_active: active,
            direct_volume_hint,
            audio_path: self.last_audio_path.clone(),
            pw_bridge_node_id: self.pw_bridge_node_id,
            direct_mixer_control: self.direct_control.clone(),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(name: &str, desc: &str) -> AudioDevice {
        AudioDevice {
            name: name.to_owned(),
            description: desc.to_owned(),
        }
    }

    #[test]
    fn device_list_deduplicates_sinks_filters_bridge_and_keeps_saved_selection() {
        let mpv = vec![
            dev("auto", "Autoselect device"),
            dev("pipewire", "Default (pipewire)"),
            dev(
                "pipewire/alsa_output.pci-0000_00_1f.3.analog-stereo",
                "Built-in Audio Analog Stereo",
            ),
            dev(
                "pipewire/rustygrade-direct-bridge",
                "TP35 Pro, USB Audio (Direct)",
            ),
            dev(
                "pulse/alsa_output.pci-0000_00_1f.3.analog-stereo",
                "Built-in Audio Analog Stereo",
            ),
            dev(
                "pulse/rustygrade-direct-bridge",
                "TP35 Pro, USB Audio (Direct)",
            ),
        ];
        let direct = vec![DirectDevice {
            name: "alsa/hw:CARD=Pro,DEV=0".to_owned(),
            description: "TP35 Pro, USB Audio".to_owned(),
            card: 0,
        }];
        let opts = build_device_options(&mpv, &direct, "alsa/hw:CARD=Pro,DEV=0");
        let labels: Vec<(&str, bool)> = opts
            .iter()
            .map(|o| (o.label.as_str(), o.is_direct))
            .collect();
        assert_eq!(
            labels,
            vec![
                ("System Default", false),
                ("Built-in Audio Analog Stereo", false),
                ("TP35 Pro, USB Audio", true)
            ]
        );
        assert_eq!(
            opts[1].id,
            "pipewire/alsa_output.pci-0000_00_1f.3.analog-stereo"
        );

        let opts = build_device_options(
            &mpv,
            &[],
            "pulse/alsa_output.pci-0000_00_1f.3.analog-stereo",
        );
        assert_eq!(opts.len(), 2);
        assert_eq!(
            opts[1].id,
            "pulse/alsa_output.pci-0000_00_1f.3.analog-stereo"
        );

        let twins = vec![
            dev("pipewire/a", "DAC"),
            dev("pipewire/b", "DAC"),
            dev("pulse/b", "DAC"),
        ];
        let labels: Vec<String> = build_device_options(&twins, &[], "auto")
            .into_iter()
            .map(|o| o.label)
            .collect();
        assert_eq!(labels, vec!["System Default", "DAC", "DAC #2"]);
    }

    #[test]
    fn saved_bridge_output_falls_back_to_auto() {
        assert_eq!(
            saved_device_replacement("pipewire/rustygrade-direct-bridge").as_deref(),
            Some("auto")
        );
        assert_eq!(
            saved_device_replacement("pulse/rustygrade-direct-bridge").as_deref(),
            Some("auto")
        );
        assert_eq!(
            saved_device_replacement("pipewire/retrograde-direct-bridge").as_deref(),
            Some("auto")
        );
        assert_eq!(saved_device_replacement("auto"), None);
        assert_eq!(
            saved_device_replacement("alsa/hw:CARD=NoSuchCard,DEV=0"),
            None
        );
    }
}
