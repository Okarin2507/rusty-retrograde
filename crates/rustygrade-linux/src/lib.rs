pub mod artwork_lookup;
pub mod device_reservation;
pub mod discord_rpc;
pub mod gpu_policy;
pub mod malloc_tuning;
pub mod mpris;
pub mod portal;
pub mod power_profile;
pub mod single_instance;
pub mod tray;

pub use artwork_lookup::ArtworkProviders;
pub use device_reservation::{DeviceReservation, ReservationError};
pub use discord_rpc::{
    DEFAULT_DISCORD_CLIENT_ID, DiscordPlaybackStatus, DiscordPresence, DiscordRpcService,
    DiscordRpcStatus, is_valid_client_id, resolve_client_id,
};
pub use gpu_policy::{
    GraphicsEnvPlan, GraphicsPolicyReport, PCI_VENDOR_AMD, PCI_VENDOR_INTEL, PCI_VENDOR_NVIDIA,
    apply_startup_graphics_policy, detect_drm_vendors, find_vulkan_icd_files, resolve_graphics_env,
};
pub use malloc_tuning::limit_malloc_arenas;
pub use mpris::{MprisCommand, MprisService, MprisState};
pub use portal::{
    pick_cover_image, pick_music_files, pick_music_folders, pick_playlist_export_file,
    pick_playlist_import_file,
};
pub use power_profile::{PowerProfile, PowerProfileMonitor, RenderTier};
pub use single_instance::{
    SingleInstance, SingleInstanceCommand, SingleInstanceGuard, SingleInstanceOutcome,
};
pub use tray::{TrayCommand, TrayService, TrayToggles};

pub(crate) fn fnv1a_64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash
}
