//! On-disk cache for detected CUDA information, valid for the current boot session.
//!
//! The installed driver and GPUs only change with a reboot in practice, so the cache is keyed on
//! an identifier of the current boot session. Reads and writes are best-effort: any failure simply
//! results in a fresh detection. Delete the file or use the `CONDA_OVERRIDE_CUDA*` variables to
//! bypass it.

use super::{CudaArchInfo, CudaInfo};
use rattler_conda_types::Version;
use serde::{Deserialize, Serialize};
use std::{path::Path, str::FromStr};

const CACHE_FILE_NAME: &str = "cuda-info-v1.json";

/// Identifies a single boot session of the machine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub(super) struct BootId(String);

impl BootId {
    /// Returns the identifier of the current boot session, or `None` if it cannot be determined
    /// (in which case no caching takes place).
    pub(super) fn current() -> Option<Self> {
        #[cfg(target_os = "linux")]
        {
            // The kernel generates a fresh UUID on every boot.
            let id = std::fs::read_to_string("/proc/sys/kernel/random/boot_id").ok()?;
            Some(Self(id.trim().to_owned()))
        }
        #[cfg(target_os = "windows")]
        {
            // Windows has no boot UUID; derive the boot time from the current time and the
            // uptime instead. The derivation drifts a little, which `matches` absorbs.
            let uptime_secs =
                unsafe { windows_sys::Win32::System::SystemInformation::GetTickCount64() } / 1000;
            let now_secs = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .ok()?
                .as_secs();
            Some(Self(now_secs.checked_sub(uptime_secs)?.to_string()))
        }
        #[cfg(not(any(target_os = "linux", target_os = "windows")))]
        {
            None
        }
    }

    /// Returns true if both identifiers refer to the same boot session.
    fn matches(&self, other: &Self) -> bool {
        #[cfg(target_os = "windows")]
        {
            // The derived boot time can drift a few seconds between processes. A real reboot
            // shifts it by at least the previous uptime, which is far larger than this tolerance.
            const BOOT_TIME_TOLERANCE_SECS: u64 = 120;
            if let (Ok(a), Ok(b)) = (self.0.parse::<u64>(), other.0.parse::<u64>()) {
                return a.abs_diff(b) <= BOOT_TIME_TOLERANCE_SECS;
            }
        }
        self == other
    }
}

#[derive(Serialize, Deserialize)]
struct CacheFile {
    /// The boot session during which detection ran.
    boot_id: BootId,
    /// Fingerprint of the installed driver when detection ran.
    driver_fingerprint: Option<String>,
    version: String,
    arch: Option<(u32, u32)>,
}

/// Returns a fingerprint of the installed NVIDIA driver, or `None` if it cannot be determined.
///
/// Drivers can be updated without a reboot, so the boot session alone is not enough to key the
/// cache on.
fn driver_fingerprint() -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        // The version of the loaded kernel module, which changes when the driver is updated and
        // the module is reloaded.
        Some(
            std::fs::read_to_string("/sys/module/nvidia/version")
                .ok()?
                .trim()
                .to_owned(),
        )
    }
    #[cfg(target_os = "windows")]
    {
        // Driver updates on Windows usually complete without a reboot but replace nvml.dll.
        let windir = std::env::var_os("WINDIR")?;
        let metadata =
            std::fs::metadata(Path::new(&windir).join("System32").join("nvml.dll")).ok()?;
        let mtime = metadata
            .modified()
            .ok()?
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?;
        Some(format!("{}:{}", mtime.as_secs(), metadata.len()))
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        None
    }
}

pub(super) fn read(cache_dir: &Path) -> Option<CudaInfo> {
    let path = cache_dir.join(CACHE_FILE_NAME);
    let Ok(content) = std::fs::read_to_string(&path) else {
        tracing::debug!("no CUDA info cache found at {}", path.display());
        return None;
    };
    let Ok(cached) = serde_json::from_str::<CacheFile>(&content) else {
        tracing::debug!("ignoring invalid CUDA info cache at {}", path.display());
        return None;
    };
    if !cached.boot_id.matches(&BootId::current()?) {
        tracing::debug!("ignoring CUDA info cache from a previous boot session");
        return None;
    }
    if cached.driver_fingerprint != driver_fingerprint() {
        tracing::debug!("ignoring CUDA info cache because the driver changed");
        return None;
    }
    let version = Version::from_str(&cached.version).ok()?;
    tracing::debug!("using CUDA info cached at {}", path.display());
    Some(CudaInfo {
        version: Some(version),
        arch_info: cached
            .arch
            .map(|(major, minor)| CudaArchInfo { major, minor }),
    })
}

pub(super) fn write(cache_dir: &Path, info: &CudaInfo) {
    // Only cache when a driver was found: detection without a driver is fast anyway, and not
    // caching the negative result means a freshly installed driver is picked up immediately.
    let Some(version) = &info.version else { return };
    let Some(boot_id) = BootId::current() else {
        return;
    };
    if std::fs::create_dir_all(cache_dir).is_err() {
        return;
    }
    let cached = CacheFile {
        boot_id,
        driver_fingerprint: driver_fingerprint(),
        version: version.to_string(),
        arch: info.arch_info.as_ref().map(|arch| (arch.major, arch.minor)),
    };
    let Ok(content) = serde_json::to_string(&cached) else {
        return;
    };
    // Write-then-rename so concurrent readers never see a partial file.
    let path = cache_dir.join(CACHE_FILE_NAME);
    let tmp = path.with_extension(format!("json.{}", std::process::id()));
    match std::fs::write(&tmp, content).and_then(|()| std::fs::rename(&tmp, &path)) {
        Ok(()) => tracing::debug!("cached CUDA info at {}", path.display()),
        Err(_) => {
            let _ = std::fs::remove_file(&tmp);
        }
    }
}
