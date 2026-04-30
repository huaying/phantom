//! DXGI Desktop Duplication capture — returns GPU-resident ID3D11Texture2D.
//! Windows only. Zero CPU readback.

use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use windows::core::Interface;
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;

pub struct DxgiCapture {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    duplication: Option<IDXGIOutputDuplication>,
    staging: ID3D11Texture2D,
    adapter: IDXGIAdapter1,
    output_idx: u32,
    adapter_name: String,
    output_device_name: String,
    output_is_nvidia: bool,
    output_matches_target: bool,
    pub width: u32,
    pub height: u32,
    frame_acquired: bool,
    last_no_frame_log: Instant,
    timeouts_since_log: u32,
    zero_present_since_log: u32,
    #[allow(dead_code)]
    target_device: Option<String>,
}

#[derive(Debug, Clone, Copy)]
pub struct BgraSampleStats {
    pub sampled: u32,
    pub black_pct: u32,
    pub mean_r: u32,
    pub mean_g: u32,
    pub mean_b: u32,
}

impl BgraSampleStats {
    pub fn is_mostly_black(&self) -> bool {
        self.black_pct >= 99 && self.mean_r < 8 && self.mean_g < 8 && self.mean_b < 8
    }
}

unsafe impl Send for DxgiCapture {}

impl DxgiCapture {
    pub fn new() -> Result<Self> {
        Self::with_target_device(None)
    }

    /// Create a new capture, optionally targeting a specific display device by name.
    /// When `target_device` is set (e.g. `\\.\DISPLAY10`), only that output is selected.
    /// This is how DCV/Parsec target their own VDD — by device name, not by resolution.
    /// Falls back to highest-resolution NVIDIA output only when no explicit
    /// target is requested. Explicit targets fail hard if they are not active.
    pub fn with_target_device(target_device: Option<&str>) -> Result<Self> {
        unsafe {
            let factory: IDXGIFactory1 = CreateDXGIFactory1()?;

            // Enumerate ALL adapters and ALL outputs to find the best one.
            // Strategy: if target is set, it must match by GDI device name.
            // Otherwise pick the highest-resolution NVIDIA output.
            struct Candidate {
                adapter: IDXGIAdapter1,
                adapter_name: String,
                output_idx: u32,
                device_name: String,
                width: u32,
                height: u32,
                is_nvidia: bool,
                matches_device: bool,
            }

            let mut best: Option<Candidate> = None;
            let mut seen_outputs: Vec<String> = Vec::new();

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
                    seen_outputs.push(format!(
                        "adapter={} '{}' output={} device='{}' {}x{} nvidia={}",
                        adapter_idx, adapter_name, output_idx, device_name, w, h, is_nvidia
                    ));
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
                    let matches_device = target_device.is_some_and(|td| device_name == td);
                    let best_matches_device = best.as_ref().is_some_and(|b| b.matches_device);

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
                            device_name,
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
            if let Some(target) = target_device {
                if !c.matches_device {
                    bail!(
                        "DXGI target output '{}' was not found among active outputs: {}",
                        target,
                        seen_outputs.join("; ")
                    );
                }
            }
            tracing::info!(
                "Selected DXGI: {} output {} device {} ({}x{}, nvidia={}, target_match={})",
                c.adapter_name,
                c.output_idx,
                c.device_name,
                c.width,
                c.height,
                c.is_nvidia,
                c.matches_device
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
                duplication: Some(duplication),
                staging,
                adapter: c.adapter,
                output_idx: c.output_idx,
                adapter_name: c.adapter_name,
                output_device_name: c.device_name,
                output_is_nvidia: c.is_nvidia,
                output_matches_target: c.matches_device,
                width,
                height,
                frame_acquired: false,
                last_no_frame_log: Instant::now() - Duration::from_secs(10),
                timeouts_since_log: 0,
                zero_present_since_log: 0,
                target_device: target_device.map(|s| s.to_string()),
            })
        }
    }

    /// Acquire next frame and copy to staging texture.
    /// Returns true if a new frame was captured, false if no new frame (static desktop).
    pub fn capture(&mut self) -> Result<bool> {
        unsafe {
            let dup = self
                .duplication
                .as_ref()
                .context("DXGI duplication not initialized")?
                .clone();

            if self.frame_acquired {
                dup.ReleaseFrame()?;
                self.frame_acquired = false;
            }

            let mut frame_info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut resource = None;

            match dup.AcquireNextFrame(33, &mut frame_info, &mut resource) {
                Ok(_) => {}
                Err(e) if e.code() == windows::Win32::Graphics::Dxgi::DXGI_ERROR_WAIT_TIMEOUT => {
                    self.record_no_frame("timeout", None);
                    return Ok(false);
                }
                Err(e)
                    if e.code() == windows::Win32::Graphics::Dxgi::DXGI_ERROR_ACCESS_LOST
                        || e.code() == windows::Win32::Graphics::Dxgi::DXGI_ERROR_ACCESS_DENIED =>
                {
                    tracing::warn!("DXGI duplication access lost/denied — recreating");
                    drop(dup);
                    self.recreate()?;
                    return Ok(false);
                }
                Err(e) => bail!("AcquireNextFrame: {e}"),
            }

            self.frame_acquired = true;

            // Desktop Duplication can return S_OK for pointer-only/no-present
            // updates. Encoding our staging texture in that case re-sends stale
            // or initial black content. Treat it like WAIT_TIMEOUT and keep the
            // last valid frame, matching WebRTC/RustDesk behavior.
            if frame_info.AccumulatedFrames == 0
                || frame_info.LastPresentTime == 0
                || resource.is_none()
            {
                dup.ReleaseFrame()?;
                self.frame_acquired = false;
                self.record_no_frame("zero_present", Some(frame_info));
                return Ok(false);
            }

            let texture: ID3D11Texture2D = resource
                .context("AcquireNextFrame returned no resource")?
                .cast()?;
            // GPU-to-GPU copy (~0.3ms)
            self.context.CopyResource(&self.staging, &texture);

            Ok(true)
        }
    }

    fn record_no_frame(&mut self, reason: &'static str, info: Option<DXGI_OUTDUPL_FRAME_INFO>) {
        match reason {
            "timeout" => self.timeouts_since_log = self.timeouts_since_log.saturating_add(1),
            "zero_present" => {
                self.zero_present_since_log = self.zero_present_since_log.saturating_add(1)
            }
            _ => {}
        }

        if self.last_no_frame_log.elapsed() < Duration::from_secs(5) {
            return;
        }

        match info {
            Some(info) => tracing::info!(
                target = %self.target_summary(),
                reason,
                timeouts = self.timeouts_since_log,
                zero_present = self.zero_present_since_log,
                accumulated_frames = info.AccumulatedFrames,
                last_present_time = info.LastPresentTime,
                last_mouse_update_time = info.LastMouseUpdateTime,
                pointer_visible = info.PointerPosition.Visible.as_bool(),
                pointer_x = info.PointerPosition.Position.x,
                pointer_y = info.PointerPosition.Position.y,
                "DXGI no present frame yet"
            ),
            None => tracing::info!(
                target = %self.target_summary(),
                reason,
                timeouts = self.timeouts_since_log,
                zero_present = self.zero_present_since_log,
                "DXGI no present frame yet"
            ),
        }

        self.timeouts_since_log = 0;
        self.zero_present_since_log = 0;
        self.last_no_frame_log = Instant::now();
    }

    /// Get the staging texture pointer for NVENC registration.
    pub fn texture_ptr(&self) -> *mut std::ffi::c_void {
        unsafe { std::mem::transmute_copy(&self.staging) }
    }

    pub fn target_summary(&self) -> String {
        format!(
            "adapter='{}' output_device='{}' output={} size={}x{} nvidia={} target_match={}",
            self.adapter_name,
            self.output_device_name,
            self.output_idx,
            self.width,
            self.height,
            self.output_is_nvidia,
            self.output_matches_target
        )
    }

    /// Copy the latest GPU staging texture to a CPU-readable texture and sample
    /// BGRA pixels. Used by install doctor/probes to catch encoded black frames.
    pub fn sample_bgra_stats(&self, max_samples: usize) -> Result<BgraSampleStats> {
        unsafe {
            let readback_desc = D3D11_TEXTURE2D_DESC {
                Width: self.width,
                Height: self.height,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_STAGING,
                BindFlags: 0,
                CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                MiscFlags: 0,
            };
            let mut readback = None;
            self.device
                .CreateTexture2D(&readback_desc, None, Some(&mut readback))?;
            let readback = readback.context("CreateTexture2D(readback) returned None")?;
            self.context.CopyResource(&readback, &self.staging);

            let resource: ID3D11Resource = readback.cast()?;
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.context
                .Map(&resource, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
            let stats = sample_mapped_bgra(
                mapped.pData as *const u8,
                mapped.RowPitch as usize,
                self.width as usize,
                self.height as usize,
                max_samples,
            );
            self.context.Unmap(&resource, 0);
            Ok(stats)
        }
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
                if let Some(ref dup) = self.duplication {
                    let _ = dup.ReleaseFrame();
                }
                self.frame_acquired = false;
            }
            // Drop old duplication BEFORE creating new one — only one allowed per output.
            self.duplication = None;

            let output: IDXGIOutput = self.adapter.EnumOutputs(self.output_idx)?;
            let output1: IDXGIOutput1 = output.cast()?;
            self.duplication = Some(output1.DuplicateOutput(&self.device)?);

            let dup_desc = self.duplication.as_ref().unwrap().GetDesc();
            self.width = dup_desc.ModeDesc.Width;
            self.height = dup_desc.ModeDesc.Height;
            tracing::debug!(self.width, self.height, "DXGI duplicator recreated");
            Ok(())
        }
    }
}

fn sample_mapped_bgra(
    data: *const u8,
    row_pitch: usize,
    width: usize,
    height: usize,
    max_samples: usize,
) -> BgraSampleStats {
    if data.is_null() || row_pitch == 0 || width == 0 || height == 0 {
        return BgraSampleStats {
            sampled: 0,
            black_pct: 100,
            mean_r: 0,
            mean_g: 0,
            mean_b: 0,
        };
    }

    let pixels = width.saturating_mul(height);
    let step = (pixels / max_samples.max(1)).max(1);
    let mut sampled = 0u32;
    let mut black = 0u32;
    let mut sum_r = 0u64;
    let mut sum_g = 0u64;
    let mut sum_b = 0u64;

    for pixel in (0..pixels).step_by(step) {
        let row = pixel / width;
        let col = pixel % width;
        let offset = row.saturating_mul(row_pitch).saturating_add(col * 4);
        unsafe {
            let b = *data.add(offset) as u32;
            let g = *data.add(offset + 1) as u32;
            let r = *data.add(offset + 2) as u32;
            sampled += 1;
            sum_r += r as u64;
            sum_g += g as u64;
            sum_b += b as u64;
            if r < 8 && g < 8 && b < 8 {
                black += 1;
            }
        }
    }

    if sampled == 0 {
        return BgraSampleStats {
            sampled: 0,
            black_pct: 100,
            mean_r: 0,
            mean_g: 0,
            mean_b: 0,
        };
    }

    BgraSampleStats {
        sampled,
        black_pct: black * 100 / sampled,
        mean_r: (sum_r / sampled as u64) as u32,
        mean_g: (sum_g / sampled as u64) as u32,
        mean_b: (sum_b / sampled as u64) as u32,
    }
}

impl Drop for DxgiCapture {
    fn drop(&mut self) {
        if self.frame_acquired {
            if let Some(ref dup) = self.duplication {
                unsafe {
                    let _ = dup.ReleaseFrame();
                }
            }
        }
    }
}
