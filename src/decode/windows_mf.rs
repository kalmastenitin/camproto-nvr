#![cfg(target_os = "windows")]

use super::{DecodeError, LatestFrame, YuvFrame};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::*;

pub struct MediaFoundationDecoder {
    transform: IMFTransform,
    latest: LatestFrame,
    width: u32,
    height: u32,
    cbsize: u32,
    is_hw: bool,
    pub frames_decoded: u32,
}

unsafe impl Send for MediaFoundationDecoder {}

impl MediaFoundationDecoder {
    /// H264: tries NVDEC hardware (system-memory NV12 output) first, falls to software.
    /// No D3D11 manager needed — NVDEC H264 can write directly to system memory.
    pub fn new_h264(sps: &[u8], pps: &[u8], latest: LatestFrame) -> Result<Self, DecodeError> {
        unsafe {
            MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET)
                .map_err(|e| DecodeError::InitFailed(format!("MFStartup: {}", e)))?;

            let hw_flags =
                MFT_ENUM_FLAG_SYNCMFT.0 | MFT_ENUM_FLAG_HARDWARE.0 | MFT_ENUM_FLAG_SORTANDFILTER.0;
            let sw_flags = MFT_ENUM_FLAG_SYNCMFT.0;
            let (transform, is_hw) = match try_mft(MFVideoFormat_H264, hw_flags) {
                Ok(t) => (t, true),
                Err(_) => (try_mft(MFVideoFormat_H264, sw_flags)?, false),
            };

            let (width, height) = (1920u32, 1080u32);
            let frame_size = pack_frame_size(width, height);
            setup_input_type(
                &transform,
                &MFVideoFormat_H264,
                frame_size,
                &annexb_header(&[], sps, pps),
            )?;
            let cbsize = setup_output_type(&transform, frame_size, width, height)?;
            begin_streaming(&transform)?;
            Ok(Self {
                transform,
                latest,
                width,
                height,
                cbsize,
                is_hw,
                frames_decoded: 0,
            })
        }
    }

    /// H265: always software (HEVC Video Extensions).
    /// NVIDIA NVDEC HEVC requires a D3D11 device manager — without it the MFT
    /// accepts input but produces no output. Software decode is reliable.
    pub fn new_h265(
        vps: &[u8],
        sps: &[u8],
        pps: &[u8],
        latest: LatestFrame,
    ) -> Result<Self, DecodeError> {
        unsafe {
            MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET)
                .map_err(|e| DecodeError::InitFailed(format!("MFStartup: {}", e)))?;

            let transform = try_mft(MFVideoFormat_HEVC, MFT_ENUM_FLAG_SYNCMFT.0)?;
            let (width, height) = (1920u32, 1080u32);
            let frame_size = pack_frame_size(width, height);
            setup_input_type(
                &transform,
                &MFVideoFormat_HEVC,
                frame_size,
                &annexb_header(vps, sps, pps),
            )?;
            let cbsize = setup_output_type(&transform, frame_size, width, height)?;
            begin_streaming(&transform)?;
            Ok(Self {
                transform,
                latest,
                width,
                height,
                cbsize,
                is_hw: false,
                frames_decoded: 0,
            })
        }
    }

    pub fn is_hardware(&self) -> bool {
        self.is_hw
    }
    pub fn frames_decoded(&self) -> u32 {
        self.frames_decoded
    }

    pub fn send_packet(&mut self, data: &[u8], pts: u64) -> Result<(), DecodeError> {
        unsafe {
            let annexb = avcc_to_annexb(data);

            let buffer: IMFMediaBuffer = MFCreateMemoryBuffer(annexb.len() as u32)
                .map_err(|e| DecodeError::SendFailed(format!("MFCreateMemoryBuffer: {}", e)))?;
            let mut buf_ptr: *mut u8 = std::ptr::null_mut();
            let mut max_len: u32 = 0;
            let mut cur_len: u32 = 0;
            buffer
                .Lock(&mut buf_ptr, Some(&mut max_len), Some(&mut cur_len))
                .map_err(|e| DecodeError::SendFailed(e.to_string()))?;
            std::ptr::copy_nonoverlapping(annexb.as_ptr(), buf_ptr, annexb.len());
            buffer
                .Unlock()
                .map_err(|e| DecodeError::SendFailed(e.to_string()))?;
            buffer
                .SetCurrentLength(annexb.len() as u32)
                .map_err(|e| DecodeError::SendFailed(e.to_string()))?;

            let sample: IMFSample = MFCreateSample()
                .map_err(|e| DecodeError::SendFailed(format!("MFCreateSample: {}", e)))?;
            sample
                .AddBuffer(&buffer)
                .map_err(|e| DecodeError::SendFailed(e.to_string()))?;
            sample
                .SetSampleTime((pts * 10) as i64)
                .map_err(|e| DecodeError::SendFailed(e.to_string()))?;

            let result = self.transform.ProcessInput(0, &sample, 0);
            if let Err(ref e) = result {
                if e.code().0 as u32 == 0xC00D36B5 {
                    // MF_E_NOTACCEPTING — drain then retry
                    self.drain_output()?;
                    self.transform.ProcessInput(0, &sample, 0).map_err(|e| {
                        DecodeError::SendFailed(format!("ProcessInput retry: {}", e))
                    })?;
                    return self.drain_output();
                }
            }
            result.map_err(|e| DecodeError::SendFailed(format!("ProcessInput: {}", e)))?;
            self.drain_output()
        }
    }

    unsafe fn drain_output(&mut self) -> Result<(), DecodeError> {
        loop {
            let out_buffer = match MFCreateMemoryBuffer(self.cbsize) {
                Ok(b) => b,
                Err(_) => break,
            };
            let out_sample = match MFCreateSample() {
                Ok(s) => s,
                Err(_) => break,
            };
            let _ = out_sample.AddBuffer(&out_buffer);

            let mut outputs = [MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: 0,
                pSample: std::mem::ManuallyDrop::new(Some(out_sample)),
                dwStatus: 0,
                pEvents: std::mem::ManuallyDrop::new(None),
            }];
            let mut status: u32 = 0;
            let hr = self.transform.ProcessOutput(0, &mut outputs, &mut status);
            let sample = std::mem::ManuallyDrop::take(&mut outputs[0].pSample);

            match hr {
                Ok(_) => {
                    if let Some(s) = sample {
                        self.extract_yuv_frame(s)?;
                    }
                }

                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => break,

                // Resolution changed — update type and cbsize, loop to drain remaining
                Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                    if let Ok(new_type) = self.transform.GetOutputAvailableType(0, 0) {
                        if let Ok(fs) = new_type.GetUINT64(&MF_MT_FRAME_SIZE) {
                            self.width = (fs >> 32) as u32;
                            self.height = (fs & 0xFFFF_FFFF) as u32;
                        }
                        new_type.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12).ok();
                        let fs = pack_frame_size(self.width, self.height);
                        new_type.SetUINT64(&MF_MT_FRAME_SIZE, fs).ok();
                        self.transform.SetOutputType(0, &new_type, 0).ok();
                    }
                    if let Ok(info) = self.transform.GetOutputStreamInfo(0) {
                        self.cbsize = info.cbSize.max(self.width * self.height * 3 / 2);
                    }
                    continue;
                }

                // MF_E_INVALIDMEDIATYPE — re-negotiate output type
                Err(e) if e.code().0 as u32 == 0xC00D36B1 => {
                    if let Ok(new_type) = self.transform.GetOutputAvailableType(0, 0) {
                        new_type.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12).ok();
                        let fs = pack_frame_size(self.width, self.height);
                        new_type.SetUINT64(&MF_MT_FRAME_SIZE, fs).ok();
                        self.transform.SetOutputType(0, &new_type, 0).ok();
                    }
                    if let Ok(info) = self.transform.GetOutputStreamInfo(0) {
                        self.cbsize = info.cbSize.max(self.width * self.height * 3 / 2);
                    }
                    continue;
                }

                // E_FAIL — cbsize may have grown after stream change
                Err(e) if e.code().0 == 0x80004005_u32 as i32 => {
                    if let Ok(info) = self.transform.GetOutputStreamInfo(0) {
                        let needed = info.cbSize.max(self.width * self.height * 3 / 2);
                        if needed > self.cbsize {
                            self.cbsize = needed;
                            continue;
                        }
                    }
                    break;
                }

                Err(e) => {
                    eprintln!("[MF] ProcessOutput: {:#010x}", e.code().0);
                    break;
                }
            }
        }
        Ok(())
    }

    unsafe fn extract_yuv_frame(&mut self, sample: IMFSample) -> Result<(), DecodeError> {
        let actual_type = self
            .transform
            .GetOutputCurrentType(0)
            .map_err(|e| DecodeError::SendFailed(format!("GetOutputCurrentType: {}", e)))?;
        let frame_size = actual_type
            .GetUINT64(&MF_MT_FRAME_SIZE)
            .unwrap_or(pack_frame_size(1920, 1080));
        let w = (frame_size >> 32) as usize;
        let h = (frame_size & 0xFFFF_FFFF) as usize;
        self.width = w as u32;
        self.height = h as u32;

        let buffer: IMFMediaBuffer = sample
            .GetBufferByIndex(0)
            .map_err(|e| DecodeError::SendFailed(e.to_string()))?;
        let mut ptr: *mut u8 = std::ptr::null_mut();
        let mut max: u32 = 0;
        let mut cur: u32 = 0;
        buffer
            .Lock(&mut ptr, Some(&mut max), Some(&mut cur))
            .map_err(|e| DecodeError::SendFailed(e.to_string()))?;

        // NV12: Y plane (w×h) then interleaved UV plane (w×h/2)
        let y_data = std::slice::from_raw_parts(ptr, w * h);
        let uv_data = std::slice::from_raw_parts(ptr.add(w * h), w * h / 2);

        // Downsample 2× in both axes → quarter-resolution YUV
        let dw = w / 2;
        let dh = h / 2;
        let mut y_plane = Vec::with_capacity(dw * dh);
        for row in (0..h).step_by(2) {
            for col in (0..w).step_by(2) {
                y_plane.push(y_data[row * w + col]);
            }
        }
        let mut u_plane = Vec::with_capacity(dw * dh / 4);
        let mut v_plane = Vec::with_capacity(dw * dh / 4);
        for row in (0..h / 2).step_by(2) {
            for col in (0..w / 2).step_by(2) {
                u_plane.push(uv_data[row * w + col * 2]);
                v_plane.push(uv_data[row * w + col * 2 + 1]);
            }
        }

        buffer.Unlock().ok();
        self.frames_decoded += 1;
        if let Ok(mut guard) = self.latest.lock() {
            *guard = Some(YuvFrame {
                y_plane,
                u_plane,
                v_plane,
                width: dw as u32,
                height: dh as u32,
                pts: 0,
            });
        }
        Ok(())
    }
}

impl Drop for MediaFoundationDecoder {
    fn drop(&mut self) {
        unsafe {
            let _ = self
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            MFShutdown().ok(); // balances MFStartup in each constructor
        }
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn pack_frame_size(w: u32, h: u32) -> u64 {
    ((w as u64) << 32) | (h as u64)
}

fn annexb_header(vps: &[u8], sps: &[u8], pps: &[u8]) -> Vec<u8> {
    let mut v = Vec::new();
    if !vps.is_empty() {
        v.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
        v.extend_from_slice(vps);
    }
    v.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
    v.extend_from_slice(sps);
    v.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
    v.extend_from_slice(pps);
    v
}

unsafe fn try_mft(subtype: windows::core::GUID, flags: i32) -> Result<IMFTransform, DecodeError> {
    let input_info = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: subtype,
    };
    let mut activates: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count: u32 = 0;
    MFTEnumEx(
        MFT_CATEGORY_VIDEO_DECODER,
        MFT_ENUM_FLAG(flags),
        Some(&input_info),
        None,
        &mut activates,
        &mut count,
    )
    .map_err(|e| DecodeError::InitFailed(e.to_string()))?;
    if count == 0 {
        return Err(DecodeError::InitFailed("no decoder found".into()));
    }
    let slice = std::slice::from_raw_parts(activates, count as usize);
    let activate = slice[0]
        .as_ref()
        .ok_or_else(|| DecodeError::InitFailed("null activate".into()))?;
    let transform: IMFTransform = activate
        .ActivateObject()
        .map_err(|e| DecodeError::InitFailed(e.to_string()))?;
    CoTaskMemFree(Some(activates as *mut _));
    Ok(transform)
}

unsafe fn setup_input_type(
    transform: &IMFTransform,
    subtype: &windows::core::GUID,
    frame_size: u64,
    seq_header: &[u8],
) -> Result<(), DecodeError> {
    let t: IMFMediaType =
        MFCreateMediaType().map_err(|e| DecodeError::InitFailed(e.to_string()))?;
    t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
        .map_err(|e| DecodeError::InitFailed(e.to_string()))?;
    t.SetGUID(&MF_MT_SUBTYPE, subtype)
        .map_err(|e| DecodeError::InitFailed(e.to_string()))?;
    t.SetUINT64(&MF_MT_FRAME_SIZE, frame_size)
        .map_err(|e| DecodeError::InitFailed(e.to_string()))?;
    t.SetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, seq_header)
        .map_err(|e| DecodeError::InitFailed(e.to_string()))?;
    transform
        .SetInputType(0, &t, 0)
        .map_err(|e| DecodeError::InitFailed(format!("SetInputType: {}", e)))?;
    Ok(())
}

unsafe fn setup_output_type(
    transform: &IMFTransform,
    frame_size: u64,
    width: u32,
    height: u32,
) -> Result<u32, DecodeError> {
    let t: IMFMediaType = transform
        .GetOutputAvailableType(0, 0)
        .map_err(|e| DecodeError::InitFailed(format!("GetOutputAvailableType: {}", e)))?;
    t.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)
        .map_err(|e| DecodeError::InitFailed(e.to_string()))?;
    let _ = t.SetUINT64(&MF_MT_FRAME_SIZE, frame_size);
    transform
        .SetOutputType(0, &t, 0)
        .map_err(|e| DecodeError::InitFailed(format!("SetOutputType: {}", e)))?;
    let info = transform
        .GetOutputStreamInfo(0)
        .map_err(|e| DecodeError::InitFailed(e.to_string()))?;
    // Use actual stream size — no artificial 13MB floor that caused OOM with 17 cameras.
    // Stream change handler (drain_output) grows cbsize as needed.
    Ok(info.cbSize.max(width * height * 3 / 2))
}

unsafe fn begin_streaming(transform: &IMFTransform) -> Result<(), DecodeError> {
    transform
        .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
        .map_err(|e| DecodeError::InitFailed(e.to_string()))?;
    transform
        .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
        .map_err(|e| DecodeError::InitFailed(e.to_string()))?;
    Ok(())
}

fn avcc_to_annexb(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + 4);
    let mut pos = 0;
    while pos + 4 <= data.len() {
        let nal_len =
            u32::from_be_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]) as usize;
        pos += 4;
        if nal_len == 0 || pos + nal_len > data.len() {
            break;
        }
        out.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
        out.extend_from_slice(&data[pos..pos + nal_len]);
        pos += nal_len;
    }
    out
}
