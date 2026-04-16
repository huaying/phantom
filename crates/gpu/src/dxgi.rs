//! DXGI Desktop Duplication capture — returns GPU-resident ID3D11Texture2D.
//! Windows only. Zero CPU readback.

use anyhow::{bail, Context, Result};
use windows::core::Interface;
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;

pub struct DxgiCapture {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    duplication: IDXGIOutputDuplication,
    staging: ID3D11Texture2D,
    pub width: u32,
    pub height: u32,
    frame_acquired: bool,
    /// Target device name for recreate (e.g. `\\.\DISPLAY11` = VDD).
    target_device: Option<String>,
}

unsafe impl Send for DxgiCapture {}

impl DxgiCapture {
    pub fn new() -> Result<Self> {
        Self::with_target_device(None)
    }

    /// Create a new capture, optionally targeting a specific display device by name.
    /// When `target_device` is set (e.g. `\\.\DISPLAY10`), only that output is selected.
    /// This is how DCV/Parsec target their own VDD — by device name, not by resolution.
    /// Falls back to highest-resolution NVIDIA output if target not found.
    pub fn with_target_device(target_device: Option<&str>) -> Result<Self> {
        unsafe {
            let factory: IDXGIFactory1 = CreateDXGIFactory1()?;

            // Enumerate ALL adapters and ALL outputs to find the best one.
            // Strategy: if target set, prefer output matching target resolution.
            // Otherwise pick the output with the highest resolution.
            struct Candidate {
                adapter: IDXGIAdapter1,
                adapter_name: String,
                output_idx: u32,
                width: u32,
                height: u32,
                is_nvidia: bool,
                matches_device: bool,
            }

            let mut best: Option<Candidate> = None;

            let mut adapter_idx = 0u32;
            while let Ok(adapter) = factory.EnumAdapters1(adapter_idx) {
                let desc = adapter.GetDesc1()?;
                let adapter_name = String::from_utf16_lossy(
                    &desc.Description[..desc
                        .Description
                        .iter()
                        .position(|&c| c == 0)
                        .unwrap_or(desc.Description.len())],
                );
                let is_nvidia = adapter_name.to_uppercase().contains("NVIDIA");

                let mut output_idx = 0u32;
                while let Ok(output) = adapter.EnumOutputs(output_idx) {
                    let out_desc = output.GetDesc()?;
                    let r = out_desc.DesktopCoordinates;
                    let w = (r.right - r.left) as u32;
                    let h = (r.bottom - r.top) as u32;
                    let device_name = String::from_utf16_lossy(
                        &out_desc.DeviceName[..out_desc
                            .DeviceName
                            .iter()
                            .position(|&c| c == 0)
                            .unwrap_or(out_desc.DeviceName.len())],
                    );
                    tracing::info!(
                        adapter = adapter_idx,
                        output = output_idx,
                        name = %adapter_name,
                        device = %device_name,
                        width = w,
                        height = h,
                        is_nvidia,
                        "DXGI output"
                    );

                    // Match by device name (e.g. \\.\DISPLAY10 = VDD)
                    let matches_device = target_device
                        .map_or(false, |td| device_name == td);
                    let best_matches_device = best.as_ref().map_or(false, |b| b.matches_device);

                    // Scoring: device name match > NVIDIA highest res > any highest res
                    let dominated = best.as_ref().is_some_and(|b| {
                        if matches_device && !best_matches_device {
                            false // device name match always wins
                        } else if !matches_device && best_matches_device {
                            true // can't beat a device name match
                        } else if is_nvidia && !b.is_nvidia {
                            false // NVIDIA beats non-NVIDIA
                        } else if !is_nvidia && b.is_nvidia {
                            true // non-NVIDIA can't beat NVIDIA
                        } else {
                            (w as u64) * (h as u64) <= (b.width as u64) * (b.height as u64)
                        }
                    });

                    if !dominated {
                        best = Some(Candidate {
                            adapter: adapter.clone(),
                            adapter_name: adapter_name.clone(),
                            output_idx,
                            width: w,
                            height: h,
                            is_nvidia,
                            matches_device,
                        });
                    }
                    output_idx += 1;
                }
                adapter_idx += 1;
            }

            let c = best.context("no DXGI adapter with active output found")?;
            tracing::info!(
                "Selected DXGI: {} output {} ({}x{})",
                c.adapter_name,
                c.output_idx,
                c.width,
                c.height
            );

            let mut device = None;
            let mut context = None;
            let feature_levels = [windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_11_0];
            let adapter_base: IDXGIAdapter = c.adapter.cast()?;
            D3D11CreateDevice(
                &adapter_base,
                D3D_DRIVER_TYPE_UNKNOWN,
                None,
                D3D11_CREATE_DEVICE_FLAG(0),
                Some(&feature_levels),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )?;
            let device = device.context("D3D11 device creation failed")?;
            let context = context.context("D3D11 context creation failed")?;

            // Duplicate the selected output
            let output: IDXGIOutput = c.adapter.EnumOutputs(c.output_idx)?;
            let output1: IDXGIOutput1 = output.cast()?;
            let duplication = output1.DuplicateOutput(&device)?;

            let dup_desc = duplication.GetDesc();
            let width = dup_desc.ModeDesc.Width;
            let height = dup_desc.ModeDesc.Height;

            // Create staging texture (GPU-resident, our own copy for NVENC to hold)
            let tex_desc = D3D11_TEXTURE2D_DESC {
                Width: width,
                Height: height,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: 0,
                CPUAccessFlags: 0,
                MiscFlags: 0,
            };
            let mut staging = None;
            device.CreateTexture2D(&tex_desc, None, Some(&mut staging))?;
            let staging = staging.context("CreateTexture2D returned None")?;

            tracing::info!(
                width,
                height,
                "DXGI Desktop Duplication initialized (GPU zero-copy)"
            );

            Ok(Self {
                device,
                context,
                duplication,
                staging,
                width,
                height,
                frame_acquired: false,
                target_device: target_device.map(|s| s.to_string()),
            })
        }
    }

    /// Acquire next frame and copy to staging texture.
    /// Returns true if a new frame was captured, false if no new frame (static desktop).
    pub fn capture(&mut self) -> Result<bool> {
        unsafe {
            if self.frame_acquired {
                self.duplication.ReleaseFrame()?;
                self.frame_acquired = false;
            }

            let mut frame_info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut resource = None;

            // Use frame_interval timeout — blocks until DWM has a new frame.
            // timeout=0 causes busy-loop and misses frames between polls.
            match self
                .duplication
                .AcquireNextFrame(33, &mut frame_info, &mut resource)
            {
                Ok(_) => {}
                Err(e) if e.code() == windows::Win32::Graphics::Dxgi::DXGI_ERROR_WAIT_TIMEOUT => {
                    return Ok(false)
                }
                Err(e) if e.code() == windows::Win32::Graphics::Dxgi::DXGI_ERROR_ACCESS_LOST => {
                    tracing::warn!("DXGI_ERROR_ACCESS_LOST — recreating");
                    self.recreate()?;
                    return Ok(false);
                }
                Err(e) => bail!("AcquireNextFrame: {e}"),
            }

            self.frame_acquired = true;

            if let Some(resource) = resource {
                let texture: ID3D11Texture2D = resource.cast()?;
                // GPU-to-GPU copy (~0.3ms)
                self.context.CopyResource(&self.staging, &texture);
            }

            Ok(true)
        }
    }

    /// Get the staging texture pointer for NVENC registration.
    pub fn texture_ptr(&self) -> *mut std::ffi::c_void {
        unsafe { std::mem::transmute_copy(&self.staging) }
    }

    /// Get the D3D11 device pointer for NVENC session.
    pub fn device_ptr(&self) -> *mut std::ffi::c_void {
        unsafe { std::mem::transmute_copy(&self.device) }
    }

    pub fn reset(&mut self) -> Result<()> {
        self.recreate()
    }

    fn recreate(&mut self) -> Result<()> {
        unsafe {
            if self.frame_acquired {
                let _ = self.duplication.ReleaseFrame();
                self.frame_acquired = false;
            }
            // Re-enumerate adapters+outputs using same target_device as initial creation.
            // This ensures recreate picks the same VDD output, not the highest-res one.
            let factory: IDXGIFactory1 = CreateDXGIFactory1()?;
            let target = self.target_device.as_deref();
            let mut best_adapter: Option<IDXGIAdapter1> = None;
            let mut best_output_idx = 0u32;
            let mut best_pixels: u64 = 0;
            let mut best_matches_device = false;

            let mut adapter_idx = 0u32;
            while let Ok(adapter) = factory.EnumAdapters1(adapter_idx) {
                let mut output_idx = 0u32;
                while let Ok(output) = adapter.EnumOutputs(output_idx) {
                    let out_desc = output.GetDesc()?;
                    let r = out_desc.DesktopCoordinates;
                    let pixels =
                        ((r.right - r.left) as u64) * ((r.bottom - r.top) as u64);
                    let device_name = String::from_utf16_lossy(
                        &out_desc.DeviceName[..out_desc
                            .DeviceName
                            .iter()
                            .position(|&c| c == 0)
                            .unwrap_or(out_desc.DeviceName.len())],
                    );
                    let matches = target.map_or(false, |t| device_name == t);

                    // device match > highest res
                    let dominated = best_adapter.is_some()
                        && ((!matches && best_matches_device)
                            || (matches == best_matches_device && pixels <= best_pixels));

                    if !dominated {
                        best_pixels = pixels;
                        best_adapter = Some(adapter.clone());
                        best_output_idx = output_idx;
                        best_matches_device = matches;
                    }
                    output_idx += 1;
                }
                adapter_idx += 1;
            }
            let adapter = best_adapter.context("no DXGI adapter with active output")?;

            let output: IDXGIOutput = adapter.EnumOutputs(best_output_idx)?;
            let output1: IDXGIOutput1 = output.cast()?;
            self.duplication = output1.DuplicateOutput(&self.device)?;

            let dup_desc = self.duplication.GetDesc();
            self.width = dup_desc.ModeDesc.Width;
            self.height = dup_desc.ModeDesc.Height;
            tracing::debug!(self.width, self.height, "DXGI duplicator recreated");
            Ok(())
        }
    }
}

impl Drop for DxgiCapture {
    fn drop(&mut self) {
        if self.frame_acquired {
            unsafe {
                let _ = self.duplication.ReleaseFrame();
            }
        }
    }
}
