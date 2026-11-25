use std::sync::Arc;

use iced::Task;
use iced::widget::operation;
use rustygrade_core::{
    Album, ExtraMap, SmartPlaylistData, SmartPlaylists, SmartRule, ZipArchiveSummary,
    inspect_backup_zip_file, suggest_export_filename,
};
use rustygrade_linux::{pick_cover_image, pick_playlist_export_file, pick_playlist_import_file};

use super::{AlbumKey, App, LibraryMode, Message, PlaylistView, SmartDraft};
use crate::controller::{ControllerCommand, NoticeKind, PlaylistExportTarget};
use crate::dialogs;
use crate::pages;
use crate::reorder;

impl App {
    pub(super) fn handle_collection_message(&mut self, msg: Message) -> Task<Message> {
        match msg {
            Message::ToggleCreatePlaylist(on) => {
                self.create_playlist_open = on;
                self.bank_menu_open = false;
                if on {
                    self.new_playlist_name.clear();
                    return operation::focus(self.create_input_id.clone());
                }
                Task::none()
            }
            Message::NewPlaylistNameChanged(s) => {
                self.new_playlist_name = s;
                Task::none()
            }
            Message::CreatePlaylist => {
                self.create_playlist_open = false;
                let name = std::mem::take(&mut self.new_playlist_name);
                if !name.trim().is_empty() {
                    let _ = self.cmd_tx.send(ControllerCommand::CreatePlaylist(name));
                }
                Task::none()
            }
            Message::DeletePlaylist(id) => {
                if let Some(p) = self.collections.playlists.iter().find(|p| p.id == id) {
                    self.confirm = Some(dialogs::ConfirmDialog {
                        title: "WARNING: DELETE_PLAYLIST",
                        target_label: "TARGET: ",
                        target: p.name.clone(),
                        target_orange: false,
                        detail: None,
                        note: "ACTION CANNOT BE UNDONE.",
                        confirm: "CONFIRM",
                        action: dialogs::ConfirmAction::DeletePlaylist(id),
                    });
                }
                Task::none()
            }
            Message::RenamePlaylistInput(s) => {
                self.rename_playlist_buf = s;
                Task::none()
            }
            Message::RenamePlaylistSubmit(id) => {
                let name = self.rename_playlist_buf.clone();
                let _ = self
                    .cmd_tx
                    .send(ControllerCommand::RenamePlaylist { id, name });
                Task::none()
            }
            Message::ToggleLike(id) => {
                let _ = self.cmd_tx.send(ControllerCommand::ToggleLike(id));
                Task::none()
            }
            Message::ToggleLikeTracks(track_ids) => {
                let _ = self
                    .cmd_tx
                    .send(ControllerCommand::ToggleLikeTracks(track_ids));
                Task::none()
            }
            Message::TogglePlaylistTracks {
                playlist_id,
                track_ids,
            } => {
                let _ = self.cmd_tx.send(ControllerCommand::TogglePlaylistTracks {
                    playlist_id,
                    track_ids,
                });
                Task::none()
            }
            Message::RemoveTrackFromPlaylist {
                playlist_id,
                track_id,
            } => {
                let _ = self
                    .cmd_tx
                    .send(ControllerCommand::RemoveTrackFromPlaylist {
                        playlist_id,
                        track_id,
                    });
                Task::none()
            }
            Message::RemovePlaylistCover(id) => {
                let _ = self.cmd_tx.send(ControllerCommand::RemovePlaylistCover(id));
                Task::none()
            }
            Message::NewSmartPlaylist => {
                self.bank_menu_open = false;
                self.close_tag_editor();
                self.smart_draft = Some(SmartDraft {
                    id: 0,
                    name: String::new(),
                    match_all: true,
                    rules: vec![SmartRule::default()],
                    sort: "album".to_owned(),
                    limit: String::new(),
                    extra: ExtraMap::new(),
                    match_count: 0,
                });
                self.recompute_smart_draft_count();
                Task::none()
            }
            Message::EditSmartPlaylist(sp) => {
                let rules = if sp.rules.is_empty() {
                    vec![SmartRule::default()]
                } else {
                    sp.rules
                };
                self.smart_draft = Some(SmartDraft {
                    id: sp.id,
                    name: sp.name,
                    match_all: sp.match_all,
                    rules,
                    sort: sp.sort,
                    limit: if sp.limit > 0 {
                        sp.limit.to_string()
                    } else {
                        String::new()
                    },
                    extra: sp.extra,
                    match_count: 0,
                });
                self.recompute_smart_draft_count();
                Task::none()
            }
            Message::SmartDraftNameChanged(s) => {
                if let Some(d) = &mut self.smart_draft {
                    d.name = s;
                }
                Task::none()
            }
            Message::SmartRuleFieldChanged(idx, f) => {
                if let Some(d) = &mut self.smart_draft
                    && let Some(rule) = d.rules.get_mut(idx)
                {
                    rule.field = f.key.to_owned();
                    let field_def = SmartPlaylists::field(f.key);
                    if !field_def.ops.contains(&rule.op.as_str()) {
                        rule.op = field_def.ops.first().copied().unwrap_or("is").to_owned();
                    }
                    self.recompute_smart_draft_count();
                }
                Task::none()
            }
            Message::SmartRuleOpChanged(idx, o) => {
                if let Some(d) = &mut self.smart_draft
                    && let Some(rule) = d.rules.get_mut(idx)
                {
                    rule.op = o.op.to_owned();
                    self.recompute_smart_draft_count();
                }
                Task::none()
            }
            Message::SmartRuleValueChanged(idx, v) => {
                if let Some(d) = &mut self.smart_draft
                    && let Some(rule) = d.rules.get_mut(idx)
                {
                    rule.value = v;
                    self.recompute_smart_draft_count();
                }
                Task::none()
            }
            Message::AddSmartRule => {
                if let Some(d) = &mut self.smart_draft {
                    d.rules.push(SmartRule::default());
                    self.recompute_smart_draft_count();
                }
                Task::none()
            }
            Message::RemoveSmartRule(idx) => {
                if let Some(d) = &mut self.smart_draft
                    && d.rules.len() > 1
                    && idx < d.rules.len()
                {
                    d.rules.remove(idx);
                    self.recompute_smart_draft_count();
                }
                Task::none()
            }
            Message::SmartDraftSortChanged(s) => {
                if let Some(d) = &mut self.smart_draft {
                    d.sort = s.key.to_owned();
                }
                Task::none()
            }
            Message::SmartDraftLimitChanged(l) => {
                if let Some(d) = &mut self.smart_draft {
                    d.limit = l;
                    self.recompute_smart_draft_count();
                }
                Task::none()
            }
            Message::SmartDraftMatchAllChanged(b) => {
                if let Some(d) = &mut self.smart_draft {
                    d.match_all = b;
                    self.recompute_smart_draft_count();
                }
                Task::none()
            }
            Message::SaveSmartPlaylist => {
                if let Some(d) = self.smart_draft.take() {
                    let sp = SmartPlaylistData {
                        id: d.id,
                        name: if d.name.trim().is_empty() {
                            "Smart Playlist".to_owned()
                        } else {
                            d.name.trim().to_owned()
                        },
                        match_all: d.match_all,
                        rules: d.rules,
                        sort: d.sort,
                        limit: d.limit.trim().parse::<usize>().unwrap_or(0),
                        extra: d.extra,
                    };
                    let _ = self.cmd_tx.send(ControllerCommand::SaveSmartPlaylist(sp));
                }
                Task::none()
            }
            Message::CancelSmartPlaylist => {
                self.smart_draft = None;
                Task::none()
            }
            Message::DeleteSmartPlaylist(id) => {
                if let Some(sp) = self.collections.smart.iter().flatten().find(|s| s.id == id) {
                    self.confirm = Some(dialogs::ConfirmDialog {
                        title: "WARNING: DELETE_SMART_PLAYLIST",
                        target_label: "TARGET: ",
                        target: sp.name.clone(),
                        target_orange: false,
                        detail: None,
                        note: "THE TRACKS STAY IN THE LIBRARY.",
                        confirm: "CONFIRM",
                        action: dialogs::ConfirmAction::DeleteSmartPlaylist(id),
                    });
                }
                Task::none()
            }
            Message::EnterSelectMode => {
                self.select_mode = true;
                self.selected_albums.clear();
                self.album_detail_open = false;
                self.artist_detail_open = false;
                Task::none()
            }
            Message::ExitSelectMode => {
                self.select_mode = false;
                self.selected_albums.clear();
                Task::none()
            }
            Message::ToggleAlbumSelected(key) => {
                if !self.selected_albums.remove(&key) {
                    self.selected_albums.insert(key);
                }
                Task::none()
            }
            Message::SelectAllAlbums => {
                self.selected_albums = self.albums.iter().map(AlbumKey::from_album).collect();
                Task::none()
            }
            Message::DeleteSelectedAlbums => {
                let pairs: Vec<(String, String)> = self
                    .selected_albums
                    .iter()
                    .map(|k| (k.name.clone(), k.artist.clone()))
                    .collect();
                if pairs.is_empty() {
                    return Task::none();
                }
                let n = pairs.len();
                self.confirm = Some(dialogs::ConfirmDialog {
                    title: "WARNING: BATCH_DELETE",
                    target_label: "TARGETS: ",
                    target: format!("{n} ALBUM(S)"),
                    target_orange: true,
                    detail: None,
                    note: "DATA WILL BE PERMANENTLY ERASED.",
                    confirm: "PURGE",
                    action: dialogs::ConfirmAction::DeleteAlbums(pairs),
                });
                Task::none()
            }
            Message::DeleteAlbum(key, from_detail) => {
                self.confirm = Some(if from_detail {
                    dialogs::ConfirmDialog {
                        title: "WARNING: PURGE_DATA",
                        target_label: "TARGET: ",
                        target: key.name.clone(),
                        target_orange: false,
                        detail: None,
                        note: "DATA WILL BE PERMANENTLY ERASED.",
                        confirm: "PURGE",
                        action: dialogs::ConfirmAction::DeleteAlbums(vec![(key.name, key.artist)]),
                    }
                } else {
                    dialogs::ConfirmDialog {
                        title: "WARNING: BATCH_DELETE",
                        target_label: "TARGETS: ",
                        target: "1 ALBUM".to_owned(),
                        target_orange: true,
                        detail: Some(key.name.clone()),
                        note: "DATA WILL BE PERMANENTLY ERASED.",
                        confirm: "PURGE",
                        action: dialogs::ConfirmAction::DeleteAlbums(vec![(key.name, key.artist)]),
                    }
                });
                Task::none()
            }
            Message::ReorderLift(ctx, index) => {
                let ids: Vec<String> = self.detail_tracks(&ctx).into_iter().map(|t| t.id).collect();
                self.reorder_lift = (index < ids.len()).then_some((ctx, index, ids));
                Task::none()
            }
            Message::ReorderDrop(moved) => {
                match moved {
                    Some((from, to)) => self.drop_reordered(from, to),
                    None => self.reorder_lift = None,
                }
                Task::none()
            }
            Message::ReorderEdge(dy) => operation::scroll_by(
                self.detail_scroll_id.clone(),
                iced::widget::scrollable::AbsoluteOffset { x: 0.0, y: dy },
            ),
            Message::PickPlaylistCover(id) => Task::perform(pick_cover_image(), move |res| {
                Message::PlaylistCoverPicked(id, res)
            }),
            Message::PlaylistCoverPicked(id, Ok(Some(image_path))) => {
                let _ = self.cmd_tx.send(ControllerCommand::SetPlaylistCover {
                    playlist_id: id,
                    image_path,
                });
                Task::none()
            }
            Message::PlaylistCoverPicked(_, Ok(None)) => Task::none(),
            Message::PlaylistCoverPicked(_, Err(err)) => self.toasts.push(NoticeKind::Error, err),
            Message::ImportPlaylistPick => {
                self.bank_menu_open = false;
                Task::perform(pick_playlist_import_file(), |res| {
                    Message::PlaylistImportPicked(false, res)
                })
            }
            Message::ImportBackupFromSettingsPick => {
                Task::perform(pick_playlist_import_file(), |res| {
                    Message::PlaylistImportPicked(true, res)
                })
            }
            Message::PlaylistImportPicked(from_settings, Ok(Some(path))) => {
                let ext = path
                    .extension()
                    .and_then(|e| e.to_str())
                    .map(str::to_ascii_lowercase);
                match ext.as_deref() {
                    Some("m3u" | "m3u8") => {
                        let _ = self
                            .cmd_tx
                            .send(ControllerCommand::ImportPlaylistM3u { path });
                        Task::none()
                    }
                    Some("zip") => {
                        let replace = from_settings && self.backup_import_replace;
                        if replace {
                            let inspect_path = path.clone();
                            Task::perform(
                                async move {
                                    tokio::task::spawn_blocking(move || {
                                        inspect_backup_zip_file(&inspect_path)
                                            .map_err(|e| e.to_string())
                                    })
                                    .await
                                    .map_err(|e| format!("Archive inspection failed: {e}"))?
                                },
                                move |res| Message::ZipReplaceInspected(path.clone(), res),
                            )
                        } else {
                            let _ = self.cmd_tx.send(ControllerCommand::ImportCollectionsZip {
                                path,
                                replace: false,
                            });
                            Task::none()
                        }
                    }
                    _ => self.toasts.push(
                        NoticeKind::Error,
                        "UNSUPPORTED_FILE // EXPECTED .m3u8, .m3u, OR .zip".to_owned(),
                    ),
                }
            }
            Message::ZipReplaceInspected(path, Ok(summary)) => {
                if summary.is_single_playlist_export {
                    let _ = self.cmd_tx.send(ControllerCommand::ImportCollectionsZip {
                        path,
                        replace: false,
                    });
                    return self.toasts.push(
                        NoticeKind::Info,
                        "SINGLE_PLAYLIST_ARCHIVE // IMPORTED IN MERGE MODE TO PROTECT COLLECTIONS"
                            .to_owned(),
                    );
                }
                let name = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("archive.zip")
                    .to_owned();
                let note = if !summary.has_collections && summary.has_stats {
                    "PLAY STATS WILL BE REPLACED."
                } else if summary.has_stats {
                    "ALL PLAYLISTS, FAVORITES, SMART RULES & PLAY STATS WILL BE REPLACED."
                } else {
                    "ALL PLAYLISTS, FAVORITES & SMART RULES WILL BE REPLACED."
                };
                self.confirm = Some(dialogs::ConfirmDialog {
                    title: "WARNING: REPLACE_COLLECTIONS",
                    target_label: "ARCHIVE: ",
                    target: name,
                    target_orange: true,
                    detail: Some(format_zip_replace_detail(&summary)),
                    note,
                    confirm: "REPLACE",
                    action: dialogs::ConfirmAction::ReplaceCollectionsZip(path),
                });
                Task::none()
            }
            Message::ZipReplaceInspected(_, Err(err)) => self
                .toasts
                .push(NoticeKind::Error, format!("IMPORT_FAILED // {err}")),
            Message::PlaylistImportPicked(_, Ok(None)) => Task::none(),
            Message::PlaylistImportPicked(_, Err(err)) => self.toasts.push(NoticeKind::Error, err),
            Message::SetBackupImportReplace(replace) => {
                self.backup_import_replace = replace;
                Task::none()
            }
            Message::ExportCollectionsZipPick => Task::perform(
                pick_playlist_export_file("retrograde-collections.zip".to_owned(), true),
                Message::CollectionsZipExportPicked,
            ),
            Message::CollectionsZipExportPicked(Ok(Some(dest))) => {
                let _ = self
                    .cmd_tx
                    .send(ControllerCommand::ExportCollectionsZip { dest });
                Task::none()
            }
            Message::CollectionsZipExportPicked(Ok(None)) => Task::none(),
            Message::CollectionsZipExportPicked(Err(err)) => {
                self.toasts.push(NoticeKind::Error, err)
            }
            Message::ExportPlaylistM3u8(view) => {
                let (target, raw_name) = match view {
                    PlaylistView::Liked => (PlaylistExportTarget::Liked, "Favorites"),
                    PlaylistView::Custom(id) => {
                        let name = self
                            .collections
                            .playlists
                            .iter()
                            .find(|p| p.id == id)
                            .map(|p| p.name.as_str())
                            .unwrap_or("playlist");
                        (PlaylistExportTarget::Custom(id), name)
                    }
                    PlaylistView::Smart(id) => {
                        let name = self
                            .collections
                            .smart
                            .as_deref()
                            .unwrap_or(&[])
                            .iter()
                            .find(|s| s.id == id)
                            .map(|s| s.name.as_str())
                            .unwrap_or("smart_playlist");
                        (PlaylistExportTarget::Smart(id), name)
                    }
                };
                let suggested = suggest_export_filename(raw_name, "m3u8");
                Task::perform(pick_playlist_export_file(suggested, false), move |res| {
                    Message::PlaylistM3u8ExportPicked(target, res)
                })
            }
            Message::PlaylistM3u8ExportPicked(target, Ok(Some(dest))) => {
                let _ = self
                    .cmd_tx
                    .send(ControllerCommand::ExportPlaylistM3u8 { target, dest });
                Task::none()
            }
            Message::PlaylistM3u8ExportPicked(_, Ok(None)) => Task::none(),
            Message::PlaylistM3u8ExportPicked(_, Err(err)) => {
                self.toasts.push(NoticeKind::Error, err)
            }
            Message::ExportPlaylistZip(id) => {
                let raw_name = self
                    .collections
                    .playlists
                    .iter()
                    .find(|p| p.id == id)
                    .map(|p| p.name.as_str())
                    .unwrap_or("playlist");
                let suggested = suggest_export_filename(raw_name, "zip");
                Task::perform(pick_playlist_export_file(suggested, true), move |res| {
                    Message::PlaylistZipExportPicked(id, res)
                })
            }
            Message::PlaylistZipExportPicked(id, Ok(Some(dest))) => {
                let _ = self.cmd_tx.send(ControllerCommand::ExportPlaylistZip {
                    playlist_id: id,
                    dest,
                });
                Task::none()
            }
            Message::PlaylistZipExportPicked(_, Ok(None)) => Task::none(),
            Message::PlaylistZipExportPicked(_, Err(err)) => {
                self.toasts.push(NoticeKind::Error, err)
            }
            Message::ConfirmCancel => {
                self.confirm = None;
                Task::none()
            }
            Message::ConfirmAccept => self.accept_confirm(),
            _ => Task::none(),
        }
    }

    pub fn can_reorder(&self, ctx: &pages::DetailCtx) -> bool {
        match ctx {
            pages::DetailCtx::Playlist(PlaylistView::Custom(_) | PlaylistView::Liked) => true,
            pages::DetailCtx::Album(key) => self.find_album(key).is_some_and(|a| {
                a.tracks.len() > 1
                    && a.tracks.iter().all(|t| t.track_number <= 0)
                    && a.tracks
                        .iter()
                        .all(|t| t.disc_number == a.tracks[0].disc_number)
            }),
            _ => false,
        }
    }

    fn drop_reordered(&mut self, from: usize, to: usize) {
        let Some((ctx, lifted, mut ids)) = self.reorder_lift.take() else {
            return;
        };
        if from != lifted || from >= ids.len() || !self.can_reorder(&ctx) {
            return;
        }
        reorder::move_item(&mut ids, from, to);
        match ctx {
            pages::DetailCtx::Playlist(PlaylistView::Custom(playlist_id)) => {
                let mut col = self.collections.clone();
                if let Some(p) = col.playlists.iter_mut().find(|p| p.id == playlist_id) {
                    rustygrade_core::reorder_by_ids(&mut p.track_ids, &ids, |s| Some(s.as_str()));
                    self.apply_collections(col);
                    let _ = self.cmd_tx.send(ControllerCommand::SetPlaylistOrder {
                        playlist_id,
                        track_ids: ids,
                    });
                }
            }
            pages::DetailCtx::Playlist(PlaylistView::Liked) => {
                let mut col = self.collections.clone();
                rustygrade_core::reorder_by_ids(&mut col.liked, &ids, |s| Some(s.as_str()));
                self.apply_collections(col);
                let _ = self.cmd_tx.send(ControllerCommand::SetLikedOrder(ids));
            }
            pages::DetailCtx::Album(key) => {
                let Some((name, artist)) = self
                    .find_album(&key)
                    .map(|a| (a.name.clone(), a.artist.clone()))
                else {
                    return;
                };
                let albums: Arc<[Album]> = Arc::from(rustygrade_core::with_album_track_order(
                    &self.albums,
                    &name,
                    &artist,
                    &ids,
                ));
                self.apply_library(albums);
                let _ = self.cmd_tx.send(ControllerCommand::SetAlbumOrder {
                    name,
                    artist,
                    track_ids: ids,
                });
            }
            _ => {}
        }
    }

    fn accept_confirm(&mut self) -> Task<Message> {
        let Some(d) = self.confirm.take() else {
            return Task::none();
        };
        match d.action {
            dialogs::ConfirmAction::PruneMissing(roots) => {
                let _ = self.cmd_tx.send(ControllerCommand::PruneMissing { roots });
                Task::none()
            }
            dialogs::ConfirmAction::DeletePlaylist(id) => {
                self.leave_deleted_page(PlaylistView::Custom(id));
                let _ = self.cmd_tx.send(ControllerCommand::DeletePlaylist(id));
                self.toasts.push(
                    NoticeKind::Info,
                    format!("PLAYLIST_DELETED // {}", d.target),
                )
            }
            dialogs::ConfirmAction::DeleteSmartPlaylist(id) => {
                self.leave_deleted_page(PlaylistView::Smart(id));
                let _ = self.cmd_tx.send(ControllerCommand::DeleteSmartPlaylist(id));
                Task::none()
            }
            dialogs::ConfirmAction::DeleteAlbums(pairs) => {
                self.select_mode = false;
                self.selected_albums.clear();
                self.deleting_albums = true;
                if let Some(sel) = &self.selected_album
                    && pairs
                        .iter()
                        .any(|(n, a)| n == &sel.name && a == &sel.artist)
                    && self.shown_tab() == super::Tab::Library
                {
                    self.library_mode = LibraryMode::Albums;
                }
                let _ = self.cmd_tx.send(ControllerCommand::DeleteAlbums(pairs));
                Task::none()
            }
            dialogs::ConfirmAction::ReplaceCollectionsZip(path) => {
                let _ = self.cmd_tx.send(ControllerCommand::ImportCollectionsZip {
                    path,
                    replace: true,
                });
                Task::none()
            }
        }
    }
}

fn format_zip_replace_detail(summary: &ZipArchiveSummary) -> String {
    let mut base = if !summary.has_collections && summary.has_stats {
        format!("ARCHIVE: {} PLAY STAT(S) ONLY", summary.stats_tracks)
    } else {
        let mut parts = vec![
            format!("{} PLAYLIST(S)", summary.playlists),
            format!("{} FAVORITE(S)", summary.liked),
            format!("{} SMART", summary.smart),
        ];
        if summary.has_stats {
            parts.push(format!("{} PLAY STAT(S)", summary.stats_tracks));
        }
        format!("ARCHIVE: {}", parts.join(", "))
    };
    if summary.has_settings {
        base.push_str(" (SETTINGS SKIPPED)");
    }
    base
}

#[cfg(test)]
mod tests {
    use super::{ZipArchiveSummary, format_zip_replace_detail};

    #[test]
    fn format_zip_replace_detail_summarizes_archive_contents() {
        let full = ZipArchiveSummary {
            playlists: 3,
            liked: 12,
            smart: 2,
            stats_tracks: 40,
            has_collections: true,
            has_stats: true,
            has_settings: true,
            is_single_playlist_export: false,
        };
        assert_eq!(
            format_zip_replace_detail(&full),
            "ARCHIVE: 3 PLAYLIST(S), 12 FAVORITE(S), 2 SMART, 40 PLAY STAT(S) (SETTINGS SKIPPED)"
        );

        let stats_only = ZipArchiveSummary {
            playlists: 0,
            liked: 0,
            smart: 0,
            stats_tracks: 7,
            has_collections: false,
            has_stats: true,
            has_settings: false,
            is_single_playlist_export: false,
        };
        assert_eq!(
            format_zip_replace_detail(&stats_only),
            "ARCHIVE: 7 PLAY STAT(S) ONLY"
        );
    }
}
