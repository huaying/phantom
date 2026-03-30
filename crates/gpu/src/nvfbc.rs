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
use std::ffi::c_void;
use std::sync::Arc;
use std::time::Instant;

/// Entry point: populates the NVFBC function pointer table.
type FnNvFBCCreateInstance = unsafe extern "C" fn(list: *mut c_void) -> NVFBCSTATUS;

pub struct NvfbcCapture {
    cuda: Arc<CudaLib>,
    ctx: CUcontext,
    api: NvFbcFunctionList,
    handle: NVFBC_SESSION_HANDLE,
    width: u32,
    height: u32,
    /// Format requested from NVFBC (BGRA for CPU path, NV12 for GPU path).
    _buffer_format: u32,
    grab_info: NvFbcFrameGrabInfo,
    _lib: DynLib,
}

unsafe impl Send for NvfbcCapture {}

impl NvfbcCapture {
    /// Create NVFBC capture session outputting BGRA to CUDA device pointer.
    pub fn new(cuda: Arc<CudaLib>, ctx: CUcontext, buffer_format: u32) -> Result<Self> {
        let lib = DynLib::open(&["libnvidia-fbc.so.1", "libnvidia-fbc.so"])
            .context("failed to load libnvidia-fbc")?;

        let create_instance: FnNvFBCCreateInstance = unsafe {
            lib.sym("NvFBCCreateInstance").context("NvFBCCreateInstance")?
        };

        // NVFBC uses a different function list pattern than NVENC.
        // The C struct has dwVersion + function pointers at fixed offsets.
        // We manually populate our Rust struct by loading the C struct into a
        // byte buffer and extracting function pointers.
        let api = load_nvfbc_api(create_instance)?;

        // Create handle
        let mut handle: NVFBC_SESSION_HANDLE = 0;
        let mut params = NvFbcCreateHandleParams::new();
        let status = unsafe { (api.create_handle)(&mut handle, &mut params) };
        if status != NVFBC_SUCCESS {
            bail!("NvFBCCreateHandle failed: {status}");
        }

        // Get status to learn screen dimensions
        let mut status_params = NvFbcGetStatusParams::new();
        let status = unsafe { (api.get_status)(handle, &mut status_params) };
        if status != NVFBC_SUCCESS {
            bail!("NvFBCGetStatus failed: {status}");
        }

        let width = status_params.screen_size.w;
        let height = status_params.screen_size.h;
        tracing::info!(width, height, "NVFBC screen detected");

        // Create capture session (CUDA output, no cursor, push model)
        let mut session_params = NvFbcCreateCaptureSessionParams::new();
        session_params.capture_type = NVFBC_CAPTURE_SHARED_CUDA;
        session_params.tracking_type = NVFBC_TRACKING_DEFAULT;
        session_params.with_cursor = NVFBC_FALSE; // client renders cursor locally
        session_params.push_model = NVFBC_TRUE;
        session_params.allow_direct_capture = NVFBC_TRUE;

        let status = unsafe { (api.create_capture_session)(handle, &mut session_params) };
        if status != NVFBC_SUCCESS {
            bail!("NvFBCCreateCaptureSession failed: {status}");
        }

        // Setup CUDA capture with requested format
        let mut setup = NvFbcToCudaSetupParams::new(buffer_format);
        let status = unsafe { (api.to_cuda_setup)(handle, &mut setup) };
        if status != NVFBC_SUCCESS {
            bail!("NvFBCToCudaSetUp failed: {status}");
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
            _buffer_format: buffer_format,
            grab_info: unsafe { std::mem::zeroed() },
            _lib: lib,
        })
    }

    /// Grab a frame, returning the CUDA device pointer and frame info.
    /// This is the zero-copy path — data stays on GPU.
    pub fn grab_cuda(&mut self) -> Result<Option<GpuFrame>> {
        let mut params = NvFbcToCudaGrabFrameParams::new(&mut self.grab_info);
        params.flags = NVFBC_TOCUDA_GRAB_FLAGS_NOWAIT_IF_NEW_FRAME_READY;

        let status = unsafe { (self.api.to_cuda_grab_frame)(self.handle, &mut params) };
        if status == NVFBC_ERR_MUST_RECREATE {
            bail!("NVFBC must recreate (display mode change)");
        }
        if status != NVFBC_SUCCESS {
            bail!("NvFBCToCudaGrabFrame failed: {status}");
        }

        if self.grab_info.is_new_frame == NVFBC_FALSE {
            return Ok(None);
        }

        let device_ptr = params.cuda_device_buffer as CUdeviceptr;

        Ok(Some(GpuFrame {
            device_ptr,
            width: self.grab_info.width,
            height: self.grab_info.height,
            byte_size: self.grab_info.byte_size,
        }))
    }
}

/// A frame captured on the GPU — data is a CUDA device pointer.
pub struct GpuFrame {
    pub device_ptr: CUdeviceptr,
    pub width: u32,
    pub height: u32,
    pub byte_size: u32,
}

impl FrameCapture for NvfbcCapture {
    /// Grabs a frame from GPU and downloads to CPU memory.
    /// This is the compatibility path for use with CPU encoders or tile differ.
    /// For zero-copy GPU encoding, use `grab_cuda()` + `NvencEncoder::encode_device_nv12()`.
    fn capture(&mut self) -> Result<Option<Frame>> {
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
        unsafe { (self.api.destroy_capture_session)(self.handle, &mut params) };

        let mut params = NvFbcDestroyHandleParams::new();
        unsafe { (self.api.destroy_handle)(self.handle, &mut params) };

        tracing::debug!("NVFBC capture session destroyed");
    }
}

/// Load the NVFBC function list from the C API struct.
///
/// The C struct `NVFBC_API_FUNCTION_LIST` has:
///   offset 0: dwVersion (u32)
///   offset 8+: function pointers (8 bytes each, with some padding/reserved slots)
///
/// We read the raw bytes and extract the function pointers we need.
fn load_nvfbc_api(create_instance: FnNvFBCCreateInstance) -> Result<NvFbcFunctionList> {
    // The NVFBC_API_FUNCTION_LIST struct layout (from NvFBC.h):
    // dwVersion, then function pointers in order:
    //   [0] GetLastErrorStr
    //   [1] CreateHandle
    //   [2] DestroyHandle
    //   [3] GetStatus
    //   [4] CreateCaptureSession
    //   [5] DestroyCaptureSession
    //   [6] ToSysSetUp
    //   [7] ToSysGrabFrame
    //   [8] ToCudaSetUp
    //   [9] ToCudaGrabFrame
    //   [10-12] pad1, pad2, pad3
    //   [13] BindContext
    //   [14] ReleaseContext
    //   [15-18] pad4-7
    //   [19] ToGLSetUp
    //   [20] ToGLGrabFrame

    // Total: dwVersion(4) + pad(4) + 21 pointers(168) = 176 bytes minimum
    // Use a generous buffer
    let mut buf = vec![0u8; 256];

    // Set version: sizeof(buf) | (1 << 16) | (NVFBC_VERSION << 24)
    // But actually we should use the real struct size. The NVFBC version macro
    // embeds sizeof into the version. Since we don't know the exact size,
    // we'll compute it as: count_of_fields * 8 + 8 (version + padding).
    // The struct has 21 pointer slots + version = 176 bytes.
    let struct_size: u32 = 176;
    let version = struct_size | (1 << 16) | (0x07 << 24);
    buf[0..4].copy_from_slice(&version.to_ne_bytes());

    let status = unsafe { create_instance(buf.as_mut_ptr() as *mut c_void) };
    if status != NVFBC_SUCCESS {
        bail!("NvFBCCreateInstance failed: {status}");
    }

    // Extract function pointers (each at offset 8 + index*8)
    let read_fn = |idx: usize| -> u64 {
        let off = 8 + idx * 8;
        u64::from_ne_bytes(buf[off..off + 8].try_into().unwrap())
    };

    let check = |name: &str, val: u64| -> Result<u64> {
        if val == 0 { bail!("NVFBC function {name} is null") }
        Ok(val)
    };

    unsafe {
        Ok(NvFbcFunctionList {
            get_last_error_str: std::mem::transmute(check("GetLastErrorStr", read_fn(0))?),
            create_handle: std::mem::transmute(check("CreateHandle", read_fn(1))?),
            destroy_handle: std::mem::transmute(check("DestroyHandle", read_fn(2))?),
            get_status: std::mem::transmute(check("GetStatus", read_fn(3))?),
            create_capture_session: std::mem::transmute(check("CreateCaptureSession", read_fn(4))?),
            destroy_capture_session: std::mem::transmute(check("DestroyCaptureSession", read_fn(5))?),
            to_cuda_setup: std::mem::transmute(check("ToCudaSetUp", read_fn(8))?),
            to_cuda_grab_frame: std::mem::transmute(check("ToCudaGrabFrame", read_fn(9))?),
        })
    }
}
