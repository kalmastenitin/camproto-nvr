#[cfg(target_os = "macos")]
pub mod macos;

#[cfg(target_os = "windows")]
pub mod windows_mf;

#[cfg(target_os = "windows")]
pub mod windows_d3d11;

#[cfg(target_os = "windows")]
use std::sync::OnceLock;

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

// ── Shared latest frame ───────────────────────────────────────────────────────

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

#[cfg(target_os = "windows")]
pub enum PlatformDecoder {
    Hardware(windows_d3d11::D3D11Decoder),
    Software(windows_mf::MediaFoundationDecoder),
}

#[cfg(target_os = "windows")]
impl PlatformDecoder {
    fn send_packet(&mut self, data: &[u8], pts: u64) -> Result<(), DecodeError> {
        match self {
            PlatformDecoder::Hardware(d) => d.send_packet(data, pts),
            PlatformDecoder::Software(d) => d.send_packet(data, pts),
        }
    }
}

// Linux / other stub
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
struct PlatformDecoder;

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
impl PlatformDecoder {
    fn send_packet(&mut self, _data: &[u8], _pts: u64) -> Result<(), DecodeError> {
        Ok(())
    }
}

#[cfg(target_os = "windows")]
struct D3D11DeviceWrapper {
    device: windows::Win32::Graphics::Direct3D11::ID3D11Device,
    context: windows::Win32::Graphics::Direct3D11::ID3D11DeviceContext,
}

#[cfg(target_os = "windows")]
unsafe impl Sync for D3D11DeviceWrapper {}
#[cfg(target_os = "windows")]
unsafe impl Send for D3D11DeviceWrapper {}

#[cfg(target_os = "windows")]
static D3D11_DEVICE: OnceLock<D3D11DeviceWrapper> = OnceLock::new();

#[cfg(target_os = "windows")]
pub fn get_or_create_d3d11_device() -> Result<
    (
        &'static windows::Win32::Graphics::Direct3D11::ID3D11Device,
        &'static windows::Win32::Graphics::Direct3D11::ID3D11DeviceContext,
    ),
    DecodeError,
> {
    use windows::Win32::Graphics::Direct3D::*;
    use windows::Win32::Graphics::Direct3D11::*;

    // return existing if already initialized
    if let Some(w) = D3D11_DEVICE.get() {
        return Ok((&w.device, &w.context));
    }

    // create device
    let mut device: Option<ID3D11Device> = None;
    let mut context: Option<ID3D11DeviceContext> = None;
    let feature_levels = [D3D_FEATURE_LEVEL_11_0];

    unsafe {
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            None,
            D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
            Some(&feature_levels),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )
        .map_err(|e| DecodeError::InitFailed(format!("D3D11CreateDevice: {}", e)))?;
    }

    // set — if racing thread already set it, discard ours and use theirs
    let _ = D3D11_DEVICE.set(D3D11DeviceWrapper {
        device: device.unwrap(),
        context: context.unwrap(),
    });

    let w = D3D11_DEVICE.get().unwrap();
    Ok((&w.device, &w.context))
}

// ── Decode task ───────────────────────────────────────────────────────────────

pub fn spawn_decode_task(
    camera_id: String,
    mut rx: tokio::sync::broadcast::Receiver<MediaFrame>,
    latest: LatestFrame,
) {
    let (tx, rx_blocking) = mpsc::sync_channel::<MediaFrame>(1);
    let camera_id_async = camera_id.clone();

    // async task: receive frames from broadcast, forward to OS thread
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(frame) => {
                    if frame.camera_id != camera_id_async {
                        continue;
                    }
                    let _ = tx.try_send(frame); // non-blocking drop if full
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
                            // try hardware first
                            match windows_d3d11::D3D11Decoder::new_h265(
                                vps.as_ref(),
                                sps.as_ref(),
                                pps.as_ref(),
                                latest.clone(),
                            ) {
                                Ok(dec) => {
                                    decoder = Some(PlatformDecoder::Hardware(dec));
                                    println!("[{}] D3D11VA H265 ready", camera_id_thread);
                                }
                                Err(e) => {
                                    eprintln!(
                                        "[{}] D3D11VA H265 failed, trying software: {}",
                                        camera_id_thread, e
                                    );
                                    match windows_mf::MediaFoundationDecoder::new_h265(
                                        vps.as_ref(),
                                        sps.as_ref(),
                                        pps.as_ref(),
                                        latest.clone(),
                                    ) {
                                        Ok(dec) => {
                                            decoder = Some(PlatformDecoder::Software(dec));
                                            println!("[{}] SW H265 ready", camera_id_thread);
                                        }
                                        Err(e) => eprintln!(
                                            "[{}] SW H265 failed: {}",
                                            camera_id_thread, e
                                        ),
                                    }
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
                            // try hardware D3D11VA first
                            match windows_d3d11::D3D11Decoder::new_h264(
                                sps.as_ref(),
                                pps.as_ref(),
                                latest.clone(),
                            ) {
                                Ok(dec) => {
                                    decoder = Some(PlatformDecoder::Hardware(dec));
                                    println!("[{}] D3D11VA H264 ready", camera_id_thread);
                                }
                                Err(e) => {
                                    eprintln!(
                                        "[{}] D3D11VA failed, falling back to software: {}",
                                        camera_id_thread, e
                                    );
                                    match windows_mf::MediaFoundationDecoder::new_h264(
                                        sps.as_ref(),
                                        pps.as_ref(),
                                        latest.clone(),
                                    ) {
                                        Ok(dec) => {
                                            decoder = Some(PlatformDecoder::Software(dec));
                                            println!("[{}] SW H264 ready", camera_id_thread);
                                        }
                                        Err(e) => eprintln!(
                                            "[{}] SW H264 failed: {}",
                                            camera_id_thread, e
                                        ),
                                    }
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
