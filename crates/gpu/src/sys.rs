//! Raw C type definitions for CUDA, NVENC, and NVFBC APIs.
//!
//! Struct layouts verified against NVIDIA Video Codec SDK 12.1 headers
//! using sizeof/offsetof on the target A40 machine (driver 535, CUDA 12.2).
//!
//! Opaque structs use byte arrays with exact sizes. Field access is via
//! offset-based methods — only the fields we actually use are exposed.

#![allow(
    non_camel_case_types,
    non_snake_case,
    dead_code,
    clippy::upper_case_acronyms,
    clippy::missing_transmute_annotations,
    clippy::new_without_default,
)]

use std::ffi::c_void;

// ============================================================
// CUDA Driver API types
// ============================================================

pub type CUresult = i32;
pub type CUdevice = i32;
pub type CUcontext = *mut c_void;
/// CUDA device pointer — u64 on 64-bit systems.
pub type CUdeviceptr = u64;

pub const CUDA_SUCCESS: CUresult = 0;

// ============================================================
// NVENC types — SDK 12.1 (driver 535+)
// ============================================================

pub type NVENCSTATUS = u32;

pub const NV_ENC_SUCCESS: NVENCSTATUS = 0;
pub const NV_ENC_ERR_INVALID_VERSION: NVENCSTATUS = 15;

/// NVENCAPI_VERSION = 12 | (1 << 24) for SDK 12.1
const NVENCAPI_VERSION: u32 = 0x0100_000C;

/// Struct version: NVENCAPI_VERSION | (ver << 16) | (0x7 << 28)
/// Some structs also set bit 31.
const fn sv(ver: u32) -> u32 {
    NVENCAPI_VERSION | (ver << 16) | (0x7 << 28)
}

pub const NV_ENCODE_API_FUNCTION_LIST_VER: u32 = sv(2);              // 0x7102000C
pub const NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER: u32 = sv(1);    // 0x7101000C
pub const NV_ENC_INITIALIZE_PARAMS_VER: u32 = sv(6) | (1 << 31);    // 0xF106000C
pub const NV_ENC_CONFIG_VER: u32 = sv(8) | (1 << 31);               // 0xF108000C
pub const NV_ENC_PRESET_CONFIG_VER: u32 = sv(4) | (1 << 31);        // 0xF104000C
pub const NV_ENC_PIC_PARAMS_VER: u32 = sv(6) | (1 << 31);           // 0xF106000C
pub const NV_ENC_REGISTER_RESOURCE_VER: u32 = sv(4);                 // 0x7104000C
pub const NV_ENC_MAP_INPUT_RESOURCE_VER: u32 = sv(4);                // 0x7104000C
pub const NV_ENC_LOCK_BITSTREAM_VER: u32 = sv(1) | (1 << 31);       // 0xF101000C
pub const NV_ENC_CREATE_BITSTREAM_BUFFER_VER: u32 = sv(1);           // 0x7101000C

// Device type
pub const NV_ENC_DEVICE_TYPE_CUDA: u32 = 1;

// Tuning info
pub const NV_ENC_TUNING_INFO_LOW_LATENCY: u32 = 2;
pub const NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY: u32 = 3;

// Rate control mode
pub const NV_ENC_PARAMS_RC_CBR: u32 = 2;

// Buffer format
pub const NV_ENC_BUFFER_FORMAT_NV12: u32 = 0x0000_0001;
pub const NV_ENC_BUFFER_FORMAT_ARGB: u32 = 0x0100_0000;
pub const NV_ENC_BUFFER_FORMAT_ABGR: u32 = 0x1000_0000;

// Input resource type
pub const NV_ENC_INPUT_RESOURCE_TYPE_CUDADEVICEPTR: u32 = 1;

// Picture struct
pub const NV_ENC_PIC_STRUCT_FRAME: u32 = 1;

// Encode pic flags
pub const NV_ENC_PIC_FLAG_FORCEINTRA: u32 = 1;
pub const NV_ENC_PIC_FLAG_FORCEIDR: u32 = 2;
pub const NV_ENC_PIC_FLAG_EOS: u32 = 16;

// Picture type (from lock bitstream)
pub const NV_ENC_PIC_TYPE_IDR: u32 = 3;
pub const NV_ENC_PIC_TYPE_I: u32 = 2;

// ---- GUIDs ----

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct GUID {
    pub data1: u32,
    pub data2: u16,
    pub data3: u16,
    pub data4: [u8; 8],
}

pub const NV_ENC_CODEC_H264_GUID: GUID = GUID {
    data1: 0x6bc82762, data2: 0x4e63, data3: 0x4ca4,
    data4: [0xaa, 0x85, 0x1e, 0x50, 0xf3, 0x21, 0xf6, 0xbf],
};

pub const NV_ENC_CODEC_HEVC_GUID: GUID = GUID {
    data1: 0x790cdc88, data2: 0x4522, data3: 0x4d7b,
    data4: [0x94, 0x25, 0xbd, 0xa9, 0x97, 0x5f, 0x76, 0x03],
};

pub const NV_ENC_CODEC_PROFILE_AUTOSELECT_GUID: GUID = GUID {
    data1: 0xbfd6f8e7, data2: 0x233c, data3: 0x4341,
    data4: [0x8b, 0x3e, 0x48, 0x18, 0x52, 0x38, 0x03, 0xf4],
};

pub const NV_ENC_PRESET_P4_GUID: GUID = GUID {
    data1: 0x90a7b826, data2: 0xdf06, data3: 0x4862,
    data4: [0xb9, 0xd2, 0xcd, 0x6d, 0x73, 0xa0, 0x86, 0x81],
};

pub const NV_ENC_H264_PROFILE_BASELINE_GUID: GUID = GUID {
    data1: 0x0727bcaa, data2: 0x78c4, data3: 0x4c83,
    data4: [0x8c, 0x2f, 0xef, 0x3d, 0xff, 0x26, 0x7c, 0x6a],
};

pub const NV_ENC_H264_PROFILE_HIGH_GUID: GUID = GUID {
    data1: 0xe7cbc309, data2: 0x4f7a, data3: 0x4b89,
    data4: [0xaf, 0x2a, 0xd5, 0x37, 0xc9, 0x2b, 0xe3, 0x10],
};

pub const NV_ENC_PRESET_P1_GUID: GUID = GUID {
    data1: 0xfc0a8d3e, data2: 0x45f8, data3: 0x4cf8,
    data4: [0x80, 0xc7, 0x29, 0x88, 0x71, 0x59, 0x0e, 0xbf],
};

// ============================================================
// Opaque NVENC structs (accessed by offset)
//
// Each struct is a byte array with the exact SDK 12.1 size.
// We only access the fields we need via typed helper methods.
// ============================================================

macro_rules! opaque_struct {
    ($name:ident, $size:expr) => {
        #[repr(C, align(8))]
        pub struct $name {
            data: [u8; $size],
        }

        impl $name {
            pub fn zeroed() -> Self {
                Self { data: [0u8; $size] }
            }

            pub fn as_mut_ptr(&mut self) -> *mut c_void {
                self.data.as_mut_ptr() as *mut c_void
            }

            pub fn as_ptr(&self) -> *const c_void {
                self.data.as_ptr() as *const c_void
            }

            fn read_u32(&self, offset: usize) -> u32 {
                u32::from_ne_bytes(self.data[offset..offset + 4].try_into().unwrap())
            }

            fn write_u32(&mut self, offset: usize, val: u32) {
                self.data[offset..offset + 4].copy_from_slice(&val.to_ne_bytes());
            }

            fn read_u64(&self, offset: usize) -> u64 {
                u64::from_ne_bytes(self.data[offset..offset + 8].try_into().unwrap())
            }

            fn write_u64(&mut self, offset: usize, val: u64) {
                self.data[offset..offset + 8].copy_from_slice(&val.to_ne_bytes());
            }

            fn read_ptr(&self, offset: usize) -> *mut c_void {
                self.read_u64(offset) as *mut c_void
            }

            fn write_ptr(&mut self, offset: usize, val: *mut c_void) {
                self.write_u64(offset, val as u64);
            }

            fn write_guid(&mut self, offset: usize, guid: &GUID) {
                self.write_u32(offset, guid.data1);
                self.data[offset + 4..offset + 6].copy_from_slice(&guid.data2.to_ne_bytes());
                self.data[offset + 6..offset + 8].copy_from_slice(&guid.data3.to_ne_bytes());
                self.data[offset + 8..offset + 16].copy_from_slice(&guid.data4);
            }
        }
    };
}

// --- NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS (1552 bytes) ---
opaque_struct!(NvEncOpenEncodeSessionExParams, 1552);

impl NvEncOpenEncodeSessionExParams {
    pub fn set_version(&mut self) { self.write_u32(0, NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER); }
    pub fn set_device_type_cuda(&mut self) { self.write_u32(4, NV_ENC_DEVICE_TYPE_CUDA); }
    pub fn set_device(&mut self, ctx: CUcontext) { self.write_ptr(8, ctx); }
    pub fn set_api_version(&mut self) { self.write_u32(24, NVENCAPI_VERSION); }
}

// --- NV_ENC_INITIALIZE_PARAMS (1808 bytes) ---
opaque_struct!(NvEncInitializeParams, 1808);

impl NvEncInitializeParams {
    pub fn set_version(&mut self) { self.write_u32(0, NV_ENC_INITIALIZE_PARAMS_VER); }
    pub fn set_encode_guid(&mut self, guid: &GUID) { self.write_guid(4, guid); }
    pub fn set_preset_guid(&mut self, guid: &GUID) { self.write_guid(20, guid); }
    pub fn set_encode_width(&mut self, w: u32) { self.write_u32(36, w); }
    pub fn set_encode_height(&mut self, h: u32) { self.write_u32(40, h); }
    pub fn set_dar_width(&mut self, w: u32) { self.write_u32(44, w); }
    pub fn set_dar_height(&mut self, h: u32) { self.write_u32(48, h); }
    pub fn set_frame_rate_num(&mut self, n: u32) { self.write_u32(52, n); }
    pub fn set_frame_rate_den(&mut self, d: u32) { self.write_u32(56, d); }
    pub fn set_enable_encode_async(&mut self, v: u32) { self.write_u32(60, v); }
    pub fn set_enable_ptd(&mut self, v: u32) { self.write_u32(64, v); }
    pub fn set_encode_config(&mut self, cfg: *mut c_void) { self.write_ptr(88, cfg); }
    pub fn set_tuning_info(&mut self, v: u32) { self.write_u32(136, v); }
}

// --- NV_ENC_CONFIG (3584 bytes) ---
opaque_struct!(NvEncConfig, 3584);

impl NvEncConfig {
    pub fn set_version(&mut self) { self.write_u32(0, NV_ENC_CONFIG_VER); }
    // profileGUID at offset 4 (16 bytes)
    pub fn set_profile_guid(&mut self, guid: &GUID) { self.write_guid(4, guid); }
    pub fn set_gop_length(&mut self, v: u32) { self.write_u32(20, v); }
    // rcParams starts at offset 40
    pub fn set_rc_mode(&mut self, v: u32) { self.write_u32(40 + 4, v); }
    pub fn set_avg_bitrate(&mut self, v: u32) { self.write_u32(40 + 20, v); }
    pub fn set_max_bitrate(&mut self, v: u32) { self.write_u32(40 + 24, v); }
    // encodeCodecConfig starts at offset 152
    // h264Config.repeatSPSPPS is bit 0 of the uint32_t at offset 152
    pub fn set_repeat_sps_pps(&mut self, enable: bool) {
        let mut v = self.read_u32(152);
        if enable { v |= 1; } else { v &= !1; }
        self.write_u32(152, v);
    }
}

// --- NV_ENC_PRESET_CONFIG (5128 bytes) ---
opaque_struct!(NvEncPresetConfig, 5128);

impl NvEncPresetConfig {
    pub fn set_version(&mut self) { self.write_u32(0, NV_ENC_PRESET_CONFIG_VER); }
    /// Set the inner presetCfg.version (at offset 8)
    pub fn set_config_version(&mut self) { self.write_u32(8, NV_ENC_CONFIG_VER); }
    /// Get a mutable pointer to the inner NV_ENC_CONFIG at offset 8
    pub fn config_ptr(&mut self) -> *mut c_void {
        unsafe { self.data.as_mut_ptr().add(8) as *mut c_void }
    }
    /// Copy the inner config (3584 bytes starting at offset 8) into an NvEncConfig
    pub fn copy_config(&self) -> NvEncConfig {
        let mut cfg = NvEncConfig::zeroed();
        cfg.data.copy_from_slice(&self.data[8..8 + 3584]);
        cfg
    }
}

// --- NV_ENC_PIC_PARAMS (3360 bytes) ---
opaque_struct!(NvEncPicParams, 3360);

impl NvEncPicParams {
    pub fn set_version(&mut self) { self.write_u32(0, NV_ENC_PIC_PARAMS_VER); }
    pub fn set_input_width(&mut self, v: u32) { self.write_u32(4, v); }
    pub fn set_input_height(&mut self, v: u32) { self.write_u32(8, v); }
    pub fn set_input_pitch(&mut self, v: u32) { self.write_u32(12, v); }
    pub fn set_encode_pic_flags(&mut self, v: u32) { self.write_u32(16, v); }
    pub fn set_input_timestamp(&mut self, v: u64) { self.write_u64(24, v); }
    pub fn set_input_buffer(&mut self, v: *mut c_void) { self.write_ptr(40, v); }
    pub fn set_output_bitstream(&mut self, v: *mut c_void) { self.write_ptr(48, v); }
    pub fn set_buffer_fmt(&mut self, v: u32) { self.write_u32(64, v); }
    pub fn set_picture_struct(&mut self, v: u32) { self.write_u32(68, v); }
}

// --- NV_ENC_REGISTER_RESOURCE (1536 bytes) ---
opaque_struct!(NvEncRegisterResource, 1536);

impl NvEncRegisterResource {
    pub fn set_version(&mut self) { self.write_u32(0, NV_ENC_REGISTER_RESOURCE_VER); }
    pub fn set_resource_type(&mut self, v: u32) { self.write_u32(4, v); }
    pub fn set_width(&mut self, v: u32) { self.write_u32(8, v); }
    pub fn set_height(&mut self, v: u32) { self.write_u32(12, v); }
    pub fn set_pitch(&mut self, v: u32) { self.write_u32(16, v); }
    pub fn set_resource_to_register(&mut self, v: *mut c_void) { self.write_ptr(24, v); }
    pub fn registered_resource(&self) -> *mut c_void { self.read_ptr(32) }
    pub fn set_buffer_format(&mut self, v: u32) { self.write_u32(40, v); }
    pub fn set_buffer_usage(&mut self, v: u32) { self.write_u32(44, v); }
}

// --- NV_ENC_MAP_INPUT_RESOURCE (1544 bytes) ---
opaque_struct!(NvEncMapInputResource, 1544);

impl NvEncMapInputResource {
    pub fn set_version(&mut self) { self.write_u32(0, NV_ENC_MAP_INPUT_RESOURCE_VER); }
    pub fn set_registered_resource(&mut self, v: *mut c_void) { self.write_ptr(16, v); }
    pub fn mapped_resource(&self) -> *mut c_void { self.read_ptr(24) }
    pub fn mapped_buffer_fmt(&self) -> u32 { self.read_u32(32) }
}

// --- NV_ENC_LOCK_BITSTREAM (1552 bytes) ---
opaque_struct!(NvEncLockBitstream, 1552);

impl NvEncLockBitstream {
    pub fn set_version(&mut self) { self.write_u32(0, NV_ENC_LOCK_BITSTREAM_VER); }
    pub fn set_output_bitstream(&mut self, v: *mut c_void) { self.write_ptr(8, v); }
    pub fn bitstream_size(&self) -> u32 { self.read_u32(36) }
    pub fn bitstream_ptr(&self) -> *const u8 { self.read_ptr(56) as *const u8 }
    pub fn picture_type(&self) -> u32 { self.read_u32(64) }
}

// --- NV_ENC_CREATE_BITSTREAM_BUFFER (776 bytes) ---
opaque_struct!(NvEncCreateBitstreamBuffer, 776);

impl NvEncCreateBitstreamBuffer {
    pub fn set_version(&mut self) { self.write_u32(0, NV_ENC_CREATE_BITSTREAM_BUFFER_VER); }
    pub fn bitstream_buffer(&self) -> *mut c_void { self.read_ptr(16) }
}

// --- NV_ENCODE_API_FUNCTION_LIST (2552 bytes) ---
//
// Function pointers at specific offsets. We store the whole struct as bytes
// and extract function pointers by offset.
opaque_struct!(NvEncFunctionList, 2552);

/// Type aliases for NVENC function pointers.
pub type FnOpenEncodeSessionEx = unsafe extern "C" fn(
    params: *mut c_void,
    encoder: *mut *mut c_void,
) -> NVENCSTATUS;

pub type FnInitializeEncoder = unsafe extern "C" fn(
    encoder: *mut c_void,
    params: *mut c_void,
) -> NVENCSTATUS;

pub type FnEncodePicture = unsafe extern "C" fn(
    encoder: *mut c_void,
    params: *mut c_void,
) -> NVENCSTATUS;

pub type FnGetEncodePresetConfigEx = unsafe extern "C" fn(
    encoder: *mut c_void,
    encode_guid: GUID,
    preset_guid: GUID,
    tuning_info: u32,
    preset_config: *mut c_void,
) -> NVENCSTATUS;

pub type FnRegisterResource = unsafe extern "C" fn(
    encoder: *mut c_void,
    params: *mut c_void,
) -> NVENCSTATUS;

pub type FnUnregisterResource = unsafe extern "C" fn(
    encoder: *mut c_void,
    resource: *mut c_void,
) -> NVENCSTATUS;

pub type FnMapInputResource = unsafe extern "C" fn(
    encoder: *mut c_void,
    params: *mut c_void,
) -> NVENCSTATUS;

pub type FnUnmapInputResource = unsafe extern "C" fn(
    encoder: *mut c_void,
    resource: *mut c_void,
) -> NVENCSTATUS;

pub type FnLockBitstream = unsafe extern "C" fn(
    encoder: *mut c_void,
    params: *mut c_void,
) -> NVENCSTATUS;

pub type FnUnlockBitstream = unsafe extern "C" fn(
    encoder: *mut c_void,
    buffer: *mut c_void,
) -> NVENCSTATUS;

pub type FnCreateBitstreamBuffer = unsafe extern "C" fn(
    encoder: *mut c_void,
    params: *mut c_void,
) -> NVENCSTATUS;

pub type FnDestroyBitstreamBuffer = unsafe extern "C" fn(
    encoder: *mut c_void,
    buffer: *mut c_void,
) -> NVENCSTATUS;

pub type FnDestroyEncoder = unsafe extern "C" fn(
    encoder: *mut c_void,
) -> NVENCSTATUS;

impl NvEncFunctionList {
    pub fn set_version(&mut self) { self.write_u32(0, NV_ENCODE_API_FUNCTION_LIST_VER); }

    // Function pointer accessors by offset (SDK 12.1 verified)
    pub fn open_encode_session_ex(&self) -> Option<FnOpenEncodeSessionEx> {
        let p = self.read_u64(240);
        if p == 0 { None } else { Some(unsafe { std::mem::transmute(p) }) }
    }
    pub fn initialize_encoder(&self) -> Option<FnInitializeEncoder> {
        let p = self.read_u64(96);
        if p == 0 { None } else { Some(unsafe { std::mem::transmute(p) }) }
    }
    pub fn encode_picture(&self) -> Option<FnEncodePicture> {
        let p = self.read_u64(136);
        if p == 0 { None } else { Some(unsafe { std::mem::transmute(p) }) }
    }
    pub fn get_encode_preset_config_ex(&self) -> Option<FnGetEncodePresetConfigEx> {
        let p = self.read_u64(320);
        if p == 0 { None } else { Some(unsafe { std::mem::transmute(p) }) }
    }
    pub fn register_resource(&self) -> Option<FnRegisterResource> {
        let p = self.read_u64(248);
        if p == 0 { None } else { Some(unsafe { std::mem::transmute(p) }) }
    }
    pub fn unregister_resource(&self) -> Option<FnUnregisterResource> {
        let p = self.read_u64(256);
        if p == 0 { None } else { Some(unsafe { std::mem::transmute(p) }) }
    }
    pub fn map_input_resource(&self) -> Option<FnMapInputResource> {
        let p = self.read_u64(208);
        if p == 0 { None } else { Some(unsafe { std::mem::transmute(p) }) }
    }
    pub fn unmap_input_resource(&self) -> Option<FnUnmapInputResource> {
        let p = self.read_u64(216);
        if p == 0 { None } else { Some(unsafe { std::mem::transmute(p) }) }
    }
    pub fn lock_bitstream(&self) -> Option<FnLockBitstream> {
        let p = self.read_u64(144);
        if p == 0 { None } else { Some(unsafe { std::mem::transmute(p) }) }
    }
    pub fn unlock_bitstream(&self) -> Option<FnUnlockBitstream> {
        let p = self.read_u64(152);
        if p == 0 { None } else { Some(unsafe { std::mem::transmute(p) }) }
    }
    pub fn create_bitstream_buffer(&self) -> Option<FnCreateBitstreamBuffer> {
        let p = self.read_u64(120);
        if p == 0 { None } else { Some(unsafe { std::mem::transmute(p) }) }
    }
    pub fn destroy_bitstream_buffer(&self) -> Option<FnDestroyBitstreamBuffer> {
        let p = self.read_u64(128);
        if p == 0 { None } else { Some(unsafe { std::mem::transmute(p) }) }
    }
    pub fn destroy_encoder(&self) -> Option<FnDestroyEncoder> {
        let p = self.read_u64(224);
        if p == 0 { None } else { Some(unsafe { std::mem::transmute(p) }) }
    }
}

// ============================================================
// NVFBC types — API version 1.7
// ============================================================

pub type NVFBC_SESSION_HANDLE = u64;
pub type NVFBCSTATUS = u32;

pub const NVFBC_SUCCESS: NVFBCSTATUS = 0;
pub const NVFBC_ERR_MUST_RECREATE: NVFBCSTATUS = 16;

const NVFBC_VERSION: u32 = 1 | (7 << 8); // 1.7 — minor | (major << 8)? No: NVFBC_VERSION_MINOR | (NVFBC_VERSION_MAJOR << 8)
// Actually: NVFBC_VERSION = NVFBC_VERSION_MINOR | (NVFBC_VERSION_MAJOR << 8) = 7 | (1 << 8) = 0x107

// NVFBC_STRUCT_VERSION embeds sizeof into the version:
// (uint32_t)(sizeof(typeName) | ((ver) << 16) | (NVFBC_VERSION << 24))
// But NVFBC_VERSION = 0x107, and shifting 24 would overflow.
// Looking closer: NVFBC_VERSION << 24 = 0x07_000000 (only low byte matters at shift 24)
// Actually the header says: NVFBC_VERSION = MINOR | (MAJOR << 8) = 7 | (1 << 8) = 0x107
// And NVFBC_VERSION << 24 = 0x07000000 (since 0x107 << 24 would lose the 1).
// Wait: 0x107 << 24 = 0x07_00_00_00 on 32-bit. The `1` is lost.
// So effectively NVFBC_VERSION << 24 = 7 << 24 = 0x0700_0000.

/// Compute NVFBC struct version: sizeof | (ver << 16) | (NVFBC_VERSION << 24)
/// NVFBC_VERSION = 1.8 = 8 | (1 << 8) = 0x108. Shifted 24: 0x08000000.
const fn nvfbc_sv(size: u32, ver: u32) -> u32 {
    size | (ver << 16) | (0x08 << 24)
}

// Capture type
pub const NVFBC_CAPTURE_SHARED_CUDA: u32 = 1;

// Tracking type
pub const NVFBC_TRACKING_DEFAULT: u32 = 0;
pub const NVFBC_TRACKING_SCREEN: u32 = 2;

// Buffer format
pub const NVFBC_BUFFER_FORMAT_BGRA: u32 = 5;
pub const NVFBC_BUFFER_FORMAT_NV12: u32 = 2;
pub const NVFBC_BUFFER_FORMAT_ARGB: u32 = 0;

// Grab flags
pub const NVFBC_TOCUDA_GRAB_FLAGS_NOFLAGS: u32 = 0;
pub const NVFBC_TOCUDA_GRAB_FLAGS_NOWAIT: u32 = 1;
pub const NVFBC_TOCUDA_GRAB_FLAGS_FORCE_REFRESH: u32 = 2;
pub const NVFBC_TOCUDA_GRAB_FLAGS_NOWAIT_IF_NEW_FRAME_READY: u32 = 4;

// NVFBC_BOOL
pub const NVFBC_TRUE: u32 = 1;
pub const NVFBC_FALSE: u32 = 0;

// ---- NVFBC structs ----
// Sizes verified against nvfbc-sys 0.2.0 bindgen output on the target machine.
// NVFBC version encoding: sizeof | (struct_ver << 16) | (0x08 << 24)

// sizeof=40 (nvfbc-sys), struct_ver=2
opaque_struct!(NvFbcCreateHandleParams, 40);
impl NvFbcCreateHandleParams {
    pub fn new() -> Self {
        let mut s = Self::zeroed();
        s.write_u32(0, nvfbc_sv(40, 2)); // version
        s
    }
    pub fn set_private_data(&mut self, data: *const c_void, size: u32) {
        self.write_ptr(8, data as *mut c_void); // offset 8: privateData
        self.write_u32(16, size);                // offset 16: privateDataSize
    }
}

// sizeof=4, struct_ver=1
opaque_struct!(NvFbcDestroyHandleParams, 8); // pad to 8 for alignment
impl NvFbcDestroyHandleParams {
    pub fn new() -> Self {
        let mut s = Self::zeroed();
        s.write_u32(0, nvfbc_sv(4, 1));
        s
    }
}

// sizeof=4, struct_ver=1
opaque_struct!(NvFbcDestroyCaptureSessionParams, 8);
impl NvFbcDestroyCaptureSessionParams {
    pub fn new() -> Self {
        let mut s = Self::zeroed();
        s.write_u32(0, nvfbc_sv(4, 1));
        s
    }
}

// sizeof=780, struct_ver=2
opaque_struct!(NvFbcGetStatusParams, 780);
impl NvFbcGetStatusParams {
    pub fn new() -> Self {
        let mut s = Self::zeroed();
        s.write_u32(0, nvfbc_sv(780, 2));
        s
    }
    pub fn screen_w(&self) -> u32 { self.read_u32(16) }
    pub fn screen_h(&self) -> u32 { self.read_u32(20) }
    pub fn nvfbc_version(&self) -> u32 { self.read_u32(772) }
    pub fn can_create_now(&self) -> bool { self.read_u32(12) != 0 }
}

// sizeof=64, struct_ver=6
opaque_struct!(NvFbcCreateCaptureSessionParams, 64);
impl NvFbcCreateCaptureSessionParams {
    pub fn new() -> Self {
        let mut s = Self::zeroed();
        s.write_u32(0, nvfbc_sv(64, 6));
        s
    }
    pub fn set_capture_type(&mut self, v: u32) { self.write_u32(4, v); }
    pub fn set_tracking_type(&mut self, v: u32) { self.write_u32(8, v); }
    pub fn set_with_cursor(&mut self, v: u32) { self.write_u32(40, v); }
    pub fn set_sampling_rate_ms(&mut self, v: u32) { self.write_u32(52, v); }
    pub fn set_push_model(&mut self, v: u32) { self.write_u32(56, v); }
}

// sizeof=8, struct_ver=1
opaque_struct!(NvFbcToCudaSetupParams, 8);
impl NvFbcToCudaSetupParams {
    pub fn new(format: u32) -> Self {
        let mut s = Self::zeroed();
        s.write_u32(0, nvfbc_sv(8, 1));
        s.write_u32(4, format);
        s
    }
}

// sizeof=48 (from nvfbc-sys)
#[repr(C)]
pub struct NvFbcFrameGrabInfo {
    pub width: u32,
    pub height: u32,
    pub byte_size: u32,
    pub current_frame: u32,
    pub is_new_frame: u32,
    _pad0: u32,
    pub timestamp_us: u64,
    pub missed_frames: u32,
    pub required_post_processing: u32,
    pub direct_capture: u32,
    _pad1: u32, // pad to 48 bytes
}

// sizeof=32, struct_ver=2
opaque_struct!(NvFbcToCudaGrabFrameParams, 32);
impl NvFbcToCudaGrabFrameParams {
    pub fn new(info: *mut NvFbcFrameGrabInfo) -> Self {
        let mut s = Self::zeroed();
        s.write_u32(0, nvfbc_sv(32, 2));
        // flags at offset 4 (default 0 = NOFLAGS)
        // pCUDADeviceBuffer at offset 8
        s.write_ptr(16, info as *mut c_void); // pFrameGrabInfo at offset 16
        // dwTimeoutMs at offset 24
        s
    }
    pub fn set_flags(&mut self, v: u32) { self.write_u32(4, v); }
    pub fn set_cuda_device_buffer(&mut self, v: *mut c_void) { self.write_ptr(8, v); }
    pub fn set_timeout_ms(&mut self, v: u32) { self.write_u32(24, v); }
}

// sizeof=4, struct_ver=1
opaque_struct!(NvFbcBindContextParams, 8);
impl NvFbcBindContextParams {
    pub fn new() -> Self {
        let mut s = Self::zeroed();
        s.write_u32(0, nvfbc_sv(4, 1));
        s
    }
}

// sizeof=4, struct_ver=1
opaque_struct!(NvFbcReleaseContextParams, 8);
impl NvFbcReleaseContextParams {
    pub fn new() -> Self {
        let mut s = Self::zeroed();
        s.write_u32(0, nvfbc_sv(4, 1));
        s
    }
}

// ---- NVFBC function pointer types ----

// All NVFBC functions take opaque param pointers — use *mut c_void for flexibility
pub type FnNvFbcGetLastErrorStr = unsafe extern "C" fn(handle: NVFBC_SESSION_HANDLE) -> *const std::ffi::c_char;
pub type FnNvFbcApi = unsafe extern "C" fn(handle: NVFBC_SESSION_HANDLE, params: *mut c_void) -> NVFBCSTATUS;
pub type FnNvFbcCreateHandle = unsafe extern "C" fn(handle: *mut NVFBC_SESSION_HANDLE, params: *mut c_void) -> NVFBCSTATUS;

/// NVFBC function pointers — loaded via dlsym
pub struct NvFbcFunctionList {
    pub get_last_error_str: FnNvFbcGetLastErrorStr,
    pub create_handle: FnNvFbcCreateHandle,
    pub destroy_handle: FnNvFbcApi,
    pub get_status: FnNvFbcApi,
    pub create_capture_session: FnNvFbcApi,
    pub destroy_capture_session: FnNvFbcApi,
    pub to_cuda_setup: FnNvFbcApi,
    pub to_cuda_grab_frame: FnNvFbcApi,
    pub bind_context: FnNvFbcApi,
    pub release_context: FnNvFbcApi,
}
