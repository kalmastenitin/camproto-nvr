#![cfg(target_os = "windows")]

use super::{DecodeError, LatestFrame, YuvFrame};
use windows::core::Interface;
use windows::Win32::Graphics::Direct3D::*;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::*;

#[allow(dead_code)]
pub struct D3D11Decoder {
    transform: IMFTransform,
    manager: IMFDXGIDeviceManager, // ← must stay alive for MFT lifetime
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    staging: Option<ID3D11Texture2D>,
    latest: LatestFrame,
    width: u32,
    height: u32,
    intermediate: Option<ID3D11Texture2D>
}

unsafe impl Send for D3D11Decoder {}

impl D3D11Decoder {
    pub fn new_h264(sps: &[u8], pps: &[u8], latest: LatestFrame) -> Result<Self, DecodeError> {
        unsafe { Self::create(sps, None, pps, latest, false) }
    }

    pub fn new_h265(
        vps: &[u8],
        sps: &[u8],
        pps: &[u8],
        latest: LatestFrame,
    ) -> Result<Self, DecodeError> {
        unsafe { Self::create(sps, Some(vps), pps, latest, true) }
    }

    unsafe fn create(
        sps: &[u8],
        vps: Option<&[u8]>,
        pps: &[u8],
        latest: LatestFrame,
        is_hevc: bool,
    ) -> Result<Self, DecodeError> {
        // MF must be started before use
        MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET)
            .map_err(|e| DecodeError::InitFailed(format!("MFStartup: {}", e)))?;

        // ── Step 1: D3D11 device with video support ───────────────────────

        let (device, context) = crate::decode::get_or_create_d3d11_device()
            .map_err(|e| DecodeError::InitFailed(format!("D3D11CreateDevice: {}", e)))?;

        let device = device.clone();
        let context = context.clone();

        // ── Step 2: device manager ────────────────────────────────────────
        let mut reset_token: u32 = 0;
        let mut manager: Option<IMFDXGIDeviceManager> = None;
        MFCreateDXGIDeviceManager(&mut reset_token, &mut manager)
            .map_err(|e| DecodeError::InitFailed(format!("MFCreateDXGIDeviceManager: {}", e)))?;
        let manager = manager.unwrap();

        manager
            .ResetDevice(&device, reset_token)
            .map_err(|e| DecodeError::InitFailed(format!("ResetDevice: {}", e)))?;

        // ── Step 3: enumerate hardware MFT ────────────────────────────────
        let subtype = if is_hevc {
            MFVideoFormat_HEVC
        } else {
            MFVideoFormat_H264
        };
        let input_info = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Video,
            guidSubtype: subtype,
        };

        let mut activates: *mut Option<IMFActivate> = std::ptr::null_mut();
        let mut count: u32 = 0;

        MFTEnumEx(
            MFT_CATEGORY_VIDEO_DECODER,
            MFT_ENUM_FLAG(
                MFT_ENUM_FLAG_SYNCMFT.0 | MFT_ENUM_FLAG_HARDWARE.0 | MFT_ENUM_FLAG_SORTANDFILTER.0,
            ),
            Some(&input_info),
            None,
            &mut activates,
            &mut count,
        )
        .map_err(|e| DecodeError::InitFailed(format!("MFTEnumEx hw: {}", e)))?;

        if count == 0 {
            return Err(DecodeError::InitFailed(
                "No hardware decoder found — GPU may not support DXVA".to_string(),
            ));
        }

        let activate_slice = std::slice::from_raw_parts(activates, count as usize);
        let activate = activate_slice[0]
            .as_ref()
            .ok_or_else(|| DecodeError::InitFailed("NULL activate".to_string()))?;
        let transform: IMFTransform = activate
            .ActivateObject()
            .map_err(|e| DecodeError::InitFailed(format!("ActivateObject hw: {}", e)))?;
        CoTaskMemFree(Some(activates as *mut _));

        // ── Step 4: attach device manager to MFT ─────────────────────────
        // ProcessMessage expects a raw pointer to IUnknown cast to usize
        let manager_ptr = manager.as_raw() as usize;
        transform
            .ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager_ptr)
            .map_err(|e| DecodeError::InitFailed(format!("SET_D3D_MANAGER: {}", e)))?;

        // ── Step 5: input type ────────────────────────────────────────────
        let input_type: IMFMediaType =
            MFCreateMediaType().map_err(|e| DecodeError::InitFailed(e.to_string()))?;
        input_type
            .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
            .map_err(|e| DecodeError::InitFailed(e.to_string()))?;
        input_type
            .SetGUID(&MF_MT_SUBTYPE, &subtype)
            .map_err(|e| DecodeError::InitFailed(e.to_string()))?;

        let width: u32 = 1920;
        let height: u32 = 1080;
        let frame_size: u64 = ((width as u64) << 32) | (height as u64);
        input_type
            .SetUINT64(&MF_MT_FRAME_SIZE, frame_size)
            .map_err(|e| DecodeError::InitFailed(e.to_string()))?;

        let mut seq_header = Vec::new();
        if let Some(vps) = vps {
            seq_header.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
            seq_header.extend_from_slice(vps);
        }
        seq_header.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
        seq_header.extend_from_slice(sps);
        seq_header.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
        seq_header.extend_from_slice(pps);
        input_type
            .SetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &seq_header)
            .map_err(|e| DecodeError::InitFailed(e.to_string()))?;

        transform
            .SetInputType(0, &input_type, 0)
            .map_err(|e| DecodeError::InitFailed(format!("SetInputType hw: {}", e)))?;

        // ── Step 6: output type NV12 ──────────────────────────────────────
        let output_type: IMFMediaType = transform
            .GetOutputAvailableType(0, 0)
            .map_err(|e| DecodeError::InitFailed(format!("GetOutputAvailableType hw: {}", e)))?;
        let _ = output_type.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12);
        let _ = output_type.SetUINT64(&MF_MT_FRAME_SIZE, frame_size);
        transform
            .SetOutputType(0, &output_type, 0)
            .map_err(|e| DecodeError::InitFailed(format!("SetOutputType hw: {}", e)))?;

        transform
            .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
            .map_err(|e| DecodeError::InitFailed(e.to_string()))?;
        transform
            .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
            .map_err(|e| DecodeError::InitFailed(e.to_string()))?;

        Ok(Self {
            transform,
            manager, // ← stored so it stays alive
            device,
            context,
            staging: None,
            latest,
            width,
            height,
            intermediate: None
        })
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

            // if output queue full, drain first then retry
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
        loop {
            // hardware MFT provides its own output sample containing D3D11 texture
            let mut outputs = [MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: 0,
                pSample: std::mem::ManuallyDrop::new(None), // NULL — MFT provides
                dwStatus: 0,
                pEvents: std::mem::ManuallyDrop::new(None),
            }];

            let mut status: u32 = 0;
            let hr = self.transform.ProcessOutput(0, &mut outputs, &mut status);
            let sample = std::mem::ManuallyDrop::take(&mut outputs[0].pSample);

            match hr {
                Ok(_) => {
                    eprintln!("[D3D11] got frame");
                    if let Some(s) = sample {
                        if let Err(e) = self.extract_d3d11_frame(s) {
                            eprintln!("[D3D11] extract error: {}", e);
                        }
                    }
                }
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => break,
                Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                    if let Ok(new_type) = self.transform.GetOutputAvailableType(0, 0) {
                        if let Ok(fs) = new_type.GetUINT64(&MF_MT_FRAME_SIZE) {
                            self.width = (fs >> 32) as u32;
                            self.height = (fs & 0xFFFF_FFFF) as u32;
                            self.staging = None; // recreate staging for new size
                        }
                        let _ = new_type.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12);
                        self.transform.SetOutputType(0, &new_type, 0).ok();
                    }
                    continue;
                }
                Err(e) => {
                    eprintln!("[D3D11] ProcessOutput error: {:#010x}", e.code().0);
                    break;
                }
            }
        }
        Ok(())
    }

    unsafe fn extract_d3d11_frame(&mut self, sample: IMFSample) -> Result<(), DecodeError> {
    let buffer: IMFMediaBuffer = sample.GetBufferByIndex(0)
        .map_err(|e| DecodeError::SendFailed(e.to_string()))?;
    let dxgi_buf: IMFDXGIBuffer = buffer.cast()
        .map_err(|e| DecodeError::SendFailed(format!("cast: {}", e)))?;

    let mut texture_raw: *mut std::ffi::c_void = std::ptr::null_mut();
    dxgi_buf.GetResource(&ID3D11Texture2D::IID, &mut texture_raw)
        .map_err(|e| DecodeError::SendFailed(format!("GetResource: {}", e)))?;
    if texture_raw.is_null() { return Ok(()); }
    let texture = ID3D11Texture2D::from_raw(texture_raw);

    let sub_idx  = dxgi_buf.GetSubresourceIndex()
        .map_err(|e| DecodeError::SendFailed(e.to_string()))?;
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    texture.GetDesc(&mut desc);
    let w = desc.Width  as usize;
    let h = desc.Height as usize;
    if w == 0 || h == 0 { return Ok(()); }
    self.width  = w as u32;
    self.height = h as u32;

    let array_size = desc.ArraySize;

    // ── recreate textures if size changed ─────────────────────────────────
    let size_changed = match &self.staging {
        None => true,
        Some(s) => {
            let mut sd = D3D11_TEXTURE2D_DESC::default();
            s.GetDesc(&mut sd);
            sd.Width != desc.Width || sd.Height != desc.Height
        }
    };

    if size_changed {
        // intermediate: DEFAULT, NV12 — receives Y and UV copies
        let inter_desc = D3D11_TEXTURE2D_DESC {
            Width: desc.Width, Height: desc.Height,
            MipLevels: 1, ArraySize: 1,
            Format: DXGI_FORMAT_NV12,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags:      0,
            CPUAccessFlags: 0,
            MiscFlags:      0,
        };
        let mut inter: Option<ID3D11Texture2D> = None;
        self.device.CreateTexture2D(&inter_desc, None, Some(&mut inter))
            .map_err(|e| DecodeError::SendFailed(format!("CreateTexture2D inter: {}", e)))?;
        self.intermediate = inter;

        // staging: same but STAGING + CPU_ACCESS_READ
        let mut stg_desc = inter_desc;
        stg_desc.Usage          = D3D11_USAGE_STAGING;
        stg_desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
        let mut stg: Option<ID3D11Texture2D> = None;
        self.device.CreateTexture2D(&stg_desc, None, Some(&mut stg))
            .map_err(|e| DecodeError::SendFailed(format!("CreateTexture2D staging: {}", e)))?;
        self.staging = stg;
    }

    let inter   = self.intermediate.as_ref()
        .ok_or_else(|| DecodeError::SendFailed("no intermediate".into()))?;
    let staging = self.staging.as_ref()
        .ok_or_else(|| DecodeError::SendFailed("no staging".into()))?;

    // copy Y plane: decoder array[sub_idx] → intermediate subresource 0
    let y_src  = sub_idx;
    let uv_src = array_size + sub_idx;
    self.context.CopySubresourceRegion(inter, 0, 0, 0, 0, &texture, y_src,  None);
    self.context.CopySubresourceRegion(inter, 1, 0, 0, 0, &texture, uv_src, None);

    // copy intermediate → staging (copies both planes)
    self.context.CopyResource(staging, inter);

    // map staging — NV12: Y at base, UV at base + stride * height
    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
    self.context.Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
        .map_err(|e| DecodeError::SendFailed(format!("Map: {}", e)))?;

    if mapped.pData.is_null() {
        self.context.Unmap(staging, 0);
        return Ok(());
    }

    let stride   = mapped.RowPitch as usize;
    let ptr      = mapped.pData as *const u8;
    let uv_start = h * stride;

    let dw = w / 2;
    let dh = h / 2;

    let mut y_plane = Vec::with_capacity(dw * dh);
    for row in (0..h).step_by(2) {
        let src = std::slice::from_raw_parts(ptr.add(row * stride), w);
        for col in (0..w).step_by(2) { y_plane.push(src[col]); }
    }

    let mut u_plane = Vec::with_capacity(dw * dh / 4);
    let mut v_plane = Vec::with_capacity(dw * dh / 4);
    for row in (0..h / 2).step_by(2) {
        let src = std::slice::from_raw_parts(
            ptr.add(uv_start + row * stride), w
        );
        for col in (0..w / 2).step_by(2) {
            u_plane.push(src[col * 2]);
            v_plane.push(src[col * 2 + 1]);
        }
    }

    self.context.Unmap(staging, 0);

    if let Ok(mut guard) = self.latest.lock() {
        *guard = Some(YuvFrame {
            y_plane, u_plane, v_plane,
            width: dw as u32, height: dh as u32, pts: 0,
        });
    }
    Ok(())
}
}

impl Drop for D3D11Decoder {
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
