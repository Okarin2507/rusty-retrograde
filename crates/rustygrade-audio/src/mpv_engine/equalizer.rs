use super::MpvEngine;

pub struct EqualizerConfig;

impl EqualizerConfig {
    pub const BAND_HZ: [i32; 10] = [32, 64, 125, 250, 500, 1000, 2000, 4000, 8000, 16000];
    pub const MAX_GAIN: f64 = 12.0;
    pub const PRESETS: [(&'static str, [f64; 10]); 6] = [
        ("FLAT", [0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]),
        ("BASS", [6.0, 5.0, 4.0, 2.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]),
        ("TREBLE", [0.0, 0.0, 0.0, 0.0, 0.0, 1.0, 2.0, 4.0, 5.0, 6.0]),
        (
            "VOCAL",
            [-2.0, -2.0, -1.0, 1.0, 3.0, 4.0, 3.0, 1.0, 0.0, -1.0],
        ),
        (
            "V-SHAPE",
            [5.0, 4.0, 2.0, 0.0, -2.0, -2.0, 0.0, 2.0, 4.0, 5.0],
        ),
        (
            "WARM",
            [3.0, 3.0, 2.0, 1.0, 0.0, -1.0, -1.0, -2.0, -2.0, -3.0],
        ),
    ];

    pub fn preamp_db(gains: &[f64]) -> f64 {
        let max_boost = gains.iter().copied().fold(0.0_f64, |acc, g| {
            let clamped = if g.is_finite() {
                g.clamp(-Self::MAX_GAIN, Self::MAX_GAIN)
            } else {
                0.0
            };
            acc.max(clamped)
        });
        -max_boost
    }

    pub fn build_filters(enabled: bool, bit_perfect: bool, gains: &[f64]) -> String {
        if !enabled || bit_perfect {
            return String::new();
        }
        let preamp = Self::preamp_db(gains);
        let mut parts = Vec::with_capacity(Self::BAND_HZ.len() + 1);
        parts.push(format!("@eqpre:lavfi=[volume={}dB]", fmt_num(preamp)));
        for (i, hz) in Self::BAND_HZ.iter().enumerate() {
            parts.push(format!(
                "@eq{i}:lavfi=[equalizer=f={hz}:t=o:w=1:g={}]",
                fmt_num(clamp_band(gains, i))
            ));
        }
        parts.join(",")
    }
}

fn clamp_band(gains: &[f64], band: usize) -> f64 {
    gains
        .get(band)
        .copied()
        .filter(|g| g.is_finite())
        .unwrap_or(0.0)
        .clamp(-EqualizerConfig::MAX_GAIN, EqualizerConfig::MAX_GAIN)
}

fn fmt_num(v: f64) -> String {
    let rounded = (v * 10.0).round() / 10.0;
    if rounded == 0.0 {
        return "0".to_owned();
    }
    let s = format!("{rounded:.1}");
    s.strip_suffix(".0").unwrap_or(&s).to_owned()
}

impl MpvEngine {
    fn filter_command(&self, label: &str, target: &str, command: &str, argument: &str) {
        if let Some(lib) = &self.lib {
            lib.command(0, &["af-command", label, command, argument, target]);
        }
    }

    pub fn update_eq_band_live(&mut self, band: usize, gains: &[f64]) {
        if band >= EqualizerConfig::BAND_HZ.len() {
            return;
        }
        self.apply_eq_live(gains, band..=band);
    }

    pub fn update_eq_all_live(&mut self, gains: &[f64]) {
        self.apply_eq_live(gains, 0..=EqualizerConfig::BAND_HZ.len() - 1);
    }

    fn apply_eq_live(&mut self, gains: &[f64], bands: std::ops::RangeInclusive<usize>) {
        let next_filters = EqualizerConfig::build_filters(true, false, gains);
        if self.audio_filters.is_empty() {
            self.set_audio_filters(&next_filters);
            return;
        }
        let preamp = EqualizerConfig::preamp_db(gains);
        self.filter_command(
            "eqpre",
            "volume",
            "volume",
            &format!("{}dB", fmt_num(preamp)),
        );
        for i in bands {
            self.filter_command(
                &format!("eq{i}"),
                "equalizer",
                "g",
                &fmt_num(clamp_band(gains, i)),
            );
        }
        self.audio_filters = next_filters;
    }
}
