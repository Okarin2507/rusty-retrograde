use std::path::Path;

use lofty::config::ParseOptions;
use lofty::file::{AudioFile, FileType, TaggedFileExt};
use lofty::probe::Probe;
use lofty::properties::FileProperties;
use lofty::tag::{Accessor, ItemKey};

#[derive(Debug, Clone)]
pub(super) struct Parsed {
    pub file_path: String,
    pub title: String,
    pub artist: String,
    pub album_artist: Option<String>,
    pub album: String,
    pub duration: f64,
    pub track_number: i32,
    pub disc_number: i32,
    pub year: i32,
    pub codec: String,
    pub sample_rate: i32,
    pub bits_per_sample: i32,
    pub channels: i32,
}

pub(super) fn parse_file(disk_path: &Path, stored_path: &str) -> Option<Parsed> {
    let probe = Probe::open(disk_path)
        .ok()?
        .options(ParseOptions::new().read_cover_art(false));
    let tagged = probe.read().ok()?;
    let props = tagged.properties();
    let tag = tagged.primary_tag().or_else(|| tagged.first_tag());

    let album_artist = tag
        .and_then(|t| t.get_string(ItemKey::AlbumArtist))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    let artist = tag
        .and_then(|t| t.artist())
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .or_else(|| album_artist.clone())
        .unwrap_or_else(|| "Unknown Artist".to_owned());
    let title = tag
        .and_then(|t| t.title())
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            disk_path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("Unknown")
                .to_owned()
        });
    let album = tag
        .and_then(|t| t.album())
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "Unknown Album".to_owned());

    let track_number = tag.and_then(|t| t.track()).unwrap_or(0) as i32;
    let disc_number = tag.and_then(|t| t.disk()).filter(|&d| d > 0).unwrap_or(1) as i32;
    let year = tag
        .and_then(|t| t.date().map(|ts| ts.year as i32))
        .unwrap_or(0);

    Some(Parsed {
        file_path: stored_path.to_owned(),
        title,
        artist,
        album_artist,
        album,
        duration: props.duration().as_secs_f64(),
        track_number,
        disc_number,
        year,
        codec: codec_name(tagged.file_type(), props, disk_path),
        sample_rate: props.sample_rate().unwrap_or(0) as i32,
        bits_per_sample: props.bit_depth().unwrap_or(0) as i32,
        channels: props.channels().unwrap_or(0) as i32,
    })
}

fn codec_name(ft: FileType, props: &FileProperties, path: &Path) -> String {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ft {
        FileType::Flac => "FLAC".to_owned(),
        FileType::Mpeg => "MPEG 1 Layer 3".to_owned(),
        FileType::Opus => "Opus".to_owned(),
        FileType::Vorbis => "Vorbis I".to_owned(),
        FileType::Wav => "PCM".to_owned(),
        FileType::Aiff => "AIFF".to_owned(),
        FileType::Aac => "AAC".to_owned(),
        FileType::Mp4 => {
            if ext == "alac" || props.bit_depth().is_some_and(|b| b > 0) {
                "ALAC".to_owned()
            } else {
                "AAC".to_owned()
            }
        }
        _ => match ext.as_str() {
            "wma" => "Windows Media Audio".to_owned(),
            "" => "AUDIO".to_owned(),
            other => other.to_ascii_uppercase(),
        },
    }
}
