#[cfg(target_os = "macos")]
pub mod macos;

#[cfg(target_os = "windows")]
pub mod windows_mf;

use camproto_ingest::frame::{Codec, MediaFrame};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

// ── Decoded frame ─────────────────────────────────────────────────────────────

pub struct YuvFrame {
    pub y_plane: Vec<u8>,
    pub u_plane: Vec<u8>,
    pub v_plane: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub pts: u64,
}

pub type LatestFrame = Arc<Mutex<Option<YuvFrame>>>;

pub fn new_latest_frame() -> LatestFrame {
    Arc::new(Mutex::new(None))
}

// ── Decode error ──────────────────────────────────────────────────────────────

#[derive(Debug)]
pub enum DecodeError {
    InitFailed(String),
    SendFailed(String),
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DecodeError::InitFailed(s) => write!(f, "init failed: {}", s),
            DecodeError::SendFailed(s) => write!(f, "send failed: {}", s),
        }
    }
}

// ── Platform decoder ──────────────────────────────────────────────────────────

#[cfg(target_os = "macos")]
type PlatformDecoder = macos::VideoToolboxDecoder;

// Windows: MF software decode only.
// H264 → MFT with HARDWARE flag (NVDEC system-memory output, no D3D11 needed)
// H265 → MFT software only (HEVC Video Extension)
#[cfg(target_os = "windows")]
type PlatformDecoder = windows_mf::MediaFoundationDecoder;

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
struct PlatformDecoder;

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
impl PlatformDecoder {
    fn send_packet(&mut self, _data: &[u8], _pts: u64) -> Result<(), DecodeError> {
        Ok(())
    }
}

// ── Decode task ───────────────────────────────────────────────────────────────

pub fn spawn_decode_task(
    camera_id: String,
    mut rx: tokio::sync::broadcast::Receiver<MediaFrame>,
    latest: LatestFrame,
) {
    let (tx, rx_blocking) = mpsc::sync_channel::<MediaFrame>(1);
    let camera_id_async = camera_id.clone();

    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(frame) => {
                    if frame.camera_id != camera_id_async {
                        continue;
                    }
                    let _ = tx.try_send(frame);
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    eprintln!("[{}] dropped {} frames", camera_id_async, n);
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });

    let camera_id_thread = camera_id.clone();

    std::thread::Builder::new()
        .name(format!("decode-{}", camera_id))
        .spawn(move || {
            #[cfg(target_os = "windows")]
            unsafe {
                use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};
                let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            }

            let mut decoder: Option<PlatformDecoder> = None;

            loop {
                let frame = match rx_blocking.recv() {
                    Ok(f) => f,
                    Err(_) => break,
                };

                match &frame.codec {
                    Codec::H265 { vps, sps, pps } => {
                        #[cfg(target_os = "macos")]
                        if decoder.is_none() && frame.is_keyframe {
                            match macos::VideoToolboxDecoder::new(
                                vps.as_ref(),
                                sps.as_ref(),
                                pps.as_ref(),
                                latest.clone(),
                            ) {
                                Ok(dec) => {
                                    decoder = Some(dec);
                                    println!("[{}] H265 ready", camera_id_thread);
                                }
                                Err(e) => eprintln!("[{}] H265 init: {}", camera_id_thread, e),
                            }
                        }

                        #[cfg(target_os = "windows")]
                        if decoder.is_none() && frame.is_keyframe {
                            match windows_mf::MediaFoundationDecoder::new_h265(
                                vps.as_ref(),
                                sps.as_ref(),
                                pps.as_ref(),
                                latest.clone(),
                            ) {
                                Ok(dec) => {
                                    println!("[{}] H265 ready (software)", camera_id_thread);
                                    decoder = Some(dec);
                                }
                                Err(e) => {
                                    eprintln!("[{}] H265 init failed: {}", camera_id_thread, e)
                                }
                            }
                        }

                        if let Some(dec) = decoder.as_mut() {
                            if let Err(e) = dec.send_packet(frame.data.as_ref(), frame.pts) {
                                eprintln!("[{}] H265 error: {}", camera_id_thread, e);
                                decoder = None;
                            }
                        }
                    }

                    Codec::H264 { sps, pps } => {
                        #[cfg(target_os = "macos")]
                        if decoder.is_none() && frame.is_keyframe {
                            match macos::VideoToolboxDecoder::new_h264(
                                sps.as_ref(),
                                pps.as_ref(),
                                latest.clone(),
                            ) {
                                Ok(dec) => {
                                    decoder = Some(dec);
                                    println!("[{}] H264 ready", camera_id_thread);
                                }
                                Err(e) => eprintln!("[{}] H264 init: {}", camera_id_thread, e),
                            }
                        }

                        #[cfg(target_os = "windows")]
                        if decoder.is_none() && frame.is_keyframe {
                            // new_h264 tries hardware (NVDEC system-memory output) first,
                            // falls back to software if hardware not available.
                            match windows_mf::MediaFoundationDecoder::new_h264(
                                sps.as_ref(),
                                pps.as_ref(),
                                latest.clone(),
                            ) {
                                Ok(dec) => {
                                    let hw = dec.is_hardware();
                                    println!(
                                        "[{}] H264 ready ({})",
                                        camera_id_thread,
                                        if hw { "hardware" } else { "software" }
                                    );
                                    decoder = Some(dec);
                                }
                                Err(e) => {
                                    eprintln!("[{}] H264 init failed: {}", camera_id_thread, e)
                                }
                            }
                        }

                        if let Some(dec) = decoder.as_mut() {
                            if let Err(e) = dec.send_packet(frame.data.as_ref(), frame.pts) {
                                eprintln!("[{}] H264 error: {}", camera_id_thread, e);
                                decoder = None;
                            }
                        }
                    }

                    _ => {}
                }
            }

            #[cfg(target_os = "windows")]
            unsafe {
                use windows::Win32::System::Com::CoUninitialize;
                CoUninitialize();
            }
        })
        .expect("failed to spawn decode thread");
}
