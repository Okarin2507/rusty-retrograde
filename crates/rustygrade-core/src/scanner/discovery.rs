use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use super::Scanner;

#[derive(Default)]
pub(super) struct Discovery {
    pub files: Vec<PathBuf>,
    pub walked_dirs: Vec<PathBuf>,
    pub unreadable_dirs: Vec<PathBuf>,
    pub gone_dirs: Vec<PathBuf>,
    pub suspect_empty_dirs: Vec<PathBuf>,
    pub root_aliases: Vec<(PathBuf, PathBuf)>,
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
            && (b'0'..=b'3').contains(&bytes[i + 1])
            && (b'0'..=b'7').contains(&bytes[i + 2])
            && (b'0'..=b'7').contains(&bytes[i + 3])
        {
            let v =
                ((bytes[i + 1] - b'0') << 6) | ((bytes[i + 2] - b'0') << 3) | (bytes[i + 3] - b'0');
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
            (!mp.is_empty()).then(|| PathBuf::from(unescape_mount_octal(mp)))
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
            (mp != "none" && mp != "swap").then(|| PathBuf::from(unescape_mount_octal(mp)))
        })
        .collect()
}

#[cfg(unix)]
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

#[allow(clippy::too_many_arguments)]
fn visit_dir(
    dir: &Path,
    is_root: bool,
    visited_dirs: &mut HashSet<(u64, u64)>,
    stack: &mut Vec<PathBuf>,
    mounts: &mut MountSnapshot,
    found: &mut Discovery,
) {
    match fs::read_dir(dir) {
        Ok(entries) => {
            if is_root {
                push_dir_with_aliases(dir, &found.root_aliases, &mut found.walked_dirs);
            }
            let first_visit =
                !is_root || dir_identity(dir).is_none_or(|id| visited_dirs.insert(id));
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
            if !had_any_entry && mounts.looks_like_unmounted(dir, &found.root_aliases) {
                push_dir_with_aliases(dir, &found.root_aliases, &mut found.suspect_empty_dirs);
            }
        }
        Err(_) => {
            push_dir_with_aliases(dir, &found.root_aliases, &mut found.unreadable_dirs);
        }
    }
}

pub(super) fn walk_paths(paths: &[PathBuf]) -> Discovery {
    let mut found = Discovery::default();
    let mut stack = Vec::new();
    let mut visited_dirs: HashSet<(u64, u64)> = HashSet::new();
    let mut mounts = MountSnapshot::new();

    for p in paths {
        if p.is_dir() {
            let dir = fs::canonicalize(p).unwrap_or_else(|_| p.clone());
            if p != &dir {
                found.root_aliases.push((dir.clone(), p.clone()));
            }
            visit_dir(
                &dir,
                true,
                &mut visited_dirs,
                &mut stack,
                &mut mounts,
                &mut found,
            );
        } else if p.is_file() {
            if Scanner::is_audio_file(p) {
                found
                    .files
                    .push(fs::canonicalize(p).unwrap_or_else(|_| p.clone()));
            }
        } else if p.is_absolute() {
            // An absent root (moved folder, unmounted drive) counts its tracks as suspect missing.
            found.gone_dirs.push(p.clone());
        }
    }

    while let Some(dir) = stack.pop() {
        visit_dir(
            &dir,
            false,
            &mut visited_dirs,
            &mut stack,
            &mut mounts,
            &mut found,
        );
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::TempDir;

    #[cfg(unix)]
    #[test]
    fn discover_skips_symlink_cycles() {
        let root = TempDir::new("symlink-cycle");
        let sub = root.join("album");
        fs::create_dir_all(&sub).unwrap();
        fs::write(sub.join("track.flac"), b"").unwrap();
        std::os::unix::fs::symlink(root.path(), sub.join("loop")).unwrap();

        let files = Scanner::discover(std::slice::from_ref(&root.to_path_buf()));
        assert_eq!(files.len(), 1);
        assert!(files[0].ends_with("track.flac"));
    }

    #[cfg(unix)]
    #[test]
    fn parse_mountinfo_and_fstab_handle_escapes_comments_and_trailing_slashes() {
        let mountinfo = "\
22 1 8:1 / / rw,relatime - ext4 /dev/sda1 rw
23 22 8:2 / /mnt/My\\040Drive/ rw - exfat /dev/sdb1 rw
24 22 0:19 / /media/usb\\011tab rw - vfat /dev/sdc1 rw
";
        assert_eq!(
            parse_mountinfo(mountinfo),
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
        assert_eq!(
            parse_fstab(fstab),
            vec![
                PathBuf::from("/"),
                PathBuf::from("/srv/music lib"),
                PathBuf::from("/mnt/Nas\\Share"),
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn unmounted_mountpoint_detection_covers_local_mounts_roots_and_fstab() {
        let active_none = vec![PathBuf::from("/")];
        let active_usb = vec![
            PathBuf::from("/"),
            PathBuf::from("/home"),
            PathBuf::from("/mnt/usb"),
        ];
        let fstab = vec![
            PathBuf::from("/"),
            PathBuf::from("/home"),
            PathBuf::from("/media/nas"),
            PathBuf::from("/srv/cold"),
        ];

        for (dir, active, want) in [
            ("/home/okarin/Music", &active_usb, false),
            ("/mnt/usb", &active_usb, false),
            ("/mnt/usb/Music", &active_usb, false),
            ("/mnt", &active_usb, false),
            ("/mnt", &active_none, true),
            ("/media", &active_none, true),
            ("/run/media", &active_none, true),
            ("/mnt/other", &active_usb, true),
            ("/run/media/okarin/DISK", &active_usb, true),
            ("/media/nas", &active_usb, true),
            ("/media/nas/Music", &active_usb, true),
            ("/srv/cold", &active_usb, true),
            ("/srv/cold/Jazz", &active_usb, true),
        ] {
            assert_eq!(
                is_unmounted_mountpoint_from(Path::new(dir), active, &fstab),
                want,
                "dir={dir}"
            );
        }
    }
}
