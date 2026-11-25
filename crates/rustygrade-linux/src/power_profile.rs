use std::path::Path;
use std::time::Duration;

use futures_util::StreamExt;
use rustygrade_core::PerformanceMode;
use tokio::sync::mpsc;
use tracing::debug;
use zbus::proxy::Builder as ProxyBuilder;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PowerProfile {
    Performance,
    #[default]
    Balanced,
    PowerSaver,
    Unknown,
}

impl PowerProfile {
    pub fn from_ppd_str(raw: &str) -> Self {
        match raw.trim() {
            "performance" => Self::Performance,
            "balanced" => Self::Balanced,
            "power-saver" => Self::PowerSaver,
            _ => Self::Unknown,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Performance => "performance",
            Self::Balanced => "balanced",
            Self::PowerSaver => "power-saver",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RenderTier {
    pub effective_profile: PowerProfile,
    pub fps_cap: u32,
    pub enable_animations: bool,
    pub poll_interval_ms: u64,
    pub scan_threads: usize,
}

impl RenderTier {
    /// Fixed animation frame rate when animations are enabled; Power Saver disables animations (0 FPS).
    pub const ANIMATION_FPS: u32 = 60;

    pub fn resolve(mode: PerformanceMode, system: PowerProfile) -> Self {
        let effective = match mode {
            PerformanceMode::Auto => match system {
                PowerProfile::Unknown => PowerProfile::Balanced,
                other => other,
            },
            PerformanceMode::Performance => PowerProfile::Performance,
            PerformanceMode::Balanced => PowerProfile::Balanced,
            PerformanceMode::PowerSaver => PowerProfile::PowerSaver,
        };

        let host_cpus = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4);

        match effective {
            PowerProfile::Performance => Self {
                effective_profile: effective,
                fps_cap: Self::ANIMATION_FPS,
                enable_animations: true,
                poll_interval_ms: 200,
                scan_threads: host_cpus.clamp(2, 8),
            },
            PowerProfile::Balanced | PowerProfile::Unknown => Self {
                effective_profile: effective,
                fps_cap: Self::ANIMATION_FPS,
                enable_animations: true,
                poll_interval_ms: 250,
                scan_threads: (host_cpus / 2).clamp(2, 4),
            },
            PowerProfile::PowerSaver => Self {
                effective_profile: effective,
                fps_cap: 0,
                enable_animations: false,
                poll_interval_ms: 1000,
                scan_threads: 2,
            },
        }
    }
}

/// How often the CPU's own power state is read again (sysfs gives no change notification).
const CPU_POLL: Duration = Duration::from_secs(5);

/// Whether the CPU itself is set to save power, whoever set it (power-profiles-daemon, TLP,
/// auto-cpufreq, cpupower, a desktop applet): the ACPI platform profile is `low-power`, every
/// cpufreq policy's energy-performance preference is `power`, or (without EPP) every policy runs the
/// `powersave` governor under a driver where that means the lowest clock.
pub fn cpu_power_saving() -> bool {
    let read = |p: &Path| std::fs::read_to_string(p).ok();
    let platform = read(Path::new("/sys/firmware/acpi/platform_profile"));
    let mut epps = Vec::new();
    let mut governors = Vec::new();
    if let Ok(dir) = std::fs::read_dir("/sys/devices/system/cpu/cpufreq") {
        for policy in dir.flatten() {
            if !policy.file_name().to_string_lossy().starts_with("policy") {
                continue;
            }
            let p = policy.path();
            if let Some(epp) = read(&p.join("energy_performance_preference")) {
                epps.push(epp);
            }
            if let (Some(gov), Some(driver)) = (
                read(&p.join("scaling_governor")),
                read(&p.join("scaling_driver")),
            ) {
                governors.push((gov, driver));
            }
        }
    }
    classify_cpu(platform.as_deref(), &epps, &governors)
}

fn classify_cpu(platform: Option<&str>, epps: &[String], governors: &[(String, String)]) -> bool {
    if platform.is_some_and(|p| p.trim() == "low-power") {
        return true;
    }
    if !epps.is_empty() {
        return epps.iter().all(|e| e.trim() == "power");
    }
    // intel_pstate / amd-pstate-epp in active mode call their normal dynamic governor "powersave".
    !governors.is_empty()
        && governors.iter().all(|(gov, driver)| {
            gov.trim() == "powersave" && !matches!(driver.trim(), "intel_pstate" | "amd-pstate-epp")
        })
}

/// The system profile the app follows in Auto mode: power-profiles-daemon's, or power saver
/// whenever the CPU itself saves power.
fn effective(ppd: PowerProfile, cpu_saving: bool) -> PowerProfile {
    if cpu_saving {
        PowerProfile::PowerSaver
    } else {
        ppd
    }
}

/// Live monitor for the system power profile: UPower's `power-profiles-daemon`
/// (`org.freedesktop.UPower.PowerProfiles`, falling back to `net.hadess.PowerProfiles`), combined
/// with the CPU's own power state ([`cpu_power_saving`], read every few seconds). Never panics or
/// fails if PPD is absent.
pub struct PowerProfileMonitor;

impl PowerProfileMonitor {
    pub async fn start() -> (PowerProfile, mpsc::UnboundedReceiver<PowerProfile>) {
        let (ppd_initial, mut ppd_rx) = Self::start_ppd().await;
        let mut cpu = tokio::task::spawn_blocking(cpu_power_saving)
            .await
            .unwrap_or(false);
        let initial = effective(ppd_initial, cpu);
        debug!(
            profile = initial.label(),
            ppd = ppd_initial.label(),
            cpu_power_saving = cpu,
            "Power profile"
        );

        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            let mut ppd = ppd_initial;
            let mut current = initial;
            let mut ppd_open = true;
            let mut poll = tokio::time::interval(CPU_POLL);
            poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            poll.tick().await;
            loop {
                tokio::select! {
                    _ = tx.closed() => break,
                    next = ppd_rx.recv(), if ppd_open => match next {
                        Some(p) => ppd = p,
                        None => ppd_open = false,
                    },
                    _ = poll.tick() => {
                        cpu = tokio::task::spawn_blocking(cpu_power_saving).await.unwrap_or(cpu);
                    }
                }
                let next = effective(ppd, cpu);
                if next != current {
                    debug!(
                        profile = next.label(),
                        cpu_power_saving = cpu,
                        "Power profile changed"
                    );
                    current = next;
                    if tx.send(next).is_err() {
                        break;
                    }
                }
            }
        });
        (initial, rx)
    }

    async fn start_ppd() -> (PowerProfile, mpsc::UnboundedReceiver<PowerProfile>) {
        let (tx, rx) = mpsc::unbounded_channel();

        let Ok(conn) = zbus::Connection::system().await else {
            debug!("System D-Bus unavailable; defaulting PPD profile to Unknown");
            return (PowerProfile::Unknown, rx);
        };

        let targets = [
            (
                "org.freedesktop.UPower.PowerProfiles",
                "/org/freedesktop/UPower/PowerProfiles",
                "org.freedesktop.UPower.PowerProfiles",
            ),
            (
                "net.hadess.PowerProfiles",
                "/net/hadess/PowerProfiles",
                "net.hadess.PowerProfiles",
            ),
        ];

        for (dest, path, iface) in targets {
            let Ok(proxy) = ProxyBuilder::<'_, zbus::Proxy<'static>>::new(&conn)
                .destination(dest)
                .path(path)
                .interface(iface)
                .build()
                .await
            else {
                continue;
            };
            let Ok(raw_profile) = proxy.get_property::<String>("ActiveProfile").await else {
                continue;
            };

            let initial = PowerProfile::from_ppd_str(&raw_profile);
            debug!(
                profile = initial.label(),
                service = dest,
                "Connected to PowerProfilesDaemon"
            );

            tokio::spawn(async move {
                // Keep system D-Bus connection alive inside this monitor task.
                let _conn_keepalive = conn;
                let mut stream = proxy
                    .receive_property_changed::<String>("ActiveProfile")
                    .await;
                let mut current = initial;
                loop {
                    tokio::select! {
                        _ = tx.closed() => break,
                        item = stream.next() => {
                            let Some(changed) = item else {
                                break;
                            };
                            if let Ok(raw) = changed.get().await {
                                let next = PowerProfile::from_ppd_str(&raw);
                                if next != current {
                                    current = next;
                                    if tx.send(next).is_err() {
                                        break;
                                    }
                                }
                            }
                        }
                    }
                }
            });

            return (initial, rx);
        }

        debug!("PowerProfilesDaemon service not found on system bus; falling back cleanly");
        (PowerProfile::Unknown, rx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_power_saving_and_render_tier_resolve() {
        let s = |v: &[&str]| v.iter().map(|x| format!("{x}\n")).collect::<Vec<_>>();
        let g = |v: &[(&str, &str)]| {
            v.iter()
                .map(|(a, b)| (a.to_string(), b.to_string()))
                .collect::<Vec<_>>()
        };
        assert!(classify_cpu(None, &s(&["power", "power"]), &[]));
        assert!(!classify_cpu(None, &s(&["power", "balance_power"]), &[]));
        assert!(!classify_cpu(
            None,
            &s(&["performance"]),
            &g(&[("powersave", "intel_pstate")])
        ));
        assert!(classify_cpu(Some("low-power\n"), &s(&["performance"]), &[]));
        assert!(classify_cpu(
            None,
            &[],
            &g(&[("powersave", "acpi-cpufreq")])
        ));
        assert!(!classify_cpu(
            None,
            &[],
            &g(&[("schedutil", "acpi-cpufreq")])
        ));
        assert!(!classify_cpu(None, &[], &[]));

        let ps = RenderTier::resolve(PerformanceMode::Auto, PowerProfile::PowerSaver);
        assert_eq!(
            (ps.fps_cap, ps.enable_animations, ps.poll_interval_ms),
            (0, false, 1000)
        );

        let forced_perf =
            RenderTier::resolve(PerformanceMode::Performance, PowerProfile::PowerSaver);
        assert_eq!(
            (
                forced_perf.effective_profile,
                forced_perf.fps_cap,
                forced_perf.enable_animations
            ),
            (PowerProfile::Performance, 60, true)
        );

        let fallback = RenderTier::resolve(PerformanceMode::Auto, PowerProfile::Unknown);
        assert_eq!(
            (fallback.effective_profile, fallback.fps_cap),
            (PowerProfile::Balanced, 60)
        );
    }
}
