#![allow(clippy::missing_transmute_annotations)]
//! NVFBC GPU screen capture — implements `FrameCapture` trait.
//!
//! Captures the X11 framebuffer directly from GPU memory via NVIDIA's
//! NVFBC API. Returns a CUdeviceptr (GPU memory) that can be fed directly
//! to NVENC for zero-copy encoding.

use crate::cuda::CudaLib;
use crate::dl::DynLib;
use crate::sys::*;
use anyhow::{bail, Context, Result};
use phantom_core::capture::FrameCapture;
use phantom_core::frame::{Frame, PixelFormat};
use std::ffi::CStr;
use std::sync::Arc;
use std::time::Instant;

pub struct NvfbcCapture {
    cuda: Arc<CudaLib>,
    ctx: CUcontext,
    api: NvFbcFunctionList,
    handle: NVFBC_SESSION_HANDLE,
    width: u32,
    height: u32,
    runtime_version: u32,
    /// Format requested from NVFBC (BGRA for CPU path, NV12 for GPU path).
    _buffer_format: u32,
    grab_info: NvFbcFrameGrabInfo,
    _lib: DynLib,
}

unsafe impl Send for NvfbcCapture {}

impl NvfbcCapture {
    /// Create NVFBC capture session outputting to CUDA device pointer.
    pub fn new(cuda: Arc<CudaLib>, ctx: CUcontext, buffer_format: u32) -> Result<Self> {
        Self::with_options(cuda, ctx, buffer_format, false)
    }

    pub fn with_options(cuda: Arc<CudaLib>, ctx: CUcontext, buffer_format: u32, with_cursor: bool) -> Result<Self> {
        let lib = DynLib::open(&["libnvidia-fbc.so.1", "libnvidia-fbc.so"])
            .context("failed to load libnvidia-fbc")?;

        // Load each function directly via dlsym — more robust than
        // NvFBCCreateInstance which has strict version requirements.
        let api = load_nvfbc_api(&lib)?;

        // Create handle
        let mut handle: NVFBC_SESSION_HANDLE = 0;
        let mut params = NvFbcCreateHandleParams::new();
        let status = unsafe { (api.create_handle)(&mut handle, params.as_mut_ptr()) };
        if status != NVFBC_SUCCESS {
            bail!("NvFBCCreateHandle failed: {status}");
        }

        // Get status to learn screen dimensions
        let mut status_params = NvFbcGetStatusParams::new();
        let status = unsafe { (api.get_status)(handle, status_params.as_mut_ptr()) };
        if status != NVFBC_SUCCESS {
            bail!("NvFBCGetStatus failed: {}", nvfbc_error_detail(&api, handle, status));
        }

        let runtime_version = status_params.nvfbc_version();
        let width = status_params.screen_w();
        let height = status_params.screen_h();
        tracing::info!(width, height, runtime_version, "NVFBC screen detected");

        // Create capture session (CUDA output, no cursor, polling mode)
        let mut session_params = NvFbcCreateCaptureSessionParams::new();
        session_params.set_capture_type(NVFBC_CAPTURE_SHARED_CUDA);
        session_params.set_tracking_type(NVFBC_TRACKING_DEFAULT);
        session_params.set_with_cursor(if with_cursor { NVFBC_TRUE } else { NVFBC_FALSE });
        session_params.set_push_model(NVFBC_FALSE);
        session_params.set_sampling_rate_ms(16); // ~60 Hz

        let status = unsafe { (api.create_capture_session)(handle, session_params.as_mut_ptr()) };
        if status != NVFBC_SUCCESS {
            bail!(
                "NvFBCCreateCaptureSession failed: {}",
                nvfbc_error_detail(&api, handle, status)
            );
        }

        // Setup CUDA capture with requested format
        let mut setup = NvFbcToCudaSetupParams::new(buffer_format);
        let status = unsafe { (api.to_cuda_setup)(handle, setup.as_mut_ptr()) };
        if status != NVFBC_SUCCESS {
            bail!(
                "NvFBCToCudaSetUp failed: {}",
                nvfbc_error_detail(&api, handle, status)
            );
        }

        tracing::info!(
            width, height,
            format = match buffer_format {
                NVFBC_BUFFER_FORMAT_BGRA => "BGRA",
                NVFBC_BUFFER_FORMAT_NV12 => "NV12",
                _ => "other",
            },
            "NVFBC capture session initialized"
        );

        Ok(Self {
            cuda,
            ctx,
            api,
            handle,
            width,
            height,
            runtime_version,
            _buffer_format: buffer_format,
            grab_info: unsafe { std::mem::zeroed() },
            _lib: lib,
        })
    }

    /// Destroy and recreate the capture session. Resets NVFBC's "seen frames"
    /// state so grab_cuda() returns frames again after a client reconnect.
    pub fn reset_session(&mut self) -> Result<()> {
        // Bind context for the entire destroy+create sequence
        let _ = self.bind_context();

        // Destroy old session (ignore error if already destroyed)
        let mut destroy = NvFbcDestroyCaptureSessionParams::new();
        let _ = unsafe { (self.api.destroy_capture_session)(self.handle, destroy.as_mut_ptr()) };

        // Recreate capture session
        let mut session_params = NvFbcCreateCaptureSessionParams::new();
        session_params.set_capture_type(NVFBC_CAPTURE_SHARED_CUDA);
        session_params.set_tracking_type(NVFBC_TRACKING_DEFAULT);
        session_params.set_with_cursor(NVFBC_FALSE);
        session_params.set_push_model(NVFBC_FALSE);
        session_params.set_sampling_rate_ms(16);

        let status = unsafe { (self.api.create_capture_session)(self.handle, session_params.as_mut_ptr()) };
        if status != NVFBC_SUCCESS {
            let _ = self.release_context();
            bail!("NvFBCCreateCaptureSession (reset) failed: {}", nvfbc_error_detail(&self.api, self.handle, status));
        }

        let mut setup = NvFbcToCudaSetupParams::new(self._buffer_format);
        let status = unsafe { (self.api.to_cuda_setup)(self.handle, setup.as_mut_ptr()) };
        if status != NVFBC_SUCCESS {
            let _ = self.release_context();
            bail!("NvFBCToCudaSetUp (reset) failed: {}", nvfbc_error_detail(&self.api, self.handle, status));
        }

        let _ = self.release_context();
        tracing::info!("NVFBC capture session reset");
        Ok(())
    }

    /// NVFBC API version reported by the runtime library.
    ///
    /// Encoded as `minor | (major << 8)` (e.g., 0x107 for 1.7).
    pub fn runtime_version(&self) -> u32 {
        self.runtime_version
    }

    /// Release the NVFBC context so other CUDA operations (e.g., NVENC) can use it.
    pub fn release_context(&self) -> Result<()> {
        let mut params = NvFbcReleaseContextParams::new();
        let status = unsafe { (self.api.release_context)(self.handle, params.as_mut_ptr()) };
        if status != NVFBC_SUCCESS {
            bail!(
                "NvFBCReleaseContext failed: {}",
                nvfbc_error_detail(&self.api, self.handle, status)
            );
        }
        Ok(())
    }

    /// Re-bind the NVFBC context to the current thread (after release).
    pub fn bind_context(&self) -> Result<()> {
        let mut params = NvFbcBindContextParams::new();
        let status = unsafe { (self.api.bind_context)(self.handle, params.as_mut_ptr()) };
        if status != NVFBC_SUCCESS {
            bail!(
                "NvFBCBindContext failed: {}",
                nvfbc_error_detail(&self.api, self.handle, status)
            );
        }
        Ok(())
    }

    /// Grab a frame, returning the CUDA device pointer and frame info.
    /// This is the zero-copy path — data stays on GPU.
    pub fn grab_cuda(&mut self) -> Result<Option<GpuFrame>> {
        self.grab_cuda_flags(NVFBC_TOCUDA_GRAB_FLAGS_NOWAIT)
    }

    /// Grab with FORCE_REFRESH — always returns a frame even if nothing changed.
    pub fn grab_cuda_force(&mut self) -> Result<GpuFrame> {
        self.grab_cuda_flags(NVFBC_TOCUDA_GRAB_FLAGS_FORCE_REFRESH)?
            .ok_or_else(|| anyhow::anyhow!("NVFBC force grab returned no frame"))
    }

    fn grab_cuda_flags(&mut self, flags: u32) -> Result<Option<GpuFrame>> {
        let mut device_buffer: *mut std::ffi::c_void = std::ptr::null_mut();

        let mut params = NvFbcToCudaGrabFrameParams::new(&mut self.grab_info);
        params.set_flags(flags);
        params.set_cuda_device_buffer(&mut device_buffer as *mut _ as *mut std::ffi::c_void);

        let status = unsafe { (self.api.to_cuda_grab_frame)(self.handle, params.as_mut_ptr()) };
        if status == NVFBC_ERR_MUST_RECREATE {
            bail!("NVFBC must recreate (display mode change)");
        }
        if status != NVFBC_SUCCESS {
            bail!(
                "NvFBCToCudaGrabFrame failed: {}",
                nvfbc_error_detail(&self.api, self.handle, status)
            );
        }

        if self.grab_info.is_new_frame == NVFBC_FALSE {
            return Ok(None);
        }

        if device_buffer.is_null() {
            bail!("NvFBCToCudaGrabFrame returned null CUDA device buffer");
        }

        let frame = GpuFrame {
            device_ptr: device_buffer as CUdeviceptr,
            width: self.grab_info.width,
            height: self.grab_info.height,
            byte_size: self.grab_info.byte_size,
        };

        tracing::debug!(
            width = frame.width,
            height = frame.height,
            byte_size = frame.byte_size,
            inferred_nv12_pitch = frame.infer_nv12_pitch(),
            required_post_processing = self.grab_info.required_post_processing == NVFBC_TRUE,
            direct_capture = self.grab_info.direct_capture == NVFBC_TRUE,
            "NVFBC grabbed CUDA frame"
        );

        Ok(Some(frame))
    }
}

/// A frame captured on the GPU — data is a CUDA device pointer.
pub struct GpuFrame {
    pub device_ptr: CUdeviceptr,
    pub width: u32,
    pub height: u32,
    pub byte_size: u32,
}

impl GpuFrame {
    /// Infer NV12 pitch (bytes/row) from frame dimensions and byte size.
    ///
    /// NV12 total bytes = pitch * height * 3 / 2.
    /// Returns `None` if byte_size is not consistent with NV12 layout.
    pub fn infer_nv12_pitch(&self) -> Option<u32> {
        if self.height == 0 {
            return None;
        }
        let num = (self.byte_size as u64) * 2;
        let den = (self.height as u64) * 3;
        if den == 0 || num % den != 0 {
            return None;
        }
        let pitch = num / den;
        u32::try_from(pitch).ok()
    }
}

impl FrameCapture for NvfbcCapture {
    /// Grabs a frame from GPU and downloads to CPU memory.
    /// This is the compatibility path for use with CPU encoders or tile differ.
    /// For zero-copy GPU encoding, use `grab_cuda()` + `NvencEncoder::encode_device_nv12()`.
    fn capture(&mut self) -> Result<Option<Frame>> {
        if self._buffer_format != NVFBC_BUFFER_FORMAT_BGRA {
            bail!(
                "NvfbcCapture::capture() requires BGRA output; got buffer_format={}",
                self._buffer_format
            );
        }

        let gpu_frame = match self.grab_cuda()? {
            Some(f) => f,
            None => return Ok(None),
        };

        let size = gpu_frame.byte_size as usize;
        let mut data = vec![0u8; size];

        unsafe { self.cuda.ctx_push(self.ctx)? };
        self.cuda.memcpy_dtoh(&mut data, gpu_frame.device_ptr)?;
        self.cuda.ctx_pop()?;

        Ok(Some(Frame {
            width: gpu_frame.width,
            height: gpu_frame.height,
            format: PixelFormat::Bgra8,
            data,
            timestamp: Instant::now(),
        }))
    }

    fn resolution(&self) -> (u32, u32) {
        (self.width, self.height)
    }
}

impl Drop for NvfbcCapture {
    fn drop(&mut self) {
        let mut params = NvFbcDestroyCaptureSessionParams::new();
        unsafe { (self.api.destroy_capture_session)(self.handle, params.as_mut_ptr()) };

        let mut params = NvFbcDestroyHandleParams::new();
        unsafe { (self.api.destroy_handle)(self.handle, params.as_mut_ptr()) };

        tracing::debug!("NVFBC capture session destroyed");
    }
}

fn nvfbc_error_detail(api: &NvFbcFunctionList, handle: NVFBC_SESSION_HANDLE, status: NVFBCSTATUS) -> String {
    let msg_ptr = unsafe { (api.get_last_error_str)(handle) };
    if msg_ptr.is_null() {
        return format!("status={status}");
    }
    let detail = unsafe { CStr::from_ptr(msg_ptr) }.to_string_lossy();
    if detail.is_empty() {
        format!("status={status}")
    } else {
        format!("status={status}, detail={detail}")
    }
}

/// Load NVFBC functions directly via dlsym.
/// This is more robust than NvFBCCreateInstance which has strict version checks.
fn load_nvfbc_api(lib: &DynLib) -> Result<NvFbcFunctionList> {
    unsafe {
        Ok(NvFbcFunctionList {
            get_last_error_str: lib.sym("NvFBCGetLastErrorStr").context("NvFBCGetLastErrorStr")?,
            create_handle: lib.sym("NvFBCCreateHandle").context("NvFBCCreateHandle")?,
            destroy_handle: lib.sym("NvFBCDestroyHandle").context("NvFBCDestroyHandle")?,
            get_status: lib.sym("NvFBCGetStatus").context("NvFBCGetStatus")?,
            create_capture_session: lib.sym("NvFBCCreateCaptureSession").context("NvFBCCreateCaptureSession")?,
            destroy_capture_session: lib.sym("NvFBCDestroyCaptureSession").context("NvFBCDestroyCaptureSession")?,
            to_cuda_setup: lib.sym("NvFBCToCudaSetUp").context("NvFBCToCudaSetUp")?,
            to_cuda_grab_frame: lib.sym("NvFBCToCudaGrabFrame").context("NvFBCToCudaGrabFrame")?,
            bind_context: lib.sym("NvFBCBindContext").context("NvFBCBindContext")?,
            release_context: lib.sym("NvFBCReleaseContext").context("NvFBCReleaseContext")?,
        })
    }
}
