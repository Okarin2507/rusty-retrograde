use std::fs;
use std::path::Path;
use std::sync::LazyLock;

use lofty::config::ParseOptions;
use lofty::file::TaggedFileExt;
use lofty::probe::Probe;
use lofty::tag::ItemKey;
use regex::Regex;

#[derive(Debug, Clone, PartialEq)]
pub struct LyricLine {
    /// Time in seconds, or `-1.0` for plain/unsynced lines.
    pub time: f64,
    pub text: String,
}

#[derive(Debug, Clone)]
pub struct LyricsDoc {
    pub lines: Vec<LyricLine>,
    pub is_synced: bool,
    /// `"LRC"` (sidecar `.lrc`) or `"TAG"` (embedded tag).
    pub source: &'static str,
}

impl LyricsDoc {
    /// Index of the line being sung at `seconds`, or `None` before the first line.
    pub fn index_at(&self, seconds: f64) -> Option<usize> {
        if !self.is_synced || self.lines.is_empty() {
            return None;
        }
        let mut lo = 0usize;
        let mut hi = self.lines.len();
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if self.lines[mid].time <= seconds {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        lo.checked_sub(1)
    }
}

static STAMP_RX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\[(\d{1,3}):(\d{1,2}(?:[.:]\d{1,3})?)\]").unwrap());
static META_RX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^\[(ar|ti|al|au|by|re|ve|length|id|#|offset)\s*:([^\]]*)\]\s*$").unwrap()
});
static INLINE_WORD_RX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"<\d{1,3}:\d{1,2}(?:[.:]\d{1,3})?>").unwrap());

pub struct LyricsLoader;

impl LyricsLoader {
    pub fn load(audio_path: &Path) -> Option<LyricsDoc> {
        let lrc_path = audio_path.with_extension("lrc");
        if lrc_path.is_file()
            && let Ok(bytes) = fs::read(&lrc_path)
            && let Some(doc) = Self::parse(&decode_text(&bytes), "LRC")
        {
            return Some(doc);
        }

        let probe = Probe::open(audio_path).ok()?.options(
            ParseOptions::new()
                .read_properties(false)
                .read_cover_art(false),
        );
        let tagged = probe.read().ok()?;
        let tag = tagged.primary_tag().or_else(|| tagged.first_tag())?;
        let text = tag.get_string(ItemKey::Lyrics)?;
        if text.trim().is_empty() {
            return None;
        }
        Self::parse(text, "TAG")
    }

    pub fn parse(text: &str, source: &'static str) -> Option<LyricsDoc> {
        let mut synced = Vec::new();
        let mut plain = Vec::new();
        let mut offset_secs = 0.0_f64;

        for raw in text
            .split('\n')
            .flat_map(|s| s.strip_suffix('\r').unwrap_or(s).split('\r'))
        {
            let line = raw.trim();
            if let Some(meta) = META_RX.captures(line) {
                if meta[1].eq_ignore_ascii_case("offset")
                    && let Ok(ms) = meta[2].trim().parse::<f64>()
                {
                    offset_secs = ms / 1000.0;
                }
                continue;
            }

            // Only consume contiguous leading timestamps so incidental `[1:23]` inside lyric
            // text never strips preceding words.
            let mut rest = line;
            let mut times = Vec::new();
            while let Some(cap) = STAMP_RX.captures(rest) {
                let m = cap.get(0).unwrap();
                if let Ok(min) = cap[1].parse::<i64>() {
                    let sec_str = cap[2].replace(':', ".");
                    if let Ok(sec) = sec_str.parse::<f64>() {
                        times.push((min as f64) * 60.0 + sec);
                    }
                }
                rest = rest[m.end()..].trim_start();
            }

            if times.is_empty() {
                plain.push(LyricLine {
                    time: -1.0,
                    text: line.to_owned(),
                });
                continue;
            }

            let body = INLINE_WORD_RX.replace_all(rest, "").trim().to_owned();
            for time in times {
                synced.push(LyricLine {
                    time,
                    text: body.clone(),
                });
            }
        }

        if !synced.is_empty() {
            for l in &mut synced {
                l.time = (l.time - offset_secs).max(0.0);
            }
            synced.sort_by(|a, b| a.time.total_cmp(&b.time));
            return Some(LyricsDoc {
                lines: synced,
                is_synced: true,
                source,
            });
        }

        let first_non_empty = plain
            .iter()
            .position(|l| !l.text.is_empty())
            .unwrap_or(plain.len());
        plain.drain(..first_non_empty);
        while plain.last().is_some_and(|l| l.text.is_empty()) {
            plain.pop();
        }
        if plain.is_empty() {
            None
        } else {
            Some(LyricsDoc {
                lines: plain,
                is_synced: false,
                source,
            })
        }
    }
}

fn decode_text(bytes: &[u8]) -> String {
    if bytes.len() >= 2 && bytes[0] == 0xFF && bytes[1] == 0xFE {
        let u16s: Vec<u16> = bytes[2..]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_le_bytes(*c))
            .collect();
        return String::from_utf16_lossy(&u16s);
    }
    if bytes.len() >= 2 && bytes[0] == 0xFE && bytes[1] == 0xFF {
        let u16s: Vec<u16> = bytes[2..]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_be_bytes(*c))
            .collect();
        return String::from_utf16_lossy(&u16s);
    }
    if bytes.len() >= 3 && bytes[0] == 0xEF && bytes[1] == 0xBB && bytes[2] == 0xBF {
        return String::from_utf8_lossy(&bytes[3..]).into_owned();
    }
    String::from_utf8_lossy(bytes).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_multi_stamp_offset_and_enhanced_lrc() {
        let lrc = "\
[ti:Test]
[offset:500]
[00:01.00][00:10.50]<00:01.10>Hello <00:01.50>world
[00:05.00]Second line [00:00.00] literal
";
        let doc = LyricsLoader::parse(lrc, "LRC").unwrap();
        assert!(doc.is_synced);
        assert_eq!(doc.lines.len(), 3);
        assert!((doc.lines[0].time - 0.5).abs() < 1e-6);
        assert_eq!(doc.lines[0].text, "Hello world");
        assert!((doc.lines[1].time - 4.5).abs() < 1e-6);
        assert_eq!(doc.lines[1].text, "Second line [00:00.00] literal");
        assert!((doc.lines[2].time - 10.0).abs() < 1e-6);
        assert_eq!(doc.index_at(0.2), None);
        assert_eq!(doc.index_at(0.5), Some(0));
        assert_eq!(doc.index_at(4.5), Some(1));
        assert_eq!(doc.index_at(15.0), Some(2));
    }
}
