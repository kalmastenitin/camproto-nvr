#[cfg(target_os = "macos")]
pub mod macos;

use camproto_ingest::frame::{Codec, MediaFrame};
use std::sync::{Arc, Mutex};

// ── Decoded frame — raw YUV420 planes from VideoToolbox ───────────────────────

pub struct YuvFrame {
    pub y_plane: Vec<u8>, // width x height bytes
    pub u_plane: Vec<u8>, // width/2 x height/2 bytes
    pub v_plane: Vec<u8>, // width/2 x height/2 bytes
    pub width: u32,
    pub height: u32,
    pub pts: u64, // microseconds
}

// ── Shared latest frame — decode thread writes, egui reads ───────────────────

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

// Platform wise decoder stub -- replaced per platform -----------------------------
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
struct PlatformDecoder;


#[cfg(target_os = "macos")]
type PlatformDecoder = macos::VideoToolboxDecoder;


#[cfg(not(any(target_os = "macos", target_os = "windows")))]
impl PlatformDecoder {
    fn send_packet(&mut self, _data: &[u8], _pts: u64) -> Result<(), DecodeError> {
        Ok(())
    }
}

// Windows — stub until MediaFoundation decoder is implemented
#[cfg(target_os = "windows")]
struct PlatformDecoder;

#[cfg(target_os = "windows")]
impl PlatformDecoder {
    fn send_packet(&mut self, _data: &[u8], _pts: u64) -> Result<(), DecodeError> {
        Ok(())
    }
}
// Linux / other — stub
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
struct PlatformDecoder;

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
impl PlatformDecoder {
    fn send_packet(&mut self, _data: &[u8], _pts: u64) -> Result<(), DecodeError> {
        Ok(())
    }
}


pub fn spawn_decode_task(
    camera_id: String,
    mut rx: tokio::sync::broadcast::Receiver<MediaFrame>,
    latest: LatestFrame,
) {
    tokio::spawn(async move {
        let mut decoder: Option<PlatformDecoder> = None;

        loop {
            match rx.recv().await {
                Ok(frame) => {
                    // only handle video frames for this camera
                    if frame.camera_id != camera_id {
                        continue;
                    }

                    match &frame.codec {
                        Codec::H265 { vps, sps, pps } => {
                            // create decoder on first keyframe
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
                                        println!("[{}] decoder ready", camera_id);
                                    }
                                    Err(e) => {
                                        eprintln!("[{}] decoder init failed: {}", camera_id, e);
                                    }
                                }
                            }

                            // feed frame to decoder
                            if let Some(dec) = decoder.as_mut() {
                                if let Err(e) = dec.send_packet(frame.data.as_ref(), frame.pts) {
                                    eprintln!("[{}] decode error: {}", camera_id, e);
                                    decoder = None; // reset — wait for next keyframe
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
                                        println!("[{}] H264 decoder ready", camera_id);
                                    }
                                    Err(e) => {
                                        eprintln!(
                                            "[{}] H264 decoder init failed: {}",
                                            camera_id, e
                                        );
                                    }
                                }
                            }
                            if let Some(dec) = decoder.as_mut() {
                                if let Err(e) = dec.send_packet(frame.data.as_ref(), frame.pts) {
                                    eprintln!("[{}] H264 decode error: {}", camera_id, e);
                                    decoder = None;
                                }
                            }
                        }

                        _ => {} // audio frames — ignore for now
                    }
                }

                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    eprintln!("[{}] dropped {} frames — resetting decoder", camera_id, n);
                    // decoder = None; // force re-init on next keyframe
                }

                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    println!("[{}] channel closed", camera_id);
                    break;
                }
            }
        }
    });
}
