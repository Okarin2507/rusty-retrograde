use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Track {
    /// Track identity is the absolute file path (shared with Electron & C# builds).
    pub id: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub duration: f64,
    pub file_path: String,
    pub track_number: i32,
    pub disc_number: i32,
    pub codec: String,
    pub sample_rate: i32,
    pub bits_per_sample: i32,
    pub channels: i32,
    pub cover_thumb_path: Option<PathBuf>,
    pub cover_full_path: Option<PathBuf>,
}

/// Canonical short codec label (`FLAC`, `ALAC`, `AAC`, `MP3`, `OGG`, `OPUS`, `WAV`, `AIFF`,
/// `WMA`, or fallback extension/uppercased prefix) shared by track display and smart playlists.
pub fn normalize_codec(codec: &str, file_path: &str) -> String {
    let trimmed = codec.trim();
    if !trimmed.is_empty() {
        let u = trimmed.to_ascii_uppercase();
        if u.contains("FLAC") {
            return "FLAC".to_owned();
        }
        if u.contains("ALAC") || u.contains("APPLE LOSSLESS") {
            return "ALAC".to_owned();
        }
        if u.contains("AAC") || u == "M4A" {
            return "AAC".to_owned();
        }
        if u.contains("OPUS") {
            return "OPUS".to_owned();
        }
        if u.contains("VORBIS") || u == "OGG" {
            return "OGG".to_owned();
        }
        if u.contains("MP3") || u.contains("MPEG") {
            return "MP3".to_owned();
        }
        if u.contains("AIFF") {
            return "AIFF".to_owned();
        }
        if u.contains("WAV") || u.contains("PCM") {
            return "WAV".to_owned();
        }
        if u.contains("WINDOWS MEDIA") || u == "WMA" {
            return "WMA".to_owned();
        }
        let ext = Path::new(file_path)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("");
        if !ext.is_empty() {
            return ext.to_ascii_uppercase();
        }
        return u.chars().take(8).collect::<String>().trim().to_owned();
    }

    let ext = Path::new(file_path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");
    if ext.is_empty() {
        String::new()
    } else if ext.eq_ignore_ascii_case("m4a") {
        "AAC".to_owned()
    } else {
        ext.to_ascii_uppercase()
    }
}

impl Track {
    pub fn short_codec(&self) -> String {
        normalize_codec(&self.codec, &self.file_path)
    }

    pub fn album_quality_label(tracks: &[Track]) -> Option<String> {
        let first = tracks.first()?;
        let first_codec = first.short_codec();
        let mixed = tracks
            .iter()
            .skip(1)
            .any(|t| t.short_codec() != first_codec);
        let mut parts = vec![if mixed {
            "MIXED".to_owned()
        } else {
            first_codec
        }];
        if first.bits_per_sample > 0 {
            parts.push(format!("{}BIT", first.bits_per_sample));
        }
        if first.sample_rate > 0 {
            if first.sample_rate >= 1000 {
                let sr = first.sample_rate;
                parts.push(if sr % 1000 == 0 {
                    format!("{}KHZ", sr / 1000)
                } else {
                    format!("{:.1}KHZ", sr as f64 / 1000.0)
                });
            } else {
                parts.push(format!("{}HZ", first.sample_rate));
            }
        }
        Some(parts.join(" / "))
    }
}

#[derive(Debug, Clone)]
pub struct Album {
    pub name: String,
    pub artist: String,
    pub cover_thumb_path: Option<PathBuf>,
    pub cover_full_path: Option<PathBuf>,
    pub year: i32,
    pub added_at: i64,
    pub tracks: Vec<Track>,
}

#[derive(Debug, Clone)]
pub struct ArtistSummary {
    pub name: String,
    pub album_indices: Vec<usize>,
    pub track_count: usize,
    pub newest_added_at: i64,
}
