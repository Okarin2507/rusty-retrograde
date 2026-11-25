use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use image::{DynamicImage, ImageFormat, Rgb, RgbImage, Rgba, RgbaImage};

static SEQ: AtomicU64 = AtomicU64::new(0);

/// RAII scratch directory removed automatically when dropped, even on test panic.
pub(crate) struct TempDir(PathBuf);

impl TempDir {
    pub(crate) fn new(label: &str) -> Self {
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("retro-test-{label}-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("create test temp dir");
        Self(path)
    }

    pub(crate) fn path(&self) -> &Path {
        &self.0
    }
}

impl std::ops::Deref for TempDir {
    type Target = Path;

    fn deref(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

pub(crate) fn minimal_flac_bytes() -> Vec<u8> {
    let mut v = Vec::with_capacity(46);
    v.extend_from_slice(b"fLaC");
    v.extend_from_slice(&[0x80, 0x00, 0x00, 0x22]); // last block = STREAMINFO (34 bytes)
    v.extend_from_slice(&[0x10, 0x00, 0x10, 0x00]); // min/max block 4096
    v.extend_from_slice(&[0x00; 6]); // min/max frame size unknown
    // 44100 Hz, 2 ch, 16 bps, 44100 samples
    v.extend_from_slice(&[0x0A, 0xC4, 0x42, 0xF0, 0x00, 0x00, 0xAC, 0x44]);
    v.extend_from_slice(&[0x00; 16]); // MD5
    // Keep `metadata_range.end < file_len` so shrinking edits via lofty 0.25.5 do not hit its EOF-truncation bug.
    v.extend_from_slice(&[0xFF, 0xF8, 0x00, 0x00]);
    v
}

pub(crate) fn solid_png(w: u32, h: u32, rgb: [u8; 3]) -> Vec<u8> {
    let img = RgbImage::from_pixel(w, h, Rgb(rgb));
    let mut cur = Cursor::new(Vec::new());
    DynamicImage::ImageRgb8(img)
        .write_to(&mut cur, ImageFormat::Png)
        .unwrap();
    cur.into_inner()
}

pub(crate) fn solid_rgba_png(w: u32, h: u32, rgba: [u8; 4]) -> Vec<u8> {
    let img = RgbaImage::from_pixel(w, h, Rgba(rgba));
    let mut cur = Cursor::new(Vec::new());
    DynamicImage::ImageRgba8(img)
        .write_to(&mut cur, ImageFormat::Png)
        .unwrap();
    cur.into_inner()
}
