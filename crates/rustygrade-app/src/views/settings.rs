use iced::widget::{
    button, column, container, pick_list, row, scrollable, space, text, text_input, toggler,
    tooltip,
};
use iced::{Alignment, Border, Color, Element, Length, Padding};
use rustygrade_core::{DirectVolumeBridge, DisplayBackend, GpuPreference, PerformanceMode};
use rustygrade_linux::DiscordRpcStatus;

use super::bold;
use super::widgets::{orbit_section, orbit_tooltip};
use crate::smooth_scroll::smooth;
use crate::{App, BridgeOption, DisplayOption, GpuOption, Message, PerfOption, theme};

const GPU_OPTS: [GpuOption; 4] = [
    GpuOption(GpuPreference::Igpu),
    GpuOption(GpuPreference::Auto),
    GpuOption(GpuPreference::Dgpu),
    GpuOption(GpuPreference::Software),
];

const DISP_OPTS: [DisplayOption; 3] = [
    DisplayOption(DisplayBackend::Auto),
    DisplayOption(DisplayBackend::Wayland),
    DisplayOption(DisplayBackend::X11),
];

const PERF_OPTS: [PerfOption; 4] = [
    PerfOption(PerformanceMode::Auto),
    PerfOption(PerformanceMode::Performance),
    PerfOption(PerformanceMode::Balanced),
    PerfOption(PerformanceMode::PowerSaver),
];

const BRIDGE_OPTS: [BridgeOption; 2] = [
    BridgeOption(DirectVolumeBridge::VirtualSink),
    BridgeOption(DirectVolumeBridge::HardwareOnly),
];

fn fmt_audio_rate(rate: i32) -> String {
    if rate % 1000 == 0 {
        format!("{} KHZ", rate / 1000)
    } else {
        format!("{:.1} KHZ", rate as f64 / 1000.0)
    }
}

fn fmt_audio_spec(f: &rustygrade_audio::AudioFormat) -> String {
    format!(
        "{} · {} · {}CH",
        fmt_audio_rate(f.rate),
        f.format.to_ascii_uppercase(),
        f.channels
    )
}

fn telemetry_tile<'a>(
    label: &'static str,
    value: String,
    value_color: Color,
    accent: Color,
    tip: String,
) -> Element<'a, Message> {
    let body = row![
        container(space::horizontal())
            .width(2)
            .height(26)
            .style(theme::fill_style(accent)),
        column![
            text(label).size(9).font(bold()).color(theme::TEXT_MUTE),
            text(value).size(11).font(bold()).color(value_color),
        ]
        .spacing(2),
    ]
    .spacing(10)
    .align_y(Alignment::Center);
    let tile = container(body)
        .padding([6, 12])
        .style(|_t| container::Style {
            background: Some(iced::Background::Color(theme::PANEL_DEEP)),
            border: Border {
                color: theme::LINE,
                width: 1.0,
                radius: 0.0.into(),
            },
            ..Default::default()
        });
    orbit_tooltip(tile, tip, accent, tooltip::Position::Bottom)
}

fn diagnostics_row(app: &App) -> Element<'_, Message> {
    let gpu_short = match app.settings.gpu_preference {
        GpuPreference::Igpu => {
            if app.graphics_report.iced_backend.as_deref() == Some("tiny-skia") {
                "SOFTWARE"
            } else {
                "iGPU"
            }
        }
        GpuPreference::Auto => "AUTO GPU",
        GpuPreference::Dgpu => "dGPU",
        GpuPreference::Software => "SOFTWARE",
    };
    let disp_short = app
        .graphics_report
        .effective_display
        .split(' ')
        .next()
        .unwrap_or("AUTO")
        .to_ascii_uppercase();
    let render_val = format!("{gpu_short} · {disp_short}");
    let render_tip = format!(
        "{}\nGPU and Display Backend changes apply after restart.",
        app.graphics_report.summary
    );

    let anim_on = app.animations_enabled();
    let power_val = format!(
        "{} · {}",
        app.power_profile.label().to_ascii_uppercase(),
        if anim_on { "60 FPS" } else { "ANIM OFF" }
    );
    let power_color = if anim_on { theme::TEAL } else { theme::GOLD };
    let power_tip = format!(
        "System power profile: {} · Effective profile: {} · Animations: {}.",
        app.power_profile.label(),
        app.render_tier.effective_profile.label(),
        if anim_on { "60 FPS" } else { "Disabled" },
    );

    let bit_perfect_ok = app.bit_perfect_status.starts_with("BIT-PERFECT");
    let out_rate = app
        .audio_path
        .output
        .as_ref()
        .map(|f| fmt_audio_rate(f.rate));
    let (audio_val, audio_color, audio_accent) = if bit_perfect_ok {
        (
            out_rate.as_deref().map_or_else(
                || "BIT-PERFECT".to_owned(),
                |r| format!("BIT-PERFECT · {r}"),
            ),
            theme::TEAL,
            theme::TEAL,
        )
    } else if app.direct_active {
        (
            out_rate
                .as_deref()
                .map_or_else(|| "DIRECT".to_owned(), |r| format!("DIRECT · {r}")),
            theme::GOLD,
            theme::GOLD,
        )
    } else {
        (
            out_rate
                .as_deref()
                .map_or_else(|| "SHARED".to_owned(), |r| format!("SHARED · {r}")),
            theme::TEXT,
            theme::ORANGE,
        )
    };
    let bridge_label = match app.pw_bridge_node_id {
        Some(id) => format!("PipeWire Bridge #{id}"),
        None if app.direct_mixer_control.is_some() => "ALSA HW Mixer".to_owned(),
        None => "Shared Audio".to_owned(),
    };
    let status_line = if app.bit_perfect_status.is_empty() {
        "SHARED OUTPUT".to_owned()
    } else {
        app.bit_perfect_status.clone()
    };
    let path_line = match (&app.audio_path.input, &app.audio_path.output) {
        (Some(i), Some(o)) => format!("{} -> {}", fmt_audio_spec(i), fmt_audio_spec(o)),
        _ => "idle".to_owned(),
    };
    let audio_tip = format!("Bridge: {bridge_label}\nStatus: {status_line}\nSignal: {path_line}");

    row![
        telemetry_tile("RENDER", render_val, theme::TEXT, theme::TEAL, render_tip),
        telemetry_tile("POWER", power_val, power_color, theme::GOLD, power_tip),
        telemetry_tile("AUDIO", audio_val, audio_color, audio_accent, audio_tip),
    ]
    .spacing(10)
    .wrap()
    .vertical_spacing(8)
    .into()
}

fn setting_row<'a>(
    label: &'a str,
    description: &'a str,
    accent: Color,
    control: Element<'a, Message>,
) -> Element<'a, Message> {
    let tip_label = orbit_tooltip(
        container(text(label).size(13).color(theme::TEXT_DIM)).width(Length::Fixed(240.0)),
        description.to_owned(),
        accent,
        tooltip::Position::Top,
    );
    row![tip_label, control]
        .spacing(12)
        .align_y(Alignment::Center)
        .into()
}

pub(super) fn view_settings(app: &App) -> Element<'_, Message> {
    let mut folders_col = column![].spacing(6);
    if app.settings.library_folders.is_empty() {
        folders_col = folders_col.push(
            text("NO_FOLDERS_CONFIGURED")
                .size(11)
                .color(theme::TEXT_MUTE),
        );
    } else {
        for f in &app.settings.library_folders {
            folders_col = folders_col.push(
                container(
                    row![
                        text(f.clone())
                            .size(12)
                            .color(theme::TEXT_DIM)
                            .width(Length::Fill),
                        button(text("REMOVE").size(10).font(bold()))
                            .padding([4, 10])
                            .style(theme::ghost_button_style(false))
                            .on_press(Message::RemoveLibraryFolder(f.clone())),
                    ]
                    .spacing(10)
                    .align_y(Alignment::Center),
                )
                .padding([8, 12])
                .style(|_t| container::Style {
                    background: Some(iced::Background::Color(theme::PANEL_DEEP)),
                    border: Border {
                        color: theme::LINE_SOFT,
                        width: 1.0,
                        radius: 0.0.into(),
                    },
                    ..Default::default()
                }),
            );
        }
    }

    let telemetry = orbit_section("RUNTIME_TELEMETRY", theme::TEAL, diagnostics_row(app));

    let graphics = orbit_section(
        "GRAPHICS_&_DISPLAY",
        theme::GOLD,
        column![
            setting_row(
                "GPU PREFERENCE",
                "Selects which GPU or CPU renderer Rustygrade uses (applies on restart).",
                theme::GOLD,
                pick_list(
                    &GPU_OPTS[..],
                    Some(GpuOption(app.settings.gpu_preference)),
                    Message::SetGpuPreference,
                )
                .text_size(12)
                .style(theme::pick_style)
                .menu_style(theme::pick_menu_style)
                .into(),
            ),
            setting_row(
                "DISPLAY BACKEND",
                "Chooses Wayland or X11 window backend (applies on restart).",
                theme::GOLD,
                pick_list(
                    &DISP_OPTS[..],
                    Some(DisplayOption(app.settings.display_backend)),
                    Message::SetDisplayBackend,
                )
                .text_size(12)
                .style(theme::pick_style)
                .menu_style(theme::pick_menu_style)
                .into(),
            ),
            setting_row(
                "PERFORMANCE / POWER MODE",
                "Follows the system power profile or locks Performance, Balanced, or Power Saver live.",
                theme::GOLD,
                pick_list(
                    &PERF_OPTS[..],
                    Some(PerfOption(app.settings.performance_mode)),
                    Message::SetPerformanceMode,
                )
                .text_size(12)
                .style(theme::pick_style)
                .menu_style(theme::pick_menu_style)
                .into(),
            ),
            setting_row(
                "ANIMATIONS",
                "Runs UI animations at 60 FPS and smooth scroll glides at display VSync, or turns all animations off (automatically paused in Power Saver).",
                theme::GOLD,
                row![
                    toggler(app.settings.enable_animations)
                        .style(theme::toggler_style)
                        .on_toggle(Message::ToggleAnimations),
                    text(if app.animations_enabled() {
                        "60 FPS"
                    } else if !app.render_tier.enable_animations {
                        "OFF (POWER SAVER)"
                    } else {
                        "OFF"
                    })
                    .size(10)
                    .color(if app.animations_enabled() {
                        theme::TEAL
                    } else if !app.render_tier.enable_animations {
                        theme::GOLD
                    } else {
                        theme::TEXT_MUTE
                    }),
                ]
                .spacing(10)
                .align_y(Alignment::Center)
                .into(),
            ),
            setting_row(
                "DYNAMIC ACCENT FROM COVER",
                "Adapts interface highlights and player accents to the current track's album cover.",
                theme::GOLD,
                toggler(app.settings.accent_from_cover)
                    .style(theme::toggler_style)
                    .on_toggle(Message::ToggleAccentFromCover)
                    .into(),
            ),
        ]
        .spacing(12)
        .into(),
    );

    let (tray_label, tray_color) = if !app.settings.enable_tray {
        ("OFF", theme::TEXT_MUTE)
    } else if app.tray_available {
        ("CONNECTED", theme::TEAL)
    } else {
        ("UNAVAILABLE", theme::TEXT_MUTE)
    };
    let (rpc_label, rpc_color) = match app.discord_rpc_status {
        DiscordRpcStatus::Connected => ("CONNECTED", theme::TEAL),
        DiscordRpcStatus::WaitingForDiscord => ("WAITING FOR DISCORD", theme::GOLD),
        DiscordRpcStatus::Rejected => ("INVALID APP ID", theme::ORANGE),
        DiscordRpcStatus::Disabled => ("OFF", theme::TEXT_MUTE),
    };
    let mut audio_rows = column![
        setting_row(
            "DIRECT ALSA VOLUME SYNC",
            "Mirrors hardware DAC mixer volume via a temporary PipeWire virtual sink or uses hardware control only.",
            theme::ORANGE,
            pick_list(
                &BRIDGE_OPTS[..],
                Some(BridgeOption(app.settings.direct_volume_bridge)),
                Message::SetDirectBridge,
            )
            .text_size(12)
            .style(theme::pick_style)
            .menu_style(theme::pick_menu_style)
            .into(),
        ),
        setting_row(
            "STATUSNOTIFIER TRAY",
            "Shows a StatusNotifierItem system tray icon with quick playback controls (applies live).",
            theme::ORANGE,
            row![
                toggler(app.settings.enable_tray)
                    .style(theme::toggler_style)
                    .on_toggle(Message::ToggleTray),
                text(tray_label).size(10).color(tray_color),
            ]
            .spacing(10)
            .align_y(Alignment::Center)
            .into(),
        ),
        setting_row(
            "MINIMIZE TO TRAY ON CLOSE",
            "Keeps music playing in the background when the main window is closed.",
            theme::ORANGE,
            toggler(app.settings.minimize_to_tray == Some(true))
                .style(theme::toggler_style)
                .on_toggle(Message::ToggleMinimizeToTray)
                .into(),
        ),
        setting_row(
            "MPRIS MEDIA CONTROLS",
            "Exposes playback metadata and transport controls on D-Bus MPRIS (applies live).",
            theme::ORANGE,
            toggler(app.settings.enable_mpris)
                .style(theme::toggler_style)
                .on_toggle(Message::ToggleMpris)
                .into(),
        ),
        setting_row(
            "DISCORD RICH PRESENCE",
            "Publishes the currently playing track and timestamps to Discord (applies live).",
            theme::ORANGE,
            row![
                toggler(app.settings.enable_discord_rpc)
                    .style(theme::toggler_style)
                    .on_toggle(Message::ToggleDiscordRpc),
                text(rpc_label).size(10).color(rpc_color),
            ]
            .spacing(10)
            .align_y(Alignment::Center)
            .into(),
        ),
    ]
    .spacing(12);
    if app.settings.enable_discord_rpc {
        audio_rows = audio_rows.push(setting_row(
            "DISCORD ALBUM ARTWORK",
            "Fetches public album cover URLs from enabled Online Artwork APIs for Discord Rich Presence.",
            theme::ORANGE,
            toggler(app.settings.discord_fetch_artwork)
                .style(theme::toggler_style)
                .on_toggle(Message::ToggleDiscordArtwork)
                .into(),
        ));
    }
    audio_rows = audio_rows.push(setting_row(
        "AUTO-WATCH MUSIC FOLDERS",
        "Watches configured music folders for added, changed, or removed audio files.",
        theme::ORANGE,
        toggler(app.settings.watch_folders)
            .style(theme::toggler_style)
            .on_toggle(Message::ToggleWatchFolders)
            .into(),
    ));
    let audio_sys = orbit_section("AUDIO_&_SYSTEM", theme::ORANGE, audio_rows.into());

    let discogs_has_token = !app.settings.discogs_token.trim().is_empty();
    let (discogs_label, discogs_color) = if !app.settings.artwork_discogs {
        ("OFF", theme::TEXT_MUTE)
    } else if discogs_has_token {
        ("READY", theme::TEAL)
    } else {
        ("TOKEN REQUIRED", theme::GOLD)
    };
    let mut artwork_rows = column![
        setting_row(
            "ITUNES",
            "Queries Apple iTunes Search API for album and track artwork (no API key required).",
            theme::TEAL,
            toggler(app.settings.artwork_itunes)
                .style(theme::toggler_style)
                .on_toggle(Message::ToggleArtworkItunes)
                .into(),
        ),
        setting_row(
            "DEEZER",
            "Queries Deezer Public API for album and track artwork (no API key required).",
            theme::TEAL,
            toggler(app.settings.artwork_deezer)
                .style(theme::toggler_style)
                .on_toggle(Message::ToggleArtworkDeezer)
                .into(),
        ),
        setting_row(
            "MUSICBRAINZ",
            "Queries MusicBrainz and Cover Art Archive for album release group artwork (album tags required).",
            theme::TEAL,
            toggler(app.settings.artwork_musicbrainz)
                .style(theme::toggler_style)
                .on_toggle(Message::ToggleArtworkMusicbrainz)
                .into(),
        ),
        setting_row(
            "DISCOGS",
            "Queries Discogs Database Search for CD, vinyl, and OST releases (requires a free Personal Access Token).",
            theme::TEAL,
            row![
                toggler(app.settings.artwork_discogs)
                    .style(theme::toggler_style)
                    .on_toggle(Message::ToggleArtworkDiscogs),
                text(discogs_label).size(10).color(discogs_color),
            ]
            .spacing(10)
            .align_y(Alignment::Center)
            .into(),
        ),
    ]
    .spacing(12);
    if app.settings.artwork_discogs {
        artwork_rows = artwork_rows.push(setting_row(
            "DISCOGS TOKEN",
            "Personal Access Token from Discogs Settings > Developers. Stored in native-settings.json and sent only to api.discogs.com.",
            theme::TEAL,
            row![
                text_input("PERSONAL ACCESS TOKEN...", &app.settings.discogs_token)
                    .secure(!app.discogs_token_visible)
                    .size(12)
                    .padding([6, 10])
                    .width(Length::Fixed(280.0))
                    .style(theme::input_style)
                    .on_input(Message::DiscogsTokenChanged)
                    .on_submit(Message::DiscogsTokenCommit(0)),
                button(
                    text(if app.discogs_token_visible { "HIDE" } else { "SHOW" })
                        .size(10)
                        .font(bold()),
                )
                .padding(Padding {
                    top: 7.0,
                    right: 10.0,
                    bottom: 6.0,
                    left: 10.0,
                })
                .style(theme::ghost_button_style(app.discogs_token_visible))
                .on_press(Message::ToggleDiscogsTokenVisible),
            ]
            .spacing(8)
            .align_y(Alignment::Center)
            .into(),
        ));
    }
    let artwork_apis = orbit_section("ONLINE_ARTWORK_APIS", theme::TEAL, artwork_rows.into());

    let library = orbit_section(
        "LIBRARY_SOURCES",
        theme::GOLD,
        column![
            row![
                button(text("+ ADD FOLDERS").size(11).font(bold()))
                    .padding([7, 14])
                    .style(theme::accent_button_style())
                    .on_press(Message::PickFolders),
                button(text("+ ADD FILES").size(11).font(bold()))
                    .padding([7, 14])
                    .style(theme::ghost_button_style(false))
                    .on_press(Message::PickFiles),
                button(text("RESCAN ALL").size(11).font(bold()))
                    .padding([7, 14])
                    .style(theme::ghost_button_style(false))
                    .on_press(Message::RescanLibrary),
                button(text("RESYNC COVERS").size(11).font(bold()))
                    .padding([7, 14])
                    .style(theme::ghost_button_style(false))
                    .on_press(Message::ResyncCovers),
            ]
            .spacing(8),
            folders_col,
        ]
        .spacing(12)
        .into(),
    );

    let mode_chip = |label: &'static str, on: bool, msg: Message| {
        button(text(label).size(10).font(bold()))
            .padding([5, 12])
            .style(theme::chip_style(on))
            .on_press(msg)
    };
    let mode_hint = if app.backup_import_replace {
        "Replaces all playlists, favorites & smart rules (confirms first)"
    } else {
        "Merges into existing collections without overwriting local IDs"
    };
    let playlists_backup = orbit_section(
        "PLAYLISTS_&_BACKUP",
        theme::TEAL,
        column![
            setting_row(
                "ZIP IMPORT MODE",
                "Controls whether importing a .zip backup merges with local playlists and favorites or replaces them after confirmation.",
                theme::TEAL,
                row![
                    mode_chip("MERGE", !app.backup_import_replace, Message::SetBackupImportReplace(false)),
                    mode_chip("REPLACE", app.backup_import_replace, Message::SetBackupImportReplace(true)),
                    text(mode_hint)
                        .size(11)
                        .color(if app.backup_import_replace {
                            theme::ORANGE
                        } else {
                            theme::TEXT_MUTE
                        }),
                ]
                .spacing(8)
                .align_y(Alignment::Center)
                .into(),
            ),
            row![
                button(text("EXPORT COLLECTIONS (.ZIP)").size(11).font(bold()))
                    .padding([7, 14])
                    .style(theme::accent_button_style())
                    .on_press(Message::ExportCollectionsZipPick),
                button(text("IMPORT PLAYLIST / BACKUP").size(11).font(bold()))
                    .padding([7, 14])
                    .style(theme::ghost_button_style(false))
                    .on_press(Message::ImportBackupFromSettingsPick),
            ]
            .spacing(8),
        ]
        .spacing(12)
        .into(),
    );

    smooth(
        scrollable(
            column![
                telemetry,
                graphics,
                audio_sys,
                artwork_apis,
                library,
                playlists_backup,
            ]
            .spacing(16)
            .padding(Padding::ZERO.right(10.0)),
        )
        .width(Length::Fill)
        .height(Length::Fill),
    )
}
