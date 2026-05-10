#![cfg(target_os = "windows")]

use super::{DecodeError, LatestFrame, YuvFrame};
use ffmpeg_sys_next::{
    AVCodecID::{AV_CODEC_ID_H264, AV_CODEC_ID_HEVC},
    *,
};
use std::ptr;

// ── get_format callback ───────────────────────────────────────────────────────
//
// FFmpeg calls this when it needs to pick an output pixel format.
// We walk the list of formats the codec supports and prefer AV_PIX_FMT_D3D11
// (NVDEC / D3D11VA hardware). If the GPU doesn't support the codec or the
// format isn't listed, we fall through to the first software format offered.
//
// This is what actually makes FFmpeg use NVDEC instead of falling back to CPU.

unsafe extern "C" fn get_format(
    _ctx: *mut AVCodecContext,
    fmts: *const AVPixelFormat,
) -> AVPixelFormat {
    let mut p = fmts;
    while *p != AVPixelFormat::AV_PIX_FMT_NONE {
        if *p == AVPixelFormat::AV_PIX_FMT_D3D11 {
            return AVPixelFormat::AV_PIX_FMT_D3D11; // hardware path
        }
        p = p.add(1);
    }
    *fmts // first software format offered
}

// ── Decoder ───────────────────────────────────────────────────────────────────

pub struct FfmpegDecoder {
    ctx:       *mut AVCodecContext,
    pkt:       *mut AVPacket,
    frame:     *mut AVFrame,
    sw_frame:  *mut AVFrame,
    is_hw:     bool,
    latest:    LatestFrame,
    pub frames_decoded: u32,
}

unsafe impl Send for FfmpegDecoder {}

impl FfmpegDecoder {
    pub fn new_h264(sps: &[u8], pps: &[u8], latest: LatestFrame) -> Result<Self, DecodeError> {
        unsafe { Self::create(AV_CODEC_ID_H264, &[], sps, pps, latest) }
    }

    pub fn new_h265(vps: &[u8], sps: &[u8], pps: &[u8], latest: LatestFrame) -> Result<Self, DecodeError> {
        unsafe { Self::create(AV_CODEC_ID_HEVC, vps, sps, pps, latest) }
    }

    pub fn is_hardware(&self)    -> bool { self.is_hw }
    pub fn frames_decoded(&self) -> u32  { self.frames_decoded }

    unsafe fn create(
        codec_id: AVCodecID,
        vps:      &[u8],
        sps:      &[u8],
        pps:      &[u8],
        latest:   LatestFrame,
    ) -> Result<Self, DecodeError> {
        let codec = avcodec_find_decoder(codec_id);
        if codec.is_null() {
            return Err(DecodeError::InitFailed("codec not found".into()));
        }

        let ctx = avcodec_alloc_context3(codec);
        if ctx.is_null() {
            return Err(DecodeError::InitFailed("context alloc failed".into()));
        }

        // ── Try GPU (D3D11VA) ─────────────────────────────────────────────────
        let mut hw_dev_ctx: *mut AVBufferRef = ptr::null_mut();
        let gpu_available = av_hwdevice_ctx_create(
            &mut hw_dev_ctx,
            AVHWDeviceType::AV_HWDEVICE_TYPE_D3D11VA,
            ptr::null(),
            ptr::null_mut(),
            0,
        ) >= 0 && !hw_dev_ctx.is_null();

        if gpu_available {
            (*ctx).hw_device_ctx = av_buffer_ref(hw_dev_ctx);
            av_buffer_unref(&mut hw_dev_ctx);

            // get_format tells FFmpeg to pick AV_PIX_FMT_D3D11 (NVDEC).
            // Without this callback, FFmpeg ignores hw_device_ctx entirely.
            (*ctx).get_format = Some(get_format);

            (*ctx).thread_count = 1; // GPU handles decode, 1 thread enough
        } else {
            (*ctx).thread_count = 2; // software decode, use 2 threads
        }

        // ── Extradata (SPS/PPS/VPS in Annex B) ───────────────────────────────
        let extra = build_extradata(vps, sps, pps);
        if !extra.is_empty() {
            let buf = av_mallocz(extra.len() + AV_INPUT_BUFFER_PADDING_SIZE as usize) as *mut u8;
            if !buf.is_null() {
                ptr::copy_nonoverlapping(extra.as_ptr(), buf, extra.len());
                (*ctx).extradata      = buf;
                (*ctx).extradata_size = extra.len() as i32;
            }
        }

        if avcodec_open2(ctx, codec, ptr::null_mut()) < 0 {
            avcodec_free_context(&mut (ctx as *mut _));
            return Err(DecodeError::InitFailed("avcodec_open2 failed".into()));
        }

        // Suppress all FFmpeg console output (POC/RPS messages at stream start)
        av_log_set_level(-8); // AV_LOG_QUIET

        // After open, check if the codec context actually kept the hw device.
        // If the codec doesn't support D3D11VA for this stream, FFmpeg clears it.
        let is_hw = !(*ctx).hw_device_ctx.is_null();

        let pkt      = av_packet_alloc();
        let frame    = av_frame_alloc();
        let sw_frame = av_frame_alloc();

        if pkt.is_null() || frame.is_null() || sw_frame.is_null() {
            avcodec_free_context(&mut (ctx as *mut _));
            return Err(DecodeError::InitFailed("alloc failed".into()));
        }

        Ok(Self { ctx, pkt, frame, sw_frame, is_hw, latest, frames_decoded: 0 })
    }

    pub fn send_packet(&mut self, data: &[u8], pts: u64) -> Result<(), DecodeError> {
        unsafe {
            let annexb = avcc_to_annexb(data);

            let buf = av_mallocz(annexb.len() + AV_INPUT_BUFFER_PADDING_SIZE as usize) as *mut u8;
            if buf.is_null() {
                return Err(DecodeError::SendFailed("av_malloc failed".into()));
            }
            ptr::copy_nonoverlapping(annexb.as_ptr(), buf, annexb.len());

            av_packet_unref(self.pkt);
            (*self.pkt).data = buf;
            (*self.pkt).size = annexb.len() as i32;
            (*self.pkt).pts  = pts as i64;

            let ret = avcodec_send_packet(self.ctx, self.pkt);

            av_free(buf as *mut _);
            (*self.pkt).data = ptr::null_mut();
            (*self.pkt).size = 0;

            if ret < 0 {
                let eagain = -(libc_eagain());
                if ret == eagain {
                    self.drain_frames()?;
                    avcodec_send_packet(self.ctx, self.pkt);
                }
                // other errors: skip packet, don't propagate
            }

            self.drain_frames()
        }
    }

    unsafe fn drain_frames(&mut self) -> Result<(), DecodeError> {
        let eagain  = -(libc_eagain());
        let eof_err = AVERROR_EOF;

        loop {
            av_frame_unref(self.frame);
            let ret = avcodec_receive_frame(self.ctx, self.frame);

            if ret == eagain || ret == eof_err { break; }
            if ret < 0 { break; } // skip undecodable frames silently

            // ── GPU or CPU? Check the actual decoded frame format ─────────────
            //
            // self.is_hw tells us what we requested, but the actual frame format
            // is the ground truth. If get_format returned AV_PIX_FMT_D3D11,
            // FFmpeg outputs hardware frames; otherwise it outputs YUV420P/NV12.
            //
            // We must NOT call av_hwframe_transfer_data on a software frame.

            let fmt = (*self.frame).format;
            let frame_is_hw = fmt == AVPixelFormat::AV_PIX_FMT_D3D11 as i32;

            let display = if frame_is_hw {
                // GPU frame in D3D11 texture → transfer to system memory (NV12)
                av_frame_unref(self.sw_frame);
                let r = av_hwframe_transfer_data(self.sw_frame, self.frame, 0);
                if r < 0 { continue; } // GPU busy or transfer failed — skip frame
                self.sw_frame
            } else {
                // Software frame (YUV420P or NV12) — use directly
                self.frame
            };

            // Update is_hw based on first real frame (more accurate than post-open check)
            if self.frames_decoded == 0 {
                self.is_hw = frame_is_hw;
            }

            if let Err(e) = self.extract_yuv(display) {
                eprintln!("[FFmpeg] extract: {}", e);
            }
        }
        Ok(())
    }

    unsafe fn extract_yuv(&mut self, frame: *mut AVFrame) -> Result<(), DecodeError> {
        let w   = (*frame).width  as usize;
        let h   = (*frame).height as usize;
        let fmt = (*frame).format;
        if w == 0 || h == 0 { return Ok(()); }

        let dw = w / 2;
        let dh = h / 2;

        let (y_plane, u_plane, v_plane) = if fmt == AVPixelFormat::AV_PIX_FMT_NV12 as i32 {
            // NV12: Y plane + interleaved UV — output of hardware transfer
            let y_stride  = (*frame).linesize[0] as usize;
            let uv_stride = (*frame).linesize[1] as usize;
            let y_ptr     = (*frame).data[0];
            let uv_ptr    = (*frame).data[1];
            if y_ptr.is_null() || uv_ptr.is_null() { return Ok(()); }

            let mut y = Vec::with_capacity(dw * dh);
            for row in (0..h).step_by(2) {
                let src = std::slice::from_raw_parts(y_ptr.add(row * y_stride), w);
                for col in (0..w).step_by(2) { y.push(src[col]); }
            }
            let mut u = Vec::with_capacity(dw * dh / 4);
            let mut v = Vec::with_capacity(dw * dh / 4);
            for row in (0..h / 2).step_by(2) {
                let src = std::slice::from_raw_parts(uv_ptr.add(row * uv_stride), w);
                for col in (0..w / 2).step_by(2) {
                    u.push(src[col * 2]);
                    v.push(src[col * 2 + 1]);
                }
            }
            (y, u, v)

        } else if fmt == AVPixelFormat::AV_PIX_FMT_YUV420P as i32 {
            // YUV420P: separate Y, U, V planes — software decode output
            let y_stride = (*frame).linesize[0] as usize;
            let u_stride = (*frame).linesize[1] as usize;
            let v_stride = (*frame).linesize[2] as usize;
            let y_ptr    = (*frame).data[0];
            let u_ptr    = (*frame).data[1];
            let v_ptr    = (*frame).data[2];
            if y_ptr.is_null() || u_ptr.is_null() || v_ptr.is_null() { return Ok(()); }

            let half_w = w / 2;
            let half_h = h / 2;

            let mut y = Vec::with_capacity(dw * dh);
            for row in (0..h).step_by(2) {
                let src = std::slice::from_raw_parts(y_ptr.add(row * y_stride), w);
                for col in (0..w).step_by(2) { y.push(src[col]); }
            }
            let mut u = Vec::with_capacity(dw * dh / 4);
            for row in (0..half_h).step_by(2) {
                let src = std::slice::from_raw_parts(u_ptr.add(row * u_stride), half_w);
                for col in (0..half_w).step_by(2) { u.push(src[col]); }
            }
            let mut v = Vec::with_capacity(dw * dh / 4);
            for row in (0..half_h).step_by(2) {
                let src = std::slice::from_raw_parts(v_ptr.add(row * v_stride), half_w);
                for col in (0..half_w).step_by(2) { v.push(src[col]); }
            }
            (y, u, v)

        } else {
            return Ok(()); // unexpected format — skip silently
        };

        self.frames_decoded += 1;
        if let Ok(mut guard) = self.latest.lock() {
            *guard = Some(YuvFrame {
                y_plane, u_plane, v_plane,
                width: dw as u32, height: dh as u32, pts: 0,
            });
        }
        Ok(())
    }
}

impl Drop for FfmpegDecoder {
    fn drop(&mut self) {
        unsafe {
            av_frame_free(&mut self.frame);
            av_frame_free(&mut self.sw_frame);
            av_packet_free(&mut self.pkt);
            avcodec_free_context(&mut self.ctx);
        }
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn build_extradata(vps: &[u8], sps: &[u8], pps: &[u8]) -> Vec<u8> {
    let mut v = Vec::new();
    if !vps.is_empty() {
        v.extend_from_slice(&[0,0,0,1]); v.extend_from_slice(vps);
    }
    v.extend_from_slice(&[0,0,0,1]); v.extend_from_slice(sps);
    v.extend_from_slice(&[0,0,0,1]); v.extend_from_slice(pps);
    v
}

fn avcc_to_annexb(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + 4);
    let mut pos = 0;
    while pos + 4 <= data.len() {
        let nal_len = u32::from_be_bytes([data[pos],data[pos+1],data[pos+2],data[pos+3]]) as usize;
        pos += 4;
        if nal_len == 0 || pos + nal_len > data.len() { break; }
        out.extend_from_slice(&[0,0,0,1]);
        out.extend_from_slice(&data[pos..pos + nal_len]);
        pos += nal_len;
    }
    out
}

#[inline]
fn libc_eagain() -> i32 { 11 } // EAGAIN = 11 on Windows