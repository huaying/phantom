//! NVENC hardware H.264 encoder — implements `FrameEncoder` trait.
//!
//! Pipeline: BGRA CPU frame → NV12 conversion (CPU) → cuMemcpyHtoD → NVENC encode (GPU)
//! With NVFBC: CUdeviceptr (GPU) → NVENC encode (GPU) — zero CPU copy.

use crate::cuda::CudaLib;
use crate::dl::DynLib;
use crate::sys::*;
use anyhow::{bail, Context, Result};
use phantom_core::encode::{EncodedFrame, FrameEncoder, VideoCodec};
use phantom_core::frame::Frame;
use std::ffi::c_void;
use std::sync::Arc;

/// Entry point symbol: populates the NVENC function pointer table.
type FnNvEncodeAPICreateInstance = unsafe extern "C" fn(list: *mut c_void) -> NVENCSTATUS;

pub struct NvencEncoder {
    cuda: Arc<CudaLib>,
    ctx: CUcontext,
    api: NvEncFunctionList,
    encoder: *mut c_void,
    output_buf: *mut c_void,
    width: u32,
    height: u32,
    /// Persistent GPU buffer for NV12 input (reused across frames).
    device_buf: CUdeviceptr,
    _device_buf_size: usize,
    /// CPU-side NV12 buffer (reused across frames).
    nv12_buf: Vec<u8>,
    force_idr: bool,
    frame_idx: u64,
    /// Saved SPS/PPS NAL units from the first keyframe.
    /// Prepended to subsequent keyframes that don't include them.
    sps_pps: Vec<u8>,
    owns_ctx: bool,
    _nvenc_lib: DynLib,
}

// NVENC encoder handle is not thread-safe, but we only use it from one thread.
unsafe impl Send for NvencEncoder {}

fn nvenc_status_name(status: NVENCSTATUS) -> &'static str {
    match status {
        NV_ENC_SUCCESS => "NV_ENC_SUCCESS",
        NV_ENC_ERR_INVALID_VERSION => "NV_ENC_ERR_INVALID_VERSION",
        10 => "NV_ENC_ERR_INVALID_PARAM",
        23 => "NV_ENC_ERR_RESOURCE_REGISTER_FAILED",
        26 => "NV_ENC_ERR_MAP_FAILED",
        _ => "NV_ENC_ERR_UNKNOWN",
    }
}

impl NvencEncoder {
    pub fn new(
        cuda: Arc<CudaLib>,
        device_ordinal: i32,
        width: u32,
        height: u32,
        fps: u32,
        bitrate_kbps: u32,
    ) -> Result<Self> {
        let dev = cuda.device_get(device_ordinal)?;
        let ctx = cuda.ctx_create(dev)?;
        unsafe { Self::with_context(cuda, ctx, true, width, height, fps, bitrate_kbps) }
    }

    /// Create encoder using an existing CUDA context (e.g., shared with NVFBC).
    ///
    /// # Safety
    /// `ctx` must be a valid CUDA context.
    pub unsafe fn with_context(
        cuda: Arc<CudaLib>,
        ctx: CUcontext,
        owns_ctx: bool,
        width: u32,
        height: u32,
        fps: u32,
        bitrate_kbps: u32,
    ) -> Result<Self> {
        // Load NVENC library
        #[cfg(unix)]
        let names = &["libnvidia-encode.so.1", "libnvidia-encode.so"];
        #[cfg(windows)]
        let names = &["nvEncodeAPI64.dll"];
        let nvenc_lib = DynLib::open(names).context("failed to load libnvidia-encode")?;

        let create_instance: FnNvEncodeAPICreateInstance = unsafe {
            nvenc_lib
                .sym("NvEncodeAPICreateInstance")
                .context("NvEncodeAPICreateInstance symbol not found")?
        };

        // Populate function table

        let mut api = NvEncFunctionList::zeroed();
        api.set_version();
        let status = unsafe { create_instance(api.as_mut_ptr()) };

        if status != NV_ENC_SUCCESS {
            bail!("NvEncodeAPICreateInstance failed: {status}");
        }

        // Open encode session
        let mut session_params = NvEncOpenEncodeSessionExParams::zeroed();
        session_params.set_version();
        session_params.set_device_type_cuda();
        session_params.set_device(ctx);
        session_params.set_api_version();

        let mut encoder: *mut c_void = std::ptr::null_mut();
        let f = api
            .open_encode_session_ex()
            .context("nvEncOpenEncodeSessionEx is null")?;
        let status = unsafe { f(session_params.as_mut_ptr(), &mut encoder) };
        if status != NV_ENC_SUCCESS {
            bail!("nvEncOpenEncodeSessionEx failed: {status}");
        }

        // Get preset config (low latency)
        let mut preset_config = NvEncPresetConfig::zeroed();
        preset_config.set_version();
        preset_config.set_config_version();

        let f = api
            .get_encode_preset_config_ex()
            .context("nvEncGetEncodePresetConfigEx is null")?;
        let status = unsafe {
            f(
                encoder,
                NV_ENC_CODEC_H264_GUID,
                NV_ENC_PRESET_P4_GUID,
                NV_ENC_TUNING_INFO_LOW_LATENCY,
                preset_config.as_mut_ptr(),
            )
        };
        if status != NV_ENC_SUCCESS {
            bail!("nvEncGetEncodePresetConfigEx failed: {status}");
        }

        // Extract and customize config
        let mut config = preset_config.copy_config();
        config.set_profile_guid(&NV_ENC_H264_PROFILE_BASELINE_GUID);
        config.set_gop_length(u32::MAX); // infinite GOP (low latency)
        config.set_rc_mode(NV_ENC_PARAMS_RC_CBR);
        config.set_avg_bitrate(bitrate_kbps * 1000);
        config.set_max_bitrate(bitrate_kbps * 1000 * 2);
        // Note: set_repeat_sps_pps(true) was attempted but offset 152 is unreliable
        // across drivers. Instead, we recreate the encoder per session to get SPS/PPS.

        // Initialize encoder
        let mut init_params = NvEncInitializeParams::zeroed();
        init_params.set_version();
        init_params.set_encode_guid(&NV_ENC_CODEC_H264_GUID);
        init_params.set_preset_guid(&NV_ENC_PRESET_P4_GUID);
        init_params.set_encode_width(width);
        init_params.set_encode_height(height);
        init_params.set_dar_width(width);
        init_params.set_dar_height(height);
        init_params.set_frame_rate_num(fps);
        init_params.set_frame_rate_den(1);
        init_params.set_enable_encode_async(0);
        init_params.set_enable_ptd(1); // picture type decision
        init_params.set_encode_config(config.as_mut_ptr());
        init_params.set_tuning_info(NV_ENC_TUNING_INFO_LOW_LATENCY);

        let f = api
            .initialize_encoder()
            .context("nvEncInitializeEncoder is null")?;
        let status = unsafe { f(encoder, init_params.as_mut_ptr()) };
        if status != NV_ENC_SUCCESS {
            bail!("nvEncInitializeEncoder failed: {status}");
        }

        // Create output bitstream buffer
        let mut buf_params = NvEncCreateBitstreamBuffer::zeroed();
        buf_params.set_version();
        let f = api
            .create_bitstream_buffer()
            .context("nvEncCreateBitstreamBuffer is null")?;
        let status = unsafe { f(encoder, buf_params.as_mut_ptr()) };
        if status != NV_ENC_SUCCESS {
            bail!("nvEncCreateBitstreamBuffer failed: {status}");
        }
        let output_buf = buf_params.bitstream_buffer();

        // Allocate persistent GPU buffer for NV12 input
        let nv12_size = (width as usize) * (height as usize) * 3 / 2;
        unsafe { cuda.ctx_push(ctx)? };
        let device_buf = cuda.mem_alloc(nv12_size)?;
        cuda.ctx_pop()?;

        tracing::info!(
            width,
            height,
            fps,
            bitrate_kbps,
            "NVENC H.264 encoder initialized (GPU)"
        );

        Ok(Self {
            cuda,
            ctx,
            api,
            encoder,
            output_buf,
            width,
            height,
            device_buf,
            _device_buf_size: nv12_size,
            nv12_buf: vec![0u8; nv12_size],
            force_idr: true, // first frame is always IDR
            frame_idx: 0,
            sps_pps: Vec::new(),
            owns_ctx,
            _nvenc_lib: nvenc_lib,
        })
    }

    pub fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Encode a raw NV12 buffer that's already on the GPU (CUdeviceptr).
    /// This is the zero-copy path used with NVFBC.
    pub fn encode_device_nv12(
        &mut self,
        device_ptr: CUdeviceptr,
        pitch: u32,
    ) -> Result<EncodedFrame> {
        if pitch == 0 {
            bail!("encode_device_nv12 got pitch=0");
        }
        self.encode_registered(device_ptr, pitch)
    }

    /// Convert BGRA CPU frame to NV12, upload, and encode.
    fn encode_cpu_frame(&mut self, frame: &Frame) -> Result<EncodedFrame> {
        let expected = (self.width as usize) * (self.height as usize) * 4;
        if frame.data.len() != expected {
            tracing::warn!(
                "frame data size mismatch: {} != {} ({}x{}x4)",
                frame.data.len(),
                expected,
                self.width,
                self.height
            );
        }

        phantom_core::color::bgra_to_nv12(
            &frame.data,
            self.width as usize,
            self.height as usize,
            &mut self.nv12_buf,
        );

        // Log first frame's Y plane stats for debugging
        if self.frame_idx == 0 {
            let y_avg = self.nv12_buf[..1000].iter().map(|&v| v as u32).sum::<u32>() / 1000;
            tracing::info!(y_avg, bgra_first_4 = ?&frame.data[..4], "NV12 conversion debug");
        }

        unsafe { self.cuda.ctx_push(self.ctx)? };
        self.cuda.memcpy_htod(self.device_buf, &self.nv12_buf)?;
        self.cuda.ctx_pop()?;

        self.encode_registered(self.device_buf, self.width)
    }

    /// Register a CUDA device pointer with NVENC, map, encode, read bitstream.
    fn encode_registered(&mut self, device_ptr: CUdeviceptr, pitch: u32) -> Result<EncodedFrame> {
        if pitch < self.width {
            bail!("invalid NV12 pitch: {pitch} < width {}", self.width);
        }

        // Only push context if it's not already current (avoids deadlock with NVFBC)
        let current_ctx = self.cuda.ctx_get_current()?;
        let need_push = current_ctx != self.ctx;
        if need_push {
            unsafe { self.cuda.ctx_push(self.ctx)? };
        }

        // Register resource
        let mut reg = NvEncRegisterResource::zeroed();
        reg.set_version();
        reg.set_resource_type(NV_ENC_INPUT_RESOURCE_TYPE_CUDADEVICEPTR);
        reg.set_width(self.width);
        reg.set_height(self.height);
        reg.set_pitch(pitch);
        reg.set_resource_to_register(device_ptr as *mut c_void);
        reg.set_buffer_format(NV_ENC_BUFFER_FORMAT_NV12);
        reg.set_buffer_usage(0); // NV_ENC_INPUT_IMAGE

        tracing::debug!(
            frame_idx = self.frame_idx,
            width = self.width,
            height = self.height,
            pitch,
            device_ptr,
            "nvEncRegisterResource"
        );

        let f = self.api.register_resource().context("register_resource")?;
        let status = unsafe { f(self.encoder, reg.as_mut_ptr()) };
        if status != NV_ENC_SUCCESS {
            if need_push {
                self.cuda.ctx_pop()?;
            }
            bail!(
                "nvEncRegisterResource failed: status={} ({}) width={} height={} pitch={}",
                status,
                nvenc_status_name(status),
                self.width,
                self.height,
                pitch
            );
        }
        let registered = reg.registered_resource();

        // Map input resource
        let mut map = NvEncMapInputResource::zeroed();
        map.set_version();
        map.set_registered_resource(registered);

        let f = self
            .api
            .map_input_resource()
            .context("map_input_resource")?;
        let status = unsafe { f(self.encoder, map.as_mut_ptr()) };
        if status != NV_ENC_SUCCESS {
            // Cleanup: unregister
            if let Some(f) = self.api.unregister_resource() {
                unsafe { f(self.encoder, registered) };
            }
            if need_push {
                self.cuda.ctx_pop()?;
            }
            bail!(
                "nvEncMapInputResource failed: status={} ({})",
                status,
                nvenc_status_name(status)
            );
        }
        let mapped = map.mapped_resource();
        let mapped_fmt = map.mapped_buffer_fmt();
        if mapped_fmt != 0 && mapped_fmt != NV_ENC_BUFFER_FORMAT_NV12 {
            tracing::warn!(
                mapped_fmt,
                expected = NV_ENC_BUFFER_FORMAT_NV12,
                "mapped buffer format differs from NV12"
            );
        }

        // Encode picture
        let mut pic = NvEncPicParams::zeroed();
        pic.set_version();
        pic.set_input_width(self.width);
        pic.set_input_height(self.height);
        pic.set_input_pitch(pitch);
        pic.set_input_buffer(mapped);
        pic.set_output_bitstream(self.output_buf);
        pic.set_buffer_fmt(NV_ENC_BUFFER_FORMAT_NV12);
        pic.set_picture_struct(NV_ENC_PIC_STRUCT_FRAME);
        pic.set_input_timestamp(self.frame_idx);

        if self.force_idr {
            pic.set_encode_pic_flags(NV_ENC_PIC_FLAG_FORCEIDR);
            self.force_idr = false;
        }

        let f = self.api.encode_picture().context("encode_picture")?;
        let status = unsafe { f(self.encoder, pic.as_mut_ptr()) };

        // Lock and read bitstream (even if encode returned error, try to clean up)
        let result = if status == NV_ENC_SUCCESS {
            self.read_bitstream()
        } else {
            Err(anyhow::anyhow!(
                "nvEncEncodePicture failed: status={} ({}) frame_idx={}",
                status,
                nvenc_status_name(status),
                self.frame_idx
            ))
        };

        // Cleanup: unmap, unregister
        if let Some(f) = self.api.unmap_input_resource() {
            unsafe { f(self.encoder, mapped) };
        }
        if let Some(f) = self.api.unregister_resource() {
            unsafe { f(self.encoder, registered) };
        }

        if need_push {
            self.cuda.ctx_pop()?;
        }
        self.frame_idx += 1;
        result
    }

    fn read_bitstream(&mut self) -> Result<EncodedFrame> {
        let mut lock = NvEncLockBitstream::zeroed();
        lock.set_version();
        lock.set_output_bitstream(self.output_buf);

        let f = self.api.lock_bitstream().context("lock_bitstream")?;
        let status = unsafe { f(self.encoder, lock.as_mut_ptr()) };
        if status != NV_ENC_SUCCESS {
            bail!("nvEncLockBitstream failed: {status}");
        }

        let size = lock.bitstream_size() as usize;
        let ptr = lock.bitstream_ptr();
        let pic_type = lock.picture_type();
        let mut data = unsafe { std::slice::from_raw_parts(ptr, size) }.to_vec();
        let is_keyframe = pic_type == NV_ENC_PIC_TYPE_IDR || pic_type == NV_ENC_PIC_TYPE_I;

        // Unlock
        if let Some(f) = self.api.unlock_bitstream() {
            unsafe { f(self.encoder, self.output_buf) };
        }

        // NVENC only outputs SPS/PPS on the very first encode after init.
        // Save it, and prepend to any subsequent keyframe that lacks it.
        if is_keyframe {
            let has_sps = data
                .windows(5)
                .any(|w| w[0..4] == [0, 0, 0, 1] && (w[4] & 0x1f) == 7);
            if has_sps && self.sps_pps.is_empty() {
                // Save everything before the IDR slice (SPS + PPS NALs)
                if let Some(idr_pos) = data
                    .windows(5)
                    .position(|w| w[0..4] == [0, 0, 0, 1] && (w[4] & 0x1f) == 5)
                {
                    self.sps_pps = data[..idr_pos].to_vec();
                    tracing::debug!(
                        sps_pps_len = self.sps_pps.len(),
                        "saved SPS/PPS from first keyframe"
                    );
                }
            } else if !has_sps && !self.sps_pps.is_empty() {
                // Prepend saved SPS/PPS
                let mut with_sps = self.sps_pps.clone();
                with_sps.extend_from_slice(&data);
                data = with_sps;
            }
        }

        Ok(EncodedFrame {
            codec: VideoCodec::H264,
            data,
            is_keyframe,
        })
    }
}

impl FrameEncoder for NvencEncoder {
    fn encode_frame(&mut self, frame: &Frame) -> Result<EncodedFrame> {
        self.encode_cpu_frame(frame)
    }

    fn force_keyframe(&mut self) {
        self.force_idr = true;
    }
}

impl Drop for NvencEncoder {
    fn drop(&mut self) {
        // Free device buffer
        unsafe { self.cuda.ctx_push(self.ctx) }.ok();
        self.cuda.mem_free(self.device_buf);

        // Destroy bitstream buffer
        if let Some(f) = self.api.destroy_bitstream_buffer() {
            unsafe { f(self.encoder, self.output_buf) };
        }

        // Destroy encoder
        if let Some(f) = self.api.destroy_encoder() {
            unsafe { f(self.encoder) };
        }

        self.cuda.ctx_pop().ok();

        // Only destroy CUDA context if we created it
        if self.owns_ctx {
            unsafe { self.cuda.ctx_destroy(self.ctx) };
        }

        tracing::debug!("NVENC encoder destroyed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use phantom_core::color::bgra_to_nv12;

    #[test]
    fn bgra_to_nv12_solid_white() {
        let w = 16;
        let h = 16;
        let bgra = vec![255u8; w * h * 4]; // white BGRA
        let mut nv12 = vec![0u8; w * h * 3 / 2];
        bgra_to_nv12(&bgra, w, h, &mut nv12);

        // Y should be ~235 (BT.601 white)
        let y_avg = nv12[..w * h].iter().map(|&v| v as u32).sum::<u32>() / (w * h) as u32;
        assert!(y_avg > 220 && y_avg < 240, "Y avg={y_avg}, expected ~235");

        // UV should be ~128 (neutral chroma)
        let uv = &nv12[w * h..];
        let uv_avg = uv.iter().map(|&v| v as u32).sum::<u32>() / uv.len() as u32;
        assert!(
            uv_avg > 120 && uv_avg < 136,
            "UV avg={uv_avg}, expected ~128"
        );
    }

    #[test]
    fn bgra_to_nv12_solid_black() {
        let w = 16;
        let h = 16;
        let mut bgra = vec![0u8; w * h * 4];
        // Set alpha to 255
        for i in 0..w * h {
            bgra[i * 4 + 3] = 255;
        }
        let mut nv12 = vec![0u8; w * h * 3 / 2];
        bgra_to_nv12(&bgra, w, h, &mut nv12);

        // Y should be ~16 (BT.601 black)
        let y_avg = nv12[..w * h].iter().map(|&v| v as u32).sum::<u32>() / (w * h) as u32;
        assert!(y_avg < 20, "Y avg={y_avg}, expected ~16");
    }

    #[test]
    fn bgra_to_nv12_correct_size() {
        let w = 1920;
        let h = 1080;
        let bgra = vec![128u8; w * h * 4];
        let mut nv12 = vec![0u8; w * h * 3 / 2];
        bgra_to_nv12(&bgra, w, h, &mut nv12);
        assert_eq!(nv12.len(), w * h * 3 / 2);
    }

    #[test]
    fn nvenc_init_requires_gpu() {
        // This test verifies graceful failure when no GPU is available
        let cuda = CudaLib::load();
        if cuda.is_err() {
            // No GPU — expected on CI/Mac. Just verify it doesn't panic.
            return;
        }
        let cuda = std::sync::Arc::new(cuda.unwrap());
        let result = NvencEncoder::new(cuda, 0, 320, 240, 30, 1000);
        // Should succeed if GPU is available
        assert!(result.is_ok(), "NVENC init failed: {:?}", result.err());
    }
}
