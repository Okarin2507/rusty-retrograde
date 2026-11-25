use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use iced::Task;
use rustygrade_core::TagEdit;
use rustygrade_linux::{ArtworkProviders, pick_cover_image};

use super::{App, Message, TagDraft, TagField};
use crate::controller::{ControllerCommand, NoticeKind};
use crate::cover_fetch;

impl App {
    pub(super) fn handle_tag_message(&mut self, msg: Message) -> Task<Message> {
        match msg {
            Message::OpenTagEditor(track, album_artist, year) => {
                let path = PathBuf::from(&track.file_path);
                let target_label = path
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("TRACK")
                    .to_owned();
                let cover_preview = track
                    .cover_full_path
                    .clone()
                    .or_else(|| track.cover_thumb_path.clone());
                if let Some(cp) = cover_preview.as_deref() {
                    self.covers.ensure_full(cp);
                }
                let year_s = if year > 0 {
                    year.to_string()
                } else {
                    String::new()
                };
                let track_s = if track.track_number > 0 {
                    track.track_number.to_string()
                } else {
                    String::new()
                };
                let disc_s = if track.disc_number > 0 {
                    track.disc_number.to_string()
                } else {
                    String::new()
                };
                let aa = if album_artist == "Unknown Artist" {
                    String::new()
                } else {
                    album_artist
                };
                self.tag_status = None;
                self.end_cover_fetches();
                let query_artist = track.artist.clone();
                self.tag_draft = Some(TagDraft {
                    query_artist,
                    fetching: None,
                    cover_source: None,
                    is_track: true,
                    paths: vec![path],
                    album_key: Some(track.album.clone()),
                    target_label,
                    orig_title: track.title.clone(),
                    orig_artist: track.artist.clone(),
                    orig_album: track.album.clone(),
                    orig_album_artist: aa.clone(),
                    orig_year: year_s.clone(),
                    orig_track: track_s.clone(),
                    orig_disc: disc_s.clone(),
                    title: track.title,
                    artist: track.artist,
                    album: track.album,
                    album_artist: aa,
                    year: year_s,
                    track: track_s,
                    disc: disc_s,
                    cover_preview,
                    new_cover: None,
                    remove_cover: false,
                    saving: false,
                });
                Task::none()
            }
            Message::OpenAlbumTagEditor(key) => {
                let Some(album) = self.find_album(&key).cloned() else {
                    return Task::none();
                };
                let paths: Vec<PathBuf> = album
                    .tracks
                    .iter()
                    .map(|t| PathBuf::from(&t.file_path))
                    .collect();
                if paths.is_empty() {
                    return Task::none();
                }
                let cover_preview = album
                    .cover_full_path
                    .clone()
                    .or_else(|| album.cover_thumb_path.clone());
                if let Some(cp) = cover_preview.as_deref() {
                    self.covers.ensure_full(cp);
                }
                let year_s = if album.year > 0 {
                    album.year.to_string()
                } else {
                    String::new()
                };
                let aa = if album.artist == "Unknown Artist" {
                    String::new()
                } else {
                    album.artist.clone()
                };
                self.tag_status = None;
                self.end_cover_fetches();
                let query_artist = album
                    .tracks
                    .first()
                    .map(|t| t.artist.clone())
                    .unwrap_or_default();
                self.tag_draft = Some(TagDraft {
                    query_artist,
                    fetching: None,
                    cover_source: None,
                    is_track: false,
                    target_label: format!("{} TRACKS", paths.len()),
                    paths,
                    album_key: Some(album.name.clone()),
                    orig_title: String::new(),
                    orig_artist: String::new(),
                    orig_album: album.name.clone(),
                    orig_album_artist: aa.clone(),
                    orig_year: year_s.clone(),
                    orig_track: String::new(),
                    orig_disc: String::new(),
                    title: String::new(),
                    artist: String::new(),
                    album: album.name,
                    album_artist: aa,
                    year: year_s,
                    track: String::new(),
                    disc: String::new(),
                    cover_preview,
                    new_cover: None,
                    remove_cover: false,
                    saving: false,
                });
                Task::none()
            }
            Message::CloseTagEditor => {
                self.close_tag_editor();
                Task::none()
            }
            Message::SetTagField(field, s) => {
                if let Some(d) = &mut self.tag_draft {
                    match field {
                        TagField::Title => d.title = s,
                        TagField::Artist => d.artist = s,
                        TagField::Album => d.album = s,
                        TagField::AlbumArtist => d.album_artist = s,
                        TagField::Year => d.year = s,
                        TagField::Track => d.track = s,
                        TagField::Disc => d.disc = s,
                    }
                }
                Task::none()
            }
            Message::TagPickCover => {
                if self.tag_draft.as_ref().is_some_and(|d| d.saving) {
                    return Task::none();
                }
                Task::perform(pick_cover_image(), Message::TagCoverPicked)
            }
            Message::TagFetchCover => {
                let Some(d) = &self.tag_draft else {
                    return Task::none();
                };
                if d.saving || d.fetching.is_some() {
                    return Task::none();
                }
                let artist = [
                    d.artist.trim(),
                    d.album_artist.trim(),
                    d.query_artist.trim(),
                ]
                .into_iter()
                .find(|a| !a.is_empty())
                .unwrap_or("")
                .to_owned();
                self.tag_status = None;
                self.cover_picker = Some(cover_fetch::Picker {
                    artist,
                    album: d.album.trim().to_owned(),
                    title: if d.is_track {
                        d.title.trim().to_owned()
                    } else {
                        String::new()
                    },
                    ..cover_fetch::Picker::default()
                });
                self.start_cover_search()
            }
            Message::CoverPickerArtist(v) => {
                if let Some(p) = &mut self.cover_picker {
                    p.artist = v;
                }
                Task::none()
            }
            Message::CoverPickerAlbum(v) => {
                if let Some(p) = &mut self.cover_picker {
                    p.album = v;
                }
                Task::none()
            }
            Message::CoverPickerSearch => self.start_cover_search(),
            Message::CoverPickerResults(seq, service, res) => {
                if let Some(p) = &mut self.cover_picker {
                    p.receive(seq, service, res);
                }
                Task::none()
            }
            Message::CoverPickerSelect(key) => {
                if let Some(p) = &mut self.cover_picker
                    && !p.downloading
                {
                    p.selected = Some(key);
                }
                Task::none()
            }
            Message::CoverPickerUse => {
                let Some(p) = &mut self.cover_picker else {
                    return Task::none();
                };
                let Some(cand) = p.selected_candidate().cloned() else {
                    return Task::none();
                };
                if p.downloading {
                    return Task::none();
                }
                p.downloading = true;
                p.error = None;
                let seq = p.seq;
                let cancel = Arc::new(AtomicBool::new(false));
                if let Some(old) = self.cover_fetch_cancel.replace(Arc::clone(&cancel)) {
                    old.store(true, Ordering::Relaxed);
                }
                if let Some(d) = &mut self.tag_draft {
                    d.fetching = Some(seq);
                }
                Task::perform(cover_fetch::download(cand, cancel), move |r| {
                    Message::TagCoverFetched(seq, r)
                })
            }
            Message::CoverPickerClose => {
                self.close_cover_picker();
                Task::none()
            }
            Message::TagCoverFetched(seq, res) => {
                let current = self
                    .tag_draft
                    .as_ref()
                    .is_some_and(|d| d.fetching == Some(seq));
                if !current {
                    if let Ok((path, _)) = res {
                        cover_fetch::discard(&path);
                    }
                    return Task::none();
                }
                self.cover_fetch_cancel = None;
                let Some(d) = &mut self.tag_draft else {
                    return Task::none();
                };
                d.fetching = None;
                match res {
                    Ok((path, source)) => {
                        self.cover_picker = None;
                        self.fetched_covers.push(path.clone());
                        d.new_cover = Some(path.clone());
                        d.remove_cover = false;
                        d.cover_preview = Some(path.clone());
                        d.cover_source = Some(source);
                        self.covers.invalidate_paths(std::slice::from_ref(&path));
                        self.covers.ensure_full(&path);
                    }
                    Err(err) => match &mut self.cover_picker {
                        Some(p) => {
                            p.downloading = false;
                            p.error = Some(err);
                        }
                        None => self.tag_status = Some(Err(err)),
                    },
                }
                Task::none()
            }
            Message::TagCoverPicked(Ok(Some(path))) => {
                if let Some(d) = &mut self.tag_draft {
                    d.cover_source = None;
                    d.new_cover = Some(path.clone());
                    d.remove_cover = false;
                    d.cover_preview = Some(path.clone());
                    self.covers.invalidate_paths(std::slice::from_ref(&path));
                    self.covers.ensure_full(&path);
                }
                Task::none()
            }
            Message::TagCoverPicked(Ok(None)) => Task::none(),
            Message::TagCoverPicked(Err(err)) => self
                .toasts
                .push(NoticeKind::Error, format!("COVER_PICK_FAILED // {err}")),
            Message::TagRemoveCover => {
                if let Some(d) = &mut self.tag_draft
                    && !d.saving
                {
                    d.remove_cover = true;
                    d.new_cover = None;
                    d.cover_preview = None;
                    d.cover_source = None;
                }
                Task::none()
            }
            Message::SaveTrackTags => self.save_track_tags(),
            _ => Task::none(),
        }
    }

    pub(crate) fn close_tag_editor(&mut self) {
        if self.tag_draft.as_ref().is_some_and(|d| d.saving) {
            return;
        }
        self.tag_draft = None;
        self.tag_status = None;
        self.end_cover_fetches();
    }

    pub(super) fn close_cover_picker(&mut self) {
        if let Some(cancel) = self.cover_fetch_cancel.take() {
            cancel.store(true, Ordering::Relaxed);
        }
        self.cover_picker = None;
        if let Some(d) = &mut self.tag_draft {
            d.fetching = None;
        }
    }

    /// Searches enabled online providers for the picker's fields; an earlier search still running
    /// is cancelled and its answers ignored.
    fn start_cover_search(&mut self) -> Task<Message> {
        let Some(p) = &mut self.cover_picker else {
            return Task::none();
        };
        if p.downloading {
            return Task::none();
        }
        self.cover_fetch_seq += 1;
        let seq = self.cover_fetch_seq;
        let cancel = Arc::new(AtomicBool::new(false));
        if let Some(old) = self.cover_fetch_cancel.replace(Arc::clone(&cancel)) {
            old.store(true, Ordering::Relaxed);
        }

        let Some(query) = cover_fetch::query(p.artist.trim(), p.album.trim(), p.title.trim())
        else {
            *p = cover_fetch::Picker {
                artist: std::mem::take(&mut p.artist),
                album: std::mem::take(&mut p.album),
                title: std::mem::take(&mut p.title),
                seq,
                error: Some("FILL IN ALBUM (AND ARTIST) TO SEARCH".to_owned()),
                ..cover_fetch::Picker::default()
            };
            return Task::none();
        };

        let providers = ArtworkProviders::from_settings(&self.settings);
        rustygrade_linux::artwork_lookup::shared_resolver().set_providers(providers.clone());
        let with_itunes = providers.itunes;
        let with_deezer = providers.deezer;
        let with_mb = providers.musicbrainz && query.use_album;
        let with_discogs = providers.discogs_ready();

        if !(with_itunes || with_deezer || with_mb || with_discogs) {
            let err = if !providers.itunes
                && !providers.deezer
                && !providers.musicbrainz
                && !providers.discogs
            {
                "ENABLE AT LEAST ONE ARTWORK API IN SETTINGS"
            } else if providers.discogs
                && !providers.discogs_ready()
                && !with_itunes
                && !with_deezer
                && !with_mb
            {
                "DISCOGS REQUIRES A TOKEN IN SETTINGS"
            } else {
                "ENTER AN ALBUM TO SEARCH MUSICBRAINZ"
            };
            *p = cover_fetch::Picker {
                artist: std::mem::take(&mut p.artist),
                album: std::mem::take(&mut p.album),
                title: std::mem::take(&mut p.title),
                seq,
                error: Some(err.to_owned()),
                ..cover_fetch::Picker::default()
            };
            return Task::none();
        }

        *p = cover_fetch::Picker {
            artist: std::mem::take(&mut p.artist),
            album: std::mem::take(&mut p.album),
            title: std::mem::take(&mut p.title),
            seq,
            itunes_pending: with_itunes,
            deezer_pending: with_deezer,
            musicbrainz_pending: with_mb,
            discogs_pending: with_discogs,
            ..cover_fetch::Picker::default()
        };

        let mut tasks = Vec::new();
        for (enabled, service) in [
            (with_itunes, cover_fetch::Service::Itunes),
            (with_deezer, cover_fetch::Service::Deezer),
            (with_mb, cover_fetch::Service::MusicBrainz),
            (with_discogs, cover_fetch::Service::Discogs),
        ] {
            if enabled {
                let q = query.clone();
                let c = Arc::clone(&cancel);
                tasks.push(Task::perform(
                    cover_fetch::search(q, service, c),
                    move |r| Message::CoverPickerResults(seq, service, r),
                ));
            }
        }
        super::batch_tasks(tasks)
    }

    fn save_track_tags(&mut self) -> Task<Message> {
        if let Some(d) = &mut self.tag_draft {
            if d.saving {
                return Task::none();
            }
            if self.is_playing
                && self.current_track.as_ref().is_some_and(|t| {
                    let cur = std::path::Path::new(&t.file_path);
                    d.paths.iter().any(|p| p == cur)
                })
            {
                self.tag_status = Some(Err(
                    "Pause or switch tracks before editing a currently playing file.".to_owned(),
                ));
                return Task::none();
            }
            let year = match parse_optional_u32(&d.year, &d.orig_year, "Year") {
                Ok(v) => v,
                Err(err) => {
                    self.tag_status = Some(Err(err));
                    return Task::none();
                }
            };
            let (title, artist, track, disc) = if d.is_track {
                let track = match parse_optional_u32(&d.track, &d.orig_track, "Track #") {
                    Ok(v) => v,
                    Err(err) => {
                        self.tag_status = Some(Err(err));
                        return Task::none();
                    }
                };
                let disc = match parse_optional_u32(&d.disc, &d.orig_disc, "Disc #") {
                    Ok(v) => v,
                    Err(err) => {
                        self.tag_status = Some(Err(err));
                        return Task::none();
                    }
                };
                (
                    (d.title != d.orig_title).then(|| d.title.clone()),
                    (d.artist != d.orig_artist).then(|| d.artist.clone()),
                    track,
                    disc,
                )
            } else {
                (None, None, None, None)
            };
            let edit = TagEdit {
                title,
                artist,
                album: (d.album != d.orig_album).then(|| d.album.clone()),
                album_artist: (d.album_artist != d.orig_album_artist)
                    .then(|| d.album_artist.clone()),
                year,
                track,
                disc,
                cover_image: d.new_cover.clone(),
                remove_cover: d.remove_cover,
            };
            if edit.title.is_none()
                && edit.artist.is_none()
                && edit.album.is_none()
                && edit.album_artist.is_none()
                && edit.year.is_none()
                && edit.track.is_none()
                && edit.disc.is_none()
                && !edit.changes_cover()
            {
                self.tag_status = Some(Ok("No changes to save.".to_owned()));
                return Task::none();
            }
            d.saving = true;
            self.tag_status = None;
            let _ = self.cmd_tx.send(ControllerCommand::SaveTags {
                paths: d.paths.clone(),
                album_key: d.album_key.clone(),
                edit,
            });
        }
        Task::none()
    }
}

fn parse_optional_u32(current: &str, orig: &str, label: &str) -> Result<Option<u32>, String> {
    if current == orig {
        return Ok(None);
    }
    let trimmed = current.trim();
    if trimmed.is_empty() {
        return Ok(Some(0));
    }
    trimmed
        .parse::<u32>()
        .map(Some)
        .map_err(|_| format!("Invalid {label}: enter a whole number (got '{trimmed}')"))
}
