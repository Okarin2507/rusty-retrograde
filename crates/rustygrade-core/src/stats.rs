use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::json_store::{LoadStatus, load_with_backup, save_atomic_locked};
use crate::paths::CoverPaths;

static WRITE_LOCK: Mutex<()> = Mutex::new(());

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct PlayStat {
    pub count: i32,
    pub last: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct PlayStats {
    pub tracks: HashMap<String, PlayStat>,
    #[serde(skip)]
    ci_index: HashMap<String, String>,
}

impl PlayStats {
    pub fn file_path() -> PathBuf {
        CoverPaths::app_dir().join("native-stats.json")
    }

    pub fn backup_path() -> PathBuf {
        CoverPaths::app_dir().join("native-stats.json.bak")
    }

    pub fn corrupt_path() -> PathBuf {
        CoverPaths::app_dir().join("native-stats.json.corrupt")
    }

    pub fn rebuild_index(&mut self) {
        self.ci_index.clear();
        self.ci_index.reserve(self.tracks.len());
        for k in self.tracks.keys() {
            self.ci_index
                .entry(k.to_ascii_lowercase())
                .or_insert_with(|| k.clone());
        }
    }

    pub fn load() -> Self {
        match load_with_backup(
            &Self::file_path(),
            &Self::backup_path(),
            &Self::corrupt_path(),
            |bytes| serde_json::from_slice::<Self>(bytes).ok(),
        ) {
            LoadStatus::Ok(mut stats) => {
                stats.rebuild_index();
                stats
            }
            LoadStatus::Missing | LoadStatus::Corrupt => Self::default(),
        }
    }

    pub fn get(&self, id: &str) -> Option<&PlayStat> {
        self.tracks.get(id).or_else(|| {
            let canonical = self.ci_index.get(&id.to_ascii_lowercase())?;
            self.tracks.get(canonical)
        })
    }

    pub fn record_now(&mut self, id: &str) {
        self.record_at(id, crate::now_ms());
    }

    pub fn record_at(&mut self, id: &str, when_ms: i64) {
        let low = id.to_ascii_lowercase();
        let key = if self.tracks.contains_key(id) {
            self.ci_index.entry(low).or_insert_with(|| id.to_owned());
            id.to_owned()
        } else if let Some(existing) = self.ci_index.get(&low).cloned() {
            existing
        } else {
            self.ci_index.insert(low, id.to_owned());
            id.to_owned()
        };
        let entry = self.tracks.entry(key).or_default();
        entry.count += 1;
        entry.last = when_ms;
    }

    /// Seconds a track must be heard before it counts as played
    /// (C# `Math.Clamp(d * 0.5, Math.Min(d, 15), 240)`, 30 s when the length is unknown).
    pub fn threshold(duration_secs: f64) -> f64 {
        if !duration_secs.is_finite() || duration_secs <= 0.0 {
            30.0
        } else {
            (duration_secs * 0.5).clamp(duration_secs.min(15.0), 240.0)
        }
    }

    pub fn save(&self) {
        if let Ok(json) = serde_json::to_vec(self) {
            let _ = save_atomic_locked(
                &WRITE_LOCK,
                &Self::file_path(),
                Some(&Self::backup_path()),
                &json,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn threshold_handles_short_and_long_durations() {
        assert_eq!(PlayStats::threshold(0.0), 30.0);
        assert_eq!(PlayStats::threshold(-1.0), 30.0);
        assert_eq!(PlayStats::threshold(f64::NAN), 30.0);
        assert_eq!(PlayStats::threshold(2.0), 2.0);
        assert_eq!(PlayStats::threshold(10.0), 10.0);
        assert_eq!(PlayStats::threshold(14.0), 14.0);
        assert_eq!(PlayStats::threshold(20.0), 15.0);
        assert_eq!(PlayStats::threshold(30.0), 15.0);
        assert_eq!(PlayStats::threshold(180.0), 90.0);
        assert_eq!(PlayStats::threshold(480.0), 240.0);
        assert_eq!(PlayStats::threshold(600.0), 240.0);
    }
}
