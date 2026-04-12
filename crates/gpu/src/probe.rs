//! Hardware probe — detect available GPU capabilities at runtime.
//!
//! Uses dlopen to check for CUDA, NVENC, and capture backends without
//! crashing if libraries are missing.

use tracing::{info, warn};

/// Result of GPU hardware probing.
#[derive(Debug, Clone)]
pub struct GpuProbeResult {
    pub has_cuda: bool,
    pub has_nvenc: bool,
    /// Whether NVENC supports AV1 encoding (Ada Lovelace / 8th gen+).
    pub has_av1: bool,
    #[cfg(target_os = "linux")]
    pub has_nvfbc: bool,
    #[cfg(target_os = "windows")]
    pub has_dxgi: bool,
    pub gpu_name: Option<String>,
}

impl GpuProbeResult {
    /// Best encoder based on what's available.
    pub fn best_encoder(&self) -> &'static str {
        if self.has_nvenc {
            "nvenc"
        } else {
            "openh264"
        }
    }

    /// Best video codec based on GPU capabilities.
    /// Prefers AV1 (better compression) when available, falls back to H.264.
    pub fn best_codec(&self) -> &'static str {
        if self.has_av1 {
            "av1"
        } else {
            "h264"
        }
    }

    /// Best capture backend based on what's available.
    pub fn best_capture(&self) -> &'static str {
        #[cfg(target_os = "linux")]
        if self.has_nvfbc {
            return "nvfbc";
        }
        #[cfg(target_os = "windows")]
        if self.has_dxgi {
            return "dxgi";
        }
        "scrap"
    }

    /// Whether a full GPU zero-copy pipeline is available.
    pub fn has_gpu_pipeline(&self) -> bool {
        #[cfg(target_os = "linux")]
        return self.has_nvenc && self.has_nvfbc;
        #[cfg(target_os = "windows")]
        return self.has_nvenc && self.has_dxgi;
        #[cfg(not(any(target_os = "linux", target_os = "windows")))]
        return false;
    }
}

/// Probe for GPU capabilities. Never fails — returns a result with all
/// flags set to false if no GPU is found.
pub fn probe() -> GpuProbeResult {
    let mut result = GpuProbeResult {
        has_cuda: false,
        has_nvenc: false,
        has_av1: false,
        #[cfg(target_os = "linux")]
        has_nvfbc: false,
        #[cfg(target_os = "windows")]
        has_dxgi: false,
        gpu_name: None,
    };

    // Try loading CUDA
    match crate::cuda::CudaLib::load() {
        Ok(cuda) => {
            result.has_cuda = true;
            // Try to get GPU name via cuDeviceGetName
            if let Ok(dev) = cuda.device_get(0) {
                result.gpu_name = get_device_name(&cuda, dev);
            }
        }
        Err(e) => {
            warn!("CUDA not available: {e}");
            return result;
        }
    }

    // Try loading NVENC
    result.has_nvenc = probe_nvenc();

    // Check AV1 support (Ada Lovelace / 8th gen NVENC and newer)
    if result.has_nvenc {
        result.has_av1 = probe_av1_support(result.gpu_name.as_deref());
    }

    // Try loading capture backend
    #[cfg(target_os = "linux")]
    {
        result.has_nvfbc = probe_nvfbc();
    }
    #[cfg(target_os = "windows")]
    {
        result.has_dxgi = probe_dxgi();
    }

    let gpu_label = result.gpu_name.as_deref().unwrap_or("unknown GPU");
    info!(
        gpu = gpu_label,
        encoder = result.best_encoder(),
        capture = result.best_capture(),
        "hardware probe complete"
    );

    result
}

fn probe_nvenc() -> bool {
    #[cfg(unix)]
    let names = &["libnvidia-encode.so.1", "libnvidia-encode.so"];
    #[cfg(windows)]
    let names = &["nvEncodeAPI64.dll", "nvEncodeAPI.dll"];

    match crate::dl::DynLib::open(names) {
        Ok(_lib) => {
            info!("NVENC library found");
            true
        }
        Err(e) => {
            warn!("NVENC not available: {e}");
            false
        }
    }
}

#[cfg(target_os = "linux")]
fn probe_nvfbc() -> bool {
    match crate::dl::DynLib::open(&["libnvidia-fbc.so.1", "libnvidia-fbc.so"]) {
        Ok(_lib) => {
            info!("NVFBC library found");
            true
        }
        Err(e) => {
            warn!("NVFBC not available: {e}");
            false
        }
    }
}

#[cfg(target_os = "windows")]
fn probe_dxgi() -> bool {
    // DXGI is always available on Windows with a GPU
    // Check if we can create a DXGI factory
    match crate::dl::DynLib::open(&["dxgi.dll"]) {
        Ok(_) => {
            info!("DXGI available");
            true
        }
        Err(e) => {
            warn!("DXGI not available: {e}");
            false
        }
    }
}

/// Check if the GPU supports AV1 encoding via NVENC.
///
/// Ada Lovelace (RTX 40xx, L40, L4) and newer GPUs have 8th gen NVENC
/// which supports AV1. We detect by GPU name heuristic first, then
/// try to actually initialize an AV1 encoder session as confirmation.
fn probe_av1_support(gpu_name: Option<&str>) -> bool {
    let name = match gpu_name {
        Some(n) => n.to_uppercase(),
        None => return false,
    };

    // Ada Lovelace (40xx series, L40, L4) and Blackwell (50xx, B200)
    let likely_av1 = name.contains("RTX 40")
        || name.contains("RTX 50")
        || name.contains(" L40")
        || name.contains(" L4 ")
        || name.contains(" L4\0")
        || name.ends_with(" L4")
        || name.contains("B200")
        || name.contains("B100")
        || name.contains("H100") // Hopper has AV1 NVENC too
        || name.contains("H200");

    if !likely_av1 {
        info!(gpu = gpu_name, "GPU does not appear to support AV1 NVENC");
        return false;
    }

    // Confirm by trying to open an NVENC session with AV1 codec
    match try_av1_encoder() {
        true => {
            info!("AV1 NVENC encoding confirmed available");
            true
        }
        false => {
            warn!(
                gpu = gpu_name,
                "GPU name suggests AV1 but encoder init failed"
            );
            false
        }
    }
}

/// Try to create a minimal NVENC AV1 encoder session.
/// Returns true if successful, false otherwise.
fn try_av1_encoder() -> bool {
    use std::sync::Arc;
    let cuda = match crate::cuda::CudaLib::load() {
        Ok(c) => Arc::new(c),
        Err(_) => return false,
    };
    // Try creating a tiny AV1 encoder (will fail fast if not supported)
    crate::nvenc::NvencEncoder::new(
        cuda,
        0,
        320,
        240,
        30,
        1000,
        phantom_core::encode::VideoCodec::Av1,
    )
    .is_ok()
}

/// Try to get the GPU device name via cuDeviceGetName.
fn get_device_name(cuda: &crate::cuda::CudaLib, dev: crate::sys::CUdevice) -> Option<String> {
    // cuDeviceGetName is not in our CudaLib wrapper, so we try to get it
    // from the already-loaded library. For simplicity, just return None
    // and rely on nvidia-smi style detection.
    // In the future we can add cuDeviceGetName to CudaLib.
    let _ = (cuda, dev);
    // Try nvidia-smi as fallback
    #[cfg(unix)]
    {
        if let Ok(output) = std::process::Command::new("nvidia-smi")
            .args(["--query-gpu=name", "--format=csv,noheader", "--id=0"])
            .output()
        {
            if output.status.success() {
                let name = String::from_utf8_lossy(&output.stdout).trim().to_string();
                if !name.is_empty() {
                    return Some(name);
                }
            }
        }
    }
    #[cfg(windows)]
    {
        if let Ok(output) = std::process::Command::new("nvidia-smi")
            .args(["--query-gpu=name", "--format=csv,noheader", "--id=0"])
            .output()
        {
            if output.status.success() {
                let name = String::from_utf8_lossy(&output.stdout).trim().to_string();
                if !name.is_empty() {
                    return Some(name);
                }
            }
        }
    }
    None
}
