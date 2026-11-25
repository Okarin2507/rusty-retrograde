use std::fs::File;
use std::path::{Path, PathBuf};

use lofty::config::WriteOptions;
use lofty::file::{AudioFile, TaggedFileExt};
use lofty::picture::{Picture, PictureType};
use lofty::probe::Probe;
use lofty::tag::items::Timestamp;
use lofty::tag::{Accessor, ItemKey, Tag};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum TagWriteError {
    #[error("Failed to open or save audio file: {0}")]
    Lofty(String),
    #[error("Failed to read cover image: {0}")]
    Io(#[from] std::io::Error),
    #[error("Unsupported audio file (no tag container available)")]
    NoTag,
}

#[derive(Debug, Clone, Default)]
pub struct TagEdit {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub album_artist: Option<String>,
    pub year: Option<u32>,
    pub track: Option<u32>,
    pub disc: Option<u32>,
    pub cover_image: Option<PathBuf>,
    pub remove_cover: bool,
}

impl TagEdit {
    pub fn changes_cover(&self) -> bool {
        self.cover_image.is_some() || self.remove_cover
    }
}

pub struct TagWriter;

fn load_front_cover(path: &Path) -> Result<Picture, TagWriteError> {
    let mut file = File::open(path)?;
    let mut picture =
        Picture::from_reader(&mut file).map_err(|e| TagWriteError::Lofty(e.to_string()))?;
    picture.set_pic_type(PictureType::CoverFront);
    picture.set_description(Some("Cover".to_owned()));
    Ok(picture)
}

impl TagWriter {
    /// Writes `edit` to every path in `paths`, returning `(written_count, failures)`
    /// without stopping on individual file errors.
    pub fn write_batch(paths: &[PathBuf], edit: &TagEdit) -> (usize, Vec<(PathBuf, String)>) {
        let preloaded = match &edit.cover_image {
            Some(p) => match load_front_cover(p) {
                Ok(pic) => Some(pic),
                Err(e) => {
                    let msg = e.to_string();
                    return (0, paths.iter().map(|t| (t.clone(), msg.clone())).collect());
                }
            },
            None => None,
        };
        let mut ok = 0usize;
        let mut errs = Vec::new();
        for p in paths {
            match Self::write_with_cover(p, edit, preloaded.as_ref()) {
                Ok(()) => ok += 1,
                Err(e) => errs.push((p.clone(), e.to_string())),
            }
        }
        (ok, errs)
    }

    pub fn write(path: &Path, edit: &TagEdit) -> Result<(), TagWriteError> {
        let cover = edit
            .cover_image
            .as_deref()
            .map(load_front_cover)
            .transpose()?;
        Self::write_with_cover(path, edit, cover.as_ref())
    }

    fn write_with_cover(
        path: &Path,
        edit: &TagEdit,
        cover: Option<&Picture>,
    ) -> Result<(), TagWriteError> {
        let mut tagged = Probe::open(path)
            .map_err(|e| TagWriteError::Lofty(e.to_string()))?
            .read()
            .map_err(|e| TagWriteError::Lofty(e.to_string()))?;
        let tag_type = tagged.primary_tag_type();
        if tagged.primary_tag().is_none() {
            tagged.insert_tag(Tag::new(tag_type));
        }
        let tag = tagged.primary_tag_mut().ok_or(TagWriteError::NoTag)?;

        if let Some(title) = &edit.title {
            let trimmed = title.trim();
            if trimmed.is_empty() {
                tag.remove_title();
            } else {
                tag.set_title(trimmed.to_owned());
            }
        }
        if let Some(artist) = &edit.artist {
            let trimmed = artist.trim();
            if trimmed.is_empty() {
                tag.remove_artist();
            } else {
                tag.set_artist(trimmed.to_owned());
            }
        }
        if let Some(album) = &edit.album {
            let trimmed = album.trim();
            if trimmed.is_empty() {
                tag.remove_album();
            } else {
                tag.set_album(trimmed.to_owned());
            }
        }
        if let Some(album_artist) = &edit.album_artist {
            let trimmed = album_artist.trim();
            if trimmed.is_empty() {
                tag.remove_key(ItemKey::AlbumArtist);
            } else {
                tag.insert_text(ItemKey::AlbumArtist, trimmed.to_owned());
            }
        }
        if let Some(year) = edit.year {
            if year == 0 {
                tag.remove_date();
            } else {
                tag.set_date(Timestamp {
                    year: year.min(u16::MAX as u32) as u16,
                    ..Timestamp::default()
                });
            }
        }
        if let Some(track) = edit.track {
            if track == 0 {
                tag.remove_track();
            } else {
                tag.set_track(track);
            }
        }
        if let Some(disc) = edit.disc {
            if disc == 0 {
                tag.remove_disk();
            } else {
                tag.set_disk(disc);
            }
        }
        if let Some(picture) = cover {
            while !tag.pictures().is_empty() {
                tag.remove_picture(0);
            }
            tag.push_picture(picture.clone());
        } else if edit.remove_cover {
            while !tag.pictures().is_empty() {
                tag.remove_picture(0);
            }
        }

        tagged
            .save_to_path(path, WriteOptions::default())
            .map_err(|e| TagWriteError::Lofty(e.to_string()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::{TempDir, minimal_flac_bytes};

    #[test]
    fn write_batch_preserves_untouched_fields_and_reports_errors() {
        let dir = TempDir::new("tag-batch");
        let a = dir.join("a.flac");
        let b = dir.join("b.flac");
        let missing = dir.join("missing.flac");
        std::fs::write(&a, minimal_flac_bytes()).unwrap();
        std::fs::write(&b, minimal_flac_bytes()).unwrap();

        for (p, title, artist) in [(&a, "Track One", "Artist A"), (&b, "Track Two", "Artist B")] {
            TagWriter::write(
                p,
                &TagEdit {
                    title: Some(title.into()),
                    artist: Some(artist.into()),
                    album: Some("Old Album".into()),
                    year: Some(2020),
                    ..Default::default()
                },
            )
            .unwrap();
        }

        let batch = TagEdit {
            album: Some("New Album".into()),
            year: Some(2024),
            ..Default::default()
        };
        let (written, errs) =
            TagWriter::write_batch(&[a.clone(), missing.clone(), b.clone()], &batch);
        assert_eq!(written, 2);
        assert_eq!(errs.len(), 1);
        assert_eq!(errs[0].0, missing);

        for (p, title, artist) in [(&a, "Track One", "Artist A"), (&b, "Track Two", "Artist B")] {
            let tf = Probe::open(p).unwrap().read().unwrap();
            let tag = tf.primary_tag().unwrap();
            assert_eq!(tag.title().as_deref(), Some(title));
            assert_eq!(tag.artist().as_deref(), Some(artist));
            assert_eq!(tag.album().as_deref(), Some("New Album"));
        }
    }
}
