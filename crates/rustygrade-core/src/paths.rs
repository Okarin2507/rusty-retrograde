use std::path::{Path, PathBuf};
use std::sync::Once;

/// Cover cache naming and app directory resolution shared with the Electron & C# builds
/// (`~/.rustygrade/covers/{hash}_thumb.jpg`).
pub struct CoverPaths;

static MIGRATED: Once = Once::new();

/// One-time move of the data folder used before the rename (`~/.retrograde`) to `~/.rustygrade`.
/// Does nothing when the new folder already exists or the old one is absent.
fn migrate_legacy_dir(home: &Path, dir: &Path) {
    let legacy = home.join(".retrograde");
    if !dir.exists() && legacy.is_dir() && std::fs::rename(&legacy, dir).is_ok() {
        // Cover URLs in the library are absolute `file://` paths into the old folder.
        for name in [
            "library.json",
            "library.json.bak",
            "native-collections.json",
            "native-collections.json.bak",
        ] {
            let path = dir.join(name);
            if let Ok(text) = std::fs::read_to_string(&path)
                && text.contains("/.retrograde/")
            {
                let tmp = path.with_extension("tmp");
                if std::fs::write(&tmp, text.replace("/.retrograde/", "/.rustygrade/")).is_ok() {
                    let _ = std::fs::rename(&tmp, &path);
                }
            }
        }
    }
}

#[cfg(test)]
std::thread_local! {
    static TEST_APP_DIR: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(crate) fn with_test_app_dir<R>(dir: &Path, f: impl FnOnce() -> R) -> R {
    struct Reset(Option<PathBuf>);
    impl Drop for Reset {
        fn drop(&mut self) {
            TEST_APP_DIR.with(|cell| {
                *cell.borrow_mut() = self.0.take();
            });
        }
    }
    let prev = TEST_APP_DIR.with(|cell| cell.borrow_mut().replace(dir.to_path_buf()));
    let _guard = Reset(prev);
    f()
}

fn is_temp_cover_name(name: &str) -> bool {
    name.contains(".tmp") || name.ends_with(".resync-old") || name.ends_with(".shrink-tmp")
}

/// Decodes `file://` / `file://localhost/` URIs (stripping `?query` and `#fragment` and
/// legacy `/C:/` drive prefixes). Returns `None` if `raw` does not start with `file://`.
pub(crate) fn decode_file_uri(raw: &str) -> Option<String> {
    if !raw
        .as_bytes()
        .get(..7)
        .is_some_and(|b| b.eq_ignore_ascii_case(b"file://"))
    {
        return None;
    }
    let mut body = &raw[7..];
    if body
        .as_bytes()
        .get(..10)
        .is_some_and(|b| b.eq_ignore_ascii_case(b"localhost/"))
    {
        body = &body[9..];
    }
    let before_qf = match (body.find('?'), body.find('#')) {
        (Some(q), Some(h)) => &body[..q.min(h)],
        (Some(q), None) => &body[..q],
        (None, Some(h)) => &body[..h],
        (None, None) => body,
    };
    let bytes = before_qf.as_bytes();
    let p = if bytes.len() > 2
        && bytes[0] == b'/'
        && bytes[2] == b':'
        && bytes[1].is_ascii_alphabetic()
    {
        &before_qf[1..]
    } else {
        before_qf
    };
    let decoded = urlencoding::decode(p)
        .map(|c| c.into_owned())
        .unwrap_or_else(|_| p.to_owned());
    Some(decoded)
}

impl CoverPaths {
    /// `~/.rustygrade`, or `RETRO_APPDIR` when set and non-empty (tests run against a scratch copy).
    pub fn app_dir() -> PathBuf {
        #[cfg(test)]
        if let Some(dir) = TEST_APP_DIR.with(|cell| cell.borrow().clone()) {
            return dir;
        }
        if let Ok(dir) = std::env::var("RETRO_APPDIR")
            && !dir.is_empty()
        {
            return PathBuf::from(dir);
        }
        let home = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        let dir = home.join(".rustygrade");
        MIGRATED.call_once(|| migrate_legacy_dir(&home, &dir));
        dir
    }

    pub fn cover_dir() -> PathBuf {
        Self::app_dir().join("covers")
    }

    /// Bit-exact port of the JS `hashName()` / C# `CoverPaths.HashName()`:
    /// iterates UTF-16 code units (`encode_utf16`), updating
    /// `hash = ((hash << 5) - hash + unit)` in wrapping 32-bit signed arithmetic,
    /// then formats `abs(hash as i64)` in lowercase base-36.
    pub fn hash_name(s: &str) -> String {
        let mut hash: i32 = 0;
        for unit in s.encode_utf16() {
            hash = hash
                .wrapping_shl(5)
                .wrapping_sub(hash)
                .wrapping_add(unit as i32);
        }
        to_base36((hash as i64).abs())
    }

    pub fn thumb_path(album_name: &str) -> PathBuf {
        Self::cover_dir().join(format!("{}_thumb.jpg", Self::hash_name(album_name)))
    }

    pub fn full_path(album_name: &str) -> PathBuf {
        Self::cover_dir().join(format!("{}_full.jpg", Self::hash_name(album_name)))
    }

    pub fn content_cover_path(hash_hex: &str, ext: &str) -> PathBuf {
        Self::cover_dir().join(format!("c_{hash_hex}.{ext}"))
    }

    pub fn is_content_cover_file(path: &Path) -> bool {
        if path.parent() != Some(Self::cover_dir().as_path()) {
            return false;
        }
        path.file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|name| name.starts_with("c_") && !is_temp_cover_name(name))
    }

    pub fn is_managed_cover_file(path: &Path) -> bool {
        if path.parent() != Some(Self::cover_dir().as_path()) {
            return false;
        }
        path.file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|name| {
                if is_temp_cover_name(name) {
                    return false;
                }
                let lower = name.to_ascii_lowercase();
                name.starts_with("c_")
                    || name.starts_with("native_pl_")
                    || lower.ends_with("_thumb.jpg")
                    || lower.ends_with("_full.jpg")
            })
    }

    /// Turns `"file:///path/to/x.jpg?t=..."` or a plain path into a local `PathBuf`.
    pub fn to_local_path(url_or_path: Option<&str>) -> Option<PathBuf> {
        let raw = url_or_path?.trim();
        if raw.is_empty() {
            return None;
        }
        if let Some(decoded) = decode_file_uri(raw) {
            return Some(PathBuf::from(decoded));
        }
        Some(PathBuf::from(raw))
    }

    pub fn to_file_url(path: &Path) -> String {
        let normalized = path.to_string_lossy().replace('\\', "/");
        let encoded = normalized
            .split('/')
            .map(|seg| urlencoding::encode(seg).into_owned())
            .collect::<Vec<_>>()
            .join("/");
        format!("file://{encoded}")
    }
}

fn to_base36(mut value: i64) -> String {
    const DIGITS: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    if value == 0 {
        return "0".to_owned();
    }
    let mut buf = [0u8; 16];
    let mut i = buf.len();
    while value > 0 {
        i -= 1;
        buf[i] = DIGITS[(value % 36) as usize];
        value /= 36;
    }
    String::from_utf8(buf[i..].to_vec()).expect("base36 digits are ASCII")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_name_matches_real_library_and_utf16_surrogates() {
        assert_eq!(
            CoverPaths::hash_name("Ado 1st Live Kigeki (Live At Zepp DiverCity, 2022)"),
            "oy1a16"
        );
        assert_eq!(CoverPaths::hash_name(""), "0");
        assert_eq!(CoverPaths::hash_name("🎵"), "120ah");
        assert_eq!(CoverPaths::hash_name("残夢"), "is53");
    }

    #[test]
    fn to_local_path_handles_file_urls_and_queries() {
        let p = CoverPaths::to_local_path(Some(
            "file:///home/okarin/.rustygrade/covers/a%20b.jpg?t=123",
        ))
        .unwrap();
        assert_eq!(p, PathBuf::from("/home/okarin/.rustygrade/covers/a b.jpg"));
        assert_eq!(
            CoverPaths::to_local_path(Some("/music/What?/01.flac")),
            Some(PathBuf::from("/music/What?/01.flac"))
        );
        let url = CoverPaths::to_file_url(Path::new("/home/okarin/My Covers/a #1?.jpg"));
        assert_eq!(url, "file:///home/okarin/My%20Covers/a%20%231%3F.jpg");
        assert_eq!(
            CoverPaths::to_local_path(Some(&url)),
            Some(PathBuf::from("/home/okarin/My Covers/a #1?.jpg"))
        );
        assert_eq!(CoverPaths::to_local_path(Some("   ")), None);
        assert_eq!(CoverPaths::to_local_path(None), None);
    }
}
