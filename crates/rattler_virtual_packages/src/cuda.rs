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
use std::process::Command;
use std::{
    mem::MaybeUninit,
    os::raw::{c_int, c_uint, c_ulong, c_void},
    ptr,
    str::FromStr,
};

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

/// Returns comprehensive CUDA information from the current platform.
///
/// This function returns both the CUDA driver version and compute capability information
/// in a single cached result. The detection is performed only once per process and the
/// result is cached for subsequent calls.
///
/// This is more efficient than calling [`cuda_version`] and [`cuda_arch`] separately
/// because the CUDA library is loaded only once.
pub fn cuda_info() -> &'static CudaInfo {
    static DETECTED_CUDA_INFO: OnceCell<CudaInfo> = OnceCell::new();
    DETECTED_CUDA_INFO.get_or_init(detect_cuda_info)
}

/// Returns the maximum CUDA version available on the current platform.
///
/// This corresponds to the `__cuda` virtual package. The result is cached,
/// so subsequent calls are very fast.
pub fn cuda_version() -> Option<Version> {
    cuda_info().version.clone()
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
/// The result is cached, so subsequent calls are very fast.
pub fn cuda_arch() -> Option<CudaArchInfo> {
    cuda_info().arch_info.clone()
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
fn detect_cuda_info() -> CudaInfo {
    if cfg!(target_env = "musl") {
        // Dynamically loading a library is not supported on musl so we have to fall-back to using
        // the nvidia-smi command.
        CudaInfo {
            version: detect_cuda_version_via_nvidia_smi(),
            arch_info: detect_cuda_arch_via_nvidia_smi(),
        }
    } else {
        // Detect via NVML, which gives us both version and architecture info from a single library
        // and, unlike libcuda, is not affected by `CUDA_VISIBLE_DEVICES`.
        detect_cuda_info_via_nvml()
    }
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
fn detect_cuda_info_via_nvml() -> CudaInfo {
    let Some(library) = nvml_library_paths()
        .iter()
        .find_map(|path| unsafe { Library::new(*path).ok() })
    else {
        return CudaInfo {
            version: None,
            arch_info: None,
        };
    };

    // The version can be queried without initializing NVML and even without any devices present.
    let version = cuda_version_from_nvml_library(&library);

    // Compute capability requires enumerating devices, which needs NVML to be initialized.
    let arch_info = detect_cuda_arch_via_nvml(&library);

    CudaInfo { version, arch_info }
}

/// Queries the CUDA driver version from an already-loaded NVML library.
///
/// `nvmlSystemGetCudaDriverVersion` reads the version straight from the CUDA driver library and
/// does not require `nvmlInit`.
fn cuda_version_from_nvml_library(library: &Library) -> Option<Version> {
    // Find the `nvmlSystemGetCudaDriverVersion_v2` function. If that function cannot be found, fall
    // back to the `nvmlSystemGetCudaDriverVersion` function instead.
    let nvml_system_get_cuda_driver_version: Symbol<'_, unsafe extern "C" fn(*mut c_int) -> c_int> =
        unsafe {
            library
                .get(b"nvmlSystemGetCudaDriverVersion_v2\0")
                .or_else(|_| library.get(b"nvmlSystemGetCudaDriverVersion\0"))
        }
        .ok()?;

    let mut cuda_driver_version = MaybeUninit::uninit();
    if unsafe { nvml_system_get_cuda_driver_version(cuda_driver_version.as_mut_ptr()) } != 0 {
        return None;
    }
    let version = unsafe { cuda_driver_version.assume_init() };

    // Convert the version integer to a version string
    Version::from_str(&format!("{}.{}", version / 1000, (version % 1000) / 10)).ok()
}

/// Detects CUDA compute capability from an already-loaded NVML library.
///
/// Initializes NVML, enumerates all devices, and returns the **minimum** compute capability across
/// them. Unlike the version query, reading device compute capabilities requires NVML to be
/// initialized (with the GPUs attached).
///
/// Returns `None` if NVML cannot be initialized, no devices are present, or device queries fail.
fn detect_cuda_arch_via_nvml(library: &Library) -> Option<CudaArchInfo> {
    // NVML device handle (`nvmlDevice_t`) is an opaque pointer.
    type NvmlDevice = *mut c_void;

    let nvml_init: Symbol<'_, unsafe extern "C" fn() -> c_int> = unsafe {
        library
            .get(b"nvmlInit_v2\0")
            .or_else(|_| library.get(b"nvmlInit\0"))
    }
    .ok()?;

    let nvml_shutdown: Symbol<'_, unsafe extern "C" fn() -> c_int> =
        unsafe { library.get(b"nvmlShutdown\0") }.ok()?;

    let nvml_device_get_count: Symbol<'_, unsafe extern "C" fn(*mut c_uint) -> c_int> = unsafe {
        library
            .get(b"nvmlDeviceGetCount_v2\0")
            .or_else(|_| library.get(b"nvmlDeviceGetCount\0"))
    }
    .ok()?;

    let nvml_device_get_handle_by_index: Symbol<
        '_,
        unsafe extern "C" fn(c_uint, *mut NvmlDevice) -> c_int,
    > = unsafe {
        library
            .get(b"nvmlDeviceGetHandleByIndex_v2\0")
            .or_else(|_| library.get(b"nvmlDeviceGetHandleByIndex\0"))
    }
    .ok()?;

    let nvml_device_get_cuda_compute_capability: Symbol<
        '_,
        unsafe extern "C" fn(NvmlDevice, *mut c_int, *mut c_int) -> c_int,
    > = unsafe { library.get(b"nvmlDeviceGetCudaComputeCapability\0") }.ok()?;

    if unsafe { nvml_init() } != 0 {
        return None;
    }

    // Enumerate devices to find the minimum compute capability. Wrapped in a closure so we always
    // reach the `nvmlShutdown` call below regardless of the outcome.
    let min_arch = (|| {
        let mut device_count = MaybeUninit::uninit();
        if unsafe { nvml_device_get_count(device_count.as_mut_ptr()) } != 0 {
            return None;
        }
        let device_count = unsafe { device_count.assume_init() };

        let mut min_arch: Option<CudaArchInfo> = None;
        for device_idx in 0..device_count {
            let mut device: NvmlDevice = ptr::null_mut();
            if unsafe { nvml_device_get_handle_by_index(device_idx, &mut device) } != 0 {
                continue;
            }

            let mut cc_major = MaybeUninit::uninit();
            let mut cc_minor = MaybeUninit::uninit();
            if unsafe {
                nvml_device_get_cuda_compute_capability(
                    device,
                    cc_major.as_mut_ptr(),
                    cc_minor.as_mut_ptr(),
                )
            } != 0
            {
                continue;
            }
            let cc_major = unsafe { cc_major.assume_init() } as u32;
            let cc_minor = unsafe { cc_minor.assume_init() } as u32;

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
        min_arch
    })();

    // Whatever happens, after initializing NVML we have to call `nvmlShutdown`.
    let _ = unsafe { nvml_shutdown() };

    min_arch
}

/// Attempts to detect the version of CUDA present in the current operating system by employing the
/// best technique available for the current environment.
pub fn detect_cuda_version() -> Option<Version> {
    if cfg!(target_env = "musl") {
        // Dynamically loading a library is not supported on musl so we have to fall-back to using
        // the nvidia-smi command.
        detect_cuda_version_via_nvidia_smi()
    } else {
        detect_cuda_version_via_nvml()
    }
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
/// `nvmlSystemGetCudaDriverVersion` reads the version straight from the CUDA driver library and,
/// unlike most NVML functions, does not require `nvmlInit`. We therefore skip initialization, which
/// avoids the expensive driver handshake / GPU attach that makes `nvmlInit` slow on Windows.
pub fn detect_cuda_version_via_nvml() -> Option<Version> {
    // Try to open the library
    let library = nvml_library_paths()
        .iter()
        .find_map(|path| unsafe { libloading::Library::new(*path).ok() })?;

    cuda_version_from_nvml_library(&library)
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
    let cuda_library = cuda_library_paths()
        .iter()
        .find_map(|path| unsafe { libloading::Library::new(*path).ok() })?;

    // Get entry points from the library
    let cu_init: Symbol<'_, unsafe extern "C" fn(c_uint) -> c_ulong> =
        unsafe { cuda_library.get(b"cuInit\0") }.ok()?;
    let cu_driver_get_version: Symbol<'_, unsafe extern "C" fn(*mut c_int) -> c_ulong> =
        unsafe { cuda_library.get(b"cuDriverGetVersion\0") }.ok()?;

    // Initialize the CUDA library
    if unsafe { cu_init(0) } != 0 {
        return None;
    }

    // Get the version from the library
    let mut version_int = MaybeUninit::uninit();
    if unsafe { cu_driver_get_version(version_int.as_mut_ptr()) != 0 } {
        return None;
    }
    let version = unsafe { version_int.assume_init() };

    // Convert the version integer to a version string
    Version::from_str(&format!("{}.{}", version / 1000, (version % 1000) / 10)).ok()
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

    // Invoke the "nvidia-smi" command to query the driver version that is usually installed when
    // Cuda drivers are installed.
    let nvidia_smi_output = Command::new("nvidia-smi")
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
        .ok()?;

    // Convert the output to Utf8. The conversion is lossy so it might contain some illegal
    // characters. If that is the case we simply assume the version in the file also wont make sense
    // during parsing.
    let output = String::from_utf8_lossy(&nvidia_smi_output.stdout);

    // Extract the version from the XML
    let version_match = CUDA_VERSION_RE.captures(&output)?;
    let version_str = version_match.get(1)?.as_str();

    // Parse and return
    Version::from_str(version_str).ok()
}

/// Attempts to detect the CUDA compute capability by executing the "nvidia-smi" command and
/// querying the `compute_cap` field of every GPU, returning the **minimum** across all devices.
///
/// Like [`detect_cuda_version_via_nvidia_smi`] this does not dynamically load a library and thus
/// also works on musl systems. The `compute_cap` query field requires a reasonably modern driver
/// (roughly R510+); on older drivers the command fails and `None` is returned.
fn detect_cuda_arch_via_nvidia_smi() -> Option<CudaArchInfo> {
    let nvidia_smi_output = Command::new("nvidia-smi")
        // Query the compute capability of every GPU as plain CSV, one line per GPU.
        .arg("--query-gpu=compute_cap")
        .arg("--format=csv,noheader")
        // See `detect_cuda_version_via_nvidia_smi` for why this variable is removed.
        .env_remove("CUDA_VISIBLE_DEVICES")
        .output()
        .ok()?;

    // On drivers that do not support the `compute_cap` field the command exits with an error.
    if !nvidia_smi_output.status.success() {
        return None;
    }

    let output = String::from_utf8_lossy(&nvidia_smi_output.stdout);

    // Find the minimum compute capability across all devices
    let mut min_arch: Option<CudaArchInfo> = None;
    for line in output.lines() {
        let line = line.trim();
        let Some((major, minor)) = line.split_once('.') else {
            continue;
        };
        let (Ok(major), Ok(minor)) = (major.parse::<u32>(), minor.parse::<u32>()) else {
            continue;
        };

        let is_new_minimum = min_arch
            .as_ref()
            .is_none_or(|min| major < min.major || (major == min.major && minor < min.minor));
        if is_new_minimum {
            min_arch = Some(CudaArchInfo { major, minor });
        }
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
        let info = cuda_info();
        println!("CUDA Info: {info:?}");
        if let Some(ref arch) = info.arch_info {
            println!("  Compute capability: {}.{}", arch.major, arch.minor);
        }
    }

    #[test]
    pub fn test_cuda_arch() {
        let arch = cuda_arch();
        println!("CUDA Arch: {arch:?}");
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
