use std::path::PathBuf;

/// Opens a native Linux folder chooser via XDG Desktop Portal (`ashpd`) when enabled,
/// keeping Wayland dialogs sandbox-compatible without GTK or X11 dependencies.
#[cfg(feature = "portal")]
pub async fn pick_music_folders() -> Result<Vec<PathBuf>, String> {
    use ashpd::desktop::file_chooser::SelectedFiles;
    use ashpd::desktop::ResponseError;

    let req = SelectedFiles::open_file()
        .title("Add Music Folders")
        .directory(true)
        .multiple(true)
        .send()
        .await
        .map_err(|e| format!("XDG Portal file chooser request failed: {e}"))?;

    match req.response() {
        Ok(response) => Ok(uris_to_local_paths(response.uris())),
        Err(ashpd::Error::Response(ResponseError::Cancelled)) => Ok(Vec::new()),
        Err(e) => Err(format!("Folder selection failed: {e}")),
    }
}

/// Opens a native Linux file chooser for audio tracks via XDG Desktop Portal.
#[cfg(feature = "portal")]
pub async fn pick_music_files() -> Result<Vec<PathBuf>, String> {
    use ashpd::desktop::file_chooser::{FileFilter, SelectedFiles};
    use ashpd::desktop::ResponseError;

    const EXTS: &[&str] = &[
        "flac", "mp3", "wav", "ogg", "opus", "m4a", "aac", "wma", "aiff", "alac", "wv", "ape",
        "dsf", "dff",
    ];
    let mut filter = FileFilter::new("Audio Files");
    for ext in EXTS {
        filter = filter.glob(&format!("*.{ext}"));
    }

    let req = SelectedFiles::open_file()
        .title("Add Music Files")
        .multiple(true)
        .filter(filter)
        .send()
        .await
        .map_err(|e| format!("XDG Portal file chooser request failed: {e}"))?;

    match req.response() {
        Ok(response) => Ok(uris_to_local_paths(response.uris())),
        Err(ashpd::Error::Response(ResponseError::Cancelled)) => Ok(Vec::new()),
        Err(e) => Err(format!("Audio file selection failed: {e}")),
    }
}

/// Opens a native Linux file chooser for a single cover image via XDG Desktop Portal.
#[cfg(feature = "portal")]
pub async fn pick_cover_image() -> Result<Option<PathBuf>, String> {
    use ashpd::desktop::file_chooser::{FileFilter, SelectedFiles};
    use ashpd::desktop::ResponseError;

    let filter = FileFilter::new("Images")
        .glob("*.jpg")
        .glob("*.jpeg")
        .glob("*.png")
        .glob("*.webp");

    let req = SelectedFiles::open_file()
        .title("Choose Cover Image")
        .multiple(false)
        .filter(filter)
        .send()
        .await
        .map_err(|e| format!("XDG Portal file chooser request failed: {e}"))?;

    match req.response() {
        Ok(response) => Ok(uris_to_local_paths(response.uris()).into_iter().next()),
        Err(ashpd::Error::Response(ResponseError::Cancelled)) => Ok(None),
        Err(e) => Err(format!("Image selection failed: {e}")),
    }
}

/// Opens a native Linux file chooser for importing `.m3u8`, `.m3u`, or `.zip` playlist/backup files.
#[cfg(feature = "portal")]
pub async fn pick_playlist_import_file() -> Result<Option<PathBuf>, String> {
    use ashpd::desktop::ResponseError;
    use ashpd::desktop::file_chooser::{FileFilter, SelectedFiles};

    let all_supported = FileFilter::new("Playlists & Backups")
        .glob("*.m3u8")
        .glob("*.m3u")
        .glob("*.zip")
        .glob("*.M3U8")
        .glob("*.M3U")
        .glob("*.ZIP");
    let m3u_only = FileFilter::new("M3U Playlists")
        .glob("*.m3u8")
        .glob("*.m3u")
        .glob("*.M3U8")
        .glob("*.M3U");
    let zip_only = FileFilter::new("Retrograde Backups")
        .glob("*.zip")
        .glob("*.ZIP");

    let req = SelectedFiles::open_file()
        .title("Import Playlist or Backup")
        .multiple(false)
        .filter(all_supported)
        .filter(m3u_only)
        .filter(zip_only)
        .send()
        .await
        .map_err(|e| format!("XDG Portal file chooser request failed: {e}"))?;

    match req.response() {
        Ok(response) => Ok(uris_to_local_paths(response.uris()).into_iter().next()),
        Err(ashpd::Error::Response(ResponseError::Cancelled)) => Ok(None),
        Err(e) => Err(format!("Playlist import selection failed: {e}")),
    }
}

/// Opens a native Linux save-file dialog for exporting `.m3u8` or `.zip` playlists/backups.
#[cfg(feature = "portal")]
pub async fn pick_playlist_export_file(
    suggested_name: String,
    is_zip: bool,
) -> Result<Option<PathBuf>, String> {
    use ashpd::desktop::ResponseError;
    use ashpd::desktop::file_chooser::{FileFilter, SelectedFiles};

    let make_filter = || {
        if is_zip {
            FileFilter::new("Retrograde Backup Archive (*.zip)")
                .glob("*.zip")
                .glob("*.ZIP")
        } else {
            FileFilter::new("Extended M3U8 Playlist (*.m3u8)")
                .glob("*.m3u8")
                .glob("*.M3U8")
        }
    };

    let title = if is_zip {
        "Export Retrograde Archive"
    } else {
        "Export M3U8 Playlist"
    };

    let req = SelectedFiles::save_file()
        .title(title)
        .current_name(suggested_name.as_str())
        .filter(make_filter())
        .current_filter(make_filter())
        .send()
        .await
        .map_err(|e| format!("XDG Portal save dialog request failed: {e}"))?;

    match req.response() {
        Ok(response) => Ok(uris_to_local_paths(response.uris())
            .into_iter()
            .next()
            .map(|p| avoid_overwrite(ensure_export_extension(p, is_zip), is_zip))),
        Err(ashpd::Error::Response(ResponseError::Cancelled)) => Ok(None),
        Err(e) => Err(format!("Export destination selection failed: {e}")),
    }
}

#[cfg(any(feature = "portal", test))]
fn is_document_portal_path(path: &std::path::Path) -> bool {
    use std::path::Component;
    let mut comps = path.components();
    matches!(
        (comps.next(), comps.next(), comps.next(), comps.next(), comps.next()),
        (
            Some(Component::RootDir),
            Some(Component::Normal(run)),
            Some(Component::Normal(user)),
            Some(Component::Normal(_uid)),
            Some(Component::Normal(doc)),
        ) if run == "run" && user == "user" && doc == "doc"
    )
}

#[cfg(any(feature = "portal", test))]
fn ensure_export_extension(path: PathBuf, is_zip: bool) -> PathBuf {
    // Sandboxed XDG Document Portal grants write access only to the exact path returned.
    if is_document_portal_path(&path) {
        return path;
    }
    let ext = path.extension().and_then(|e| e.to_str());
    let has_valid = if is_zip {
        ext.is_some_and(|e| e.eq_ignore_ascii_case("zip"))
    } else {
        ext.is_some_and(|e| e.eq_ignore_ascii_case("m3u8") || e.eq_ignore_ascii_case("m3u"))
    };
    if has_valid {
        return path;
    }
    let mut os = path.into_os_string();
    os.push(if is_zip { ".zip" } else { ".m3u8" });
    PathBuf::from(os)
}

/// The portal only confirmed the name the user typed; if we appended an extension and that
/// file already exists, pick a free `name (n).ext` rather than silently overwriting it.
#[cfg(any(feature = "portal", test))]
fn avoid_overwrite(path: PathBuf, _is_zip: bool) -> PathBuf {
    if is_document_portal_path(&path) || !path.exists() {
        return path;
    }
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("export").to_owned();
    let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("").to_owned();
    for n in 1..1000 {
        let name = if ext.is_empty() { format!("{stem} ({n})") } else { format!("{stem} ({n}).{ext}") };
        let cand = path.with_file_name(name);
        if !cand.exists() {
            return cand;
        }
    }
    path
}

#[cfg(feature = "portal")]
fn uris_to_local_paths(uris: &[ashpd::Uri]) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    for uri in uris {
        if let Some(path) = rustygrade_core::CoverPaths::to_local_path(Some(uri.as_str())) {
            paths.push(path);
        }
    }
    paths
}

#[cfg(not(feature = "portal"))]
pub async fn pick_music_folders() -> Result<Vec<PathBuf>, String> {
    Err("XDG Desktop Portal support was disabled at compile time".to_owned())
}

#[cfg(not(feature = "portal"))]
pub async fn pick_music_files() -> Result<Vec<PathBuf>, String> {
    Err("XDG Desktop Portal support was disabled at compile time".to_owned())
}

#[cfg(not(feature = "portal"))]
pub async fn pick_cover_image() -> Result<Option<PathBuf>, String> {
    Err("XDG Desktop Portal support was disabled at compile time".to_owned())
}

#[cfg(not(feature = "portal"))]
pub async fn pick_playlist_import_file() -> Result<Option<PathBuf>, String> {
    Err("XDG Desktop Portal support was disabled at compile time".to_owned())
}

#[cfg(not(feature = "portal"))]
pub async fn pick_playlist_export_file(
    _suggested_name: String,
    _is_zip: bool,
) -> Result<Option<PathBuf>, String> {
    Err("XDG Desktop Portal support was disabled at compile time".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn avoid_overwrite_picks_free_name() {
        let dir = std::env::temp_dir().join(format!("rg-ovw-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("mix.m3u8");
        assert_eq!(avoid_overwrite(p.clone(), false), p);
        std::fs::write(&p, b"x").unwrap();
        assert_eq!(avoid_overwrite(p.clone(), false), dir.join("mix (1).m3u8"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ensure_export_extension_appends_when_missing_or_preserving_dots_but_skips_doc_portal() {
        assert_eq!(
            ensure_export_extension(PathBuf::from("/home/okarin/Music/mix"), false),
            PathBuf::from("/home/okarin/Music/mix.m3u8")
        );
        assert_eq!(
            ensure_export_extension(PathBuf::from("/home/okarin/Music/my.mix"), true),
            PathBuf::from("/home/okarin/Music/my.mix.zip")
        );
        assert_eq!(
            ensure_export_extension(PathBuf::from("/home/okarin/Music/MIX.M3U"), false),
            PathBuf::from("/home/okarin/Music/MIX.M3U")
        );
        assert_eq!(
            ensure_export_extension(PathBuf::from("/home/okarin/Music/backup.ZIP"), true),
            PathBuf::from("/home/okarin/Music/backup.ZIP")
        );
        assert_eq!(
            ensure_export_extension(PathBuf::from("/run/user/1000/doc/deadbeef/mix"), false),
            PathBuf::from("/run/user/1000/doc/deadbeef/mix")
        );
        assert_eq!(
            ensure_export_extension(PathBuf::from("/run/user/1000/doc/deadbeef/backup"), true),
            PathBuf::from("/run/user/1000/doc/deadbeef/backup")
        );
    }
}
