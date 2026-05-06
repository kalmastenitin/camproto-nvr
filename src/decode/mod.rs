#[cfg(target_os = "macos")]
pub mod macos;

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
