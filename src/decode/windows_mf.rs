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
}

unsafe impl Send for MediaFoundationDecoder {}

impl MediaFoundationDecoder {
    pub fn new_h264(sps: &[u8], pps: &[u8], latest: LatestFrame) -> Result<Self, DecodeError> {
        unsafe {
            MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET)
                .map_err(|e| DecodeError::InitFailed(format!("MFStartup: {}", e)))?;

            // enumerate software H.264 decoders (sync only — no D3D11 required)
            let input_info = MFT_REGISTER_TYPE_INFO {
                guidMajorType: MFMediaType_Video,
                guidSubtype: MFVideoFormat_H264,
            };
            let mut activates: *mut Option<IMFActivate> = std::ptr::null_mut();
            let mut count: u32 = 0;

            MFTEnumEx(
                MFT_CATEGORY_VIDEO_DECODER,
                MFT_ENUM_FLAG(MFT_ENUM_FLAG_SYNCMFT.0),
                Some(&input_info),
                None,
                &mut activates,
                &mut count,
            )
            .map_err(|e| DecodeError::InitFailed(format!("MFTEnumEx: {}", e)))?;

            if count == 0 {
                return Err(DecodeError::InitFailed(
                    "No software H.264 decoder found".to_string(),
                ));
            }

            let activate_slice = std::slice::from_raw_parts(activates, count as usize);
            let activate = activate_slice[0]
                .as_ref()
                .ok_or_else(|| DecodeError::InitFailed("NULL IMFActivate".to_string()))?;
            let transform: IMFTransform = activate
                .ActivateObject()
                .map_err(|e| DecodeError::InitFailed(format!("ActivateObject: {}", e)))?;
            CoTaskMemFree(Some(activates as *mut _));

            // input type: H.264 with SPS/PPS extradata
            let input_type: IMFMediaType =
                MFCreateMediaType().map_err(|e| DecodeError::InitFailed(e.to_string()))?;
            input_type
                .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
                .map_err(|e| DecodeError::InitFailed(e.to_string()))?;
            input_type
                .SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)
                .map_err(|e| DecodeError::InitFailed(e.to_string()))?;

            let width: u32 = 1920;
            let height: u32 = 1080;
            let frame_size: u64 = ((width as u64) << 32) | (height as u64);
            input_type
                .SetUINT64(&MF_MT_FRAME_SIZE, frame_size)
                .map_err(|e| DecodeError::InitFailed(e.to_string()))?;

            // SPS/PPS as Annex B extradata — required for RTP streams (out-of-band params)
            let mut seq_header = Vec::new();
            seq_header.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
            seq_header.extend_from_slice(sps);
            seq_header.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
            seq_header.extend_from_slice(pps);
            input_type
                .SetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &seq_header)
                .map_err(|e| DecodeError::InitFailed(e.to_string()))?;

            transform
                .SetInputType(0, &input_type, 0)
                .map_err(|e| DecodeError::InitFailed(format!("SetInputType: {}", e)))?;

            // output type: NV12 (query first, then set subtype)
            let output_type: IMFMediaType = transform
                .GetOutputAvailableType(0, 0)
                .map_err(|e| DecodeError::InitFailed(format!("GetOutputAvailableType: {}", e)))?;
            output_type
                .SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)
                .map_err(|e| DecodeError::InitFailed(e.to_string()))?;
            let _ = output_type.SetUINT64(&MF_MT_FRAME_SIZE, frame_size);
            transform
                .SetOutputType(0, &output_type, 0)
                .map_err(|e| DecodeError::InitFailed(format!("SetOutputType: {}", e)))?;

            transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
                .map_err(|e| DecodeError::InitFailed(e.to_string()))?;
            transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
                .map_err(|e| DecodeError::InitFailed(e.to_string()))?;

            let stream_info = transform
                .GetOutputStreamInfo(0)
                .map_err(|e| DecodeError::InitFailed(e.to_string()))?;
            let cbsize = stream_info
                .cbSize
                .max((width * height * 3 / 2) as u32)
                .max(6_000_000); // safe minimum for 2560×1440 NV12

            Ok(Self {
                transform,
                latest,
                width,
                height,
                cbsize,
            })
        }
    }

    pub fn new_h265(
        vps: &[u8],
        sps: &[u8],
        pps: &[u8],
        latest: LatestFrame,
    ) -> Result<Self, DecodeError> {
        unsafe {
            MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET)
                .map_err(|e| DecodeError::InitFailed(format!("MFStartup: {}", e)))?;

            // enumerate H.265/HEVC decoders
            let input_info = MFT_REGISTER_TYPE_INFO {
                guidMajorType: MFMediaType_Video,
                guidSubtype: MFVideoFormat_HEVC, // ← HEVC not H264
            };
            let mut activates: *mut Option<IMFActivate> = std::ptr::null_mut();
            let mut count: u32 = 0;

            MFTEnumEx(
                MFT_CATEGORY_VIDEO_DECODER,
                MFT_ENUM_FLAG(MFT_ENUM_FLAG_SYNCMFT.0),
                Some(&input_info),
                None,
                &mut activates,
                &mut count,
            )
            .map_err(|e| DecodeError::InitFailed(format!("MFTEnumEx H265: {}", e)))?;

            if count == 0 {
                return Err(DecodeError::InitFailed(
                    "No H.265 decoder found — install HEVC Video Extensions from Microsoft Store"
                        .to_string(),
                ));
            }

            let activate_slice = std::slice::from_raw_parts(activates, count as usize);
            let activate = activate_slice[0]
                .as_ref()
                .ok_or_else(|| DecodeError::InitFailed("NULL IMFActivate H265".to_string()))?;
            let transform: IMFTransform = activate
                .ActivateObject()
                .map_err(|e| DecodeError::InitFailed(format!("ActivateObject H265: {}", e)))?;
            CoTaskMemFree(Some(activates as *mut _));

            let input_type: IMFMediaType =
                MFCreateMediaType().map_err(|e| DecodeError::InitFailed(e.to_string()))?;
            input_type
                .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
                .map_err(|e| DecodeError::InitFailed(e.to_string()))?;
            input_type
                .SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_HEVC) // ← HEVC
                .map_err(|e| DecodeError::InitFailed(e.to_string()))?;

            let width: u32 = 1920;
            let height: u32 = 1080;
            let frame_size: u64 = ((width as u64) << 32) | (height as u64);
            input_type
                .SetUINT64(&MF_MT_FRAME_SIZE, frame_size)
                .map_err(|e| DecodeError::InitFailed(e.to_string()))?;

            // VPS + SPS + PPS as Annex B extradata
            let mut seq_header = Vec::new();
            seq_header.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
            seq_header.extend_from_slice(vps);
            seq_header.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
            seq_header.extend_from_slice(sps);
            seq_header.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
            seq_header.extend_from_slice(pps);
            input_type
                .SetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &seq_header)
                .map_err(|e| DecodeError::InitFailed(e.to_string()))?;

            transform
                .SetInputType(0, &input_type, 0)
                .map_err(|e| DecodeError::InitFailed(format!("SetInputType H265: {}", e)))?;

            let output_type: IMFMediaType =
                transform.GetOutputAvailableType(0, 0).map_err(|e| {
                    DecodeError::InitFailed(format!("GetOutputAvailableType H265: {}", e))
                })?;
            output_type
                .SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)
                .map_err(|e| DecodeError::InitFailed(e.to_string()))?;
            let _ = output_type.SetUINT64(&MF_MT_FRAME_SIZE, frame_size);
            transform
                .SetOutputType(0, &output_type, 0)
                .map_err(|e| DecodeError::InitFailed(format!("SetOutputType H265: {}", e)))?;

            transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
                .map_err(|e| DecodeError::InitFailed(e.to_string()))?;
            transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
                .map_err(|e| DecodeError::InitFailed(e.to_string()))?;

            let stream_info = transform
                .GetOutputStreamInfo(0)
                .map_err(|e| DecodeError::InitFailed(e.to_string()))?;
            let cbsize = stream_info
                .cbSize
                .max((width * height * 3 / 2) as u32)
                .max(6_000_000); // safe minimum for 2560×1440 NV12

            Ok(Self {
                transform,
                latest,
                width,
                height,
                cbsize,
            })
        }
    }

    pub fn send_packet(&mut self, data: &[u8], pts: u64) -> Result<(), DecodeError> {
        unsafe {
            // AVCC (4-byte length prefix) → Annex B (start codes)
            // MFVideoFormat_H264 requires Annex B format
            let annexb = avcc_to_annexb(data);

            let buffer: IMFMediaBuffer = MFCreateMemoryBuffer(annexb.len() as u32)
                .map_err(|e| DecodeError::SendFailed(format!("MFCreateMemoryBuffer: {}", e)))?;

            let mut buf_ptr: *mut u8 = std::ptr::null_mut();
            let mut max_len: u32 = 0;
            let mut cur_len: u32 = 0;
            buffer
                .Lock(&mut buf_ptr, Some(&mut max_len), Some(&mut cur_len))
                .map_err(|e| DecodeError::SendFailed(e.to_string()))?;
            // copy annexb (not original data — annexb may be longer)
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

            // if decoder output queue is full, drain first
            let result = self.transform.ProcessInput(0, &sample, 0);
            if let Err(ref e) = result {
                if e.code().0 as u32 == 0xC00D36B5 {
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
        let buf_size = self.cbsize;
        loop {
            let out_buffer = match MFCreateMemoryBuffer(buf_size) {
                Ok(b) => b,
                Err(_) => break, // OOM — skip frame
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
                Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                    if let Ok(new_type) = self.transform.GetOutputAvailableType(0, 0) {
                        // get actual new dimensions
                        if let Ok(fs) = new_type.GetUINT64(&MF_MT_FRAME_SIZE) {
                            self.width = (fs >> 32) as u32;
                            self.height = (fs & 0xFFFF_FFFF) as u32;
                        }
                        new_type.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12).ok();
                        let fs = ((self.width as u64) << 32) | self.height as u64;
                        new_type.SetUINT64(&MF_MT_FRAME_SIZE, fs).ok();
                        self.transform.SetOutputType(0, &new_type, 0).ok();
                    }
                    // refresh cbsize for new resolution
                    if let Ok(info) = self.transform.GetOutputStreamInfo(0) {
                        self.cbsize = info.cbSize.max((self.width * self.height * 3 / 2) as u32);
                    }
                    continue;
                }
                Err(e) if e.code().0 as u32 == 0xC00D36B1 => {
                    if let Ok(new_type) = self.transform.GetOutputAvailableType(0, 0) {
                        new_type.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12).ok();
                        let fs = ((self.width as u64) << 32) | self.height as u64;
                        new_type.SetUINT64(&MF_MT_FRAME_SIZE, fs).ok();
                        self.transform.SetOutputType(0, &new_type, 0).ok();
                    }
                    if let Ok(info) = self.transform.GetOutputStreamInfo(0) {
                        self.cbsize = info.cbSize.max((self.width * self.height * 3 / 2) as u32);
                    }
                    continue; //
                }

                Err(e) => {
                    eprintln!("[MF] ProcessOutput error: {:#010x}", e.code().0);
                    break;
                }
            }
        }
        Ok(())
    }

    unsafe fn extract_yuv_frame(&mut self, sample: IMFSample) -> Result<(), DecodeError> {
        // read actual dimensions from decoder output type
        let actual_type = self
            .transform
            .GetOutputCurrentType(0)
            .map_err(|e| DecodeError::SendFailed(format!("GetOutputCurrentType: {}", e)))?;
        let frame_size = actual_type
            .GetUINT64(&MF_MT_FRAME_SIZE)
            .unwrap_or(((1920u64) << 32) | 1080u64);
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

        // NV12: contiguous Y (w×h), then interleaved UV (w×h/2)
        let y_data = std::slice::from_raw_parts(ptr, w * h);
        let uv_data = std::slice::from_raw_parts(ptr.add(w * h), w * h / 2);

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

        if let Ok(mut guard) = self.latest.lock() {
            *guard = Some(YuvFrame {
                y_plane,
                u_plane,
                v_plane,
                width: (w / 2) as u32,
                height: (h / 2) as u32,
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
            MFShutdown().ok();
        }
    }
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
