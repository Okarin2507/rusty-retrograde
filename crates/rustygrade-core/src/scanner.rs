use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::LazyLock;
use std::time::{SystemTime, UNIX_EPOCH};

use lofty::config::ParseOptions;
use lofty::file::{AudioFile, FileType, TaggedFileExt};
use lofty::picture::PictureType;
use lofty::probe::Probe;
use lofty::tag::{Accessor, ItemKey};
use rayon::prelude::*;
use regex::Regex;

use crate::collections::CollectionsStore;
use crate::covers::{
    ImageLimits, MAX_COVER_FILE_BYTES, PlaylistCovers, delete_unreferenced_covers,
    save_cover_bytes, sha256,
};
use crate::library::{AlbumDto, LibraryError, LibraryMap, LibraryStore, TrackDto};
use crate::paths::CoverPaths;

pub const AUDIO_EXTENSIONS: &[&str] = &[
    "flac", "mp3", "wav", "aac", "ogg", "m4a", "wma", "aiff", "alac", "opus",
];

const FOLDER_COVER_NAMES: &[&str] = &["cover", "folder", "front", "album", "albumart"];

static DISC_FOLDER_RX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^(cd|disc|disk|dvd|vol|volume)[\s._-]*\d+").unwrap());
static ARTIST_SEP_RX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\s*(?:/|&|,|;|、|，|＆|\s+feat\.?\s+|\s+ft\.?\s+|\s+x\s+|\s+with\s+)\s*")
        .unwrap()
});
static SPLIT_ALBUM_RX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(?P<base>.+) \((?P<who>[^()]+)\)(?: \d+)?$").unwrap());

#[derive(Debug, Clone)]
pub struct ScanProgress {
    pub phase: &'static str,
    pub scanned: usize,
    pub total: usize,
    pub current_file: String,
}

#[derive(Debug, Clone, Default)]
pub struct ScanResult {
    pub added_tracks: usize,
    pub new_albums: usize,
    pub covers: usize,
    pub updated_tracks: usize,
    pub removed_tracks: usize,
    pub removed_albums: usize,
    pub missing_tracks: usize,
    pub merged_albums: usize,
}

impl ScanResult {
    pub fn changed(&self) -> bool {
        self.added_tracks > 0
            || self.covers > 0
            || self.updated_tracks > 0
            || self.removed_tracks > 0
            || self.removed_albums > 0
            || self.merged_albums > 0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PrunePolicy {
    #[default]
    Never,
    Safe,
    ForceAll,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct ScanOptions {
    pub prune: PrunePolicy,
    pub max_threads: Option<usize>,
    pub force: bool,
    /// Re-extracts album covers during the scan without disturbing covers when extraction fails.
    pub resync_covers: bool,
}

#[derive(Default)]
struct Discovery {
    files: Vec<PathBuf>,
    walked_dirs: Vec<PathBuf>,
    unreadable_dirs: Vec<PathBuf>,
    gone_dirs: Vec<PathBuf>,
    suspect_empty_dirs: Vec<PathBuf>,
}

#[derive(Debug, Clone)]
struct Parsed {
    file_path: String,
    title: String,
    artist: String,
    album_artist: Option<String>,
    album: String,
    duration: f64,
    track_number: i32,
    disc_number: i32,
    year: i32,
    codec: String,
    sample_rate: i32,
    bits_per_sample: i32,
    channels: i32,
    has_picture: bool,
}

#[derive(Default)]
struct AlbumFolders {
    own: HashSet<String>,
    parents: HashSet<String>,
}

impl AlbumFolders {
    fn from_album(album: &AlbumDto) -> Self {
        let mut f = Self::default();
        if let Some(tracks) = &album.tracks {
            for t in tracks {
                if let Some(fp) = &t.file_path {
                    f.add(fp);
                }
            }
        }
        f
    }

    fn add(&mut self, file_path: &str) {
        let folder = album_folder(file_path);
        let key = folder.to_lowercase();
        if self.own.insert(key)
            && let Some(parent) = Path::new(&folder).parent()
        {
            let p = parent.to_string_lossy().to_lowercase();
            if !p.is_empty() {
                self.parents.insert(p);
            }
        }
    }

    fn is_same_folder(&self, file_path: &str) -> bool {
        self.own.contains(&album_folder(file_path).to_lowercase())
    }

    fn is_near(&self, file_path: &str) -> bool {
        let folder = album_folder(file_path);
        let key = folder.to_lowercase();
        if self.own.contains(&key) || self.parents.contains(&key) {
            return true;
        }
        if let Some(parent) = Path::new(&folder).parent() {
            let pk = parent.to_string_lossy().to_lowercase();
            return !pk.is_empty() && (self.parents.contains(&pk) || self.own.contains(&pk));
        }
        false
    }
}

pub struct Scanner;

impl Scanner {
    pub fn is_audio_file(path: &Path) -> bool {
        path.extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| AUDIO_EXTENSIONS.iter().any(|x| x.eq_ignore_ascii_case(e)))
    }

    pub fn discover(paths: &[PathBuf]) -> Vec<PathBuf> {
        walk_paths(paths).files
    }

    pub fn scan(
        input_paths: &[PathBuf],
        options: ScanOptions,
        cancel: Option<&AtomicBool>,
        mut progress: Option<&mut dyn FnMut(ScanProgress)>,
    ) -> Result<ScanResult, LibraryError> {
        if let Some(cb) = progress.as_deref_mut() {
            cb(ScanProgress {
                phase: "discovering",
                scanned: 0,
                total: 0,
                current_file: "Discovering files...".to_owned(),
            });
        }

        let found = walk_paths(input_paths);
        let _guard = LibraryStore::write_lock()
            .lock()
            .expect("library write lock poisoned");
        scan_locked(found, options, cancel, progress)
    }
}

fn dir_identity(path: &Path) -> Option<(u64, u64)> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        fs::metadata(path).ok().map(|m| (m.dev(), m.ino()))
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

#[cfg(unix)]
fn unescape_mount_octal(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\'
            && i + 3 < bytes.len()
            && bytes[i + 1..i + 4].iter().all(|b| (b'0'..=b'7').contains(b))
        {
            let v = ((bytes[i + 1] - b'0') << 6)
                | ((bytes[i + 2] - b'0') << 3)
                | (bytes[i + 3] - b'0');
            out.push(v);
            i += 4;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(unix)]
fn parse_mountinfo(content: &str) -> Vec<PathBuf> {
    content
        .lines()
        .filter_map(|line| {
            let mp = line.split(' ').nth(4)?;
            if mp.is_empty() {
                None
            } else {
                Some(PathBuf::from(unescape_mount_octal(mp)))
            }
        })
        .collect()
}

#[cfg(unix)]
fn parse_fstab(content: &str) -> Vec<PathBuf> {
    content
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                return None;
            }
            let mp = trimmed.split_whitespace().nth(1)?;
            if mp == "none" || mp == "swap" {
                None
            } else {
                Some(PathBuf::from(unescape_mount_octal(mp)))
            }
        })
        .collect()
}

fn is_unmounted_mountpoint_from(
    dir: &Path,
    active_mounts: &[PathBuf],
    fstab_mounts: &[PathBuf],
) -> bool {
    if active_mounts.iter().any(|m| m == dir) {
        return false;
    }
    if fstab_mounts.iter().any(|m| {
        m != Path::new("/")
            && (dir == m || dir.starts_with(m))
            && !active_mounts.iter().any(|a| a == m)
    }) {
        return true;
    }
    for prefix in ["/mnt", "/media", "/run/media"] {
        let p = Path::new(prefix);
        if dir.starts_with(p) {
            let has_active = active_mounts.iter().any(|m| {
                m.starts_with(p) && m != p && (dir == m || dir.starts_with(m) || m.starts_with(dir))
            });
            if !has_active {
                return true;
            }
        }
    }
    false
}

struct MountSnapshot {
    #[cfg(unix)]
    active_mounts: Vec<PathBuf>,
    #[cfg(unix)]
    fstab_mounts: Vec<PathBuf>,
    #[cfg(unix)]
    loaded: bool,
}

impl MountSnapshot {
    fn new() -> Self {
        Self {
            #[cfg(unix)]
            active_mounts: Vec::new(),
            #[cfg(unix)]
            fstab_mounts: Vec::new(),
            #[cfg(unix)]
            loaded: false,
        }
    }

    fn looks_like_unmounted(&mut self, dir: &Path, root_aliases: &[(PathBuf, PathBuf)]) -> bool {
        #[cfg(unix)]
        {
            if !self.loaded {
                self.loaded = true;
                if let Ok(text) = fs::read_to_string("/proc/self/mountinfo") {
                    self.active_mounts = parse_mountinfo(&text);
                }
                if let Ok(text) = fs::read_to_string("/etc/fstab") {
                    self.fstab_mounts = parse_fstab(&text);
                }
            }
            if is_unmounted_mountpoint_from(dir, &self.active_mounts, &self.fstab_mounts) {
                return true;
            }
            for (canon_root, raw_root) in root_aliases {
                if let Ok(rel) = dir.strip_prefix(canon_root) {
                    let aliased = if rel.as_os_str().is_empty() {
                        raw_root.clone()
                    } else {
                        raw_root.join(rel)
                    };
                    if aliased != dir
                        && is_unmounted_mountpoint_from(
                            &aliased,
                            &self.active_mounts,
                            &self.fstab_mounts,
                        )
                    {
                        return true;
                    }
                }
            }
            false
        }
        #[cfg(not(unix))]
        {
            let _ = (dir, root_aliases);
            false
        }
    }
}

fn push_dir_with_aliases(dir: &Path, root_aliases: &[(PathBuf, PathBuf)], out: &mut Vec<PathBuf>) {
    if !out.iter().any(|existing| existing == dir) {
        out.push(dir.to_path_buf());
    }
    for (canon_root, raw_root) in root_aliases {
        if let Ok(rel) = dir.strip_prefix(canon_root) {
            let aliased = if rel.as_os_str().is_empty() {
                raw_root.clone()
            } else {
                raw_root.join(rel)
            };
            if aliased != dir && !out.iter().any(|existing| existing == &aliased) {
                out.push(aliased);
            }
        }
    }
}

fn walk_paths(paths: &[PathBuf]) -> Discovery {
    let mut found = Discovery::default();
    let mut stack = Vec::new();
    let mut visited_dirs: HashSet<(u64, u64)> = HashSet::new();
    let mut mounts = MountSnapshot::new();
    let mut root_aliases: Vec<(PathBuf, PathBuf)> = Vec::new();

    for p in paths {
        if p.is_dir() {
            let dir = fs::canonicalize(p).unwrap_or_else(|_| p.clone());
            if p != &dir {
                root_aliases.push((dir.clone(), p.clone()));
            }
            match fs::read_dir(&dir) {
                Ok(entries) => {
                    push_dir_with_aliases(&dir, &root_aliases, &mut found.walked_dirs);
                    let first_visit = dir_identity(&dir).is_none_or(|id| visited_dirs.insert(id));
                    let mut had_any_entry = false;
                    for entry in entries.flatten() {
                        had_any_entry = true;
                        if !first_visit {
                            break;
                        }
                        let path = entry.path();
                        let Ok(ft) = entry.file_type() else {
                            continue;
                        };
                        if ft.is_symlink() {
                            continue;
                        }
                        if ft.is_dir() {
                            if dir_identity(&path).is_none_or(|id| visited_dirs.insert(id)) {
                                stack.push(path);
                            }
                        } else if ft.is_file() && Scanner::is_audio_file(&path) {
                            found.files.push(path);
                        }
                    }
                    if !had_any_entry && mounts.looks_like_unmounted(&dir, &root_aliases) {
                        push_dir_with_aliases(&dir, &root_aliases, &mut found.suspect_empty_dirs);
                    }
                }
                Err(_) => {
                    push_dir_with_aliases(&dir, &root_aliases, &mut found.unreadable_dirs);
                }
            }
        } else if p.is_file() {
            if Scanner::is_audio_file(p) {
                found
                    .files
                    .push(fs::canonicalize(p).unwrap_or_else(|_| p.clone()));
            }
        } else if p.is_absolute() {
            // An absent root (moved folder, unmounted drive) counts its tracks as suspect missing.
            // Dropping them needs `PrunePolicy::ForceAll`, which the user has to confirm.
            found.gone_dirs.push(p.clone());
        }
    }

    while let Some(dir) = stack.pop() {
        match fs::read_dir(&dir) {
            Ok(entries) => {
                let mut had_any_entry = false;
                for entry in entries.flatten() {
                    had_any_entry = true;
                    let path = entry.path();
                    let Ok(ft) = entry.file_type() else {
                        continue;
                    };
                    if ft.is_symlink() {
                        continue;
                    }
                    if ft.is_dir() {
                        if dir_identity(&path).is_none_or(|id| visited_dirs.insert(id)) {
                            stack.push(path);
                        }
                    } else if ft.is_file() && Scanner::is_audio_file(&path) {
                        found.files.push(path);
                    }
                }
                if !had_any_entry && mounts.looks_like_unmounted(&dir, &root_aliases) {
                    push_dir_with_aliases(&dir, &root_aliases, &mut found.suspect_empty_dirs);
                }
            }
            Err(_) => {
                push_dir_with_aliases(&dir, &root_aliases, &mut found.unreadable_dirs);
            }
        }
    }
    found
}

fn scan_locked(
    found: Discovery,
    options: ScanOptions,
    cancel: Option<&AtomicBool>,
    mut progress: Option<&mut dyn FnMut(ScanProgress)>,
) -> Result<ScanResult, LibraryError> {
    let mut library = LibraryStore::load_raw()?;
    let mut dirty = false;
    let mut touched: HashSet<String> = HashSet::new();
    let mut mixed: HashSet<String> = HashSet::new();
    let mut detached_covers: Vec<PathBuf> = Vec::new();
    let mut removed_albums = 0usize;

    let merged = merge_split_albums(&mut library, &mut touched, &mut mixed, &mut detached_covers);

    // Map lowercase filePath -> (album_key, track_index)
    let mut by_path: HashMap<String, (String, usize)> = HashMap::new();
    for (key, album) in &library {
        if let Some(tracks) = &album.tracks {
            for (idx, t) in tracks.iter().enumerate() {
                if let Some(fp) = &t.file_path {
                    by_path.entry(fp.to_lowercase()).or_insert((key.clone(), idx));
                }
            }
        }
    }

    let mut to_parse: Vec<PathBuf> = Vec::new();
    let mut changed: HashSet<String> = HashSet::new();
    let mut stamps: HashMap<String, (i64, i64)> = HashMap::new();
    let mut in_scan: HashSet<String> = HashSet::new();
    let mut seen_lower: HashSet<String> = HashSet::new();

    for f in &found.files {
        let f_str = f.to_string_lossy().into_owned();
        let f_low = f_str.to_lowercase();
        seen_lower.insert(f_low.clone());
        let Some(stamp) = file_stamp(f) else {
            continue;
        };
        stamps.insert(f_low.clone(), stamp);

        if let Some((album_key, track_idx)) = by_path.get(&f_low) {
            in_scan.insert(album_key.clone());
            if let Some(album) = library.get_mut(album_key)
                && let Some(tracks) = &mut album.tracks
                && let Some(track) = tracks.get_mut(*track_idx)
            {
                if track.mtime.is_none() || track.size.is_none() {
                    track.mtime = Some(stamp.0);
                    track.size = Some(stamp.1);
                    dirty = true;
                } else if options.force || track.mtime != Some(stamp.0) || track.size != Some(stamp.1) {
                    to_parse.push(f.clone());
                    changed.insert(f_low);
                }
            }
        } else {
            to_parse.push(f.clone());
        }
    }

    let missing_tracks = collect_missing(&library, &by_path, &found, &seen_lower);
    let (removed, missing) = match options.prune {
        PrunePolicy::Never => (0, missing_tracks.safe.len() + missing_tracks.suspect.len()),
        PrunePolicy::Safe => (
            prune_tracks(&mut library, &missing_tracks.safe, &mut touched),
            missing_tracks.suspect.len(),
        ),
        PrunePolicy::ForceAll => {
            let mut all_gone = missing_tracks.safe;
            all_gone.extend(missing_tracks.suspect);
            (prune_tracks(&mut library, &all_gone, &mut touched), 0)
        }
    };

    let mut parsed_results: Vec<Parsed> = Vec::new();
    if !to_parse.is_empty() {
        let total = to_parse.len();
        if let Some(cb) = progress.as_deref_mut() {
            cb(ScanProgress {
                phase: "scanning",
                scanned: 0,
                total,
                current_file: String::new(),
            });
        }
        let threads = options
            .max_threads
            .unwrap_or_else(|| std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).min(4))
            .max(1);
        let done = AtomicUsize::new(0);
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .build()
            .map_err(std::io::Error::other)?;
        parsed_results = pool.install(|| {
            to_parse
                .par_iter()
                .filter_map(|p| {
                    if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
                        return None;
                    }
                    let res = parse_file(p);
                    done.fetch_add(1, Ordering::Relaxed);
                    res
                })
                .collect()
        });
        if let Some(cb) = progress.as_deref_mut() {
            cb(ScanProgress {
                phase: "scanning",
                scanned: total,
                total,
                current_file: String::new(),
            });
        }
    }

    if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
        return Ok(ScanResult::default());
    }

    parsed_results.sort_by_key(|a| a.file_path.to_lowercase());

    let mut folder_cache: HashMap<String, AlbumFolders> = HashMap::new();
    let mut picture_source: HashMap<String, String> = HashMap::new();
    let mut added = 0usize;
    let mut updated = 0usize;
    let mut new_albums = 0usize;
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);

    // Track existing file paths (lowercased) inside albums to avoid duplicate additions.
    let mut known_paths: HashSet<String> = HashSet::new();
    for album in library.values() {
        for t in album.tracks.iter().flatten() {
            if let Some(fp) = &t.file_path {
                known_paths.insert(fp.to_lowercase());
            }
        }
    }

    for r in parsed_results {
        let low = r.file_path.to_lowercase();
        let mut is_update = false;
        // Where a re-read track sat in its album: it goes back there if it stays in that album, so
        // a hand-made order (albums without track numbers) survives tag edits.
        let mut old_slot: Option<(String, usize)> = None;
        if changed.contains(&low)
            && let Some((old_key, _)) = by_path.get(&low).cloned()
            && let Some(old_album) = library.get_mut(&old_key)
            && let Some(tracks) = &mut old_album.tracks
        {
            if let Some(pos) = tracks.iter().position(|t| {
                t.file_path
                    .as_ref()
                    .is_some_and(|fp| fp.eq_ignore_ascii_case(&r.file_path))
            }) {
                old_slot = Some((old_key.clone(), pos));
            }
            tracks.retain(|t| {
                t.file_path
                    .as_ref()
                    .is_none_or(|fp| !fp.eq_ignore_ascii_case(&r.file_path))
            });
            folder_cache.remove(&old_key);
            touched.insert(old_key);
            known_paths.remove(&low);
            is_update = true;
        }

        let (key, created) = resolve_album(&mut library, &mut folder_cache, &r, now_ms);
        if created {
            new_albums += 1;
        }
        let album = library.get_mut(&key).expect("album just resolved");
        if album.year == 0 && r.year > 0 {
            album.year = r.year;
        }
        if !created
            && let Some(ra) = &r.album_artist
            && !ra.trim().is_empty()
            && let Some(aa) = &album.album_artist
            && !aa.trim().is_empty()
            && !same_artist(aa, ra)
        {
            mixed.insert(key.clone());
        } else if album.album_artist.as_deref().unwrap_or("").is_empty()
            && let Some(ra) = &r.album_artist
            && !ra.is_empty()
            && !mixed.contains(&key)
        {
            album.album_artist = Some(ra.clone());
        }

        if r.has_picture && !picture_source.contains_key(&key) {
            picture_source.insert(key.clone(), r.file_path.clone());
        }
        if known_paths.contains(&low) {
            continue;
        }

        let stamp = stamps.get(&low).copied();
        let track = TrackDto {
            id: Some(r.file_path.clone()),
            title: Some(r.title),
            artist: Some(r.artist),
            album: Some(key.clone()),
            duration: r.duration,
            file_path: Some(r.file_path.clone()),
            src: Some(CoverPaths::to_file_url(Path::new(&r.file_path))),
            track_number: Some(r.track_number),
            disc_number: if r.disc_number <= 0 { 1 } else { r.disc_number },
            codec: Some(r.codec),
            sample_rate: r.sample_rate,
            bits_per_sample: r.bits_per_sample,
            channels: r.channels,
            mtime: stamp.map(|s| s.0),
            size: stamp.map(|s| s.1),
            extra: Default::default(),
        };
        let tracks = album.tracks.get_or_insert_with(Vec::new);
        match old_slot {
            Some((old_key, pos)) if old_key == key => tracks.insert(pos.min(tracks.len()), track),
            _ => tracks.push(track),
        }
        folder_cache
            .entry(key.clone())
            .or_insert_with(|| AlbumFolders::from_album(album))
            .add(&r.file_path);
        known_paths.insert(low);
        touched.insert(key.clone());
        in_scan.insert(key);
        if is_update {
            updated += 1;
        } else {
            added += 1;
        }
    }

    for key in &touched {
        if let Some(a) = library.get(key)
            && a.album_artist.as_deref().unwrap_or("").trim().is_empty()
            && let Some(tracks) = &a.tracks
            && tracks.len() > 1
        {
            let distinct: HashSet<String> = tracks
                .iter()
                .map(|t| normalize_artist(t.artist.as_deref().unwrap_or("")))
                .collect();
            if distinct.len() > 1 {
                mixed.insert(key.clone());
            }
        }
    }
    for key in &mixed {
        if let Some(album) = library.get_mut(key)
            && let Some(tracks) = &album.tracks
            && !tracks.is_empty()
        {
            album.album_artist = None;
            album.artist = Some(common_artist(tracks.iter().map(|t| t.artist.as_deref())));
        }
    }

    if options.prune != PrunePolicy::Never {
        for (key, album) in &library {
            if album.tracks.as_ref().is_none_or(|t| t.is_empty()) {
                touched.insert(key.clone());
            }
        }
    }

    for key in &touched {
        let empty = library
            .get(key)
            .is_some_and(|a| a.tracks.as_ref().is_none_or(|t| t.is_empty()));
        if empty {
            if let Some(removed_album) = library.shift_remove(key) {
                removed_albums += 1;
                collect_album_cover_candidates(key, &removed_album, &mut detached_covers);
            }
            continue;
        }
        if let Some(album) = library.get_mut(key)
            && let Some(tracks) = &mut album.tracks
        {
            tracks.sort_by(|a, b| {
                let da = if a.disc_number <= 0 { 1 } else { a.disc_number };
                let db = if b.disc_number <= 0 { 1 } else { b.disc_number };
                da.cmp(&db)
                    .then_with(|| a.track_number.unwrap_or(0).cmp(&b.track_number.unwrap_or(0)))
            });
        }
    }

    let needing: Vec<String> = in_scan
        .into_iter()
        .filter(|k| {
            library
                .get(k)
                .is_some_and(|a| options.resync_covers || album_cover_missing(a))
        })
        .collect();
    let mut covers = 0usize;
    let mut cover_dedup_cache: HashMap<[u8; 32], String> = HashMap::new();
    let total_covers = needing.len();
    for (i, key) in needing.into_iter().enumerate() {
        if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
            break;
        }
        if let Some(cb) = progress.as_deref_mut() {
            cb(ScanProgress {
                phase: "covers",
                scanned: i,
                total: total_covers,
                current_file: key.clone(),
            });
        }
        let pic_src = picture_source.get(&key).map(String::as_str);
        if let Some(album) = library.get(&key)
            && let Some((thumb, full)) =
                find_and_save_cover(album, pic_src, &mut cover_dedup_cache)
            && let Some(album_mut) = library.get_mut(&key)
        {
            let new_thumb_local = CoverPaths::to_local_path(Some(&thumb));
            let new_full_local = CoverPaths::to_local_path(Some(&full));
            if let Some(old) = CoverPaths::to_local_path(album_mut.cover_art.as_deref())
                && new_thumb_local.as_ref() != Some(&old)
            {
                detached_covers.push(old);
            }
            if let Some(old) = CoverPaths::to_local_path(album_mut.cover_art_full.as_deref())
                && new_full_local.as_ref() != Some(&old)
            {
                detached_covers.push(old);
            }
            if album_mut.cover_art.as_deref() != Some(thumb.as_str())
                || album_mut.cover_art_full.as_deref() != Some(full.as_str())
            {
                dirty = true;
            }
            album_mut.cover_art = Some(thumb);
            album_mut.cover_art_full = Some(full);
            covers += 1;
        }
    }

    let result = ScanResult {
        added_tracks: added,
        new_albums,
        covers,
        updated_tracks: updated,
        removed_tracks: removed,
        removed_albums,
        missing_tracks: missing,
        merged_albums: merged,
    };
    if !mixed.is_empty() {
        dirty = true;
    }
    if result.changed() || dirty {
        LibraryStore::save_raw(&library)?;
        if !detached_covers.is_empty() {
            let collections = CollectionsStore::load();
            delete_unreferenced_covers(&detached_covers, &library, &collections);
        }
    }
    Ok(result)
}

fn collect_album_cover_candidates(key: &str, album: &AlbumDto, out: &mut Vec<PathBuf>) {
    if let Some(p) = CoverPaths::to_local_path(album.cover_art.as_deref()) {
        out.push(p);
    }
    if let Some(p) = CoverPaths::to_local_path(album.cover_art_full.as_deref()) {
        out.push(p);
    }
    out.push(CoverPaths::thumb_path(key));
    out.push(CoverPaths::full_path(key));
}

fn album_cover_missing(album: &AlbumDto) -> bool {
    let Some(thumb) = CoverPaths::to_local_path(album.cover_art.as_deref()) else {
        return true;
    };
    if !thumb.is_file() {
        return true;
    }
    if let Some(full) = CoverPaths::to_local_path(album.cover_art_full.as_deref())
        && !full.is_file()
    {
        return true;
    }
    false
}

#[derive(Default)]
struct MissingTracks {
    safe: Vec<(String, String)>,
    suspect: Vec<(String, String)>,
}

/// Library tracks under the scanned roots whose files are gone, split into `safe` (online folder)
/// and `suspect` (absent root or empty directory looking like an unmounted mount point).
/// Tracks under unreadable folders are skipped.
fn collect_missing(
    library: &LibraryMap,
    by_path: &HashMap<String, (String, usize)>,
    found: &Discovery,
    seen_lower: &HashSet<String>,
) -> MissingTracks {
    if found.walked_dirs.is_empty() && found.gone_dirs.is_empty() {
        return MissingTracks::default();
    }
    let mut out = MissingTracks::default();
    for (low_path, (album_key, idx)) in by_path {
        if seen_lower.contains(low_path) {
            continue;
        }
        let Some(fp) = library
            .get(album_key)
            .and_then(|a| a.tracks.as_ref())
            .and_then(|ts| ts.get(*idx))
            .and_then(|t| t.file_path.as_deref())
        else {
            continue;
        };
        if is_under_any(fp, &found.unreadable_dirs) {
            continue;
        }
        if !is_under_any(fp, &found.walked_dirs) && !is_under_any(fp, &found.gone_dirs) {
            continue;
        }
        if Path::new(fp).exists() {
            continue;
        }
        let item = (low_path.clone(), album_key.clone());
        if is_under_any(fp, &found.gone_dirs) || is_under_any(fp, &found.suspect_empty_dirs) {
            out.suspect.push(item);
        } else {
            out.safe.push(item);
        }
    }
    out
}

/// Drops the given missing tracks from the library; returns how many were removed.
fn prune_tracks(
    library: &mut LibraryMap,
    gone: &[(String, String)],
    touched: &mut HashSet<String>,
) -> usize {
    let gone_paths: HashSet<&str> = gone.iter().map(|(p, _)| p.as_str()).collect();
    let mut removed = 0usize;
    for (_, album_key) in gone {
        if let Some(album) = library.get_mut(album_key)
            && let Some(tracks) = &mut album.tracks
        {
            let before = tracks.len();
            tracks.retain(|t| {
                t.file_path
                    .as_ref()
                    .is_none_or(|fp| !gone_paths.contains(fp.to_lowercase().as_str()))
            });
            removed += before - tracks.len();
            touched.insert(album_key.clone());
        }
    }
    removed
}

fn file_stamp(path: &Path) -> Option<(i64, i64)> {
    let meta = fs::metadata(path).ok()?;
    let modified = meta.modified().ok()?;
    let ms = modified
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    Some((ms, meta.len() as i64))
}

fn is_under_any(path: &str, dirs: &[PathBuf]) -> bool {
    let low_path = path.to_lowercase();
    for d in dirs {
        let ds = d.to_string_lossy();
        let trimmed = ds.trim_end_matches(['/', '\\']).to_lowercase();
        if low_path.len() > trimmed.len() + 1
            && low_path.starts_with(&trimmed)
            && matches!(low_path.as_bytes().get(trimmed.len()), Some(b'/') | Some(b'\\'))
        {
            return true;
        }
    }
    false
}

fn album_folder(file_path: &str) -> String {
    let p = Path::new(file_path);
    let Some(dir) = p.parent() else {
        return String::new();
    };
    if let Some(name) = dir.file_name().and_then(|n| n.to_str())
        && DISC_FOLDER_RX.is_match(name)
        && let Some(parent) = dir.parent()
    {
        let ps = parent.to_string_lossy();
        if !ps.is_empty() {
            return ps.into_owned();
        }
    }
    dir.to_string_lossy().into_owned()
}

fn same_artist(a: &str, b: &str) -> bool {
    normalize_artist(a) == normalize_artist(b)
}

fn normalize_artist(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_whitespace())
        .flat_map(|c| c.to_lowercase())
        .collect()
}

pub fn common_artist<'a>(track_artists: impl IntoIterator<Item = Option<&'a str>>) -> String {
    let mut common: Option<Vec<String>> = None;
    for artist in track_artists {
        let names: Vec<String> = ARTIST_SEP_RX
            .split(artist.unwrap_or(""))
            .map(|n| n.trim().to_owned())
            .filter(|n| !n.is_empty())
            .collect();
        match &mut common {
            None => common = Some(names),
            Some(prev) => {
                let keys: HashSet<String> = names.iter().map(|n| normalize_artist(n)).collect();
                prev.retain(|n| keys.contains(&normalize_artist(n)));
                if prev.is_empty() {
                    break;
                }
            }
        }
    }
    common
        .and_then(|c| c.into_iter().next())
        .unwrap_or_else(|| "Various Artists".to_owned())
}

fn merge_split_albums(
    library: &mut LibraryMap,
    touched: &mut HashSet<String>,
    mixed: &mut HashSet<String>,
    detached_covers: &mut Vec<PathBuf>,
) -> usize {
    let mut merged = 0usize;
    let keys: Vec<String> = library.keys().cloned().collect();
    for key in keys {
        let Some(cap) = SPLIT_ALBUM_RX.captures(&key) else {
            continue;
        };
        let base_key = cap["base"].to_owned();
        let who = cap["who"].to_owned();
        if base_key == key || !library.contains_key(&base_key) {
            continue;
        }
        let Some(split) = library.get(&key).cloned() else {
            continue;
        };
        let Some(split_tracks) = &split.tracks else {
            continue;
        };
        if split_tracks.is_empty() {
            continue;
        }
        let matches_who = split
            .album_artist
            .as_deref()
            .is_some_and(|a| same_artist(&who, a))
            || split.artist.as_deref().is_some_and(|a| same_artist(&who, a));
        if !matches_who {
            continue;
        }

        let target_folders: HashSet<String> = library
            .get(&base_key)
            .and_then(|a| a.tracks.as_ref())
            .into_iter()
            .flatten()
            .filter_map(|t| t.file_path.as_deref())
            .map(|fp| album_folder(fp).to_lowercase())
            .collect();
        if target_folders.is_empty() {
            continue;
        }
        let shares_folder = split_tracks
            .iter()
            .filter_map(|t| t.file_path.as_deref())
            .any(|fp| target_folders.contains(&album_folder(fp).to_lowercase()));
        if !shares_folder {
            continue;
        }

        let Some(mut split_owned) = library.shift_remove(&key) else {
            continue;
        };
        let Some(target) = library.get_mut(&base_key) else {
            library.insert(key, split_owned);
            continue;
        };
        if let Some(tracks) = &mut split_owned.tracks {
            let target_tracks = target.tracks.get_or_insert_with(Vec::new);
            for mut t in tracks.drain(..) {
                t.album = Some(base_key.clone());
                target_tracks.push(t);
            }
        }
        if target.cover_art.as_deref().unwrap_or("").is_empty()
            && !split_owned.cover_art.as_deref().unwrap_or("").is_empty()
        {
            target.cover_art = split_owned.cover_art;
            target.cover_art_full = split_owned.cover_art_full;
        } else {
            collect_album_cover_candidates(&key, &split_owned, detached_covers);
        }
        if target.year == 0 {
            target.year = split_owned.year;
        }
        let t_at = if target.added_at == 0 { i64::MAX } else { target.added_at };
        let s_at = if split_owned.added_at == 0 { i64::MAX } else { split_owned.added_at };
        let min_at = t_at.min(s_at);
        target.added_at = if min_at == i64::MAX { 0 } else { min_at };

        let s_aa = split_owned
            .album_artist
            .as_deref()
            .or(split_owned.artist.as_deref())
            .unwrap_or("");
        let t_aa = target
            .album_artist
            .as_deref()
            .or(target.artist.as_deref())
            .unwrap_or("");
        if !same_artist(s_aa, t_aa) {
            mixed.insert(base_key.clone());
        }
        touched.insert(base_key);
        merged += 1;
    }
    merged
}

fn same_album(
    cache: &mut HashMap<String, AlbumFolders>,
    key: &str,
    album: &AlbumDto,
    r: &Parsed,
) -> bool {
    if album.tracks.as_ref().is_none_or(|t| t.is_empty()) {
        return true;
    }
    let folders = cache
        .entry(key.to_owned())
        .or_insert_with(|| AlbumFolders::from_album(album));
    if folders.is_same_folder(&r.file_path) {
        return true;
    }
    if let Some(aa) = album.album_artist.as_deref().filter(|s| !s.trim().is_empty())
        && let Some(ra) = r.album_artist.as_deref().filter(|s| !s.trim().is_empty())
    {
        return same_artist(aa, ra);
    }
    if folders.is_near(&r.file_path) {
        return true;
    }
    let candidate = r.album_artist.as_deref().unwrap_or(&r.artist);
    album
        .artist
        .as_deref()
        .is_some_and(|a| same_artist(a, candidate))
        || album
            .album_artist
            .as_deref()
            .is_some_and(|a| same_artist(a, &r.artist))
}

fn resolve_album(
    library: &mut LibraryMap,
    cache: &mut HashMap<String, AlbumFolders>,
    r: &Parsed,
    now_ms: i64,
) -> (String, bool) {
    let artist = r
        .album_artist
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or(&r.artist)
        .to_owned();

    for attempt in 0..=20 {
        let key = match attempt {
            0 => r.album.clone(),
            1 => format!("{} ({})", r.album, artist),
            n => format!("{} ({}) {}", r.album, artist, n),
        };
        if !library.contains_key(&key) {
            library.insert(
                key.clone(),
                AlbumDto {
                    name: Some(key.clone()),
                    artist: Some(artist),
                    album_artist: r
                        .album_artist
                        .as_deref()
                        .filter(|s| !s.trim().is_empty())
                        .map(str::to_owned),
                    year: 0,
                    added_at: now_ms,
                    tracks: Some(Vec::new()),
                    ..AlbumDto::default()
                },
            );
            return (key, true);
        }
        if attempt >= 20 {
            return (key, false);
        }
        let existing = &library[&key];
        if same_album(cache, &key, existing, r) {
            return (key, false);
        }
    }
    unreachable!()
}

fn parse_file(path: &Path) -> Option<Parsed> {
    let probe = Probe::open(path)
        .ok()?
        .options(ParseOptions::new().read_cover_art(false));
    let tagged = probe.read().ok()?;
    let props = tagged.properties();
    let tag = tagged.primary_tag().or_else(|| tagged.first_tag());

    let file_path = path.to_string_lossy().into_owned();
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
            path.file_stem()
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

    // Quick cover presence check without reading large blobs for FLAC/MP3/MP4 where supported;
    // Note: with read_cover_art(false), tag.pictures() may be empty, so cover lookup inspects the
    // first tracks of albums lacking cover art in stage 6.
    let has_picture = tag.is_some_and(|t| !t.pictures().is_empty());

    Some(Parsed {
        file_path,
        title,
        artist,
        album_artist,
        album,
        duration: props.duration().as_secs_f64(),
        track_number,
        disc_number,
        year,
        codec: codec_name(tagged.file_type(), path),
        sample_rate: props.sample_rate().unwrap_or(0) as i32,
        bits_per_sample: props.bit_depth().unwrap_or(0) as i32,
        channels: props.channels().unwrap_or(0) as i32,
        has_picture,
    })
}

fn codec_name(ft: FileType, path: &Path) -> String {
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
        FileType::Wav | FileType::Aiff => "PCM".to_owned(),
        FileType::Aac => "AAC".to_owned(),
        FileType::Mp4 => {
            if ext == "alac" {
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

fn find_and_save_cover(
    album: &AlbumDto,
    picture_source: Option<&str>,
    cache: &mut HashMap<[u8; 32], String>,
) -> Option<(String, String)> {
    let mut data: Option<Vec<u8>> = picture_source.and_then(|p| read_embedded_picture(Path::new(p)));
    let tracks: Vec<&str> = album
        .tracks
        .as_deref()
        .unwrap_or(&[])
        .iter()
        .filter_map(|t| t.file_path.as_deref())
        .collect();

    if data.is_none() {
        for &p in tracks.iter().take(3) {
            if picture_source.is_some_and(|ps| ps.eq_ignore_ascii_case(p)) {
                continue;
            }
            if let Some(bytes) = read_embedded_picture(Path::new(p)) {
                data = Some(bytes);
                break;
            }
        }
    }

    if data.is_none() {
        let mut folders: Vec<String> = Vec::new();
        for &p in &tracks {
            if let Some(dir) = Path::new(p).parent() {
                let d = dir.to_string_lossy().into_owned();
                if !d.is_empty() && !folders.iter().any(|x| x.eq_ignore_ascii_case(&d)) {
                    folders.push(d);
                }
            }
            let af = album_folder(p);
            if !af.is_empty() && !folders.iter().any(|x| x.eq_ignore_ascii_case(&af)) {
                folders.push(af);
            }
            if folders.len() >= 8 {
                break;
            }
        }
        for folder in folders.into_iter().take(8) {
            if let Some(bytes) = read_folder_image(Path::new(&folder)) {
                data = Some(bytes);
                break;
            }
        }
    }

    save_cover(&data?, cache)
}

fn read_embedded_picture(source_file: &Path) -> Option<Vec<u8>> {
    let tagged = Probe::open(source_file).ok()?.read().ok()?;
    let tag = tagged.primary_tag().or_else(|| tagged.first_tag())?;
    let pic = tag
        .get_picture_type(PictureType::CoverFront)
        .or_else(|| tag.pictures().first())?;
    if pic.data().is_empty() {
        None
    } else {
        Some(pic.data().to_vec())
    }
}

fn read_folder_image(folder: &Path) -> Option<Vec<u8>> {
    let entries = fs::read_dir(folder).ok()?;
    let images: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file() && PlaylistCovers::is_supported_ext(p))
        .collect();
    for &target_name in FOLDER_COVER_NAMES {
        if let Some(match_path) = images.iter().find(|p| {
            p.file_stem()
                .and_then(|s| s.to_str())
                .is_some_and(|stem| stem.eq_ignore_ascii_case(target_name))
        }) && let Ok(meta) = fs::metadata(match_path)
            && meta.len() <= MAX_COVER_FILE_BYTES
        {
            return fs::read(match_path).ok();
        }
    }
    None
}

fn save_cover(data: &[u8], cache: &mut HashMap<[u8; 32], String>) -> Option<(String, String)> {
    if !ImageLimits::is_sane_bytes(data) {
        return None;
    }
    let raw_hash = sha256(data);
    if let Some(url) = cache.get(&raw_hash) {
        return Some((url.clone(), url.clone()));
    }
    let path = save_cover_bytes(data).ok()?;
    let url = CoverPaths::to_file_url(&path);
    cache.insert(raw_hash, url.clone());
    Some((url.clone(), url))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn common_artist_splits_separators() {
        let res = common_artist([
            Some("Cö shu Nie & Wuthering Waves"),
            Some("Wuthering Waves, SoYeon"),
        ]);
        assert_eq!(res, "Wuthering Waves");
        assert_eq!(
            common_artist([Some("Alice"), Some("Bob")]),
            "Various Artists"
        );
    }

    #[test]
    fn merges_split_albums_in_same_folder() {
        let mut lib = LibraryMap::new();
        lib.insert(
            "EP".to_owned(),
            AlbumDto {
                name: Some("EP".to_owned()),
                artist: Some("Artist A".to_owned()),
                album_artist: Some("Artist A".to_owned()),
                tracks: Some(vec![TrackDto {
                    file_path: Some("/music/ep/01.flac".to_owned()),
                    artist: Some("Artist A".to_owned()),
                    ..TrackDto::default()
                }]),
                ..AlbumDto::default()
            },
        );
        lib.insert(
            "EP (Artist B)".to_owned(),
            AlbumDto {
                name: Some("EP (Artist B)".to_owned()),
                artist: Some("Artist B".to_owned()),
                album_artist: Some("Artist B".to_owned()),
                tracks: Some(vec![TrackDto {
                    file_path: Some("/music/ep/02.flac".to_owned()),
                    artist: Some("Artist B".to_owned()),
                    ..TrackDto::default()
                }]),
                ..AlbumDto::default()
            },
        );

        let mut touched = HashSet::new();
        let mut mixed = HashSet::new();
        let mut detached = Vec::new();
        let merged = merge_split_albums(&mut lib, &mut touched, &mut mixed, &mut detached);
        assert_eq!(merged, 1);
        assert!(lib.contains_key("EP"));
        assert!(!lib.contains_key("EP (Artist B)"));
        assert_eq!(lib["EP"].tracks.as_ref().unwrap().len(), 2);
        assert!(mixed.contains("EP"));
    }

    fn album_with(paths: &[&str]) -> AlbumDto {
        AlbumDto {
            tracks: Some(
                paths
                    .iter()
                    .map(|p| TrackDto {
                        file_path: Some((*p).to_owned()),
                        ..TrackDto::default()
                    })
                    .collect(),
            ),
            ..AlbumDto::default()
        }
    }

    #[test]
    fn unmounted_mountpoint_detection_distinguishes_local_from_offline_mounts() {
        let active = vec![PathBuf::from("/"), PathBuf::from("/home"), PathBuf::from("/mnt/mounted")];
        let fstab = vec![
            PathBuf::from("/"),
            PathBuf::from("/home"),
            PathBuf::from("/media/nas"),
        ];

        assert!(!is_unmounted_mountpoint_from(
            Path::new("/home/okarin/Music"),
            &active,
            &fstab
        ));
        assert!(!is_unmounted_mountpoint_from(
            Path::new("/mnt/mounted"),
            &active,
            &fstab
        ));
        assert!(!is_unmounted_mountpoint_from(
            Path::new("/mnt/mounted/Music"),
            &active,
            &fstab
        ));
        assert!(is_unmounted_mountpoint_from(
            Path::new("/mnt/usb"),
            &active,
            &fstab
        ));
        assert!(is_unmounted_mountpoint_from(
            Path::new("/run/media/okarin/DISK"),
            &active,
            &fstab
        ));
        assert!(is_unmounted_mountpoint_from(
            Path::new("/media/nas"),
            &active,
            &fstab
        ));
        assert!(is_unmounted_mountpoint_from(
            Path::new("/media/nas/Music"),
            &active,
            &fstab
        ));
    }

    #[test]
    fn safe_prune_removes_deleted_online_tracks_while_protecting_offline_roots() {
        let base = std::env::temp_dir().join(format!("rustygrade-prune-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        // Empty local folder (e.g. user deleted all tracks inside ~/Music/Emptied).
        let emptied_local = base.join("local-music");
        fs::create_dir_all(&emptied_local).unwrap();
        let emptied_local = fs::canonicalize(&emptied_local).unwrap();
        // A root that is gone together with its parent.
        let absent = base.join("gone-parent").join("music");
        // A healthy root with one present track and one deleted track.
        let healthy = base.join("ok");
        fs::create_dir_all(&healthy).unwrap();
        let healthy = fs::canonicalize(&healthy).unwrap();
        let present = healthy.join("01.flac");
        let deleted_ok = healthy.join("02-deleted.flac");
        fs::write(&present, b"").unwrap();

        let local_gone_track = emptied_local.join("Album").join("01.flac");
        let absent_track = absent.join("Album").join("02.flac");
        let elsewhere = base.join("not-scanned").join("03.flac");
        let s = |p: &Path| p.to_string_lossy().into_owned();
        let mut library = LibraryMap::new();
        library.insert("LocalEmptied".to_owned(), album_with(&[&s(&local_gone_track)]));
        library.insert("Absent".to_owned(), album_with(&[&s(&absent_track)]));
        library.insert("Ok".to_owned(), album_with(&[&s(&present), &s(&deleted_ok)]));
        library.insert("Elsewhere".to_owned(), album_with(&[&s(&elsewhere)]));

        let mut by_path = HashMap::new();
        for (key, album) in &library {
            for (idx, t) in album.tracks.iter().flatten().enumerate() {
                by_path.insert(t.file_path.as_ref().unwrap().to_lowercase(), (key.clone(), idx));
            }
        }
        let mut found = walk_paths(&[emptied_local.clone(), absent.clone(), healthy.clone()]);
        // Simulate an unmounted external mountpoint directory alongside the scanned roots.
        let unmounted = base.join("unmounted-usb");
        let unmounted_track = unmounted.join("Album").join("04.flac");
        library.insert("USB".to_owned(), album_with(&[&s(&unmounted_track)]));
        by_path.insert(s(&unmounted_track).to_lowercase(), ("USB".to_owned(), 0));
        found.walked_dirs.push(unmounted.clone());
        found.suspect_empty_dirs.push(unmounted);

        let seen: HashSet<String> = found
            .files
            .iter()
            .map(|f| f.to_string_lossy().to_lowercase())
            .collect();

        let missing_res = collect_missing(&library, &by_path, &found, &seen);
        let mut safe_gone = missing_res.safe;
        let mut suspect_gone = missing_res.suspect;
        safe_gone.sort();
        suspect_gone.sort();
        let safe_keys: Vec<&str> = safe_gone.iter().map(|(_, k)| k.as_str()).collect();
        let suspect_keys: Vec<&str> = suspect_gone.iter().map(|(_, k)| k.as_str()).collect();
        assert_eq!(safe_keys, vec!["LocalEmptied", "Ok"]);
        assert_eq!(suspect_keys, vec!["Absent", "USB"]);

        // Safe prune drops only safe_gone.
        let mut touched = HashSet::new();
        assert_eq!(prune_tracks(&mut library, &safe_gone, &mut touched), 2);
        assert!(library["LocalEmptied"].tracks.as_ref().unwrap().is_empty());
        assert_eq!(library["Ok"].tracks.as_ref().unwrap().len(), 1);
        assert_eq!(library["Absent"].tracks.as_ref().unwrap().len(), 1);
        assert_eq!(library["USB"].tracks.as_ref().unwrap().len(), 1);
        assert_eq!(library["Elsewhere"].tracks.as_ref().unwrap().len(), 1);

        // ForceAll prunes suspect_gone too.
        assert_eq!(prune_tracks(&mut library, &suspect_gone, &mut touched), 2);
        assert!(library["Absent"].tracks.as_ref().unwrap().is_empty());
        assert!(library["USB"].tracks.as_ref().unwrap().is_empty());

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn scan_deduplicates_identical_covers_and_refcounts_on_prune() {
        let base = std::env::temp_dir().join(format!("rustygrade-scan-dedup-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let app_dir = base.join("app");
        let music_dir = base.join("music");
        let alb1_dir = music_dir.join("Alb1");
        let alb2_dir = music_dir.join("Alb2");
        fs::create_dir_all(&alb1_dir).unwrap();
        fs::create_dir_all(&alb2_dir).unwrap();

        fn minimal_flac_bytes() -> Vec<u8> {
            let mut v = Vec::with_capacity(42);
            v.extend_from_slice(b"fLaC");
            v.extend_from_slice(&[0x80, 0x00, 0x00, 0x22]);
            v.extend_from_slice(&[0x10, 0x00, 0x10, 0x00]);
            v.extend_from_slice(&[0x00; 6]);
            v.extend_from_slice(&[0x0A, 0xC4, 0x42, 0xF0, 0x00, 0x00, 0xAC, 0x44]);
            v.extend_from_slice(&[0x00; 16]);
            v
        }

        let t1 = alb1_dir.join("01.flac");
        let t2 = alb2_dir.join("01.flac");
        fs::write(&t1, minimal_flac_bytes()).unwrap();
        fs::write(&t2, minimal_flac_bytes()).unwrap();
        crate::tag_writer::TagWriter::write(
            &t1,
            &crate::tag_writer::TagEdit {
                title: Some("Song 1".into()),
                artist: Some("Artist".into()),
                album: Some("Album One".into()),
                ..Default::default()
            },
        )
        .unwrap();
        crate::tag_writer::TagWriter::write(
            &t2,
            &crate::tag_writer::TagEdit {
                title: Some("Song 2".into()),
                artist: Some("Artist".into()),
                album: Some("Album Two".into()),
                ..Default::default()
            },
        )
        .unwrap();

        let mut png_buf = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(image::RgbImage::new(128, 128))
            .write_to(&mut png_buf, image::ImageFormat::Png)
            .unwrap();
        let cover_bytes = png_buf.into_inner();
        fs::write(alb1_dir.join("cover.png"), &cover_bytes).unwrap();
        fs::write(alb2_dir.join("cover.png"), &cover_bytes).unwrap();

        crate::paths::with_test_app_dir(&app_dir, || {
            let res = Scanner::scan(
                std::slice::from_ref(&music_dir),
                ScanOptions {
                    prune: PrunePolicy::Safe,
                    max_threads: Some(1),
                    force: false,
                    resync_covers: false,
                },
                None,
                None,
            )
            .unwrap();
            assert_eq!(res.added_tracks, 2);
            assert_eq!(res.new_albums, 2);
            assert_eq!(res.covers, 2);

            let lib = LibraryStore::load_raw().unwrap();
            let c1 = lib["Album One"].cover_art.clone().unwrap();
            let c2 = lib["Album Two"].cover_art.clone().unwrap();
            assert_eq!(c1, c2, "identical folder covers should share one content-addressed file");
            assert_eq!(lib["Album One"].cover_art_full.as_ref(), Some(&c1));
            let cover_files: Vec<_> = fs::read_dir(CoverPaths::cover_dir()).unwrap().flatten().collect();
            assert_eq!(cover_files.len(), 1);
            let cover_file_path = CoverPaths::to_local_path(Some(&c1)).unwrap();
            assert!(cover_file_path.exists());

            // Delete Album One's folder completely and rescan with PrunePolicy::Safe.
            fs::remove_dir_all(&alb1_dir).unwrap();
            let res2 = Scanner::scan(
                std::slice::from_ref(&music_dir),
                ScanOptions {
                    prune: PrunePolicy::Safe,
                    max_threads: Some(1),
                    force: false,
                    resync_covers: false,
                },
                None,
                None,
            )
            .unwrap();
            assert_eq!(res2.removed_tracks, 1);
            assert_eq!(res2.removed_albums, 1);
            let lib2 = LibraryStore::load_raw().unwrap();
            assert!(!lib2.contains_key("Album One"));
            assert!(lib2.contains_key("Album Two"));
            assert!(cover_file_path.exists(), "Album Two still references the shared cover");

            // Delete Album Two's track (leaving music_dir completely empty) and rescan with Safe prune.
            fs::remove_dir_all(&alb2_dir).unwrap();
            let res3 = Scanner::scan(
                std::slice::from_ref(&music_dir),
                ScanOptions {
                    prune: PrunePolicy::Safe,
                    max_threads: Some(1),
                    force: false,
                    resync_covers: false,
                },
                None,
                None,
            )
            .unwrap();
            assert_eq!(res3.removed_tracks, 1);
            assert_eq!(res3.removed_albums, 1);
            assert!(LibraryStore::load_raw().unwrap().is_empty());
            assert!(!cover_file_path.exists(), "shared cover should be removed once all albums are gone");
        });

        let _ = fs::remove_dir_all(&base);
    }

    #[cfg(unix)]
    #[test]
    fn discover_skips_symlink_cycles() {
        let root = std::env::temp_dir().join(format!("rustygrade-symlink-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let sub = root.join("album");
        fs::create_dir_all(&sub).unwrap();
        let flac = sub.join("track.flac");
        fs::write(&flac, b"").unwrap();
        std::os::unix::fs::symlink(&root, sub.join("loop")).unwrap();

        let files = Scanner::discover(std::slice::from_ref(&root));
        assert_eq!(files.len(), 1);
        assert!(files[0].ends_with("track.flac"));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn parse_mountinfo_and_fstab_handle_escapes_comments_and_trailing_slashes() {
        let mountinfo = "\
22 1 8:1 / / rw,relatime - ext4 /dev/sda1 rw
23 22 8:2 / /mnt/My\\040Drive/ rw - exfat /dev/sdb1 rw
24 22 0:19 / /media/usb\\011tab rw - vfat /dev/sdc1 rw
";
        let mounts = parse_mountinfo(mountinfo);
        assert_eq!(
            mounts,
            vec![
                PathBuf::from("/"),
                PathBuf::from("/mnt/My Drive"),
                PathBuf::from("/media/usb\ttab"),
            ]
        );

        let fstab = "\
# comment line
UUID=1234 / ext4 defaults 0 1
UUID=5678 none swap sw 0 0
UUID=90ab /srv/music\\040lib/ ext4 noauto,user 0 0
//nas/share /mnt/Nas\\134Share cifs noauto 0 0
badline
";
        let fstab_mounts = parse_fstab(fstab);
        assert_eq!(
            fstab_mounts,
            vec![
                PathBuf::from("/"),
                PathBuf::from("/srv/music lib"),
                PathBuf::from("/mnt/Nas\\Share"),
            ]
        );
    }

    #[test]
    fn is_unmounted_mountpoint_checks_mount_roots_directly() {
        let active_none = vec![PathBuf::from("/")];
        let active_usb = vec![PathBuf::from("/"), PathBuf::from("/mnt/usb")];
        let fstab = vec![PathBuf::from("/"), PathBuf::from("/srv/cold")];

        // `/mnt`, `/media`, `/run/media` directly have no active mounts underneath -> suspect.
        assert!(is_unmounted_mountpoint_from(
            Path::new("/mnt"),
            &active_none,
            &fstab
        ));
        assert!(is_unmounted_mountpoint_from(
            Path::new("/media"),
            &active_none,
            &fstab
        ));
        assert!(is_unmounted_mountpoint_from(
            Path::new("/run/media"),
            &active_none,
            &fstab
        ));

        // Once `/mnt/usb` is active underneath `/mnt`, `/mnt` itself sees an active child mount.
        assert!(!is_unmounted_mountpoint_from(
            Path::new("/mnt"),
            &active_usb,
            &fstab
        ));
        assert!(!is_unmounted_mountpoint_from(
            Path::new("/mnt/usb"),
            &active_usb,
            &fstab
        ));
        assert!(!is_unmounted_mountpoint_from(
            Path::new("/mnt/usb/Music"),
            &active_usb,
            &fstab
        ));

        // Sibling `/mnt/other` without its own active mount remains suspect.
        assert!(is_unmounted_mountpoint_from(
            Path::new("/mnt/other"),
            &active_usb,
            &fstab
        ));
        // Unmounted fstab entry and nested folder under it are suspect; active mount isn't.
        assert!(is_unmounted_mountpoint_from(
            Path::new("/srv/cold"),
            &active_usb,
            &fstab
        ));
        assert!(is_unmounted_mountpoint_from(
            Path::new("/srv/cold/Jazz"),
            &active_usb,
            &fstab
        ));
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_root_protects_nested_unreadable_subdirs() {
        use std::os::unix::fs::PermissionsExt;

        let base = std::env::temp_dir().join(format!("rustygrade-sym-unreadable-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let real_root = base.join("real");
        let unreadable_sub = real_root.join("LockedAlbum");
        let link_root = base.join("link_music");
        fs::create_dir_all(&unreadable_sub).unwrap();
        std::os::unix::fs::symlink(&real_root, &link_root).unwrap();

        fs::set_permissions(&unreadable_sub, fs::Permissions::from_mode(0o000)).unwrap();
        let locked = fs::read_dir(&unreadable_sub).is_err();
        if locked {
            let found = walk_paths(std::slice::from_ref(&link_root));
            let raw_locked = link_root.join("LockedAlbum");
            let canon_locked = fs::canonicalize(&real_root).unwrap().join("LockedAlbum");
            assert!(
                found.unreadable_dirs.contains(&raw_locked),
                "expected raw aliased locked dir in {:?}",
                found.unreadable_dirs
            );
            assert!(found.unreadable_dirs.contains(&canon_locked));

            let mut lib = LibraryMap::new();
            let raw_track = raw_locked.join("01.flac");
            let s_raw = raw_track.to_string_lossy().into_owned();
            lib.insert(
                "Locked".to_owned(),
                AlbumDto {
                    tracks: Some(vec![TrackDto {
                        file_path: Some(s_raw.clone()),
                        ..TrackDto::default()
                    }]),
                    ..AlbumDto::default()
                },
            );
            let mut by_path = HashMap::new();
            by_path.insert(s_raw.to_lowercase(), ("Locked".to_owned(), 0));
            let missing = collect_missing(&lib, &by_path, &found, &HashSet::new());
            assert!(missing.safe.is_empty());
            assert!(missing.suspect.is_empty());
        }
        let _ = fs::set_permissions(&unreadable_sub, fs::Permissions::from_mode(0o755));
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn scan_resync_covers_preserves_unextractable_and_refcounts_replacements() {
        let base = std::env::temp_dir().join(format!("rustygrade-resync-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let app_dir = base.join("app");
        let music_dir = base.join("music");
        let d1 = music_dir.join("Alb1");
        let d2 = music_dir.join("Alb2");
        let d3 = music_dir.join("Alb3");
        fs::create_dir_all(&d1).unwrap();
        fs::create_dir_all(&d2).unwrap();
        fs::create_dir_all(&d3).unwrap();

        fn minimal_flac_bytes() -> Vec<u8> {
            let mut v = Vec::with_capacity(42);
            v.extend_from_slice(b"fLaC");
            v.extend_from_slice(&[0x80, 0x00, 0x00, 0x22]);
            v.extend_from_slice(&[0x10, 0x00, 0x10, 0x00]);
            v.extend_from_slice(&[0x00; 6]);
            v.extend_from_slice(&[0x0A, 0xC4, 0x42, 0xF0, 0x00, 0x00, 0xAC, 0x44]);
            v.extend_from_slice(&[0x00; 16]);
            v
        }

        fn solid_png(r: u8, g: u8, b: u8) -> Vec<u8> {
            let mut img = image::RgbImage::new(64, 64);
            for p in img.pixels_mut() {
                *p = image::Rgb([r, g, b]);
            }
            let mut cur = std::io::Cursor::new(Vec::new());
            image::DynamicImage::ImageRgb8(img)
                .write_to(&mut cur, image::ImageFormat::Png)
                .unwrap();
            cur.into_inner()
        }

        for (dir, title, album) in [
            (&d1, "S1", "Resync One"),
            (&d2, "S2", "Resync Two"),
            (&d3, "S3", "Resync Three"),
        ] {
            let t = dir.join("01.flac");
            fs::write(&t, minimal_flac_bytes()).unwrap();
            crate::tag_writer::TagWriter::write(
                &t,
                &crate::tag_writer::TagEdit {
                    title: Some(title.into()),
                    artist: Some("Artist".into()),
                    album: Some(album.into()),
                    ..Default::default()
                },
            )
            .unwrap();
        }

        let shared_png = solid_png(200, 20, 20);
        let unique_png = solid_png(20, 200, 20);
        let replacement_png = solid_png(20, 20, 200);
        fs::write(d1.join("cover.png"), &shared_png).unwrap();
        fs::write(d2.join("cover.png"), &shared_png).unwrap();
        fs::write(d3.join("cover.png"), &unique_png).unwrap();

        crate::paths::with_test_app_dir(&app_dir, || {
            Scanner::scan(
                std::slice::from_ref(&music_dir),
                ScanOptions {
                    prune: PrunePolicy::Safe,
                    max_threads: Some(1),
                    ..Default::default()
                },
                None,
                None,
            )
            .unwrap();

            let lib_before = LibraryStore::load_raw().unwrap();
            let shared_url = lib_before["Resync One"].cover_art.clone().unwrap();
            assert_eq!(shared_url, lib_before["Resync Two"].cover_art.clone().unwrap());
            let unique_url = lib_before["Resync Three"].cover_art.clone().unwrap();
            let shared_file = CoverPaths::to_local_path(Some(&shared_url)).unwrap();
            let unique_file = CoverPaths::to_local_path(Some(&unique_url)).unwrap();

            // Replace Alb1's cover, keep Alb2's shared cover, and delete Alb3's source image.
            fs::write(d1.join("cover.png"), &replacement_png).unwrap();
            fs::remove_file(d3.join("cover.png")).unwrap();

            // Cancelled resync must leave covers intact.
            let cancelled = AtomicBool::new(true);
            Scanner::scan(
                std::slice::from_ref(&music_dir),
                ScanOptions {
                    prune: PrunePolicy::Never,
                    max_threads: Some(1),
                    resync_covers: true,
                    ..Default::default()
                },
                Some(&cancelled),
                None,
            )
            .unwrap();
            let lib_after_cancel = LibraryStore::load_raw().unwrap();
            assert_eq!(lib_after_cancel["Resync One"].cover_art.as_ref(), Some(&shared_url));

            // Full cover resync updates Alb1, keeps Alb2's shared file, and preserves Alb3's cover.
            let res = Scanner::scan(
                std::slice::from_ref(&music_dir),
                ScanOptions {
                    prune: PrunePolicy::Never,
                    max_threads: Some(1),
                    resync_covers: true,
                    ..Default::default()
                },
                None,
                None,
            )
            .unwrap();
            assert_eq!(res.covers, 2);

            let lib_after = LibraryStore::load_raw().unwrap();
            let new_url_1 = lib_after["Resync One"].cover_art.clone().unwrap();
            assert_ne!(new_url_1, shared_url);
            assert!(shared_file.exists(), "Alb2 still keeps shared file alive");
            assert_eq!(lib_after["Resync Three"].cover_art.as_ref(), Some(&unique_url));
            assert!(unique_file.exists(), "unextractable cover must be preserved");

            // Now replace Alb2's cover too; old shared_file must be deleted once no longer referenced.
            fs::write(d2.join("cover.png"), &replacement_png).unwrap();
            Scanner::scan(
                std::slice::from_ref(&music_dir),
                ScanOptions {
                    prune: PrunePolicy::Never,
                    max_threads: Some(1),
                    resync_covers: true,
                    ..Default::default()
                },
                None,
                None,
            )
            .unwrap();
            assert!(!shared_file.exists(), "replaced unique/shared file should be deleted once unreferenced");
        });

        let _ = fs::remove_dir_all(&base);
    }
}
