//! DXGI Desktop Duplication capture — returns GPU-resident ID3D11Texture2D.
//! Windows only. Zero CPU readback.

use anyhow::{bail, Context, Result};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::core::Interface;

pub struct DxgiCapture {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    duplication: IDXGIOutputDuplication,
    staging: ID3D11Texture2D,
    pub width: u32,
    pub height: u32,
    frame_acquired: bool,
}

unsafe impl Send for DxgiCapture {}

impl DxgiCapture {
    pub fn new() -> Result<Self> {
        unsafe {
            // Create D3D11 device on the default adapter
            let factory: IDXGIFactory1 = CreateDXGIFactory1()?;
            let adapter = factory.EnumAdapters1(0)?;
            let desc = adapter.GetDesc1()?;
            let name = String::from_utf16_lossy(
                &desc.Description[..desc.Description.iter().position(|&c| c == 0).unwrap_or(desc.Description.len())]
            );
            tracing::info!("DXGI adapter: {name}");

            let mut device = None;
            let mut context = None;
            let feature_levels = [windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_11_0];
            let adapter_base: IDXGIAdapter = adapter.cast()?;
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

            // Get primary output and duplicate
            let output: IDXGIOutput = adapter.EnumOutputs(0)?;
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
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: 0,
                CPUAccessFlags: 0,
                MiscFlags: 0,
            };
            let mut staging = None;
            device.CreateTexture2D(&tex_desc, None, Some(&mut staging))?;
            let staging = staging.context("CreateTexture2D returned None")?;

            tracing::info!(width, height, "DXGI Desktop Duplication initialized (GPU zero-copy)");

            Ok(Self { device, context, duplication, staging, width, height, frame_acquired: false })
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
            match self.duplication.AcquireNextFrame(33, &mut frame_info, &mut resource) {
                Ok(_) => {}
                Err(e) if e.code() == windows::Win32::Graphics::Dxgi::DXGI_ERROR_WAIT_TIMEOUT => return Ok(false),
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
            let factory: IDXGIFactory1 = CreateDXGIFactory1()?;
            let adapter = factory.EnumAdapters1(0)?;
            let output: IDXGIOutput = adapter.EnumOutputs(0)?;
            let output1: IDXGIOutput1 = output.cast()?;
            self.duplication = output1.DuplicateOutput(&self.device)?;

            let dup_desc = self.duplication.GetDesc();
            self.width = dup_desc.ModeDesc.Width;
            self.height = dup_desc.ModeDesc.Height;
            tracing::debug!("DXGI duplicator recreated");
            Ok(())
        }
    }
}

impl Drop for DxgiCapture {
    fn drop(&mut self) {
        if self.frame_acquired {
            unsafe { let _ = self.duplication.ReleaseFrame(); }
        }
    }
}
