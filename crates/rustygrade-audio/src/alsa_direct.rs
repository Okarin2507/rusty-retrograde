use std::fs;
use std::path::Path;

pub const DIRECT_PREFIX: &str = "alsa/hw:";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectDevice {
    pub name: String,
    pub description: String,
    pub card: i32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlsaHwParams {
    pub format: String,
    pub rate: i32,
    pub channels: i32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioFormat {
    pub format: String,
    pub rate: i32,
    pub channels: i32,
}

impl AudioFormat {
    pub fn integer_bits(&self) -> i32 {
        match self.format.trim_end_matches('p') {
            "u8" => 8,
            "s16" => 16,
            "s24" => 24,
            "s32" => 32,
            "s64" => 64,
            _ => 0,
        }
    }
}

#[derive(Debug, Clone)]
struct CardInfo {
    index: i32,
    id: String,
    name: String,
}

pub struct AlsaDirect;

impl AlsaDirect {
    pub fn is_direct(device: &str) -> bool {
        device.starts_with(DIRECT_PREFIX)
    }

    /// A USB sound card (external DAC): the kernel's usb-audio driver gives it a `usbid` file, and
    /// its sysfs device sits under a USB port. Internal (HDA) and HDMI cards are not offered for
    /// direct output: taking them from PipeWire silences the desktop for no fidelity gain.
    pub fn is_usb_card(card: i32) -> bool {
        if Path::new(&format!("/proc/asound/card{card}/usbid")).exists() {
            return true;
        }
        fs::canonicalize(format!("/sys/class/sound/card{card}/device")).is_ok_and(|p| {
            p.components()
                .any(|c| c.as_os_str().to_string_lossy().starts_with("usb"))
        })
    }

    /// Direct (hw:) outputs of USB sound cards, the only ones offered for bit-perfect playback.
    pub fn list_devices() -> Vec<DirectDevice> {
        let mut list = Vec::new();
        for card in read_cards() {
            if !Self::is_usb_card(card.index) {
                continue;
            }
            let dir = Path::new("/proc/asound").join(format!("card{}", card.index));
            let Ok(entries) = fs::read_dir(&dir) else {
                continue;
            };
            let mut pcms: Vec<String> = entries
                .flatten()
                .filter_map(|e| {
                    let name = e.file_name().to_string_lossy().into_owned();
                    (name.starts_with("pcm") && name.ends_with('p')).then_some(name)
                })
                .collect();
            pcms.sort();

            let mut monitor_present: Option<bool> = None;
            for pcm in pcms {
                let dev_str = &pcm[3..pcm.len() - 1];
                let Ok(dev) = dev_str.parse::<i32>() else {
                    continue;
                };
                let info_path = dir.join(&pcm).join("info");
                let pcm_name =
                    read_field(&info_path, "name").unwrap_or_else(|| format!("Device {dev}"));
                if pcm_name
                    .get(..4)
                    .is_some_and(|p| p.eq_ignore_ascii_case("HDMI"))
                {
                    let has_mon = *monitor_present.get_or_insert_with(|| has_monitor(&dir));
                    if !has_mon {
                        continue;
                    }
                }
                list.push(DirectDevice {
                    name: format!("{DIRECT_PREFIX}CARD={},DEV={dev}", card.id),
                    description: format!("{}, {pcm_name}", card.name),
                    card: card.index,
                });
            }
        }
        // Non-HDMI sound cards first.
        list.sort_by_key(|d| usize::from(d.description.to_ascii_lowercase().contains("hdmi")));
        list
    }

    pub fn card_of(device: &str) -> Option<i32> {
        if !Self::is_direct(device) {
            return None;
        }
        let name = &device["alsa/".len()..];
        if let Some(card_num) = parse_hw_card_num(name) {
            return Some(card_num);
        }
        if let Some(id) = parse_named_field(name, "CARD=") {
            return read_cards()
                .into_iter()
                .find(|c| c.id == id)
                .map(|c| c.index);
        }
        None
    }

    pub fn device_number(device: &str) -> i32 {
        let name = device.strip_prefix("alsa/").unwrap_or(device);
        if let Some(dev) = parse_named_digits(name, "DEV=") {
            return dev;
        }
        if let Some(dev) = parse_hw_dev_num(name) {
            return dev;
        }
        0
    }

    /// Whether any process has the playback PCM behind `device` open (`/proc/asound/cardN/pcmDp/sub*/status`
    /// reads `closed` for a free substream). `None` when the card or PCM cannot be found.
    pub fn pcm_is_open(device: &str) -> Option<bool> {
        let card = Self::card_of(device)?;
        let dir = format!(
            "/proc/asound/card{card}/pcm{}p",
            Self::device_number(device)
        );
        let entries = fs::read_dir(dir).ok()?;
        let mut any = false;
        for e in entries.flatten() {
            if !e.file_name().to_string_lossy().starts_with("sub") {
                continue;
            }
            any = true;
            if let Ok(text) = fs::read_to_string(e.path().join("status"))
                && !status_is_closed(&text)
            {
                return Some(true);
            }
        }
        any.then_some(false)
    }

    /// Names (`comm`) of the other processes holding the playback PCM behind `device` open, found by
    /// their open `/dev/snd/pcmC{card}D{dev}p` descriptors. Processes of other users are not readable
    /// and are left out.
    pub fn pcm_holders(device: &str) -> Vec<String> {
        let Some(card) = Self::card_of(device) else {
            return Vec::new();
        };
        let node = format!("/dev/snd/pcmC{card}D{}p", Self::device_number(device));
        let own = std::process::id();
        let mut names = Vec::new();
        let Ok(procs) = fs::read_dir("/proc") else {
            return names;
        };
        for p in procs.flatten() {
            let Some(pid) = p.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) else {
                continue;
            };
            if pid == own {
                continue;
            }
            let Ok(fds) = fs::read_dir(p.path().join("fd")) else {
                continue;
            };
            let holds = fds
                .flatten()
                .any(|fd| fs::read_link(fd.path()).is_ok_and(|t| t.as_os_str() == node.as_str()));
            if holds {
                let comm = fs::read_to_string(p.path().join("comm"))
                    .map(|s| s.trim().to_owned())
                    .unwrap_or_else(|_| format!("pid {pid}"));
                if !names.contains(&comm) {
                    names.push(comm);
                }
            }
        }
        names
    }

    pub fn read_hw_params(device: &str) -> Option<AlsaHwParams> {
        let card = Self::card_of(device)?;
        let dir = format!(
            "/proc/asound/card{card}/pcm{}p",
            Self::device_number(device)
        );
        let mut subs: Vec<_> = fs::read_dir(dir)
            .ok()?
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with("sub"))
            .collect();
        subs.sort_by_key(|e| e.file_name());
        for sub in subs {
            if let Ok(text) = fs::read_to_string(sub.path().join("hw_params"))
                && let Some(params) = parse_hw_params(&text)
            {
                return Some(params);
            }
        }
        None
    }
}

fn take_ascii_digits(s: &str) -> &str {
    let end = s
        .bytes()
        .position(|b| !b.is_ascii_digit())
        .unwrap_or(s.len());
    &s[..end]
}

fn parse_named_field<'a>(name: &'a str, prefix: &str) -> Option<&'a str> {
    let (_, after) = name.split_once(prefix)?;
    let val = after.split(',').next()?;
    (!val.is_empty()).then_some(val)
}

fn parse_named_digits(name: &str, prefix: &str) -> Option<i32> {
    let (_, after) = name.split_once(prefix)?;
    let digits = take_ascii_digits(after);
    (!digits.is_empty())
        .then(|| digits.parse::<i32>().ok())
        .flatten()
}

fn parse_hw_card_num(name: &str) -> Option<i32> {
    let after = name.strip_prefix("hw:")?;
    let digits = take_ascii_digits(after);
    (!digits.is_empty())
        .then(|| digits.parse::<i32>().ok())
        .flatten()
}

fn parse_hw_dev_num(name: &str) -> Option<i32> {
    let after = name.strip_prefix("hw:")?;
    let (card, rest) = after.split_once(',')?;
    if card.is_empty() || !card.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let digits = take_ascii_digits(rest);
    (!digits.is_empty())
        .then(|| digits.parse::<i32>().ok())
        .flatten()
}

fn parse_card_line(line: &str) -> Option<CardInfo> {
    let (left, right) = line.split_once("]:")?;
    let (idx_part, id_part) = left.split_once('[')?;
    let index = idx_part.trim().parse::<i32>().ok()?;
    let id = id_part.trim();
    if id.is_empty() || id.contains(char::is_whitespace) {
        return None;
    }
    let name = right.match_indices('-').find_map(|(pos, _)| {
        let before_ws = right[..pos]
            .chars()
            .next_back()
            .is_some_and(char::is_whitespace);
        let after_ws = right[pos + 1..]
            .chars()
            .next()
            .is_some_and(char::is_whitespace);
        (before_ws && after_ws).then(|| right[pos + 1..].trim())
    })?;
    Some(CardInfo {
        index,
        id: id.to_owned(),
        name: name.to_owned(),
    })
}

/// A PCM substream's `status` file: `closed` when nobody has it open.
pub(crate) fn status_is_closed(text: &str) -> bool {
    text.trim_start().starts_with("closed")
}

pub fn parse_hw_params(text: &str) -> Option<AlsaHwParams> {
    if status_is_closed(text) {
        return None;
    }
    let format = read_kv(text, "format")?;
    let rate = read_kv(text, "rate")?
        .split_whitespace()
        .next()?
        .parse::<i32>()
        .ok()?;
    let channels = read_kv(text, "channels")?.parse::<i32>().ok()?;
    Some(AlsaHwParams {
        format,
        rate,
        channels,
    })
}

pub fn evaluate_bit_perfect(
    src: Option<&AudioFormat>,
    out_exists: bool,
    hw: Option<&AlsaHwParams>,
    track_bits_per_sample: i32,
    sample_alteration: Option<&str>,
) -> String {
    let (Some(src), true, Some(hw)) = (src, out_exists, hw) else {
        return "ACTIVE. PLAY A TRACK TO CHECK WHAT REACHES THE CARD.".to_owned();
    };
    let src_int_bits = src.integer_bits();
    let lossy = src_int_bits == 0;
    let out_bits = hw_bits(&hw.format);
    let src_bits = if track_bits_per_sample > 0 && src_int_bits > 0 {
        track_bits_per_sample.min(src_int_bits)
    } else {
        src_int_bits
    };

    if hw.rate != src.rate {
        return format!(
            "NOT BIT-PERFECT // RESAMPLED {} TO {} KHZ: THE CARD DOES NOT TAKE THIS RATE",
            khz(src.rate),
            khz(hw.rate)
        );
    }
    if hw.channels != src.channels {
        return format!(
            "NOT BIT-PERFECT // {} CHANNELS MIXED TO {}",
            src.channels, hw.channels
        );
    }
    if !lossy && out_bits < src_bits {
        return format!(
            "NOT BIT-PERFECT // {src_bits}-BIT SOURCE REDUCED TO {}",
            hw.format
        );
    }
    if lossy {
        return format!(
            "DIRECT // {} KHZ // DECODED STRAIGHT TO THE CARD ({})",
            khz(hw.rate),
            hw.format
        );
    }
    if let Some(altered) = sample_alteration {
        return format!("NOT BIT-PERFECT // {altered}");
    }
    format!(
        "BIT-PERFECT // {} KHZ // {src_bits}-BIT // {}",
        khz(hw.rate),
        hw.format
    )
}

fn khz(hz: i32) -> String {
    let v = hz as f64 / 1000.0;
    let s = format!("{v:.3}");
    s.trim_end_matches('0').trim_end_matches('.').to_owned()
}

fn hw_bits(format: &str) -> i32 {
    if format.starts_with("FLOAT64") {
        64
    } else if format.starts_with("S32") || format.starts_with("U32") {
        32
    } else if format.starts_with("S24")
        || format.starts_with("U24")
        || format.starts_with("FLOAT_")
        || format == "FLOAT"
    {
        // IEEE 754 f32 carries 24 bits of integer precision (23 explicit mantissa bits + 1 implicit).
        24
    } else if format.starts_with("S20") || format.starts_with("U20") {
        20
    } else if format.starts_with("S16") || format.starts_with("U16") {
        16
    } else if format == "U8" || format == "S8" {
        8
    } else {
        0
    }
}

fn read_cards() -> Vec<CardInfo> {
    let Ok(text) = fs::read_to_string("/proc/asound/cards") else {
        return Vec::new();
    };
    text.lines().filter_map(parse_card_line).collect()
}

fn has_monitor(card_dir: &Path) -> bool {
    let Ok(entries) = fs::read_dir(card_dir) else {
        return false;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        if name.to_string_lossy().starts_with("eld#")
            && let Ok(text) = fs::read_to_string(entry.path())
            && text
                .lines()
                .any(|l| l.starts_with("monitor_present") && l.trim_end().ends_with('1'))
        {
            return true;
        }
    }
    false
}

fn read_field(path: &Path, field: &str) -> Option<String> {
    let text = fs::read_to_string(path).ok()?;
    read_kv(&text, field)
}

fn read_kv(text: &str, key: &str) -> Option<String> {
    let prefix = format!("{key}:");
    text.lines()
        .find(|l| l.starts_with(&prefix))
        .map(|l| l[prefix.len()..].trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hw_params_status_cards_and_evaluates_bit_perfect() {
        let raw = "\
access: MMAP_INTERLEAVED
format: S32_LE
subformat: STD
channels: 2
rate: 96000 (96000/1)
period_size: 4800
buffer_size: 19200
";
        let hw = parse_hw_params(raw).unwrap();
        assert_eq!(hw.format, "S32_LE");
        assert_eq!(hw.rate, 96000);
        assert_eq!(hw.channels, 2);

        let src = AudioFormat {
            format: "s32".to_owned(),
            rate: 96000,
            channels: 2,
        };
        assert_eq!(
            evaluate_bit_perfect(Some(&src), true, Some(&hw), 24, None),
            "BIT-PERFECT // 96 KHZ // 24-BIT // S32_LE"
        );
        assert_eq!(
            evaluate_bit_perfect(
                Some(&src),
                true,
                Some(&hw),
                24,
                Some("SOFTWARE VOLUME IS ON")
            ),
            "NOT BIT-PERFECT // SOFTWARE VOLUME IS ON"
        );
        assert!(parse_hw_params("  closed\n").is_none());
        assert!(status_is_closed("closed\n"));
        assert!(!status_is_closed("state: RUNNING\nowner_pid   : 1234\n"));

        assert_eq!(hw_bits("U24_3LE"), 24);
        assert_eq!(hw_bits("FLOAT_LE"), 24);
        assert_eq!(hw_bits("U32_LE"), 32);
        assert_eq!(AlsaDirect::device_number("alsa/hw:CARD=Pro,DEV=2"), 2);
        assert_eq!(AlsaDirect::device_number("alsa/hw:1,3"), 3);
        let card = parse_card_line(" 1 [Pro            ]: USB-Audio - Topping D10s").unwrap();
        assert_eq!(
            (card.index, card.id.as_str(), card.name.as_str()),
            (1, "Pro", "Topping D10s")
        );
    }

    #[test]
    fn list_devices_reads_proc_asound_without_panic() {
        for d in AlsaDirect::list_devices() {
            assert!(AlsaDirect::is_usb_card(d.card), "{d:?} is not a USB card");
        }
        let _ = AlsaDirect::pcm_is_open("alsa/hw:CARD=DoesNotExist,DEV=0");
        assert!(AlsaDirect::pcm_holders("pipewire/foo").is_empty());
    }
}
