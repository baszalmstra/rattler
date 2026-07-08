//! Provides functionality to detect CUDA information present on the current system.
//!
//! This module detects two types of CUDA information:
//!
//! ## CUDA Driver Version (`__cuda`)
//!
//! The CUDA driver version represents the maximum CUDA version supported by the installed
//! NVIDIA drivers. This is detected via:
//!
//! * NVIDIA Management Library (NVML): Standard method
//! * nvidia-smi command: Fallback on musl systems where dynamic library loading is not supported
//!
//! ## CUDA Compute Capability (`__cuda_arch`)
//!
//! The CUDA compute capability (also known as SM version or architecture version) represents
//! the **minimum** compute capability of all CUDA devices detected on the system.

use libloading::{Library, Symbol};
use once_cell::sync::OnceCell;
use rattler_conda_types::Version;
use serde::{Deserialize, Serialize};
use std::process::Command;
use std::{
    mem::MaybeUninit,
    os::raw::{c_int, c_uint, c_ulong, c_void},
    path::Path,
    ptr,
    str::FromStr,
};

mod cache;

const NVML_SUCCESS: c_int = 0;
const NVML_ERROR_UNINITIALIZED: c_int = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NvmlCudaVersionError {
    MissingSymbol,
    Nvml(c_int),
    InvalidVersion,
}

impl NvmlCudaVersionError {
    fn should_retry_after_init(self) -> bool {
        matches!(self, Self::Nvml(NVML_ERROR_UNINITIALIZED))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum CudaDetectionMethod {
    NvmlNoInit,
    NvmlInitialized,
    Libcuda,
    NvidiaSmi,
}

impl CudaDetectionMethod {
    fn as_str(self) -> &'static str {
        match self {
            Self::NvmlNoInit => "nvml_no_init",
            Self::NvmlInitialized => "nvml_initialized",
            Self::Libcuda => "libcuda",
            Self::NvidiaSmi => "nvidia_smi",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct CudaInfoSources {
    pub version: Option<CudaDetectionMethod>,
    pub arch: Option<CudaDetectionMethod>,
}

impl CudaInfoSources {
    fn version_str(self) -> &'static str {
        self.version
            .map_or("<unknown>", CudaDetectionMethod::as_str)
    }

    fn arch_str(self) -> &'static str {
        self.arch.map_or("<unknown>", CudaDetectionMethod::as_str)
    }
}

struct DetectedCudaInfo {
    info: CudaInfo,
    sources: CudaInfoSources,
}

/// Validates that a string is in the format "major.minor" where both parts are digits.
///
/// Returns `true` if the format is valid for CUDA compute capability.
pub(crate) fn is_valid_cuda_version_format(s: &str) -> bool {
    let mut parts = s.split('.');
    match (parts.next(), parts.next(), parts.next()) {
        (Some(major), Some(minor), None) => {
            !major.is_empty()
                && major.chars().all(|c| c.is_ascii_digit())
                && !minor.is_empty()
                && minor.chars().all(|c| c.is_ascii_digit())
        }
        _ => false,
    }
}

/// Information about CUDA compute capability.
///
/// The compute capability (also called SM version) defines the set of features and
/// instructions supported by a CUDA device. Higher compute capabilities generally
/// support more features and newer instruction sets.
///
/// According to the CEP specification, this represents the minimum compute capability
/// across all detected CUDA devices, formatted as `{major}.{minor}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CudaArchInfo {
    /// Major version of the compute capability (e.g., 8 for compute capability 8.6)
    pub major: u32,
    /// Minor version of the compute capability (e.g., 6 for compute capability 8.6)
    pub minor: u32,
}

/// Combined CUDA information detected from the system.
///
/// This struct contains both the CUDA driver version and compute capability information.
/// Each field is optional because detection can fail independently:
///
/// * `version` may be present even without GPUs (driver installed but no devices)
/// * `arch_info` requires at least one GPU device to be present
/// * Both may be `None` if CUDA is not available or detection fails
#[derive(Debug, Clone)]
pub struct CudaInfo {
    /// The maximum CUDA version supported by the installed driver.
    ///
    /// This corresponds to the `__cuda` virtual package.
    pub version: Option<Version>,

    /// Information about the minimum compute capability across all detected devices.
    ///
    /// This corresponds to the `__cuda_arch` virtual package. Returns `None` if
    /// no CUDA devices are detected or if device enumeration fails.
    pub arch_info: Option<CudaArchInfo>,
}

fn display_cuda_version(version: Option<&Version>) -> String {
    version.map_or_else(|| "<none>".to_string(), ToString::to_string)
}

fn display_cuda_arch(arch_info: Option<&CudaArchInfo>) -> String {
    arch_info.map_or_else(
        || "<none>".to_string(),
        |arch| format!("{}.{}", arch.major, arch.minor),
    )
}

/// Returns comprehensive CUDA information from the current platform.
///
/// This function returns both the CUDA driver version and compute capability information
/// in a single cached result. The detection is performed only once per process and the
/// result is cached for subsequent calls.
///
/// This is more efficient than calling [`cuda_version`] and [`cuda_arch`] separately
/// because the CUDA library is loaded only once.
///
/// `cache_dir` is the directory used to cache the detection result across processes until the
/// next reboot; pass `None` to disable the on-disk cache. Detection runs at most once per
/// process, so the cache directory is only used by the first call.
pub fn cuda_info(cache_dir: Option<&Path>) -> &'static CudaInfo {
    static DETECTED_CUDA_INFO: OnceCell<CudaInfo> = OnceCell::new();
    if let Some(info) = DETECTED_CUDA_INFO.get() {
        tracing::trace!(?info, "using process-cached CUDA info");
        return info;
    }

    DETECTED_CUDA_INFO.get_or_init(|| {
        // Initializing the driver to detect the GPU can be slow, so the result is cached on disk
        // and reused until the next reboot.
        if let Some(cache_dir) = cache_dir {
            tracing::trace!(cache_dir = %cache_dir.display(), "checking CUDA info cache");
            if let Some(cached) = cache::read(cache_dir) {
                tracing::debug!(
                    version = %display_cuda_version(cached.info.version.as_ref()),
                    arch = %display_cuda_arch(cached.info.arch_info.as_ref()),
                    version_source = cached.sources.version_str(),
                    arch_source = cached.sources.arch_str(),
                    "using disk-cached CUDA info"
                );
                return cached.info;
            }
        } else {
            tracing::trace!("CUDA info disk cache disabled");
        }

        tracing::trace!("detecting CUDA info from host");
        let detected = detect_cuda_info();
        tracing::debug!(
            version = %display_cuda_version(detected.info.version.as_ref()),
            arch = %display_cuda_arch(detected.info.arch_info.as_ref()),
            version_source = detected.sources.version_str(),
            arch_source = detected.sources.arch_str(),
            "detected CUDA info from host"
        );
        if let Some(cache_dir) = cache_dir {
            cache::write(cache_dir, &detected.info, detected.sources);
        }
        detected.info
    })
}

/// Returns the maximum CUDA version available on the current platform.
///
/// This corresponds to the `__cuda` virtual package. The result is cached,
/// so subsequent calls are very fast. See [`cuda_info`] for the `cache_dir` semantics.
pub fn cuda_version(cache_dir: Option<&Path>) -> Option<Version> {
    cuda_info(cache_dir).version.clone()
}

/// Returns CUDA compute capability information from the current platform.
///
/// This function returns the **minimum** compute capability across all detected
/// CUDA devices, along with the name of the device that has this minimum capability.
///
/// Returns `None` if:
/// * No CUDA drivers are installed
/// * No CUDA devices are detected
/// * Device enumeration fails
///
/// The result is cached, so subsequent calls are very fast. See [`cuda_info`] for the `cache_dir`
/// semantics.
pub fn cuda_arch(cache_dir: Option<&Path>) -> Option<CudaArchInfo> {
    cuda_info(cache_dir).arch_info.clone()
}

/// Detects comprehensive CUDA information from the current system.
///
/// This function performs unified detection of both CUDA driver version and compute
/// capability by loading NVML once and querying all necessary information.
///
/// The detection process:
/// 1. Attempts to load NVML (`libnvidia-ml`/`nvml.dll`)
/// 2. Queries the driver version (for `__cuda` virtual package); this does not require init
/// 3. Initializes NVML and enumerates all CUDA devices to query their compute capabilities
/// 4. Returns the minimum compute capability across all devices (for `__cuda_arch` virtual package)
///
/// On musl systems, both are detected via the `nvidia-smi` command since dynamic library
/// loading is not supported.
fn detect_cuda_info() -> DetectedCudaInfo {
    if cfg!(target_env = "musl") {
        tracing::trace!("detecting CUDA info via nvidia-smi because musl cannot load NVML");
        // Dynamically loading a library is not supported on musl so we have to fall-back to using
        // the nvidia-smi command.
        let version = detect_cuda_version_via_nvidia_smi();
        let arch_info = detect_cuda_arch_via_nvidia_smi();
        return DetectedCudaInfo {
            sources: CudaInfoSources {
                version: version.is_some().then_some(CudaDetectionMethod::NvidiaSmi),
                arch: arch_info
                    .is_some()
                    .then_some(CudaDetectionMethod::NvidiaSmi),
            },
            info: CudaInfo { version, arch_info },
        };
    }

    tracing::trace!("detecting CUDA info via NVML");
    // Prefer NVML because it is not affected by `CUDA_VISIBLE_DEVICES`, but fall back to the
    // older probes so systems that expose libcuda (or nvidia-smi) without NVML still report
    // `__cuda`.
    let mut detected = detect_cuda_info_via_nvml();

    if detected.info.version.is_none() {
        tracing::debug!(
            "NVML did not detect a CUDA driver version; trying libcuda/nvidia-smi fallbacks"
        );
        if let Some((version, source)) = detect_cuda_version_fallbacks() {
            detected.info.version = Some(version);
            detected.sources.version = Some(source);
        }
    }

    if detected.info.version.is_some() && detected.info.arch_info.is_none() {
        tracing::debug!("NVML did not detect CUDA compute capability; trying nvidia-smi fallback");
        detected.info.arch_info = detect_cuda_arch_via_nvidia_smi();
        if detected.info.arch_info.is_some() {
            detected.sources.arch = Some(CudaDetectionMethod::NvidiaSmi);
        }
    }

    detected
}

/// Detects CUDA version and architecture information via the NVIDIA Management Library.
///
/// The library is loaded once and used to query both the driver version and device compute
/// capabilities. NVML is preferred over libcuda because it is not affected by
/// `CUDA_VISIBLE_DEVICES`.
///
/// Returns a `CudaInfo` struct where:
/// * `version` is `None` if the driver version cannot be determined
/// * `arch_info` is `None` if no devices are present or device queries fail
fn detect_cuda_info_via_nvml() -> DetectedCudaInfo {
    let mut library = None;
    for path in nvml_library_paths() {
        match unsafe { Library::new(*path) } {
            Ok(loaded) => {
                tracing::trace!(library_path = *path, "loaded NVML library");
                library = Some(loaded);
                break;
            }
            Err(err) => {
                tracing::trace!(library_path = *path, error = %err, "failed to load NVML library");
            }
        }
    }

    let Some(library) = library else {
        tracing::debug!("could not load NVML library from any known path");
        return DetectedCudaInfo {
            info: CudaInfo {
                version: None,
                arch_info: None,
            },
            sources: CudaInfoSources::default(),
        };
    };

    // Try the cheap no-init query first. Some drivers allow it, but NVML does not consistently
    // guarantee that across platforms/versions, so retry after init only for that specific error.
    let (version, version_source, retry_version_after_init) = match cuda_version_from_nvml_library(
        &library,
    ) {
        Ok(version) => {
            tracing::trace!(%version, "detected CUDA driver version via NVML without init");
            (Some(version), Some(CudaDetectionMethod::NvmlNoInit), false)
        }
        Err(err) => {
            let retry_after_init = err.should_retry_after_init();
            if retry_after_init {
                tracing::debug!(
                    "NVML CUDA driver version query requires initialization; retrying after nvmlInit"
                );
            } else {
                tracing::trace!(
                    ?err,
                    "CUDA driver version query via NVML without init failed"
                );
            }
            (None, None, retry_after_init)
        }
    };

    // Compute capability requires enumerating devices, which needs NVML to be initialized. If the
    // no-init version query failed with NVML_ERROR_UNINITIALIZED, retry it while NVML is initialized.
    let (initialized_version, arch_info) =
        detect_cuda_initialized_info_via_nvml(&library, retry_version_after_init, true);

    let initialized_version_source = initialized_version
        .as_ref()
        .map(|_| CudaDetectionMethod::NvmlInitialized);
    let arch_source = arch_info
        .as_ref()
        .map(|_| CudaDetectionMethod::NvmlInitialized);

    DetectedCudaInfo {
        info: CudaInfo {
            version: version.or(initialized_version),
            arch_info,
        },
        sources: CudaInfoSources {
            version: version_source.or(initialized_version_source),
            arch: arch_source,
        },
    }
}

/// Queries the CUDA driver version from an already-loaded NVML library.
///
/// Some drivers allow `nvmlSystemGetCudaDriverVersion` before `nvmlInit`, but others return
/// `NVML_ERROR_UNINITIALIZED`; callers may retry after initializing NVML for that error.
fn cuda_version_from_nvml_library(library: &Library) -> Result<Version, NvmlCudaVersionError> {
    // Find the `nvmlSystemGetCudaDriverVersion_v2` function. If that function cannot be found, fall
    // back to the `nvmlSystemGetCudaDriverVersion` function instead.
    let nvml_system_get_cuda_driver_version: Symbol<'_, unsafe extern "C" fn(*mut c_int) -> c_int> =
        unsafe {
            library
                .get(b"nvmlSystemGetCudaDriverVersion_v2\0")
                .or_else(|_| library.get(b"nvmlSystemGetCudaDriverVersion\0"))
        }
        .map_err(|_err| NvmlCudaVersionError::MissingSymbol)?;

    let mut cuda_driver_version = MaybeUninit::uninit();
    let result = unsafe { nvml_system_get_cuda_driver_version(cuda_driver_version.as_mut_ptr()) };
    if result != NVML_SUCCESS {
        return Err(NvmlCudaVersionError::Nvml(result));
    }
    let version = unsafe { cuda_driver_version.assume_init() };

    // Convert the version integer to a version string
    Version::from_str(&format!("{}.{}", version / 1000, (version % 1000) / 10))
        .map_err(|_err| NvmlCudaVersionError::InvalidVersion)
}

/// Queries information that requires initialized NVML.
///
/// Returns `(version, arch_info)`. Each field is only queried when the corresponding `query_*`
/// argument is true. The initialized version query is a compatibility fallback for drivers that
/// reject `nvmlSystemGetCudaDriverVersion` before `nvmlInit`.
fn detect_cuda_initialized_info_via_nvml(
    library: &Library,
    query_version: bool,
    query_arch: bool,
) -> (Option<Version>, Option<CudaArchInfo>) {
    // NVML device handle (`nvmlDevice_t`) is an opaque pointer.
    type NvmlDevice = *mut c_void;

    let Some(nvml_init): Option<Symbol<'_, unsafe extern "C" fn() -> c_int>> = (unsafe {
        library
            .get(b"nvmlInit_v2\0")
            .or_else(|_| library.get(b"nvmlInit\0"))
            .ok()
    }) else {
        tracing::debug!("missing nvmlInit symbol");
        return (None, None);
    };

    let Some(nvml_shutdown): Option<Symbol<'_, unsafe extern "C" fn() -> c_int>> =
        (unsafe { library.get(b"nvmlShutdown\0").ok() })
    else {
        tracing::debug!("missing nvmlShutdown symbol");
        return (None, None);
    };

    tracing::trace!(query_version, query_arch, "initializing NVML");
    let init_result = unsafe { nvml_init() };
    if init_result != NVML_SUCCESS {
        tracing::debug!(return_code = init_result, "nvmlInit failed");
        return (None, None);
    }

    let version = if query_version {
        match cuda_version_from_nvml_library(library) {
            Ok(version) => {
                tracing::debug!(%version, "detected CUDA driver version via initialized NVML");
                Some(version)
            }
            Err(err) => {
                tracing::debug!(
                    ?err,
                    "CUDA driver version query via initialized NVML failed"
                );
                None
            }
        }
    } else {
        None
    };

    // Enumerate devices to find the minimum compute capability. Wrapped in a closure so we always
    // reach the `nvmlShutdown` call below regardless of the outcome.
    let arch_info = query_arch
        .then(|| {
            tracing::trace!("querying CUDA compute capability via NVML");
            let nvml_device_get_count: Symbol<'_, unsafe extern "C" fn(*mut c_uint) -> c_int> =
                match unsafe {
                    library
                        .get(b"nvmlDeviceGetCount_v2\0")
                        .or_else(|_| library.get(b"nvmlDeviceGetCount\0"))
                } {
                    Ok(symbol) => symbol,
                    Err(err) => {
                        tracing::trace!(error = %err, "missing NVML device count symbol");
                        return None;
                    }
                };

            let nvml_device_get_handle_by_index: Symbol<
                '_,
                unsafe extern "C" fn(c_uint, *mut NvmlDevice) -> c_int,
            > = match unsafe {
                library
                    .get(b"nvmlDeviceGetHandleByIndex_v2\0")
                    .or_else(|_| library.get(b"nvmlDeviceGetHandleByIndex\0"))
            } {
                Ok(symbol) => symbol,
                Err(err) => {
                    tracing::trace!(error = %err, "missing NVML device handle symbol");
                    return None;
                }
            };

            let nvml_device_get_cuda_compute_capability: Symbol<
                '_,
                unsafe extern "C" fn(NvmlDevice, *mut c_int, *mut c_int) -> c_int,
            > = match unsafe { library.get(b"nvmlDeviceGetCudaComputeCapability\0") } {
                Ok(symbol) => symbol,
                Err(err) => {
                    tracing::trace!(
                        error = %err,
                        "missing NVML CUDA compute capability symbol"
                    );
                    return None;
                }
            };

            let mut device_count = MaybeUninit::uninit();
            let device_count_result = unsafe { nvml_device_get_count(device_count.as_mut_ptr()) };
            if device_count_result != NVML_SUCCESS {
                tracing::trace!(
                    return_code = device_count_result,
                    "nvmlDeviceGetCount failed"
                );
                return None;
            }
            let device_count = unsafe { device_count.assume_init() };
            tracing::trace!(device_count, "enumerating CUDA devices via NVML");

            let mut min_arch: Option<CudaArchInfo> = None;
            for device_idx in 0..device_count {
                let mut device: NvmlDevice = ptr::null_mut();
                let handle_result =
                    unsafe { nvml_device_get_handle_by_index(device_idx, &mut device) };
                if handle_result != NVML_SUCCESS {
                    tracing::trace!(
                        device_idx,
                        return_code = handle_result,
                        "failed to get NVML device handle"
                    );
                    continue;
                }

                let mut cc_major = MaybeUninit::uninit();
                let mut cc_minor = MaybeUninit::uninit();
                let compute_capability_result = unsafe {
                    nvml_device_get_cuda_compute_capability(
                        device,
                        cc_major.as_mut_ptr(),
                        cc_minor.as_mut_ptr(),
                    )
                };
                if compute_capability_result != NVML_SUCCESS {
                    tracing::trace!(
                        device_idx,
                        return_code = compute_capability_result,
                        "failed to get CUDA compute capability via NVML"
                    );
                    continue;
                }
                let cc_major = unsafe { cc_major.assume_init() } as u32;
                let cc_minor = unsafe { cc_minor.assume_init() } as u32;
                tracing::trace!(
                    device_idx,
                    major = cc_major,
                    minor = cc_minor,
                    "detected CUDA compute capability via NVML"
                );

                let is_new_minimum = min_arch.as_ref().is_none_or(|min| {
                    cc_major < min.major || (cc_major == min.major && cc_minor < min.minor)
                });
                if is_new_minimum {
                    min_arch = Some(CudaArchInfo {
                        major: cc_major,
                        minor: cc_minor,
                    });
                }
            }
            if let Some(arch) = min_arch.as_ref() {
                tracing::debug!(
                    major = arch.major,
                    minor = arch.minor,
                    "selected minimum CUDA compute capability"
                );
            } else {
                tracing::debug!("no CUDA compute capability detected via NVML");
            }
            min_arch
        })
        .flatten();

    // Whatever happens, after initializing NVML we have to call `nvmlShutdown`.
    let shutdown_result = unsafe { nvml_shutdown() };
    if shutdown_result != NVML_SUCCESS {
        tracing::debug!(return_code = shutdown_result, "nvmlShutdown failed");
    }

    (version, arch_info)
}

/// Attempts to detect the version of CUDA present in the current operating system by employing the
/// best technique available for the current environment.
pub fn detect_cuda_version() -> Option<Version> {
    if cfg!(target_env = "musl") {
        // Dynamically loading a library is not supported on musl so we have to fall-back to using
        // the nvidia-smi command.
        detect_cuda_version_via_nvidia_smi()
    } else {
        detect_cuda_version_via_nvml().or_else(|| {
            tracing::debug!(
                "NVML did not detect a CUDA driver version; trying libcuda/nvidia-smi fallbacks"
            );
            detect_cuda_version_fallbacks().map(|(version, _source)| version)
        })
    }
}

fn detect_cuda_version_fallbacks() -> Option<(Version, CudaDetectionMethod)> {
    if cfg!(target_env = "musl") {
        return detect_cuda_version_via_nvidia_smi()
            .map(|version| (version, CudaDetectionMethod::NvidiaSmi));
    }

    let version = detect_cuda_version_via_libcuda();
    if let Some(version) = version {
        return Some((version, CudaDetectionMethod::Libcuda));
    }

    tracing::debug!("libcuda did not detect a CUDA driver version; trying nvidia-smi fallback");
    detect_cuda_version_via_nvidia_smi().map(|version| (version, CudaDetectionMethod::NvidiaSmi))
}

/// Attempts to detect the version of CUDA present in the current operating system by loading the
/// NVIDIA Management Library and querying the CUDA driver version. The method is preferred over
/// [`detect_cuda_version_via_libcuda`] because that method might fail base on environment
/// variables.
///
/// Although the required methods in the runtime are not implemented on much older machines it is
/// considered old enough to be usable for our use case. Since Conda doesn't provide old versions of
/// the CUDA SDK anyway this is considered a non-issue.
///
/// Some drivers can answer `nvmlSystemGetCudaDriverVersion` without `nvmlInit`, avoiding the
/// expensive driver handshake / GPU attach that makes `nvmlInit` slow on Windows. If that no-init
/// query fails, this falls back to querying while NVML is initialized.
pub fn detect_cuda_version_via_nvml() -> Option<Version> {
    // Try to open the library
    let mut library = None;
    for path in nvml_library_paths() {
        match unsafe { libloading::Library::new(*path) } {
            Ok(loaded) => {
                tracing::trace!(library_path = *path, "loaded NVML library");
                library = Some(loaded);
                break;
            }
            Err(err) => {
                tracing::trace!(library_path = *path, error = %err, "failed to load NVML library");
            }
        }
    }
    let library = library?;

    match cuda_version_from_nvml_library(&library) {
        Ok(version) => {
            tracing::trace!(%version, "detected CUDA driver version via NVML without init");
            Some(version)
        }
        Err(err) if err.should_retry_after_init() => {
            tracing::debug!(?err, "retrying CUDA driver version query after nvmlInit");
            detect_cuda_initialized_info_via_nvml(&library, true, false).0
        }
        Err(err) => {
            tracing::debug!(?err, "CUDA driver version query via NVML failed");
            None
        }
    }
}

/// Returns platform specific set of search paths for the CUDA library.
///
/// On Windows and Linux, the nvml library is installed by the NVIDIA driver package, and is
/// typically found in the standard library path, rather than with the CUDA SDK (which is optional
/// for running CUDA apps).
///
/// On macOS, the CUDA library is only installed with the CUDA SDK, and might not be in the library
/// path.
fn nvml_library_paths() -> &'static [&'static str] {
    #[cfg(target_os = "macos")]
    static FILENAMES: &[&str] = &[
        "libnvidia-ml.1.dylib", // Check library path first
        "libnvidia-ml.dylib",
        "/usr/local/cuda/lib/libnvidia-ml.1.dylib",
        "/usr/local/cuda/lib/libnvidia-ml.dylib",
    ];
    #[cfg(target_os = "linux")]
    static FILENAMES: &[&str] = &[
        "libnvidia-ml.so.1", // Check library path first
        "libnvidia-ml.so",
        "/usr/lib64/nvidia/libnvidia-ml.so.1", // RHEL/Centos/Fedora
        "/usr/lib64/nvidia/libnvidia-ml.so",
        "/usr/lib/x86_64-linux-gnu/libnvidia-ml.so.1", // Ubuntu
        "/usr/lib/x86_64-linux-gnu/libnvidia-ml.so",
        "/usr/lib/wsl/lib/libnvidia-ml.so.1", // WSL
        "/usr/lib/wsl/lib/libnvidia-ml.so",
    ];
    #[cfg(windows)]
    static FILENAMES: &[&str] = &["nvml.dll"];
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    static FILENAMES: &[&str] = &[];
    FILENAMES
}

/// Attempts to detect the version of CUDA present in the current operating system by loading the
/// cuda runtime library and querying the CUDA driver version.
///
/// The behavior of functions from `libcuda` depend on the environment variable
/// `CUDA_VISIBLE_DEVICES`. If users have this variable set in their environment this function will
/// likely not return the correct value.
///
/// Therefore you should use the function [`detect_cuda_version_via_nvml`] instead which does not
/// have this limitation.
pub fn detect_cuda_version_via_libcuda() -> Option<Version> {
    // Try to open the library
    let mut cuda_library = None;
    for path in cuda_library_paths() {
        match unsafe { libloading::Library::new(*path) } {
            Ok(loaded) => {
                tracing::trace!(library_path = *path, "loaded CUDA driver library");
                cuda_library = Some(loaded);
                break;
            }
            Err(err) => {
                tracing::trace!(
                    library_path = *path,
                    error = %err,
                    "failed to load CUDA driver library"
                );
            }
        }
    }
    let cuda_library = cuda_library?;

    // Get entry points from the library
    let cu_init: Symbol<'_, unsafe extern "C" fn(c_uint) -> c_ulong> =
        match unsafe { cuda_library.get(b"cuInit\0") } {
            Ok(symbol) => symbol,
            Err(err) => {
                tracing::debug!(error = %err, "missing cuInit symbol");
                return None;
            }
        };
    let cu_driver_get_version: Symbol<'_, unsafe extern "C" fn(*mut c_int) -> c_ulong> =
        match unsafe { cuda_library.get(b"cuDriverGetVersion\0") } {
            Ok(symbol) => symbol,
            Err(err) => {
                tracing::debug!(error = %err, "missing cuDriverGetVersion symbol");
                return None;
            }
        };

    // Initialize the CUDA library
    let init_result = unsafe { cu_init(0) };
    if init_result != 0 {
        tracing::debug!(return_code = init_result, "cuInit failed");
        return None;
    }

    // Get the version from the library
    let mut version_int = MaybeUninit::uninit();
    let version_result = unsafe { cu_driver_get_version(version_int.as_mut_ptr()) };
    if version_result != 0 {
        tracing::debug!(return_code = version_result, "cuDriverGetVersion failed");
        return None;
    }
    let version = unsafe { version_int.assume_init() };

    // Convert the version integer to a version string
    let version = Version::from_str(&format!("{}.{}", version / 1000, (version % 1000) / 10)).ok();
    if let Some(version) = &version {
        tracing::trace!(%version, "detected CUDA driver version via libcuda");
    } else {
        tracing::trace!("failed to parse CUDA driver version reported by libcuda");
    }
    version
}

/// Returns platform specific set of search paths for the CUDA library.
///
/// On Windows and Linux, the CUDA library is installed by the NVIDIA driver package, and is
/// typically found in the standard library path, rather than with the CUDA SDK (which is optional
/// for running CUDA apps).
///
/// On macOS, the CUDA library is only installed with the CUDA SDK, and might not be in the library
/// path.
fn cuda_library_paths() -> &'static [&'static str] {
    #[cfg(target_os = "macos")]
    static FILENAMES: &[&str] = &[
        "libcuda.1.dylib", // Check library path first
        "libcuda.dylib",
        "/usr/local/cuda/lib/libcuda.1.dylib",
        "/usr/local/cuda/lib/libcuda.dylib",
    ];
    #[cfg(target_os = "linux")]
    static FILENAMES: &[&str] = &[
        "libcuda.so.1", // Check library path first
        "libcuda.so",
        "/usr/lib64/nvidia/libcuda.so.1", // RHEL/Centos/Fedora
        "/usr/lib64/nvidia/libcuda.so",
        "/usr/lib/x86_64-linux-gnu/libcuda.so.1", // Ubuntu
        "/usr/lib/x86_64-linux-gnu/libcuda.so",
        "/usr/lib/wsl/lib/libcuda.so.1", // WSL
        "/usr/lib/wsl/lib/libcuda.so",
    ];
    #[cfg(windows)]
    static FILENAMES: &[&str] = &["nvcuda.dll"];
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    static FILENAMES: &[&str] = &[];
    FILENAMES
}

/// Attempts to detect the version of CUDA present in the current operating system by executing the
/// "nvidia-smi" command and extracting the CUDA driver version from it.
///
/// The behavior of "nvidia-smi" depends on the environment variable `CUDA_VISIBLE_DEVICES`. If
/// users have this variable set in their environment this function will likely not return the
/// correct value. To ensure a consistent response this environment variable is unset when invoking
/// the command.
///
/// The upside of using this detection function over any of the others is that this method does not
/// dynamically load a library which might not be supported on all systems. The downside is that
/// executing a subprocess is generally slower and more prone to errors.
fn detect_cuda_version_via_nvidia_smi() -> Option<Version> {
    static CUDA_VERSION_RE: once_cell::sync::Lazy<regex::Regex> =
        once_cell::sync::Lazy::new(|| {
            regex::Regex::new("<cuda_version>(.*)<\\/cuda_version>").unwrap()
        });

    tracing::trace!("detecting CUDA driver version via nvidia-smi");
    // Invoke the "nvidia-smi" command to query the driver version that is usually installed when
    // Cuda drivers are installed.
    let nvidia_smi_output = match Command::new("nvidia-smi")
        // Display GPU or unit info
        .arg("--query")
        // Show unit, rather than GPU, attributes
        .arg("-u")
        // Produce XML output.
        .arg("-x")
        // The behavior of functions from `libcuda` depend on the environment variable
        // `CUDA_VISIBLE_DEVICES`. If users have this variable set in their environment this
        // function will likely not return the correct value. Therefor, we remove this variable
        // to ensure a consistent result.
        // TODO: Is this really the proper way to do it? Should we maybe clear the entire
        // environment.
        .env_remove("CUDA_VISIBLE_DEVICES")
        .output()
    {
        Ok(output) => output,
        Err(err) => {
            tracing::debug!(error = %err, "failed to run nvidia-smi for CUDA driver version");
            return None;
        }
    };

    if !nvidia_smi_output.status.success() {
        tracing::debug!(
            status = %nvidia_smi_output.status,
            stderr = %String::from_utf8_lossy(&nvidia_smi_output.stderr),
            "nvidia-smi CUDA driver version query failed"
        );
        return None;
    }

    // Convert the output to Utf8. The conversion is lossy so it might contain some illegal
    // characters. If that is the case we simply assume the version in the file also wont make sense
    // during parsing.
    let output = String::from_utf8_lossy(&nvidia_smi_output.stdout);

    // Extract the version from the XML
    let Some(version_match) = CUDA_VERSION_RE.captures(&output) else {
        tracing::trace!("nvidia-smi output did not contain a CUDA driver version");
        return None;
    };
    let Some(version_match) = version_match.get(1) else {
        tracing::trace!("nvidia-smi CUDA driver version match was empty");
        return None;
    };
    let version_str = version_match.as_str();

    // Parse and return
    match Version::from_str(version_str) {
        Ok(version) => {
            tracing::trace!(%version, "detected CUDA driver version via nvidia-smi");
            Some(version)
        }
        Err(err) => {
            tracing::trace!(version = version_str, error = %err, "failed to parse nvidia-smi CUDA driver version");
            None
        }
    }
}

/// Attempts to detect the CUDA compute capability by executing the "nvidia-smi" command and
/// querying the `compute_cap` field of every GPU, returning the **minimum** across all devices.
///
/// Like [`detect_cuda_version_via_nvidia_smi`] this does not dynamically load a library and thus
/// also works on musl systems. The `compute_cap` query field requires a reasonably modern driver
/// (roughly R510+); on older drivers the command fails and `None` is returned.
fn detect_cuda_arch_via_nvidia_smi() -> Option<CudaArchInfo> {
    tracing::trace!("detecting CUDA compute capability via nvidia-smi");
    let nvidia_smi_output = match Command::new("nvidia-smi")
        // Query the compute capability of every GPU as plain CSV, one line per GPU.
        .arg("--query-gpu=compute_cap")
        .arg("--format=csv,noheader")
        // See `detect_cuda_version_via_nvidia_smi` for why this variable is removed.
        .env_remove("CUDA_VISIBLE_DEVICES")
        .output()
    {
        Ok(output) => output,
        Err(err) => {
            tracing::debug!(error = %err, "failed to run nvidia-smi for CUDA compute capability");
            return None;
        }
    };

    // On drivers that do not support the `compute_cap` field the command exits with an error.
    if !nvidia_smi_output.status.success() {
        tracing::debug!(
            status = %nvidia_smi_output.status,
            stderr = %String::from_utf8_lossy(&nvidia_smi_output.stderr),
            "nvidia-smi CUDA compute capability query failed"
        );
        return None;
    }

    let output = String::from_utf8_lossy(&nvidia_smi_output.stdout);

    // Find the minimum compute capability across all devices
    let mut min_arch: Option<CudaArchInfo> = None;
    for (device_idx, line) in output.lines().enumerate() {
        let line = line.trim();
        let Some((major, minor)) = line.split_once('.') else {
            tracing::trace!(
                device_idx,
                line,
                "ignoring invalid nvidia-smi compute capability line"
            );
            continue;
        };
        let (Ok(major), Ok(minor)) = (major.parse::<u32>(), minor.parse::<u32>()) else {
            tracing::trace!(
                device_idx,
                line,
                "ignoring unparsable nvidia-smi compute capability line"
            );
            continue;
        };
        tracing::trace!(
            device_idx,
            major,
            minor,
            "detected CUDA compute capability via nvidia-smi"
        );

        let is_new_minimum = min_arch
            .as_ref()
            .is_none_or(|min| major < min.major || (major == min.major && minor < min.minor));
        if is_new_minimum {
            min_arch = Some(CudaArchInfo { major, minor });
        }
    }

    if let Some(arch) = min_arch.as_ref() {
        tracing::debug!(
            major = arch.major,
            minor = arch.minor,
            "selected minimum CUDA compute capability from nvidia-smi"
        );
    } else {
        tracing::debug!("no CUDA compute capability detected via nvidia-smi");
    }

    min_arch
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    pub fn doesnt_crash() {
        let version = detect_cuda_version_via_nvml();
        println!("Cuda {version:?}");
    }

    #[test]
    pub fn doesnt_crash_nvidia_smi() {
        let version = detect_cuda_version_via_nvidia_smi();
        println!("Cuda {version:?}");
    }

    #[test]
    pub fn doesnt_crash_nvidia_smi_arch() {
        let arch = detect_cuda_arch_via_nvidia_smi();
        println!("Cuda arch {arch:?}");
    }

    #[test]
    pub fn test_cuda_info() {
        let info = cuda_info(None);
        println!("CUDA Info: {info:?}");
        if let Some(ref arch) = info.arch_info {
            println!("  Compute capability: {}.{}", arch.major, arch.minor);
        }
    }

    #[test]
    pub fn test_cuda_arch() {
        let arch = cuda_arch(None);
        println!("CUDA Arch: {arch:?}");
    }

    #[test]
    fn test_cache_roundtrip() {
        let dir = tempfile::tempdir().unwrap();

        // Nothing cached yet
        assert!(cache::read(dir.path()).is_none());

        // Negative results are not cached
        cache::write(
            dir.path(),
            &CudaInfo {
                version: None,
                arch_info: None,
            },
            CudaInfoSources::default(),
        );
        assert!(cache::read(dir.path()).is_none());

        let info = CudaInfo {
            version: Some(Version::from_str("12.4").unwrap()),
            arch_info: Some(CudaArchInfo { major: 8, minor: 6 }),
        };
        let sources = CudaInfoSources {
            version: Some(CudaDetectionMethod::NvmlInitialized),
            arch: Some(CudaDetectionMethod::NvidiaSmi),
        };
        cache::write(dir.path(), &info, sources);

        if cache::BootId::current().is_none() {
            // Cannot key the cache without a boot id on this platform
            return;
        }
        let cached = cache::read(dir.path()).unwrap();
        assert_eq!(cached.info.version, info.version);
        assert_eq!(cached.info.arch_info, info.arch_info);
        assert_eq!(cached.sources, sources);

        // Updating the cache should atomically replace the previous file.
        let updated_info = CudaInfo {
            version: Some(Version::from_str("12.5").unwrap()),
            arch_info: None,
        };
        let updated_sources = CudaInfoSources {
            version: Some(CudaDetectionMethod::Libcuda),
            arch: None,
        };
        cache::write(dir.path(), &updated_info, updated_sources);
        let cached = cache::read(dir.path()).unwrap();
        assert_eq!(cached.info.version, updated_info.version);
        assert_eq!(cached.info.arch_info, updated_info.arch_info);
        assert_eq!(cached.sources, updated_sources);
    }

    #[test]
    fn test_cache_invalidated_after_driver_change() {
        let dir = tempfile::tempdir().unwrap();
        cache::write(
            dir.path(),
            &CudaInfo {
                version: Some(Version::from_str("12.4").unwrap()),
                arch_info: None,
            },
            CudaInfoSources {
                version: Some(CudaDetectionMethod::NvmlNoInit),
                arch: None,
            },
        );
        let path = dir.path().join("cuda-info-v1.json");
        let Ok(content) = std::fs::read_to_string(&path) else {
            // Nothing was cached because this platform has no boot id
            return;
        };

        // A cache file written with a different driver installed is ignored
        let mut cached: serde_json::Value = serde_json::from_str(&content).unwrap();
        cached["driver_fingerprint"] = "definitely-stale".into();
        std::fs::write(&path, serde_json::to_string(&cached).unwrap()).unwrap();
        assert!(cache::read(dir.path()).is_none());
    }

    #[test]
    fn test_cache_invalidated_after_reboot() {
        let dir = tempfile::tempdir().unwrap();

        // A cache file from a different boot session is ignored
        std::fs::write(
            dir.path().join("cuda-info-v1.json"),
            r#"{"boot_id":"not-the-current-boot","driver_fingerprint":null,"version":"12.4","arch":[8,6]}"#,
        )
        .unwrap();
        assert!(cache::read(dir.path()).is_none());
    }

    #[test]
    fn test_is_valid_cuda_version_format() {
        // Valid formats
        assert!(is_valid_cuda_version_format("8.6"));
        assert!(is_valid_cuda_version_format("7.5"));
        assert!(is_valid_cuda_version_format("10.2"));
        assert!(is_valid_cuda_version_format("0.0"));
        assert!(is_valid_cuda_version_format("12.0"));

        // Invalid formats - not major.minor
        assert!(!is_valid_cuda_version_format("8"));
        assert!(!is_valid_cuda_version_format("8.6.1"));
        assert!(!is_valid_cuda_version_format("8.6.1.0"));
        assert!(!is_valid_cuda_version_format(""));
        assert!(!is_valid_cuda_version_format(".6"));
        assert!(!is_valid_cuda_version_format("8."));
        assert!(!is_valid_cuda_version_format("."));

        // Invalid formats - non-digit characters
        assert!(!is_valid_cuda_version_format("8.6a"));
        assert!(!is_valid_cuda_version_format("a.6"));
        assert!(!is_valid_cuda_version_format("8.b"));
        assert!(!is_valid_cuda_version_format("eight.six"));
        assert!(!is_valid_cuda_version_format("8-6"));
        assert!(!is_valid_cuda_version_format("8_6"));
    }
}
