#![cfg(target_os = "macos")]
#![allow(dead_code, unused_imports)]

use super::{DecodeError, LatestFrame, YuvFrame};
use std::ffi::c_void;
use std::sync::{Arc, Mutex};

// ── Raw C type aliases ────────────────────────────────────────────────────────

type OSStatus = i32;
type CVPixelBufferRef = *mut c_void;
type VTSessionRef = *mut c_void; // VTDecompressionSessionRef
type CMFormatDescRef = *mut c_void; // CMVideoFormatDescriptionRef
type CMSampleBufferRef = *mut c_void;
type CMBlockBufferRef = *mut c_void;

// ── CMTime — Apple rational time ──────────────────────────────────────────────

#[repr(C)]
#[derive(Clone, Copy)]
struct CMTime {
    value: i64,     // numerator
    timescale: i32, // denominator
    flags: u32,     // 1 = valid
    epoch: i64,
}

impl CMTime {
    fn from_pts_us(pts_us: u64) -> Self {
        CMTime {
            value: pts_us as i64,
            timescale: 1_000_000,
            flags: 1,
            epoch: 0,
        }
    }
    fn zero() -> Self {
        CMTime {
            value: 0,
            timescale: 1_000_000,
            flags: 1,
            epoch: 0,
        }
    }
}

// ── CMSampleTimingInfo ────────────────────────────────────────────────────────

#[repr(C)]
#[derive(Clone, Copy)]
struct CMSampleTimingInfo {
    duration: CMTime,
    presentation_timestamp: CMTime,
    decode_timestamp: CMTime,
}

// ── VT callback record ────────────────────────────────────────────────────────

#[repr(C)]
struct VTDecompressionOutputCallbackRecord {
    callback: Option<
        unsafe extern "C" fn(
            refcon: *mut c_void,
            source_ref: *mut c_void,
            status: OSStatus,
            info_flags: u32,
            image_buffer: CVPixelBufferRef,
            pts: CMTime,
            duration: CMTime,
        ),
    >,
    refcon: *mut c_void,
}

// ── Linked frameworks ─────────────────────────────────────────────────────────

#[link(name = "VideoToolbox", kind = "framework")]
#[link(name = "CoreMedia", kind = "framework")]
#[link(name = "CoreVideo", kind = "framework")]
extern "C" {
    fn CMVideoFormatDescriptionCreateFromHEVCParameterSets(
        allocator: *const c_void,
        parameter_set_count: usize,
        parameter_set_ptrs: *const *const u8,
        parameter_set_sizes: *const usize,
        nal_unit_header_length: i32,
        extensions: *const c_void,
        format_desc_out: *mut CMFormatDescRef,
    ) -> OSStatus;

    fn VTDecompressionSessionCreate(
        allocator: *const c_void,
        video_format_desc: CMFormatDescRef,
        video_decoder_spec: *const c_void,
        dest_image_buf_attrs: *const c_void,
        output_callback: *const VTDecompressionOutputCallbackRecord,
        session_out: *mut VTSessionRef,
    ) -> OSStatus;

    fn VTDecompressionSessionDecodeFrame(
        session: VTSessionRef,
        sample_buf: CMSampleBufferRef,
        decode_flags: u32,
        source_refcon: *mut c_void,
        info_flags_out: *mut u32,
    ) -> OSStatus;

    fn VTDecompressionSessionInvalidate(session: VTSessionRef);
    fn VTDecompressionSessionRelease(session: VTSessionRef);

    fn CMVideoFormatDescriptionRelease(desc: CMFormatDescRef);

    fn CMBlockBufferCreateWithMemoryBlock(
        allocator: *const c_void,
        memory_block: *mut c_void,
        block_length: usize,
        block_allocator: *const c_void,
        custom_block_source: *const c_void,
        offset_to_data: usize,
        data_length: usize,
        flags: u32,
        block_buf_out: *mut CMBlockBufferRef,
    ) -> OSStatus;

    fn CMSampleBufferCreateReady(
        allocator: *const c_void,
        data_buffer: CMBlockBufferRef,
        format_description: CMFormatDescRef,
        num_samples: i64,
        num_sample_timing: i64,
        sample_timing_arr: *const CMSampleTimingInfo,
        num_sample_sizes: i64,
        sample_sizes_arr: *const usize,
        sample_buf_out: *mut CMSampleBufferRef,
    ) -> OSStatus;

    fn CFRelease(cf: *const c_void);

    // CVPixelBuffer access
    fn CVPixelBufferLockBaseAddress(buf: CVPixelBufferRef, flags: u64) -> OSStatus;
    fn CVPixelBufferUnlockBaseAddress(buf: CVPixelBufferRef, flags: u64) -> OSStatus;
    fn CVPixelBufferGetWidth(buf: CVPixelBufferRef) -> usize;
    fn CVPixelBufferGetHeight(buf: CVPixelBufferRef) -> usize;
    fn CVPixelBufferGetBaseAddressOfPlane(buf: CVPixelBufferRef, plane: usize) -> *mut u8;
    fn CVPixelBufferGetBytesPerRowOfPlane(buf: CVPixelBufferRef, plane: usize) -> usize;
}

pub struct VideoToolboxDecoder {
    session: VTSessionRef,
    format_desc: CMFormatDescRef,
    latest: LatestFrame,
}

unsafe impl Send for VideoToolboxDecoder {}

impl VideoToolboxDecoder {
    pub fn new(
        vps: &[u8],
        sps: &[u8],
        pps: &[u8],
        latest: LatestFrame,
    ) -> Result<Self, DecodeError> {
        unsafe {
            // ── Step 1: create format description ────────────────────────────
            // Pass VPS, SPS, PPS as array of pointers + sizes
            let param_ptrs = [vps.as_ptr(), sps.as_ptr(), pps.as_ptr()];
            let param_sizes = [vps.len(), sps.len(), pps.len()];

            let mut format_desc: CMFormatDescRef = std::ptr::null_mut();
            let status = CMVideoFormatDescriptionCreateFromHEVCParameterSets(
                std::ptr::null(), // allocator = NULL (use default)
                3,                // 3 parameter sets: VPS + SPS + PPS
                param_ptrs.as_ptr(),
                param_sizes.as_ptr(),
                4,                // NAL unit header length = 4 bytes
                std::ptr::null(), // extensions = NULL
                &mut format_desc,
            );
            if status != 0 {
                return Err(DecodeError::InitFailed(format!(
                    "CMVideoFormatDescriptionCreateFromHEVCParameterSets: {}",
                    status
                )));
            }

            // ── Step 2: build callback record ────────────────────────────────
            // Pass Arc pointer as refcon — VT will give it back in callback
            let refcon = Arc::into_raw(latest.clone()) as *mut c_void;
            //           ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
            //           converts Arc to raw pointer WITHOUT dropping it
            //           Arc refcount goes up by 1
            //           we hold this pointer for the lifetime of the session

            let callback_record = VTDecompressionOutputCallbackRecord {
                callback: Some(decompress_callback),
                refcon,
            };

            // ── Step 3: create decompression session ─────────────────────────
            let mut session: VTSessionRef = std::ptr::null_mut();
            let status = VTDecompressionSessionCreate(
                std::ptr::null(), // allocator
                format_desc,      // format description with VPS/SPS/PPS
                std::ptr::null(), // decoder specification (NULL = let VT choose)
                std::ptr::null(), // destination image buffer attributes (NULL = default)
                &callback_record,
                &mut session,
            );
            if status != 0 {
                CFRelease(format_desc as *const c_void);
                return Err(DecodeError::InitFailed(format!(
                    "VTDecompressionSessionCreate: {}",
                    status
                )));
            }

            Ok(Self {
                session,
                format_desc,
                latest,
            })
        }
    }
}

unsafe extern "C" fn decompress_callback(
    refcon: *mut c_void,
    _source_ref: *mut c_void,
    status: OSStatus,
    _info_flags: u32,
    image_buffer: CVPixelBufferRef,
    pts: CMTime,
    _duration: CMTime,
) {
    // ignore: CMTime pts and duration params — add them later
    // for now just extract the frame

    if status != 0 || image_buffer.is_null() {
        return;
    }

    let pts_us = if pts.timescale != 0 {
        (pts.value as u64) * 1_000_000 / pts.timescale as u64
    } else {
        0
    };
    // ── Lock pixel buffer for CPU access ─────────────────────────────────────
    CVPixelBufferLockBaseAddress(image_buffer, 0);

    let width = CVPixelBufferGetWidth(image_buffer) as u32;
    let height = CVPixelBufferGetHeight(image_buffer) as u32;

    // ── Copy Y plane (luma, full resolution) ─────────────────────────────────
    let y_ptr = CVPixelBufferGetBaseAddressOfPlane(image_buffer, 0);
    let y_stride = CVPixelBufferGetBytesPerRowOfPlane(image_buffer, 0);
    let mut y_plane = Vec::with_capacity((width * height) as usize);
    for row in 0..height as usize {
        let src = std::slice::from_raw_parts(y_ptr.add(row * y_stride), width as usize);
        y_plane.extend_from_slice(src);
    }

    // ── Copy UV plane (chroma, half resolution, interleaved) ─────────────────
    // VideoToolbox outputs NV12: one interleaved UV plane
    // We need planar U and V for our shader
    let uv_ptr = CVPixelBufferGetBaseAddressOfPlane(image_buffer, 1);
    let uv_stride = CVPixelBufferGetBytesPerRowOfPlane(image_buffer, 1);
    let uv_height = (height / 2) as usize;
    let uv_width = (width / 2) as usize;

    let mut u_plane = Vec::with_capacity(uv_width * uv_height);
    let mut v_plane = Vec::with_capacity(uv_width * uv_height);

    for row in 0..uv_height {
        let src = std::slice::from_raw_parts(uv_ptr.add(row * uv_stride), width as usize);
        // interleaved: UVUVUV... → separate U and V
        for col in 0..uv_width {
            u_plane.push(src[col * 2]);
            v_plane.push(src[col * 2 + 1]);
        }
    }

    CVPixelBufferUnlockBaseAddress(image_buffer, 0);

    // ── Write to shared LatestFrame ───────────────────────────────────────────
    let latest = &*(refcon as *const LatestFrame);
    if let Ok(mut guard) = latest.lock() {
        *guard = Some(YuvFrame {
            y_plane,
            u_plane,
            v_plane,
            width,
            height,
            pts: pts_us,
        });
    }
}
