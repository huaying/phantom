//! H.264 hardware decoder using Apple VideoToolbox (macOS only).
//! Decode time: ~0.5ms vs ~10ms with OpenH264 software decode.

use anyhow::{bail, Result};
use phantom_core::encode::FrameDecoder;
use std::ffi::c_void;
use std::ptr;
use std::sync::{Arc, Mutex};

// --- Core Foundation types ---
type CFAllocatorRef = *const c_void;
type CFDictionaryRef = *const c_void;
type CFMutableDictionaryRef = *mut c_void;
type CFStringRef = *const c_void;
type CFNumberRef = *const c_void;
type CFTypeRef = *const c_void;
type OSStatus = i32;
type Boolean = u8;

// --- CoreMedia types ---
type CMVideoFormatDescriptionRef = *mut c_void;
type CMSampleBufferRef = *mut c_void;
type CMBlockBufferRef = *mut c_void;

// --- CoreVideo types ---
type CVPixelBufferRef = *mut c_void;

// --- VideoToolbox types ---
type VTDecompressionSessionRef = *mut c_void;

#[repr(C)]
struct CMSampleTimingInfo {
    duration: CMTime,
    presentation_time_stamp: CMTime,
    decode_time_stamp: CMTime,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct CMTime {
    value: i64,
    timescale: i32,
    flags: u32,
    epoch: i64,
}

const K_CM_TIME_INVALID: CMTime = CMTime {
    value: 0,
    timescale: 0,
    flags: 0,
    epoch: 0,
};

#[allow(non_snake_case)]
type VTDecompressionOutputCallback = unsafe extern "C" fn(
    decompressionOutputRefCon: *mut c_void,
    sourceFrameRefCon: *mut c_void,
    status: OSStatus,
    infoFlags: u32,
    imageBuffer: CVPixelBufferRef,
    presentationTimeStamp: CMTime,
    presentationDuration: CMTime,
);

#[repr(C)]
struct VTDecompressionOutputCallbackRecord {
    decompression_output_callback: VTDecompressionOutputCallback,
    decompression_output_ref_con: *mut c_void,
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    static kCFAllocatorDefault: CFAllocatorRef;
    static kCFAllocatorNull: CFAllocatorRef;
    fn CFRelease(cf: CFTypeRef);
    fn CFDictionaryCreateMutable(
        allocator: CFAllocatorRef,
        capacity: isize,
        key_callbacks: *const c_void,
        value_callbacks: *const c_void,
    ) -> CFMutableDictionaryRef;
    fn CFDictionarySetValue(dict: CFMutableDictionaryRef, key: CFTypeRef, value: CFTypeRef);
    fn CFNumberCreate(
        allocator: CFAllocatorRef,
        the_type: isize,
        value_ptr: *const c_void,
    ) -> CFNumberRef;
    static kCFTypeDictionaryKeyCallBacks: c_void;
    static kCFTypeDictionaryValueCallBacks: c_void;
}

#[repr(C)]
struct CMVideoDimensions {
    width: i32,
    height: i32,
}

#[link(name = "CoreMedia", kind = "framework")]
extern "C" {
    fn CMVideoFormatDescriptionCreateFromH264ParameterSets(
        allocator: CFAllocatorRef,
        parameter_set_count: usize,
        parameter_set_pointers: *const *const u8,
        parameter_set_sizes: *const usize,
        nal_unit_header_length: i32,
        format_description_out: *mut CMVideoFormatDescriptionRef,
    ) -> OSStatus;

    fn CMVideoFormatDescriptionGetDimensions(
        video_desc: CMVideoFormatDescriptionRef,
    ) -> CMVideoDimensions;

    fn CMBlockBufferCreateWithMemoryBlock(
        structure_allocator: CFAllocatorRef,
        memory_block: *mut c_void,
        block_length: usize,
        block_allocator: CFAllocatorRef,
        custom_block_source: *const c_void,
        offset_to_data: usize,
        data_length: usize,
        flags: u32,
        block_buffer_out: *mut CMBlockBufferRef,
    ) -> OSStatus;

    fn CMSampleBufferCreate(
        allocator: CFAllocatorRef,
        data_buffer: CMBlockBufferRef,
        data_ready: Boolean,
        make_data_ready_callback: *const c_void,
        make_data_ready_refcon: *const c_void,
        format_description: CMVideoFormatDescriptionRef,
        num_samples: isize,
        num_sample_timing_entries: isize,
        sample_timing_array: *const CMSampleTimingInfo,
        num_sample_size_entries: isize,
        sample_size_array: *const usize,
        sample_buffer_out: *mut CMSampleBufferRef,
    ) -> OSStatus;
}

#[link(name = "CoreVideo", kind = "framework")]
extern "C" {
    static kCVPixelBufferPixelFormatTypeKey: CFStringRef;
    fn CVPixelBufferLockBaseAddress(pixel_buffer: CVPixelBufferRef, lock_flags: u64) -> i32;
    fn CVPixelBufferUnlockBaseAddress(pixel_buffer: CVPixelBufferRef, lock_flags: u64) -> i32;
    fn CVPixelBufferGetBaseAddress(pixel_buffer: CVPixelBufferRef) -> *mut u8;
    fn CVPixelBufferGetBytesPerRow(pixel_buffer: CVPixelBufferRef) -> usize;
    fn CVPixelBufferGetWidth(pixel_buffer: CVPixelBufferRef) -> usize;
    fn CVPixelBufferGetHeight(pixel_buffer: CVPixelBufferRef) -> usize;
}

#[link(name = "VideoToolbox", kind = "framework")]
extern "C" {
    fn VTDecompressionSessionCreate(
        allocator: CFAllocatorRef,
        video_format_description: CMVideoFormatDescriptionRef,
        video_decoder_specification: CFDictionaryRef,
        destination_image_buffer_attributes: CFDictionaryRef,
        output_callback: *const VTDecompressionOutputCallbackRecord,
        decompression_session_out: *mut VTDecompressionSessionRef,
    ) -> OSStatus;

    fn VTDecompressionSessionDecodeFrame(
        session: VTDecompressionSessionRef,
        sample_buffer: CMSampleBufferRef,
        decode_flags: u32,
        source_frame_ref_con: *mut c_void,
        info_flags_out: *mut u32,
    ) -> OSStatus;

    fn VTDecompressionSessionWaitForAsynchronousFrames(
        session: VTDecompressionSessionRef,
    ) -> OSStatus;

    fn VTDecompressionSessionInvalidate(session: VTDecompressionSessionRef);
}

// kCVPixelFormatType_32BGRA = 'BGRA' = 0x42475241
const K_CV_PIXEL_FORMAT_TYPE_32BGRA: i32 = 0x42475241u32 as i32;
// kCFNumberSInt32Type = 3
const K_CF_NUMBER_SINT32_TYPE: isize = 3;

/// Shared buffer for receiving decoded frames from the callback.
type SharedFrame = Arc<Mutex<Option<Vec<u32>>>>;

pub struct VideoToolboxDecoder {
    session: VTDecompressionSessionRef,
    format_desc: CMVideoFormatDescriptionRef,
    width: usize,
    height: usize,
    decoded_frame: SharedFrame,
    /// Saved SPS/PPS for creating format description.
    sps: Vec<u8>,
    pps: Vec<u8>,
}

unsafe impl Send for VideoToolboxDecoder {}

impl VideoToolboxDecoder {
    pub fn new(width: u32, height: u32) -> Result<Self> {
        tracing::info!(
            width,
            height,
            "VideoToolbox decoder created (waiting for SPS/PPS)"
        );
        Ok(Self {
            session: ptr::null_mut(),
            format_desc: ptr::null_mut(),
            width: width as usize,
            height: height as usize,
            decoded_frame: Arc::new(Mutex::new(None)),
            sps: Vec::new(),
            pps: Vec::new(),
        })
    }

    /// Extract SPS and PPS NAL units from an Annex B keyframe.
    /// Handles both 3-byte (00 00 01) and 4-byte (00 00 00 01) start codes.
    fn extract_sps_pps(data: &[u8]) -> (Option<Vec<u8>>, Option<Vec<u8>>) {
        let mut sps = None;
        let mut pps = None;

        // Find all NAL unit start positions
        let mut nal_starts: Vec<usize> = Vec::new();
        let mut i = 0;
        while i + 2 < data.len() {
            if i + 3 < data.len() && data[i..i + 4] == [0, 0, 0, 1] {
                nal_starts.push(i + 4);
                i += 4;
            } else if data[i..i + 3] == [0, 0, 1] {
                nal_starts.push(i + 3);
                i += 3;
            } else {
                i += 1;
            }
        }

        for (idx, &start) in nal_starts.iter().enumerate() {
            if start >= data.len() {
                continue;
            }
            let nal_type = data[start] & 0x1f;
            let end = if idx + 1 < nal_starts.len() {
                // Find the start code before the next NAL
                let next = nal_starts[idx + 1];
                if next >= 4 && data[next - 4..next - 1] == [0, 0, 0] {
                    next - 4
                } else if next >= 3 {
                    next - 3
                } else {
                    next
                }
            } else {
                data.len()
            };
            match nal_type {
                7 => sps = Some(data[start..end].to_vec()),
                8 => pps = Some(data[start..end].to_vec()),
                _ => {}
            }
            if sps.is_some() && pps.is_some() {
                break;
            }
        }

        (sps, pps)
    }

    /// Initialize the VideoToolbox session once we have SPS/PPS.
    fn init_session(&mut self) -> Result<()> {
        if self.session != ptr::null_mut() {
            return Ok(()); // already initialized
        }
        if self.sps.is_empty() || self.pps.is_empty() {
            bail!("SPS/PPS not yet available");
        }

        unsafe {
            // Create format description from SPS + PPS
            let param_sets: [*const u8; 2] = [self.sps.as_ptr(), self.pps.as_ptr()];
            let param_sizes: [usize; 2] = [self.sps.len(), self.pps.len()];
            let mut format_desc: CMVideoFormatDescriptionRef = ptr::null_mut();
            let status = CMVideoFormatDescriptionCreateFromH264ParameterSets(
                kCFAllocatorDefault,
                2,
                param_sets.as_ptr(),
                param_sizes.as_ptr(),
                4, // NAL unit header length
                &mut format_desc,
            );
            if status != 0 {
                bail!("CMVideoFormatDescriptionCreateFromH264ParameterSets failed: {status}");
            }

            // Read actual dimensions from SPS (handles resolution changes)
            let dims = CMVideoFormatDescriptionGetDimensions(format_desc);
            if dims.width > 0 && dims.height > 0 {
                let new_w = dims.width as usize;
                let new_h = dims.height as usize;
                if new_w != self.width || new_h != self.height {
                    tracing::info!(
                        old_w = self.width, old_h = self.height,
                        new_w, new_h,
                        "VideoToolbox: resolution changed from SPS"
                    );
                    self.width = new_w;
                    self.height = new_h;
                }
            }

            // Create image buffer attributes requesting BGRA output
            let attrs = CFDictionaryCreateMutable(
                kCFAllocatorDefault,
                0,
                &kCFTypeDictionaryKeyCallBacks,
                &kCFTypeDictionaryValueCallBacks,
            );
            let pixel_format = K_CV_PIXEL_FORMAT_TYPE_32BGRA;
            let pixel_format_num = CFNumberCreate(
                kCFAllocatorDefault,
                K_CF_NUMBER_SINT32_TYPE,
                &pixel_format as *const i32 as *const c_void,
            );
            CFDictionarySetValue(
                attrs,
                kCVPixelBufferPixelFormatTypeKey as CFTypeRef,
                pixel_format_num as CFTypeRef,
            );
            CFRelease(pixel_format_num as CFTypeRef);

            // Create output callback
            let decoded_frame = Arc::clone(&self.decoded_frame);
            let callback_ref = Box::into_raw(Box::new(decoded_frame));

            let callback_record = VTDecompressionOutputCallbackRecord {
                decompression_output_callback: vt_decode_callback,
                decompression_output_ref_con: callback_ref as *mut c_void,
            };

            let mut session: VTDecompressionSessionRef = ptr::null_mut();
            let status = VTDecompressionSessionCreate(
                kCFAllocatorDefault,
                format_desc,
                ptr::null(),
                attrs as CFDictionaryRef,
                &callback_record,
                &mut session,
            );
            CFRelease(attrs as CFTypeRef);

            if status != 0 {
                CFRelease(format_desc as CFTypeRef);
                bail!("VTDecompressionSessionCreate failed: {status}");
            }

            self.session = session;
            self.format_desc = format_desc;
            tracing::info!(
                width = self.width,
                height = self.height,
                "VideoToolbox H.264 hardware decoder initialized"
            );
            Ok(())
        }
    }

    /// Convert Annex B NAL units to AVCC format (4-byte big-endian length prefix).
    fn annex_b_to_avcc(data: &[u8]) -> Vec<u8> {
        let mut avcc = Vec::with_capacity(data.len());
        let mut i = 0;

        while i < data.len() {
            // Find start code (00 00 00 01 or 00 00 01)
            let (_sc_len, nal_start) = if i + 4 <= data.len() && data[i..i + 4] == [0, 0, 0, 1] {
                (4, i + 4)
            } else if i + 3 <= data.len() && data[i..i + 3] == [0, 0, 1] {
                (3, i + 3)
            } else {
                i += 1;
                continue;
            };

            // Find end of NAL (next start code or end)
            let mut nal_end = data.len();
            for j in nal_start..data.len().saturating_sub(3) {
                if data[j..j + 4] == [0, 0, 0, 1]
                    || (j + 3 <= data.len() && data[j..j + 3] == [0, 0, 1])
                {
                    nal_end = j;
                    break;
                }
            }

            let nal_data = &data[nal_start..nal_end];
            let nal_type = nal_data[0] & 0x1f;

            // Skip SPS (7) and PPS (8) — already in format description
            if nal_type != 7 && nal_type != 8 {
                let len = nal_data.len() as u32;
                avcc.extend_from_slice(&len.to_be_bytes());
                avcc.extend_from_slice(nal_data);
            }

            i = nal_end;
        }

        avcc
    }
}

/// VideoToolbox decode callback — called on arbitrary thread.
unsafe extern "C" fn vt_decode_callback(
    ref_con: *mut c_void,
    _source_ref_con: *mut c_void,
    status: OSStatus,
    _info_flags: u32,
    image_buffer: CVPixelBufferRef,
    _pts: CMTime,
    _duration: CMTime,
) {
    if status != 0 || image_buffer.is_null() {
        return;
    }

    let shared = &*(ref_con as *const SharedFrame);

    CVPixelBufferLockBaseAddress(image_buffer, 1); // read-only
    let base = CVPixelBufferGetBaseAddress(image_buffer);
    let stride = CVPixelBufferGetBytesPerRow(image_buffer);
    let width = CVPixelBufferGetWidth(image_buffer);
    let height = CVPixelBufferGetHeight(image_buffer);

    // Convert BGRA → 0RGB u32 (for softbuffer display)
    let mut rgb32 = Vec::with_capacity(width * height);
    for y in 0..height {
        let row = std::slice::from_raw_parts(base.add(y * stride), width * 4);
        for x in 0..width {
            let b = row[x * 4] as u32;
            let g = row[x * 4 + 1] as u32;
            let r = row[x * 4 + 2] as u32;
            rgb32.push((r << 16) | (g << 8) | b);
        }
    }

    CVPixelBufferUnlockBaseAddress(image_buffer, 1);

    if let Ok(mut guard) = shared.lock() {
        *guard = Some(rgb32);
    }
}

impl FrameDecoder for VideoToolboxDecoder {
    fn decode_frame(&mut self, data: &[u8]) -> Result<Vec<u32>> {
        // Extract SPS/PPS from keyframes
        let (sps, pps) = Self::extract_sps_pps(data);
        if let Some(s) = sps {
            if s != self.sps {
                self.sps = s;
                // SPS changed — need to recreate session
                if !self.session.is_null() {
                    unsafe {
                        VTDecompressionSessionInvalidate(self.session);
                        CFRelease(self.session as CFTypeRef);
                        CFRelease(self.format_desc as CFTypeRef);
                    }
                    self.session = ptr::null_mut();
                    self.format_desc = ptr::null_mut();
                }
            }
        }
        if let Some(p) = pps {
            self.pps = p;
        }

        // Initialize session if needed
        self.init_session()?;

        // Convert Annex B → AVCC
        let avcc = Self::annex_b_to_avcc(data);
        if avcc.is_empty() {
            bail!("no decodable NAL units in frame");
        }

        unsafe {
            // Create CMBlockBuffer
            let mut block_buf: CMBlockBufferRef = ptr::null_mut();
            let status = CMBlockBufferCreateWithMemoryBlock(
                kCFAllocatorDefault,
                avcc.as_ptr() as *mut c_void,
                avcc.len(),
                kCFAllocatorNull, // don't free our Rust Vec memory
                ptr::null(),
                0,
                avcc.len(),
                0,
                &mut block_buf,
            );
            if status != 0 {
                bail!("CMBlockBufferCreateWithMemoryBlock failed: {status}");
            }

            // Create CMSampleBuffer
            let mut sample_buf: CMSampleBufferRef = ptr::null_mut();
            let sample_size = avcc.len();
            let timing = CMSampleTimingInfo {
                duration: K_CM_TIME_INVALID,
                presentation_time_stamp: K_CM_TIME_INVALID,
                decode_time_stamp: K_CM_TIME_INVALID,
            };
            let status = CMSampleBufferCreate(
                kCFAllocatorDefault,
                block_buf,
                1, // data ready
                ptr::null(),
                ptr::null(),
                self.format_desc,
                1, // num samples
                1, // num timing entries
                &timing,
                1, // num size entries
                &sample_size,
                &mut sample_buf,
            );
            CFRelease(block_buf as CFTypeRef);
            if status != 0 {
                bail!("CMSampleBufferCreate failed: {status}");
            }

            // Decode
            let status = VTDecompressionSessionDecodeFrame(
                self.session,
                sample_buf,
                0, // synchronous
                ptr::null_mut(),
                ptr::null_mut(),
            );
            CFRelease(sample_buf as CFTypeRef);
            if status != 0 {
                bail!("VTDecompressionSessionDecodeFrame failed: {status}");
            }

            // Wait for callback
            VTDecompressionSessionWaitForAsynchronousFrames(self.session);

            // Get decoded frame from shared buffer
            let frame = self
                .decoded_frame
                .lock()
                .map_err(|_| anyhow::anyhow!("decoded_frame mutex poisoned"))?
                .take();
            match frame {
                Some(f) => Ok(f),
                None => bail!("VideoToolbox produced no output frame"),
            }
        }
    }

    fn dimensions(&self) -> (u32, u32) {
        (self.width as u32, self.height as u32)
    }
}

impl Drop for VideoToolboxDecoder {
    fn drop(&mut self) {
        unsafe {
            if !self.session.is_null() {
                VTDecompressionSessionInvalidate(self.session);
                CFRelease(self.session as CFTypeRef);
            }
            if !self.format_desc.is_null() {
                CFRelease(self.format_desc as CFTypeRef);
            }
        }
    }
}
