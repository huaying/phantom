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
