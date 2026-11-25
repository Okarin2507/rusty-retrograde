use serde_json::{Value, json};

use crate::artwork_lookup::validate_artwork_url;

use super::{DiscordPlaybackStatus, DiscordPresence};

/// Trims, applies a fallback when empty, truncates safely by UTF-16 code units, and ensures >= 2 chars.
pub fn clamp_discord_str(input: &str, fallback: &str, max_utf16_units: usize) -> String {
    let trimmed = input.trim();
    let src = if trimmed.is_empty() {
        fallback
    } else {
        trimmed
    };
    let total_units: usize = src.chars().map(char::len_utf16).sum();
    let mut s = if total_units <= max_utf16_units {
        src.to_owned()
    } else if max_utf16_units == 0 {
        String::new()
    } else {
        let budget = max_utf16_units - 1;
        let mut out = String::new();
        let mut used = 0usize;
        for ch in src.chars() {
            let u = ch.len_utf16();
            if used + u > budget {
                break;
            }
            out.push(ch);
            used += u;
        }
        out.push('…');
        out
    };
    if s.chars().count() < 2 {
        // Discord rejects activity strings shorter than 2 characters.
        s.push('\u{200B}');
    }
    s
}

/// Builds a Discord `SET_ACTIVITY` payload for the given playback snapshot.
pub fn build_set_activity_payload(
    pid: u32,
    nonce: u64,
    presence: &DiscordPresence,
    now_unix_s: i64,
) -> Value {
    let activity = match presence.status {
        DiscordPlaybackStatus::Stopped => Value::Null,
        DiscordPlaybackStatus::Playing | DiscordPlaybackStatus::Paused => {
            let title = clamp_discord_str(&presence.title, "Unknown Track", 128);
            let album = clamp_discord_str(&presence.album, "Rustygrade", 128);
            let state = match presence.status {
                DiscordPlaybackStatus::Playing => {
                    clamp_discord_str(&presence.artist, "Unknown Artist", 128)
                }
                DiscordPlaybackStatus::Paused => {
                    let artist = clamp_discord_str(&presence.artist, "Unknown Artist", 116);
                    clamp_discord_str(&format!("{artist} • Paused"), "Paused", 128)
                }
                DiscordPlaybackStatus::Stopped => unreachable!(),
            };
            let mut obj = serde_json::Map::new();
            obj.insert("type".to_owned(), json!(2));
            obj.insert("details".to_owned(), json!(title));
            obj.insert("state".to_owned(), json!(state));
            let mut assets = serde_json::Map::new();
            if let Some(url) = presence
                .artwork_url
                .as_deref()
                .and_then(validate_artwork_url)
            {
                assets.insert("large_image".to_owned(), json!(url));
                assets.insert("large_text".to_owned(), json!(album));
                assets.insert("small_image".to_owned(), json!("retrograde"));
                assets.insert("small_text".to_owned(), json!("Rustygrade"));
            } else {
                assets.insert("large_image".to_owned(), json!("retrograde"));
                assets.insert("large_text".to_owned(), json!(album));
            }
            obj.insert("assets".to_owned(), Value::Object(assets));
            if presence.status == DiscordPlaybackStatus::Playing
                && presence.duration_s.is_finite()
                && presence.duration_s > 0.0
            {
                let pos = if presence.position_s.is_finite() {
                    presence.position_s.clamp(0.0, presence.duration_s)
                } else {
                    0.0
                };
                let start = now_unix_s - pos.round() as i64;
                let end = (start + presence.duration_s.round() as i64).max(start + 1);
                obj.insert(
                    "timestamps".to_owned(),
                    json!({ "start": start, "end": end }),
                );
            }
            Value::Object(obj)
        }
    };

    json!({
        "cmd": "SET_ACTIVITY",
        "args": {
            "pid": pid,
            "activity": activity,
        },
        "nonce": nonce.to_string(),
    })
}
