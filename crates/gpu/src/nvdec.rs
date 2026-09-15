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
use std::ffi::{c_ulong, c_void};
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
const CUVID_PKT_ENDOFPICTURE: c_ulong = 0x08;

type CUvideodecoder = *mut c_void;
type CUvideoparser = *mut c_void;

// ── CUVID struct layouts (64-bit Linux) ─────────────────────────────────────
//
// These match the NVIDIA Video Codec SDK 12.x headers.
// We use opaque byte arrays with accessor methods to avoid layout mismatches.

/// CUVIDDECODECREATEINFO — passed to cuvidCreateDecoder.
/// Size: 176 bytes on x86_64 Linux (verified with gcc offsetof).
#[repr(C, align(8))]
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
    fn set_display_rect(&mut self, rect: [i32; 4]) {
        for (index, value) in rect.into_iter().enumerate() {
            self.write_i16(80 + index * 2, value as i16);
        }
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
/// SDK 12.2 layout: 136 bytes on 64-bit targets; pUserData starts at 40.
#[repr(C, align(8))]
struct ParserParams {
    data: [u8; 136],
}

impl ParserParams {
    fn zeroed() -> Self {
        Self { data: [0u8; 136] }
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
    // 40: pUserData (void*)
    fn set_user_data(&mut self, ptr: *mut c_void) {
        self.write_ptr(40, ptr);
    }
    // 48: pfnSequenceCallback
    fn set_sequence_callback(&mut self, f: usize) {
        self.write_ptr(48, f as *mut c_void);
    }
    // 56: pfnDecodePicture
    fn set_decode_callback(&mut self, f: usize) {
        self.write_ptr(56, f as *mut c_void);
    }
    // 64: pfnDisplayPicture
    fn set_display_callback(&mut self, f: usize) {
        self.write_ptr(64, f as *mut c_void);
    }
}

/// CUVIDSOURCEDATAPACKET — feed compressed data to parser.
/// SDK fields are unsigned long: 64 bits on Linux, 32 bits on Windows.
#[repr(C)]
struct SourceDataPacket {
    flags: c_ulong,
    payload_size: c_ulong,
    payload: *const u8,
    timestamp: i64,
}

/// CUVIDPROCPARAMS — for cuvidMapVideoFrame.
#[repr(C, align(8))]
struct ProcParams {
    data: [u8; 264],
}
impl ProcParams {
    fn zeroed() -> Self {
        Self { data: [0u8; 264] }
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

/// CUVIDEOFORMAT, SDK 12.2. The coded surface can be larger than the visible
/// rectangle (for example 1920x1088 coded, 1920x1080 displayed).
#[repr(C)]
struct VideoFormat {
    codec: i32,
    frame_rate: [u32; 2],
    progressive_sequence: u8,
    bit_depth_luma_minus8: u8,
    bit_depth_chroma_minus8: u8,
    min_num_decode_surfaces: u8,
    coded_width: u32,
    coded_height: u32,
    display_area: [i32; 4],
    chroma_format: i32,
    bitrate: u32,
    display_aspect_ratio: [i32; 2],
    video_signal_description: [u8; 4],
    seqhdr_data_length: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct DecodeGeometry {
    coded_width: u32,
    coded_height: u32,
    display_area: [i32; 4],
    surfaces: u32,
}

impl DecodeGeometry {
    fn from_format(format: &VideoFormat) -> Result<Self> {
        anyhow::ensure!(
            format.chroma_format == CUDA_VIDEO_CHROMA_FORMAT_420
                && format.bit_depth_luma_minus8 == 0
                && format.bit_depth_chroma_minus8 == 0,
            "NVDEC output requires 8-bit 4:2:0 video"
        );
        let [left, top, right, bottom] = format.display_area;
        anyhow::ensure!(
            left >= 0
                && top >= 0
                && right > left
                && bottom > top
                && right <= i32::from(i16::MAX)
                && bottom <= i32::from(i16::MAX)
                && right as u32 <= format.coded_width
                && bottom as u32 <= format.coded_height
                && (right - left) % 2 == 0
                && (bottom - top) % 2 == 0,
            "invalid NVDEC display rectangle {:?} for {}x{}",
            format.display_area,
            format.coded_width,
            format.coded_height
        );
        Ok(Self {
            coded_width: format.coded_width,
            coded_height: format.coded_height,
            display_area: format.display_area,
            surfaces: u32::from(format.min_num_decode_surfaces).max(8),
        })
    }

    fn dimensions(self) -> (u32, u32) {
        let [left, top, right, bottom] = self.display_area;
        ((right - left) as u32, (bottom - top) as u32)
    }
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
    width: u32,
    height: u32,
    codec: i32,
    geometry: Option<DecodeGeometry>,
    last_error: Option<String>,
    fn_create_decoder: FnCreateDecoder,
    fn_destroy_decoder: FnDestroyDecoder,
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
extern "C" fn on_sequence(user_data: *mut c_void, format: *mut c_void) -> i32 {
    if user_data.is_null() || format.is_null() {
        return 0;
    }
    let state = unsafe { &mut *(user_data as *mut CallbackState) };
    let format = unsafe { &*(format as *const VideoFormat) };
    let result = (|| -> Result<u32> {
        anyhow::ensure!(
            format.codec == state.codec,
            "NVDEC codec changed unexpectedly"
        );
        let geometry = DecodeGeometry::from_format(format)?;
        if state.geometry == Some(geometry) {
            return Ok(geometry.surfaces);
        }
        let (width, height) = geometry.dimensions();
        let mut info = DecodeCreateInfo::zeroed();
        info.set_coded_width(geometry.coded_width);
        info.set_coded_height(geometry.coded_height);
        info.set_num_decode_surfaces(geometry.surfaces);
        info.set_codec_type(state.codec);
        info.set_chroma_format(CUDA_VIDEO_CHROMA_FORMAT_420);
        info.set_output_format(CUDA_VIDEO_SURFACE_FORMAT_NV12);
        info.set_deinterlace_mode(CUDA_VIDEO_DEINTERLACE_WEAVE);
        info.set_target_width(width);
        info.set_target_height(height);
        info.set_num_output_surfaces(2);
        info.set_create_flags(CUDA_VIDEO_CREATE_PREFER_CUVID);
        info.set_display_rect(geometry.display_area);
        let mut replacement = std::ptr::null_mut();
        let status = unsafe { (state.fn_create_decoder)(&mut replacement, info.as_mut_ptr()) };
        if status != 0 {
            if !replacement.is_null() {
                unsafe { (state.fn_destroy_decoder)(replacement) };
            }
            bail!("cuvidCreateDecoder for new sequence failed: {status}");
        }
        if !state.decoder.is_null() {
            unsafe { (state.fn_destroy_decoder)(state.decoder) };
        }
        state.decoder = replacement;
        state.geometry = Some(geometry);
        state.width = width;
        state.height = height;
        state.output_queue.clear();
        tracing::info!(
            width,
            height,
            coded_width = geometry.coded_width,
            coded_height = geometry.coded_height,
            surfaces = geometry.surfaces,
            "NVDEC sequence configured"
        );
        Ok(geometry.surfaces)
    })();
    match result {
        Ok(surfaces) => surfaces as i32,
        Err(error) => {
            state.last_error = Some(error.to_string());
            0
        }
    }
}

/// Decode callback — called for each picture to decode.
extern "C" fn on_decode(user_data: *mut c_void, pic_params: *mut c_void) -> i32 {
    tracing::trace!("NVDEC on_decode callback fired");
    if user_data.is_null() {
        tracing::error!("on_decode: user_data is null!");
        return 0;
    }
    let state = unsafe { &mut *(user_data as *mut CallbackState) };
    if state.decoder.is_null() {
        tracing::error!("on_decode: decoder is null!");
        return 0;
    }
    let status = unsafe { (state.fn_decode_picture)(state.decoder, pic_params) };
    if status != 0 {
        state.last_error = Some(format!("cuvidDecodePicture failed: {status}"));
        return 0;
    }
    1
}

/// Display callback — called when a decoded picture is ready for display.
extern "C" fn on_display(user_data: *mut c_void, disp_info: *mut c_void) -> i32 {
    tracing::trace!("NVDEC on_display callback fired");
    if disp_info.is_null() {
        return 1; // End of stream signal
    }
    if user_data.is_null() {
        return 0;
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
        state.last_error = Some(format!("cuvidMapVideoFrame failed: {status}"));
        return 0;
    }

    let w = state.width as usize;
    let h = state.height as usize;
    if pitch < state.width || dev_ptr == 0 {
        unsafe { (state.fn_unmap_video_frame)(state.decoder, dev_ptr) };
        state.last_error = Some("NVDEC returned an invalid NV12 surface".into());
        return 0;
    }

    // Copy NV12 from GPU to CPU
    let nv12_size = pitch as usize * h * 3 / 2;
    let mut nv12 = vec![0u8; nv12_size];

    // Use cuMemcpyDtoH to copy from device
    let copied = state.cuda.memcpy_dtoh(&mut nv12, dev_ptr);
    if copied.is_ok() {
        // Convert NV12 → RGB32 using SIMD-accelerated conversion
        let rgb = phantom_core::color::nv12_to_rgb32(&nv12, w, h, pitch as usize);
        state.output_queue.push_back(rgb);
    }

    // Unmap
    let unmapped = unsafe { (state.fn_unmap_video_frame)(state.decoder, dev_ptr) };
    if let Err(error) = copied {
        state.last_error = Some(error.to_string());
        return 0;
    }
    if unmapped != 0 {
        state.last_error = Some(format!("cuvidUnmapVideoFrame failed: {unmapped}"));
        return 0;
    }
    1
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
}

impl NvdecDecoder {
    pub fn new(
        cuda: Arc<CudaLib>,
        device_ordinal: i32,
        width: u32,
        height: u32,
        codec: phantom_core::encode::VideoCodec,
    ) -> Result<Self> {
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

        let dev = cuda.device_get(device_ordinal)?;
        let ctx = cuda.ctx_create(dev)?;
        struct ContextGuard {
            cuda: Arc<CudaLib>,
            ctx: CUcontext,
        }
        impl Drop for ContextGuard {
            fn drop(&mut self) {
                if !self.ctx.is_null() {
                    unsafe { self.cuda.ctx_destroy(self.ctx) };
                }
            }
        }
        let mut context_guard = ContextGuard {
            cuda: Arc::clone(&cuda),
            ctx,
        };

        let cuvid_codec = match codec {
            phantom_core::encode::VideoCodec::Av1 => CUDA_VIDEO_CODEC_AV1,
            _ => CUDA_VIDEO_CODEC_H264,
        };

        // cuCtxCreate already made this context current. Pop that binding on
        // success; the guard destroys it (and its binding) on every error.

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
            if !decoder.is_null() {
                unsafe { fn_destroy_decoder(decoder) };
            }
            bail!("cuvidCreateDecoder failed: {status}");
        }

        // Create callback state (leaked — freed in Drop).
        // Use a guard to prevent leaks if code below panics.
        let callback_state = Box::into_raw(Box::new(CallbackState {
            decoder,
            cuda: Arc::clone(&cuda),
            width,
            height,
            codec: cuvid_codec,
            geometry: None,
            last_error: None,
            fn_create_decoder,
            fn_destroy_decoder,
            fn_decode_picture,
            fn_map_video_frame,
            fn_unmap_video_frame,
            output_queue: VecDeque::new(),
        }));

        /// Guard that reclaims the CallbackState Box on drop (panic safety).
        struct CallbackStateGuard {
            state: *mut CallbackState,
            parser: CUvideoparser,
            destroy_parser: FnDestroyParser,
        }
        impl Drop for CallbackStateGuard {
            fn drop(&mut self) {
                if !self.state.is_null() {
                    unsafe {
                        if !self.parser.is_null() {
                            (self.destroy_parser)(self.parser);
                        }
                        let state = Box::from_raw(self.state);
                        if !state.decoder.is_null() {
                            (state.fn_destroy_decoder)(state.decoder);
                        }
                    }
                }
            }
        }
        let mut guard = CallbackStateGuard {
            state: callback_state,
            parser: std::ptr::null_mut(),
            destroy_parser: fn_destroy_parser,
        };

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
        guard.parser = parser;
        if status != 0 {
            // Guards reclaim parser, decoder, callback state and CUDA context.
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

        // Defuse the guard — ownership transfers to Self (freed in Drop)
        guard.state = std::ptr::null_mut();
        context_guard.ctx = std::ptr::null_mut();

        Ok(Self {
            cuda,
            ctx,
            _lib: lib,
            parser,
            callback_state,
            fn_parse_video_data,
            fn_destroy_decoder,
            fn_destroy_parser,
        })
    }

    /// Feed compressed bitstream and get decoded RGB32 frame(s).
    ///
    /// Returns empty Vec if no frame is ready yet (decoder buffering).
    pub fn decode(&mut self, data: &[u8]) -> Result<Vec<u32>> {
        let packet = SourceDataPacket {
            // Each Phantom VideoFrame is one complete access unit. Without
            // this flag, an idle desktop's last picture can stay buffered.
            flags: CUVID_PKT_ENDOFPICTURE,
            payload_size: data.len().try_into().context("NVDEC packet too large")?,
            payload: data.as_ptr(),
            timestamp: 0,
        };

        unsafe { self.cuda.ctx_push(self.ctx)? };
        unsafe { (*self.callback_state).last_error = None };

        let status = unsafe { (self.fn_parse_video_data)(self.parser, &packet) };
        if status != 0 {
            self.cuda.ctx_pop()?;
            let state = unsafe { &mut *self.callback_state };
            state.output_queue.clear();
            let detail = state.last_error.take().unwrap_or_default();
            bail!("cuvidParseVideoData failed: {status}: {detail}");
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

    fn dimensions(&self) -> (u32, u32) {
        let state = unsafe { &*self.callback_state };
        (state.width, state.height)
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
        unsafe { self.cuda.ctx_destroy(self.ctx) };
    }
}

unsafe impl Send for NvdecDecoder {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cuvid_layout_matches_sdk_12_2_headers() {
        // Independently checked with sizeof/offsetof compiled against the
        // NVIDIA headers in FFmpeg/nv-codec-headers tag n12.2.72.0.
        assert_eq!(std::mem::size_of::<ParserParams>(), 136);
        assert_eq!(std::mem::align_of::<ParserParams>(), 8);
        assert_eq!(std::mem::size_of::<ProcParams>(), 264);
        assert_eq!(std::mem::size_of::<VideoFormat>(), 64);
        assert_eq!(std::mem::offset_of!(VideoFormat, coded_width), 16);
        assert_eq!(std::mem::offset_of!(VideoFormat, display_area), 24);
        assert_eq!(std::mem::offset_of!(VideoFormat, chroma_format), 40);
        #[cfg(not(windows))]
        {
            assert_eq!(std::mem::size_of::<SourceDataPacket>(), 32);
            assert_eq!(std::mem::offset_of!(SourceDataPacket, payload_size), 8);
            assert_eq!(std::mem::offset_of!(SourceDataPacket, payload), 16);
            assert_eq!(std::mem::size_of::<DecodeCreateInfo>(), 176);
        }
        let mut parser = ParserParams::zeroed();
        parser.set_user_data(0x1111_usize as *mut c_void);
        parser.set_sequence_callback(0x2222);
        parser.set_decode_callback(0x3333);
        parser.set_display_callback(0x4444);
        for (offset, expected) in [(40, 0x1111_u64), (48, 0x2222), (56, 0x3333), (64, 0x4444)] {
            assert_eq!(
                u64::from_ne_bytes(parser.data[offset..offset + 8].try_into().unwrap()),
                expected
            );
        }
    }

    fn format_1080p() -> VideoFormat {
        let mut format: VideoFormat = unsafe { std::mem::zeroed() };
        format.codec = CUDA_VIDEO_CODEC_H264;
        format.chroma_format = CUDA_VIDEO_CHROMA_FORMAT_420;
        format.coded_width = 1920;
        format.coded_height = 1088;
        format.display_area = [0, 0, 1920, 1080];
        format.min_num_decode_surfaces = 12;
        format
    }

    #[test]
    fn nvdec_uses_visible_crop_and_required_decode_surfaces() {
        let geometry = DecodeGeometry::from_format(&format_1080p()).unwrap();
        assert_eq!(geometry.dimensions(), (1920, 1080));
        assert_eq!(geometry.coded_height, 1088);
        assert_eq!(geometry.surfaces, 12);
        let mut format = format_1080p();
        format.display_area = [8, 4, 1912, 1080];
        assert_eq!(
            DecodeGeometry::from_format(&format).unwrap().dimensions(),
            (1904, 1076)
        );
    }

    #[test]
    fn nvdec_rejects_unsupported_or_out_of_bounds_surfaces() {
        for rect in [
            [0, 0, 1921, 1080],
            [0, 0, 1920, 1090],
            [-2, 0, 1920, 1080],
            [0, 0, 1919, 1080],
            [0, 0, 0, 0],
        ] {
            let mut format = format_1080p();
            format.display_area = rect;
            assert!(DecodeGeometry::from_format(&format).is_err());
        }
        let mut format = format_1080p();
        format.bit_depth_luma_minus8 = 2;
        assert!(DecodeGeometry::from_format(&format).is_err());
        format.bit_depth_luma_minus8 = 0;
        format.chroma_format = 3;
        assert!(DecodeGeometry::from_format(&format).is_err());
    }

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
