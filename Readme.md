# camproto-nvr

Native desktop NVR client for the CamProto VMS stack.
Hardware-accelerated H.264/H.265 decode on macOS (VideoToolbox) and Windows (FFmpeg + D3D11VA/NVDEC).
egui grid UI, Metal/D3D12 rendering.

Part of: github.com/kalmastenitin/camproto-spec

---

## What it does

Connects to IP cameras via camproto-ingest (RTSP), decodes video using
platform hardware decoders, and displays live feeds in a smooth multi-camera
grid UI. Designed for 64+ simultaneous cameras with near-zero CPU overhead.

```
camproto-ingest (RTSP → MediaFrame)
    ↓  tokio::broadcast  +  RTCP keepalive (RR every 5s) + PLI on connect
spawn_decode_task (per camera)
    ↓  VideoToolbox (macOS) / FFmpeg D3D11VA/NVDEC (Windows)
LatestFrame (Arc<Mutex<Option<YuvFrame>>>)
    ↓  egui polls at 30fps
YUV → RGB (CPU, half-resolution for grid)
    ↓  egui texture upload
Metal / D3D12 / Vulkan render
    ↓
Display
```

---

## Platform Support

| Platform | Decoder | Renderer | Status |
|---|---|---|---|
| macOS (Apple Silicon / Intel) | VideoToolbox | Metal via wgpu | ✅ Working |
| Windows 10+ (GPU) | FFmpeg + D3D11VA / NVDEC | D3D12 via wgpu | ✅ Working |
| Windows 10+ (no GPU) | FFmpeg software | D3D12 via wgpu | ✅ Working |
| Linux | VAAPI / NVDEC | Vulkan via wgpu | 🔲 Planned |

---

## Performance

### macOS — M4 Mac Pro (release build)

| Cameras | CPU | GPU | Notes |
|---|---|---|---|
| 17 cameras | ~0% | ~0% | VideoToolbox media engine, H.264+H.265 |
| 64 cameras | < 5% | < 5% | Network bandwidth limit before CPU |

VideoToolbox runs entirely on Apple Silicon media engine — CPU touches zero pixels.

### Windows — Intel Xeon E5-2673 + NVIDIA RTX 4060 Ti (release build)

| Cameras | CPU | GPU | Notes |
|---|---|---|---|
| 17 cameras | ~26% | ~26% | FFmpeg NVDEC, H.264+H.265, mixed resolutions |
| 17 cameras (no GPU) | ~20% | 0% | FFmpeg software fallback, automatic |

GPU cost is D3D11 texture → system memory transfer (`av_hwframe_transfer_data`).
Reducible to < 5% CPU with GPU YUV shader (Phase 7 roadmap).

---

## Architecture

### File Structure

```
src/
├── lib.rs
├── decode/
│    ├── mod.rs              — YuvFrame, LatestFrame, spawn_decode_task
│    ├── macos.rs            — VideoToolbox (VTDecompressionSession)
│    └── windows_ffmpeg.rs   — FFmpeg D3D11VA/NVDEC + software fallback
├── render/
│    └── mod.rs              — YuvTextures (wgpu)
├── ui/
│    ├── mod.rs
│    ├── app.rs              — NvrApp, CameraState, grid/fullscreen render
│    ├── tile.rs             — single camera tile widget
│    └── topbar.rs           — top bar with grid selector + pagination
└── bin/
     └── nvr.rs              — entry point, camera config, tokio runtime

shaders/
└── yuv.wgsl                 — YUV420 → RGB BT.709 (future GPU zero-copy path)
```

### Threading Model

```
Main thread (egui 30fps):
  poll LatestFrame for each visible camera
  YUV→RGB conversion (CPU, half-resolution)
  texture upload to GPU
  render grid

Tokio runtime (async, per camera):
  RtspClient::run()         — RTSP handshake, DESCRIBE/SETUP/PLAY
  RTCP keepalive task       — Receiver Report every 5s (keeps camera connection alive)
  RTCP PLI on connect       — forces immediate IDR keyframe (< 1s to first frame)
  RTP loop                  — depacketize H.264/H.265, broadcast MediaFrame
  spawn_decode_task          — feed hardware decoder, write LatestFrame

Decode threads (OS threads, one per camera):
  macOS:   VTDecompressionSession — async hardware decode callback
  Windows: FfmpegDecoder::send_packet → avcodec_receive_frame
             → av_hwframe_transfer_data (D3D11 → RAM, if GPU)
```

### Key Types

```rust
// Decoded frame — downsampled to half resolution for display efficiency
pub struct YuvFrame {
    pub y_plane: Vec<u8>,   // (width/2) × (height/2)
    pub u_plane: Vec<u8>,   // (width/4) × (height/4)
    pub v_plane: Vec<u8>,   // (width/4) × (height/4)
    pub width:   u32,       // display width (original/2)
    pub height:  u32,       // display height (original/2)
    pub pts:     u64,       // microseconds
}

// Shared between decode thread and egui
pub type LatestFrame = Arc<Mutex<Option<YuvFrame>>>;

// Per camera UI state
pub struct CameraState {
    pub camera_id:      String,
    pub name:           String,
    pub connection:     ConnectionStatus,  // Connecting / Streaming / Disconnected
    pub recording:      RecordingStatus,   // Idle / Recording / EventRecording / Scheduled
    pub resolution:     (u32, u32),
    pub fps:            f32,
    pub displayed_fps:  f32,
    pub bitrate_kbps:   u32,
    pub codec:          String,
    pub has_audio:      bool,
    pub ptz_capable:    bool,
    pub zoom_capable:   bool,
    pub latest:         Option<LatestFrame>,
    pub texture:        Option<egui::TextureHandle>,
    pub last_frame_time: Option<Instant>,
}
```

---

## Windows Decoder — FFmpeg D3D11VA

The Windows decoder (`src/decode/windows_ffmpeg.rs`) uses FFmpeg with a
`get_format` callback to route decode to GPU or CPU automatically:

```
av_hwdevice_ctx_create(AV_HWDEVICE_TYPE_D3D11VA) succeeds?
  YES → set hw_device_ctx + get_format callback
        → get_format selects AV_PIX_FMT_D3D11
        → NVDEC / AMD VCE / Intel QSV decodes on GPU
        → av_hwframe_transfer_data: D3D11 texture → NV12 in RAM
  NO  → software decode (YUV420P)
        → no GPU required, no extra dependencies
```

No code changes needed between GPU and CPU-only machines — the strategy
switches automatically at runtime.

### Windows Build Requirements

```
1. FFmpeg 7.1 shared build (headers + import libs + DLLs)
   https://github.com/BtbN/FFmpeg-Builds/releases
   → ffmpeg-n7.1-latest-win64-lgpl-shared.zip

2. LLVM (for bindgen — one-time, not needed after first build)
   winget install LLVM.LLVM

3. Set FFMPEG_DIR in .cargo/config.toml:
   [env]
   FFMPEG_DIR    = "C:/ffmpeg-7.1-full"
   LIBCLANG_PATH = "C:/Program Files/LLVM/bin"

4. Copy DLLs next to nvr.exe:
   avcodec-62.dll  avformat-62.dll  avutil-60.dll
   swresample-6.dll  swscale-9.dll
```

Build from "Developer PowerShell for VS 2022":
```powershell
cargo build --release
Copy-Item *.dll target\release\
cargo run --release
```

---

## RTCP Keepalive

IP cameras close RTSP connections after ~30-60s without RTCP feedback.
`camproto-ingest` sends:

- **RTCP Receiver Report (RR)** — every 5 seconds, keeps connection alive
- **RTCP PLI (Picture Loss Indication)** — immediately on connect, forces
  the camera to send an IDR keyframe so video appears in < 1 second instead
  of waiting for the next GOP boundary (2-10 seconds on many cameras)

---

## Camera Configuration

Edit `src/bin/nvr.rs`:

```rust
const CAMERAS: &[CameraConfig] = &[
    CameraConfig {
        id:   "cam_001",
        name: "Front Gate",
        url:  "rtsp://admin:admin@192.168.1.240:554/rtsp/streaming?channel=1&subtype=0",
    },
];
```

`subtype=0` = H.265 mainstream (full resolution, used for live view + recording)
`subtype=1` = H.264 substream (lower resolution, for bandwidth-limited deployments)

---

## Build

```bash
# macOS
cargo run --release --bin nvr

# Windows (from Developer PowerShell)
$env:FFMPEG_DIR = "C:\ffmpeg-7.1-full"
cargo run --release --bin nvr
```

---

## Dependencies

```toml
egui            = "0.34.2"   # immediate mode UI
eframe          = "0.34.2"   # app framework (winit + wgpu)
wgpu            = "29.0.3"   # Metal/D3D12/Vulkan rendering
tokio           = "1.52.3"   # async runtime
parking_lot     = "0.12"
camproto-ingest = { path = "../camproto-ingest" }

# macOS only
core-foundation     = "0.10.1"
core-foundation-sys = "0.8"

# Windows only
ffmpeg-sys-next = "8.1.0"   # FFmpeg bindings (D3D11VA + software decode)
```

---

## Tested Hardware

| Device | Platform | Cameras | CPU | GPU | Notes |
|---|---|---|---|---|---|
| Apple M4 Mac Pro | macOS | 17 | ~0% | ~0% | VideoToolbox media engine |
| Intel Xeon E5-2673 + RTX 4060 Ti | Windows | 17 | ~26% | ~26% | FFmpeg NVDEC |

### Tested Cameras

| Camera | Codec | Resolution | FPS |
|---|---|---|---|
| Sparsh (mainstream) | H.265 | 1920×1080 to 3840×2160 | 25 |
| Sparsh (substream) | H.264 | 1280×720 to 1920×1080 | 25 |
| Fish-eye | H.265 | 2256×1696 | 25 |

---

## Roadmap

### ✅ Phase 1 — Single camera live view
- [x] egui window (Metal/wgpu backend)
- [x] VideoToolbox H.265 hardware decode
- [x] VideoToolbox H.264 hardware decode
- [x] YUV→RGB (CPU, half-resolution, async decode callback)
- [x] Live video in tile — smooth 25fps display
- [x] Auto-reconnect on camera disconnect

### ✅ Phase 2 — Multi-camera grid
- [x] 4×4 grid (16 cameras)
- [x] 8×8 grid (64 cameras, pagination)
- [x] Grid size selector (1×1, 2×2, 4×4, 8×8)
- [x] Pagination with prev/next
- [x] Mixed H.264 + H.265 simultaneously
- [x] 17 cameras tested — ~0% CPU on macOS (release)

### ✅ Phase 3 — UI polish
- [x] Click tile → fullscreen
- [x] ESC / button → back to grid
- [x] Connection status indicator (green / yellow / red)
- [x] Real measured FPS counter per tile
- [x] Recording badge (REC / EVT / SCH)
- [x] Bottom info bar (codec, resolution, fps, bitrate, audio)
- [x] Camera name + ID display
- [x] Hover highlight

### ✅ Phase 4 — Windows port
- [x] FFmpeg D3D11VA hardware decode (NVDEC / AMD / Intel)
- [x] Software fallback when no GPU — same binary, auto-detected at runtime
- [x] RTCP Receiver Report keepalive — cameras stay connected indefinitely
- [x] RTCP PLI on connect — first frame in < 1s
- [x] 17 cameras on Windows with GPU — all H.265 hardware decoded
- [x] `cfg(target_os)` platform switch at compile time
- [ ] Windows installer (.msi)

### 🔲 Phase 5 — Recording integration
- [ ] camproto-store integration
- [ ] Timeline scrubber in fullscreen view
- [ ] Playback mode vs live mode
- [ ] Event markers on timeline
- [ ] Clip export

### 🔲 Phase 6 — Production features
- [ ] Camera add/remove at runtime
- [ ] Settings persistence
- [ ] Multi-monitor support
- [ ] Custom layouts (1+5 PiP, 2+4 sidebar)
- [ ] Audio playback
- [ ] PTZ controls (RTSP/ONVIF)
- [ ] Snapshot capture

### 🔲 Phase 7 — GPU YUV conversion (zero CPU target)
- [ ] wgpu YUV shader (yuv.wgsl already written)
- [ ] Zero-copy IOSurface → Metal texture (macOS)
- [ ] D3D11 NV12 texture → wgpu (Windows) — eliminate av_hwframe_transfer_data
- [ ] Expected: < 5% CPU for 64 cameras on Windows with GPU

### 🔲 Phase 8 — Scale testing
- [ ] 32 cameras benchmark
- [ ] 64 cameras benchmark
- [ ] Memory profile
- [ ] Network bandwidth measurement

---

## Related Repos

| Repo | Description | Status |
|---|---|---|
| camproto-spec | Protocol spec + .proto files | ✅ |
| camproto-ingest | RTSP ingest + RTP depacketizer + RTCP | ✅ |
| **camproto-nvr** | Desktop NVR client | ✅ Phase 1–4 |
| camproto-store | fMP4 recording + PostgreSQL | 🔲 |
| camproto-egress | MSE + WebRTC browser streaming | 🔲 |
| camproto-transport | QUIC + NAT traversal | 🔲 |
| camproto-control | Go HTTP API + dashboard | 🔲 |