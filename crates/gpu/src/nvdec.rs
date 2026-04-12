//! NVDEC hardware video decoder via CUVID API.
//!
//! Architecture:
//! 1. `cuvidCreateVideoParser` — parses H.264/AV1 bitstream, calls back for each picture
//! 2. Parser callbacks invoke `cuvidDecodePicture` and `cuvidMapVideoFrame`
//! 3. Decoded NV12 frames are copied from GPU → CPU and converted to RGB32
//!
//! The parser callback approach is required because CUVID handles all NAL/OBU
//! parsing, reference frame management, and DPB internally.

use std::collections::VecDeque;
use std::ffi::c_void;
use std::sync::Arc;

use anyhow::{bail, Context, Result};

use crate::cuda::CudaLib;
use crate::dl::DynLib;
use crate::sys::*;

// ── Constants ───────────────────────────────────────────────────────────────

const CUDA_VIDEO_CODEC_H264: i32 = 4;
const CUDA_VIDEO_CODEC_AV1: i32 = 11;
const CUDA_VIDEO_SURFACE_FORMAT_NV12: i32 = 0;
const CUDA_VIDEO_CHROMA_FORMAT_420: i32 = 1;
const CUDA_VIDEO_DEINTERLACE_WEAVE: i32 = 0;
const CUDA_VIDEO_CREATE_PREFER_CUVID: u32 = 4;
const CUVID_PKT_TIMESTAMP: u32 = 0x02;

type CUvideodecoder = *mut c_void;
type CUvideoparser = *mut c_void;

// ── CUVID struct layouts (64-bit Linux) ─────────────────────────────────────
//
// These match the NVIDIA Video Codec SDK 12.x headers.
// We use opaque byte arrays with accessor methods to avoid layout mismatches.

/// CUVIDDECODECREATEINFO — passed to cuvidCreateDecoder.
/// Size: 176 bytes on x86_64 Linux (verified with gcc offsetof).
#[repr(C)]
struct DecodeCreateInfo {
    data: [u8; 176],
}

impl DecodeCreateInfo {
    fn zeroed() -> Self {
        Self { data: [0u8; 176] }
    }
    fn write_u32(&mut self, offset: usize, val: u32) {
        self.data[offset..offset + 4].copy_from_slice(&val.to_ne_bytes());
    }
    fn write_u64(&mut self, offset: usize, val: u64) {
        self.data[offset..offset + 8].copy_from_slice(&val.to_ne_bytes());
    }
    fn write_i16(&mut self, offset: usize, val: i16) {
        self.data[offset..offset + 2].copy_from_slice(&val.to_ne_bytes());
    }
    fn as_mut_ptr(&mut self) -> *mut c_void {
        self.data.as_mut_ptr() as *mut c_void
    }

    // Offsets verified via gcc on x86_64 Linux:
    // 0: ulWidth (unsigned long = 8)
    fn set_coded_width(&mut self, v: u32) {
        self.write_u64(0, v as u64);
    }
    // 8: ulHeight
    fn set_coded_height(&mut self, v: u32) {
        self.write_u64(8, v as u64);
    }
    // 16: ulNumDecodeSurfaces
    fn set_num_decode_surfaces(&mut self, v: u32) {
        self.write_u64(16, v as u64);
    }
    // 24: CodecType (enum/int = 4)
    fn set_codec_type(&mut self, v: i32) {
        self.write_u32(24, v as u32);
    }
    // 28: ChromaFormat (enum/int = 4)
    fn set_chroma_format(&mut self, v: i32) {
        self.write_u32(28, v as u32);
    }
    // 80: display_area { short left(2), top(2), right(2), bottom(2) }
    fn set_display_area(&mut self, right: u32, bottom: u32) {
        self.write_i16(80, 0); // left
        self.write_i16(82, 0); // top
        self.write_i16(84, right as i16);
        self.write_i16(86, bottom as i16);
    }
    // 88: OutputFormat (enum/int = 4)
    fn set_output_format(&mut self, v: i32) {
        self.write_u32(88, v as u32);
    }
    // 92: DeinterlaceMode (enum/int = 4)
    fn set_deinterlace_mode(&mut self, v: i32) {
        self.write_u32(92, v as u32);
    }
    // 96: ulTargetWidth
    fn set_target_width(&mut self, v: u32) {
        self.write_u64(96, v as u64);
    }
    // 104: ulTargetHeight
    fn set_target_height(&mut self, v: u32) {
        self.write_u64(104, v as u64);
    }
    // 112: ulNumOutputSurfaces
    fn set_num_output_surfaces(&mut self, v: u32) {
        self.write_u64(112, v as u64);
    }
    // 32: ulCreationFlags (unsigned long = 8)
    fn set_create_flags(&mut self, v: u32) {
        self.write_u64(32, v as u64);
    }
}

/// CUVIDPARSERPARAMS — passed to cuvidCreateVideoParser.
/// Size: 80+ bytes (verified with gcc offsetof).
#[repr(C)]
struct ParserParams {
    data: [u8; 256],
}

impl ParserParams {
    fn zeroed() -> Self {
        Self { data: [0u8; 256] }
    }
    fn write_u32(&mut self, offset: usize, val: u32) {
        self.data[offset..offset + 4].copy_from_slice(&val.to_ne_bytes());
    }
    fn write_ptr(&mut self, offset: usize, ptr: *mut c_void) {
        let bytes = (ptr as u64).to_ne_bytes();
        self.data[offset..offset + 8].copy_from_slice(&bytes);
    }
    fn as_mut_ptr(&mut self) -> *mut c_void {
        self.data.as_mut_ptr() as *mut c_void
    }

    // Offsets verified via gcc on x86_64 Linux:
    // 0: CodecType (int)
    fn set_codec_type(&mut self, v: i32) {
        self.write_u32(0, v as u32);
    }
    // 4: ulMaxNumDecodeSurfaces (unsigned int)
    fn set_max_num_decode_surfaces(&mut self, v: u32) {
        self.write_u32(4, v);
    }
    // 16: ulMaxDisplayDelay (unsigned int)
    fn set_max_display_delay(&mut self, v: u32) {
        self.write_u32(16, v);
    }
    // 48: pUserData (void*)
    fn set_user_data(&mut self, ptr: *mut c_void) {
        self.write_ptr(48, ptr);
    }
    // 56: pfnSequenceCallback
    fn set_sequence_callback(&mut self, f: usize) {
        self.write_ptr(56, f as *mut c_void);
    }
    // 64: pfnDecodePicture
    fn set_decode_callback(&mut self, f: usize) {
        self.write_ptr(64, f as *mut c_void);
    }
    // 72: pfnDisplayPicture
    fn set_display_callback(&mut self, f: usize) {
        self.write_ptr(72, f as *mut c_void);
    }
}

/// CUVIDSOURCEDATAPACKET — feed compressed data to parser.
#[repr(C)]
struct SourceDataPacket {
    flags: u32,
    payload_size: u32,
    payload: *const u8,
    timestamp: i64,
}

/// CUVIDPROCPARAMS — for cuvidMapVideoFrame.
#[repr(C)]
struct ProcParams {
    data: [u8; 256],
}
impl ProcParams {
    fn zeroed() -> Self {
        Self { data: [0u8; 256] }
    }
    fn set_progressive_frame(&mut self, v: i32) {
        self.data[0..4].copy_from_slice(&v.to_ne_bytes());
    }
    fn as_mut_ptr(&mut self) -> *mut c_void {
        self.data.as_mut_ptr() as *mut c_void
    }
}

/// CUVIDPARSERDISP INFO — from display callback, offset 0: picture_index(i32)
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct DispInfo {
    picture_index: i32,
    progressive_frame: i32,
    top_field_first: i32,
    repeat_first_field: i32,
    timestamp: i64,
}

// ── Function pointer types ──────────────────────────────────────────────────

type FnCreateDecoder = unsafe extern "C" fn(*mut CUvideodecoder, *mut c_void) -> i32;
type FnDestroyDecoder = unsafe extern "C" fn(CUvideodecoder) -> i32;
type FnDecodePicture = unsafe extern "C" fn(CUvideodecoder, *const c_void) -> i32;
type FnMapVideoFrame =
    unsafe extern "C" fn(CUvideodecoder, i32, *mut u64, *mut u32, *mut c_void) -> i32;
type FnUnmapVideoFrame = unsafe extern "C" fn(CUvideodecoder, u64) -> i32;
type FnCreateParser = unsafe extern "C" fn(*mut CUvideoparser, *mut c_void) -> i32;
type FnDestroyParser = unsafe extern "C" fn(CUvideoparser) -> i32;
type FnParseVideoData = unsafe extern "C" fn(CUvideoparser, *const SourceDataPacket) -> i32;

// ── Shared state for parser callbacks ───────────────────────────────────────

struct CallbackState {
    decoder: CUvideodecoder,
    cuda: Arc<CudaLib>,
    #[allow(dead_code)]
    ctx: CUcontext,
    width: u32,
    height: u32,
    fn_decode_picture: FnDecodePicture,
    fn_map_video_frame: FnMapVideoFrame,
    fn_unmap_video_frame: FnUnmapVideoFrame,
    /// Decoded RGB32 frames ready for consumption.
    output_queue: VecDeque<Vec<u32>>,
}

// ── Parser callbacks (extern "C") ───────────────────────────────────────────
//
// These are called by cuvidParseVideoData. `user_data` points to CallbackState.

/// Sequence callback — called when parser detects stream parameters.
/// Return the number of decode surfaces to allocate.
extern "C" fn on_sequence(user_data: *mut c_void, _format: *mut c_void) -> i32 {
    let _ = user_data;
    // Return number of decode surfaces (must match what we created the decoder with)
    8
}

/// Decode callback — called for each picture to decode.
extern "C" fn on_decode(user_data: *mut c_void, pic_params: *mut c_void) -> i32 {
    let state = unsafe { &*(user_data as *const CallbackState) };
    let status = unsafe { (state.fn_decode_picture)(state.decoder, pic_params) };
    if status != 0 {
        tracing::warn!("cuvidDecodePicture failed: {status}");
        return 0;
    }
    1
}

/// Display callback — called when a decoded picture is ready for display.
/// We map the frame, convert NV12→RGB, and push to the output queue.
extern "C" fn on_display(user_data: *mut c_void, disp_info: *mut c_void) -> i32 {
    if disp_info.is_null() {
        return 1; // End of stream signal
    }
    let state = unsafe { &mut *(user_data as *mut CallbackState) };
    let info = unsafe { &*(disp_info as *const DispInfo) };

    // Map the decoded frame
    let mut dev_ptr: u64 = 0;
    let mut pitch: u32 = 0;
    let mut proc_params = ProcParams::zeroed();
    proc_params.set_progressive_frame(info.progressive_frame);

    let status = unsafe {
        (state.fn_map_video_frame)(
            state.decoder,
            info.picture_index,
            &mut dev_ptr,
            &mut pitch,
            proc_params.as_mut_ptr(),
        )
    };
    if status != 0 {
        tracing::warn!("cuvidMapVideoFrame failed: {status}");
        return 0;
    }

    let w = state.width as usize;
    let h = state.height as usize;

    // Copy NV12 from GPU to CPU
    let nv12_size = pitch as usize * h * 3 / 2;
    let mut nv12 = vec![0u8; nv12_size];

    // Use cuMemcpyDtoH to copy from device
    if let Ok(()) = state.cuda.memcpy_dtoh(&mut nv12, dev_ptr) {
        // Convert NV12 → RGB32
        let rgb = nv12_to_rgb32(&nv12, w, h, pitch as usize);
        state.output_queue.push_back(rgb);
    } else {
        tracing::warn!("cuMemcpyDtoH failed for decoded frame");
    }

    // Unmap
    unsafe { (state.fn_unmap_video_frame)(state.decoder, dev_ptr) };

    1
}

/// Convert NV12 (Y plane + interleaved UV plane) to 0RGB32.
fn nv12_to_rgb32(nv12: &[u8], w: usize, h: usize, pitch: usize) -> Vec<u32> {
    let mut rgb = vec![0u32; w * h];
    let y_plane = &nv12[..pitch * h];
    let uv_plane = &nv12[pitch * h..];

    for row in 0..h {
        for col in 0..w {
            let y = y_plane[row * pitch + col] as f32;
            let uv_row = row / 2;
            let uv_col = (col / 2) * 2;
            let u = uv_plane[uv_row * pitch + uv_col] as f32 - 128.0;
            let v = uv_plane[uv_row * pitch + uv_col + 1] as f32 - 128.0;

            let r = (y + 1.402 * v).clamp(0.0, 255.0) as u32;
            let g = (y - 0.344136 * u - 0.714136 * v).clamp(0.0, 255.0) as u32;
            let b = (y + 1.772 * u).clamp(0.0, 255.0) as u32;

            rgb[row * w + col] = (r << 16) | (g << 8) | b;
        }
    }
    rgb
}

// ── NvdecDecoder ────────────────────────────────────────────────────────────

/// NVDEC hardware decoder using CUVID API.
///
/// Feed compressed bitstream via `decode()`, get decoded RGB32 frames back.
pub struct NvdecDecoder {
    cuda: Arc<CudaLib>,
    ctx: CUcontext,
    _lib: DynLib,
    parser: CUvideoparser,
    /// Leaked Box — the parser callbacks hold a raw pointer to this.
    /// Freed in Drop.
    callback_state: *mut CallbackState,
    fn_parse_video_data: FnParseVideoData,
    fn_destroy_decoder: FnDestroyDecoder,
    fn_destroy_parser: FnDestroyParser,
    #[allow(dead_code)]
    width: u32,
    #[allow(dead_code)]
    height: u32,
}

impl NvdecDecoder {
    pub fn new(
        cuda: Arc<CudaLib>,
        device_ordinal: i32,
        width: u32,
        height: u32,
        codec: phantom_core::encode::VideoCodec,
    ) -> Result<Self> {
        let dev = cuda.device_get(device_ordinal)?;
        let ctx = cuda.ctx_create(dev)?;

        let lib = DynLib::open(&["libnvcuvid.so.1", "libnvcuvid.so"])
            .context("failed to load libnvcuvid")?;

        let fn_create_decoder: FnCreateDecoder = unsafe { lib.sym("cuvidCreateDecoder")? };
        let fn_destroy_decoder: FnDestroyDecoder = unsafe { lib.sym("cuvidDestroyDecoder")? };
        let fn_decode_picture: FnDecodePicture = unsafe { lib.sym("cuvidDecodePicture")? };
        let fn_map_video_frame: FnMapVideoFrame = unsafe { lib.sym("cuvidMapVideoFrame64")? };
        let fn_unmap_video_frame: FnUnmapVideoFrame = unsafe { lib.sym("cuvidUnmapVideoFrame64")? };
        let fn_create_parser: FnCreateParser = unsafe { lib.sym("cuvidCreateVideoParser")? };
        let fn_destroy_parser: FnDestroyParser = unsafe { lib.sym("cuvidDestroyVideoParser")? };
        let fn_parse_video_data: FnParseVideoData = unsafe { lib.sym("cuvidParseVideoData")? };

        let cuvid_codec = match codec {
            phantom_core::encode::VideoCodec::Av1 => CUDA_VIDEO_CODEC_AV1,
            _ => CUDA_VIDEO_CODEC_H264,
        };

        unsafe { cuda.ctx_push(ctx)? };

        // Create decoder
        let mut create_info = DecodeCreateInfo::zeroed();
        create_info.set_coded_width(width);
        create_info.set_coded_height(height);
        create_info.set_num_decode_surfaces(8);
        create_info.set_codec_type(cuvid_codec);
        create_info.set_chroma_format(CUDA_VIDEO_CHROMA_FORMAT_420);
        create_info.set_output_format(CUDA_VIDEO_SURFACE_FORMAT_NV12);
        create_info.set_deinterlace_mode(CUDA_VIDEO_DEINTERLACE_WEAVE);
        create_info.set_target_width(width);
        create_info.set_target_height(height);
        create_info.set_num_output_surfaces(2);
        create_info.set_create_flags(CUDA_VIDEO_CREATE_PREFER_CUVID);
        create_info.set_display_area(width, height);

        let mut decoder: CUvideodecoder = std::ptr::null_mut();
        let status = unsafe { fn_create_decoder(&mut decoder, create_info.as_mut_ptr()) };
        if status != 0 {
            cuda.ctx_pop()?;
            bail!("cuvidCreateDecoder failed: {status}");
        }

        // Create callback state (leaked — freed in Drop)
        let callback_state = Box::into_raw(Box::new(CallbackState {
            decoder,
            cuda: Arc::clone(&cuda),
            ctx,
            width,
            height,
            fn_decode_picture,
            fn_map_video_frame,
            fn_unmap_video_frame,
            output_queue: VecDeque::new(),
        }));

        // Create parser
        let mut parser_params = ParserParams::zeroed();
        parser_params.set_codec_type(cuvid_codec);
        parser_params.set_max_num_decode_surfaces(8);
        parser_params.set_max_display_delay(0); // Low latency: display immediately
        parser_params.set_user_data(callback_state as *mut c_void);
        parser_params.set_sequence_callback(on_sequence as extern "C" fn(_, _) -> _ as usize);
        parser_params.set_decode_callback(on_decode as extern "C" fn(_, _) -> _ as usize);
        parser_params.set_display_callback(on_display as extern "C" fn(_, _) -> _ as usize);

        let mut parser: CUvideoparser = std::ptr::null_mut();
        let status = unsafe { fn_create_parser(&mut parser, parser_params.as_mut_ptr()) };
        if status != 0 {
            unsafe {
                fn_destroy_decoder(decoder);
                let _ = Box::from_raw(callback_state);
            }
            cuda.ctx_pop()?;
            bail!("cuvidCreateVideoParser failed: {status}");
        }

        cuda.ctx_pop()?;

        let codec_name = match codec {
            phantom_core::encode::VideoCodec::Av1 => "AV1",
            _ => "H.264",
        };
        tracing::info!(
            width,
            height,
            codec = codec_name,
            "NVDEC decoder initialized"
        );

        Ok(Self {
            cuda,
            ctx,
            _lib: lib,
            parser,
            callback_state,
            fn_parse_video_data,
            fn_destroy_decoder,
            fn_destroy_parser,
            width,
            height,
        })
    }

    /// Feed compressed bitstream and get decoded RGB32 frame(s).
    ///
    /// Returns empty Vec if no frame is ready yet (decoder buffering).
    pub fn decode(&mut self, data: &[u8]) -> Result<Vec<u32>> {
        unsafe { self.cuda.ctx_push(self.ctx)? };

        let mut packet = SourceDataPacket {
            flags: CUVID_PKT_TIMESTAMP,
            payload_size: data.len() as u32,
            payload: data.as_ptr(),
            timestamp: 0,
        };

        let status = unsafe { (self.fn_parse_video_data)(self.parser, &mut packet) };
        if status != 0 {
            self.cuda.ctx_pop()?;
            bail!("cuvidParseVideoData failed: {status}");
        }

        // Check if the display callback produced a frame
        let state = unsafe { &mut *self.callback_state };
        let frame = state.output_queue.pop_front().unwrap_or_default();

        self.cuda.ctx_pop()?;
        Ok(frame)
    }
}

impl phantom_core::encode::FrameDecoder for NvdecDecoder {
    fn decode_frame(&mut self, data: &[u8]) -> Result<Vec<u32>> {
        self.decode(data)
    }
}

impl Drop for NvdecDecoder {
    fn drop(&mut self) {
        let _ = unsafe { self.cuda.ctx_push(self.ctx) };
        if !self.parser.is_null() {
            unsafe { (self.fn_destroy_parser)(self.parser) };
        }
        let state = unsafe { &*self.callback_state };
        if !state.decoder.is_null() {
            unsafe { (self.fn_destroy_decoder)(state.decoder) };
        }
        // Reclaim the leaked callback state
        unsafe {
            let _ = Box::from_raw(self.callback_state);
        }
        let _ = self.cuda.ctx_pop();
    }
}

unsafe impl Send for NvdecDecoder {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_nvdec_create() {
        let cuda = match CudaLib::load() {
            Ok(c) => Arc::new(c),
            Err(e) => {
                eprintln!("CUDA not available: {e}");
                return;
            }
        };
        match NvdecDecoder::new(cuda, 0, 320, 240, phantom_core::encode::VideoCodec::H264) {
            Ok(_) => eprintln!("NVDEC H264 decoder created OK"),
            Err(e) => eprintln!("NVDEC H264 failed: {e}"),
        }
    }
}
