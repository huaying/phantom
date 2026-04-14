//! DXGI Desktop Duplication → NVENC zero-copy pipeline (Windows only).
//! Entire flow stays on GPU — no CPU readback, no CPU color conversion.
//!
//! Flow: DXGI capture → ID3D11Texture2D (BGRA, GPU) → NVENC encode → H.264 bytes
//! Expected latency: ~3ms at 1080p (vs ~20ms with CPU path).

use crate::dl::DynLib;
use crate::dxgi::DxgiCapture;
use crate::sys::*;
use anyhow::{bail, Context, Result};
use phantom_core::encode::{EncodedFrame, VideoCodec};
use std::ffi::c_void;

type FnNvEncodeAPICreateInstance = unsafe extern "C" fn(list: *mut c_void) -> NVENCSTATUS;

pub struct DxgiNvencPipeline {
    pub capture: DxgiCapture,
    api: NvEncFunctionList,
    encoder: *mut c_void,
    output_buf: *mut c_void,
    registered_resource: *mut c_void,
    pub width: u32,
    pub height: u32,
    force_idr: bool,
    frame_idx: u64,
    /// Saved SPS/PPS NAL units from first keyframe (prepended to subsequent keyframes).
    sps_pps: Vec<u8>,
    _nvenc_lib: DynLib,
}

unsafe impl Send for DxgiNvencPipeline {}

impl DxgiNvencPipeline {
    pub fn new(fps: u32, bitrate_kbps: u32) -> Result<Self> {
        let capture = DxgiCapture::new()?;
        let width = capture.width;
        let height = capture.height;

        // Load NVENC
        let nvenc_lib =
            DynLib::open(&["nvEncodeAPI64.dll"]).context("failed to load nvEncodeAPI64.dll")?;
        let create_instance: FnNvEncodeAPICreateInstance = unsafe {
            nvenc_lib
                .sym("NvEncodeAPICreateInstance")
                .context("NvEncodeAPICreateInstance not found")?
        };
        let mut api = NvEncFunctionList::zeroed();
        api.set_version();
        let status = unsafe { create_instance(api.as_mut_ptr()) };
        if status != NV_ENC_SUCCESS {
            bail!("NvEncodeAPICreateInstance failed: {status}");
        }

        // Open session with D3D11 device
        let mut session_params = NvEncOpenEncodeSessionExParams::zeroed();
        session_params.set_version();
        session_params.set_device_type_directx();
        session_params.set_device(capture.device_ptr());
        session_params.set_api_version();

        let mut encoder: *mut c_void = std::ptr::null_mut();
        let f = api
            .open_encode_session_ex()
            .context("nvEncOpenEncodeSessionEx is null")?;
        let status = unsafe { f(session_params.as_mut_ptr(), &mut encoder) };
        if status != NV_ENC_SUCCESS {
            bail!(
                "nvEncOpenEncodeSessionEx (D3D11) failed: {}",
                nvenc_status_name(status)
            );
        }

        // Get preset config
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
            bail!(
                "nvEncGetEncodePresetConfigEx failed: {}",
                nvenc_status_name(status)
            );
        }

        // Customize config
        let mut config = preset_config.copy_config();
        config.set_profile_guid(&NV_ENC_H264_PROFILE_BASELINE_GUID);
        config.set_gop_length(u32::MAX);
        config.set_rc_mode(NV_ENC_PARAMS_RC_CBR);
        config.set_avg_bitrate(bitrate_kbps * 1000);
        config.set_max_bitrate(bitrate_kbps * 1000 * 2);
        config.set_repeat_sps_pps(true);

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
        init_params.set_enable_ptd(1);
        init_params.set_encode_config(config.as_mut_ptr());
        init_params.set_tuning_info(NV_ENC_TUNING_INFO_LOW_LATENCY);

        let f = api
            .initialize_encoder()
            .context("nvEncInitializeEncoder is null")?;
        let status = unsafe { f(encoder, init_params.as_mut_ptr()) };
        if status != NV_ENC_SUCCESS {
            bail!(
                "nvEncInitializeEncoder (D3D11) failed: {}",
                nvenc_status_name(status)
            );
        }

        // Create output bitstream buffer
        let mut buf_params = NvEncCreateBitstreamBuffer::zeroed();
        buf_params.set_version();
        let f = api
            .create_bitstream_buffer()
            .context("nvEncCreateBitstreamBuffer is null")?;
        let status = unsafe { f(encoder, buf_params.as_mut_ptr()) };
        if status != NV_ENC_SUCCESS {
            bail!(
                "nvEncCreateBitstreamBuffer failed: {}",
                nvenc_status_name(status)
            );
        }
        let output_buf = buf_params.bitstream_buffer();

        // Register the staging texture with NVENC (once)
        let mut reg = NvEncRegisterResource::zeroed();
        reg.set_version();
        reg.set_resource_type(NV_ENC_INPUT_RESOURCE_TYPE_DIRECTX);
        reg.set_width(width);
        reg.set_height(height);
        reg.set_resource_to_register(capture.texture_ptr());
        reg.set_buffer_format(NV_ENC_BUFFER_FORMAT_ARGB);
        reg.set_pitch(0); // driver infers from D3D11 texture

        let f = api
            .register_resource()
            .context("nvEncRegisterResource is null")?;
        let status = unsafe { f(encoder, reg.as_mut_ptr()) };
        if status != NV_ENC_SUCCESS {
            bail!(
                "nvEncRegisterResource (D3D11) failed: {}",
                nvenc_status_name(status)
            );
        }
        let registered_resource = reg.registered_resource();

        tracing::info!(
            width,
            height,
            fps,
            bitrate_kbps,
            "DXGI→NVENC zero-copy pipeline initialized"
        );

        Ok(Self {
            capture,
            api,
            encoder,
            output_buf,
            registered_resource,
            width,
            height,
            force_idr: true,
            frame_idx: 0,
            sps_pps: Vec::new(),
            _nvenc_lib: nvenc_lib,
        })
    }

    /// Capture a frame and encode it. Returns None if no new frame (static desktop).
    pub fn capture_and_encode(&mut self) -> Result<Option<EncodedFrame>> {
        if !self.capture.capture()? {
            return Ok(None);
        }

        // Map the registered resource
        let mut map = NvEncMapInputResource::zeroed();
        map.set_version();
        map.set_registered_resource(self.registered_resource);
        let f = self
            .api
            .map_input_resource()
            .context("nvEncMapInputResource is null")?;
        let status = unsafe { f(self.encoder, map.as_mut_ptr()) };
        if status != NV_ENC_SUCCESS {
            bail!(
                "nvEncMapInputResource failed: {}",
                nvenc_status_name(status)
            );
        }
        let mapped = map.mapped_resource();

        // Encode
        let mut pic = NvEncPicParams::zeroed();
        pic.set_version();
        pic.set_input_buffer(mapped);
        pic.set_output_bitstream(self.output_buf);
        pic.set_buffer_fmt(NV_ENC_BUFFER_FORMAT_ARGB);
        pic.set_input_width(self.width);
        pic.set_input_height(self.height);
        pic.set_input_pitch(self.width * 4);
        pic.set_picture_struct(NV_ENC_PIC_STRUCT_FRAME);
        if self.force_idr {
            pic.set_encode_pic_flags(NV_ENC_PIC_FLAG_FORCEIDR);
            self.force_idr = false;
        }
        self.frame_idx += 1;

        let f = self
            .api
            .encode_picture()
            .context("nvEncEncodePicture is null")?;
        let status = unsafe { f(self.encoder, pic.as_mut_ptr()) };
        if status != NV_ENC_SUCCESS {
            // Unmap before returning error
            if let Some(f) = self.api.unmap_input_resource() {
                unsafe { f(self.encoder, mapped) };
            }
            bail!("nvEncEncodePicture failed: {}", nvenc_status_name(status));
        }

        // Lock bitstream and read result
        let mut lock = NvEncLockBitstream::zeroed();
        lock.set_version();
        lock.set_output_bitstream(self.output_buf);
        let f = self
            .api
            .lock_bitstream()
            .context("nvEncLockBitstream is null")?;
        let status = unsafe { f(self.encoder, lock.as_mut_ptr()) };
        if status != NV_ENC_SUCCESS {
            if let Some(f) = self.api.unmap_input_resource() {
                unsafe { f(self.encoder, mapped) };
            }
            bail!("nvEncLockBitstream failed: {}", nvenc_status_name(status));
        }

        let size = lock.bitstream_size() as usize;
        let ptr = lock.bitstream_ptr();
        let pic_type = lock.picture_type();
        let mut data = unsafe { std::slice::from_raw_parts(ptr, size) }.to_vec();
        let is_keyframe = pic_type == NV_ENC_PIC_TYPE_IDR || pic_type == NV_ENC_PIC_TYPE_I;

        // Unlock + unmap
        if let Some(f) = self.api.unlock_bitstream() {
            unsafe { f(self.encoder, self.output_buf) };
        }
        if let Some(f) = self.api.unmap_input_resource() {
            unsafe { f(self.encoder, mapped) };
        }

        // SPS/PPS handling: NVENC only outputs SPS/PPS on first encode after init.
        // set_repeat_sps_pps is unreliable across drivers. Save and prepend manually.
        if is_keyframe {
            let has_sps = data
                .windows(5)
                .any(|w| w[0..4] == [0, 0, 0, 1] && (w[4] & 0x1F) == 7);
            if has_sps && self.sps_pps.is_empty() {
                if let Some(idr_pos) = data
                    .windows(5)
                    .position(|w| w[0..4] == [0, 0, 0, 1] && (w[4] & 0x1F) == 5)
                {
                    self.sps_pps = data[..idr_pos].to_vec();
                    tracing::debug!(len = self.sps_pps.len(), "DxgiNvenc: saved SPS/PPS");
                }
            } else if !has_sps && !self.sps_pps.is_empty() {
                let mut with_sps = self.sps_pps.clone();
                with_sps.extend_from_slice(&data);
                data = with_sps;
            }
        }

        Ok(Some(EncodedFrame {
            codec: VideoCodec::H264,
            data,
            is_keyframe,
        }))
    }

    pub fn force_keyframe(&mut self) {
        self.force_idr = true;
    }

    /// Recreate NVENC encoder for new session (ensures SPS/PPS in first keyframe).
    pub fn reset_for_new_session(&mut self) -> Result<()> {
        // Unregister old resource
        if let Some(f) = self.api.unregister_resource() {
            unsafe { f(self.encoder, self.registered_resource) };
        }
        // Destroy old encoder
        if let Some(f) = self.api.destroy_bitstream_buffer() {
            unsafe { f(self.encoder, self.output_buf) };
        }
        if let Some(f) = self.api.destroy_encoder() {
            unsafe { f(self.encoder) };
        }

        // Recreate (reuse the same DxgiCapture + NvEncFunctionList)
        let mut session_params = NvEncOpenEncodeSessionExParams::zeroed();
        session_params.set_version();
        session_params.set_device_type_directx();
        session_params.set_device(self.capture.device_ptr());
        session_params.set_api_version();

        let mut encoder: *mut c_void = std::ptr::null_mut();
        let f = self.api.open_encode_session_ex().context("open session")?;
        let status = unsafe { f(session_params.as_mut_ptr(), &mut encoder) };
        if status != NV_ENC_SUCCESS {
            bail!(
                "nvEncOpenEncodeSessionEx reset failed: {}",
                nvenc_status_name(status)
            );
        }
        self.encoder = encoder;

        // Re-init with same settings
        let mut preset_config = NvEncPresetConfig::zeroed();
        preset_config.set_version();
        preset_config.set_config_version();
        let f = self
            .api
            .get_encode_preset_config_ex()
            .context("NVENC API: get_encode_preset_config_ex not loaded")?;
        let status = unsafe {
            f(
                self.encoder,
                NV_ENC_CODEC_H264_GUID,
                NV_ENC_PRESET_P4_GUID,
                NV_ENC_TUNING_INFO_LOW_LATENCY,
                preset_config.as_mut_ptr(),
            )
        };
        if status != NV_ENC_SUCCESS {
            bail!("preset config failed on reset");
        }

        let mut config = preset_config.copy_config();
        config.set_profile_guid(&NV_ENC_H264_PROFILE_BASELINE_GUID);
        config.set_gop_length(u32::MAX);
        config.set_rc_mode(NV_ENC_PARAMS_RC_CBR);
        config.set_avg_bitrate(5_000_000);
        config.set_max_bitrate(10_000_000);
        config.set_repeat_sps_pps(true);

        let mut init_params = NvEncInitializeParams::zeroed();
        init_params.set_version();
        init_params.set_encode_guid(&NV_ENC_CODEC_H264_GUID);
        init_params.set_preset_guid(&NV_ENC_PRESET_P4_GUID);
        init_params.set_encode_width(self.width);
        init_params.set_encode_height(self.height);
        init_params.set_dar_width(self.width);
        init_params.set_dar_height(self.height);
        init_params.set_frame_rate_num(30);
        init_params.set_frame_rate_den(1);
        init_params.set_enable_encode_async(0);
        init_params.set_enable_ptd(1);
        init_params.set_encode_config(config.as_mut_ptr());
        init_params.set_tuning_info(NV_ENC_TUNING_INFO_LOW_LATENCY);

        let f = self
            .api
            .initialize_encoder()
            .context("NVENC API: initialize_encoder not loaded")?;
        let status = unsafe { f(self.encoder, init_params.as_mut_ptr()) };
        if status != NV_ENC_SUCCESS {
            bail!("init encoder failed on reset");
        }

        // Recreate output buffer
        let mut buf_params = NvEncCreateBitstreamBuffer::zeroed();
        buf_params.set_version();
        let f = self
            .api
            .create_bitstream_buffer()
            .context("NVENC API: create_bitstream_buffer not loaded")?;
        let status = unsafe { f(self.encoder, buf_params.as_mut_ptr()) };
        if status != NV_ENC_SUCCESS {
            bail!("create bitstream failed on reset");
        }
        self.output_buf = buf_params.bitstream_buffer();

        // Re-register texture
        let mut reg = NvEncRegisterResource::zeroed();
        reg.set_version();
        reg.set_resource_type(NV_ENC_INPUT_RESOURCE_TYPE_DIRECTX);
        reg.set_width(self.width);
        reg.set_height(self.height);
        reg.set_resource_to_register(self.capture.texture_ptr());
        reg.set_buffer_format(NV_ENC_BUFFER_FORMAT_ARGB);
        reg.set_pitch(0);
        let f = self
            .api
            .register_resource()
            .context("NVENC API: register_resource not loaded")?;
        let status = unsafe { f(self.encoder, reg.as_mut_ptr()) };
        if status != NV_ENC_SUCCESS {
            bail!("register resource failed on reset");
        }
        self.registered_resource = reg.registered_resource();

        self.force_idr = true;
        self.frame_idx = 0;
        self.sps_pps.clear();
        tracing::info!("DXGI→NVENC pipeline reset for new session");
        Ok(())
    }
}

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

impl Drop for DxgiNvencPipeline {
    fn drop(&mut self) {
        if let Some(f) = self.api.unregister_resource() {
            unsafe { f(self.encoder, self.registered_resource) };
        }
        if let Some(f) = self.api.destroy_bitstream_buffer() {
            unsafe { f(self.encoder, self.output_buf) };
        }
        if let Some(f) = self.api.destroy_encoder() {
            unsafe { f(self.encoder) };
        }
    }
}
