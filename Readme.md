# camproto-nvr

Native desktop NVR client for the CamProto VMS stack.
Hardware-accelerated H.264/H.265 decode, egui grid UI, Metal/D3D12 rendering.

Part of: github.com/kalmastenitin/camproto-spec

---

## What it does

Connects to IP cameras via camproto-ingest (RTSP), decodes video using
platform hardware decoders, and displays live feeds in a smooth multi-camera
grid UI. Designed for 64+ simultaneous cameras with near-zero CPU overhead.

```
camproto-ingest (RTSP → MediaFrame)
    ↓  tokio::broadcast
spawn_decode_task (per camera)
    ↓  VideoToolbox / MediaFoundation / VAAPI
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
| Windows 10+ | MediaFoundation + D3D11VA | D3D12 via wgpu | 🔲 Planned |
| Linux | VAAPI / NVDEC | Vulkan via wgpu | 🔲 Planned |

---

## Performance — M4 Mac Pro (release build)

| Cameras | CPU (User) | Notes |
|---|---|---|
| 8 cameras | ~25% (debug) / ~0% (release) | Mixed H.264+H.265 |
| 17 cameras | ~0% additional | Media engine handles decode |
| 64 cameras | Estimated < 10% | Network bandwidth limits before CPU |

VideoToolbox runs entirely on Apple Silicon media engine.
CPU touches zero pixels between decode and display.

---

## Architecture

### File Structure

```
src/
├── lib.rs
├── decode/
│    ├── mod.rs          — YuvFrame, LatestFrame, spawn_decode_task
│    └── macos.rs        — VideoToolbox bindings (VTDecompressionSession)
├── render/
│    └── mod.rs          — YuvTextures (wgpu, planned)
├── ui/
│    ├── mod.rs
│    ├── app.rs          — NvrApp, CameraState, grid/fullscreen render
│    ├── tile.rs         — single camera tile widget
│    └── topbar.rs       — top bar with grid selector + pagination
└── bin/
     └── nvr.rs          — entry point, camera config, tokio runtime

shaders/
└── yuv.wgsl             — YUV420 → RGB BT.709 fragment shader (future GPU path)
```

### Threading Model

```
Main thread (egui 30fps):
  poll LatestFrame for each visible camera
  YUV→RGB conversion (CPU, half-resolution)
  texture upload to GPU
  render grid

Tokio runtime (async):
  per camera: RtspClient::run() — RTSP handshake, RTP receive, MediaFrame broadcast
  per camera: spawn_decode_task — receives MediaFrame, feeds hardware decoder

VideoToolbox thread (OS managed):
  VTDecompressionSession — hardware H.265/H.264 decode
  decompress_callback — copies YUV planes (downsampled), writes LatestFrame
```

### Key Types

```rust
// Decoded frame — downsampled to half resolution in callback
pub struct YuvFrame {
    pub y_plane: Vec<u8>,   // (width/2) × (height/2)
    pub u_plane: Vec<u8>,   // (width/4) × (height/4)
    pub v_plane: Vec<u8>,   // (width/4) × (height/4)
    pub width:   u32,       // display width (original/2)
    pub height:  u32,       // display height (original/2)
    pub pts:     u64,       // microseconds
}

// Shared between decode callback and egui
pub type LatestFrame = Arc<Mutex<Option<YuvFrame>>>;

// Per camera UI state
pub struct CameraState {
    pub camera_id:      String,
    pub name:           String,
    pub connection:     ConnectionStatus,  // Connecting / Streaming / Disconnected
    pub recording:      RecordingStatus,   // Idle / Recording / EventRecording / Scheduled
    pub resolution:     (u32, u32),        // actual camera resolution
    pub fps:            f32,               // SDP framerate
    pub displayed_fps:  f32,               // measured display fps
    pub bitrate_kbps:   u32,
    pub codec:          String,
    pub has_audio:      bool,
    pub ptz_capable:    bool,              // reserved
    pub zoom_capable:   bool,              // reserved
    pub latest:         Option<LatestFrame>,
    pub texture:        Option<egui::TextureHandle>,
    pub last_frame_time: Option<Instant>,
}
```

---

## Camera Configuration

Edit `src/bin/nvr.rs` to add cameras:

```rust
const CAMERAS: &[CameraConfig] = &[
    CameraConfig {
        id:   "cam_001",
        name: "Front Gate",
        // subtype=1 = H.264 substream — maximum compatibility
        // subtype=0 = H.265 mainstream — use for recording
        url:  "rtsp://admin:admin@192.168.1.240:554/rtsp/streaming?channel=1&subtype=1",
    },
    // add more cameras here
];
```

**Important:** Use `subtype=1` (H.264 substream) for live view.
H.265 mainstream (`subtype=0`) is for recording only — H.265 has compatibility
issues on older Windows hardware without GPU or codec packs.

---

## Dual Stream Architecture

```
Camera
  ├── subtype=0 (H.265 mainstream, 2-4 Mbps)  → camproto-store (recording)
  └── subtype=1 (H.264 substream, 256-512 Kbps) → camproto-nvr live view
                                                   → browser MSE/WebRTC
```

This eliminates all H.265 Windows compatibility issues while preserving
recording quality and efficiency.

---

## VideoToolbox Implementation Notes

### Format Description

```rust
// H.265: requires VPS + SPS + PPS from SDP a=fmtp line
CMVideoFormatDescriptionCreateFromHEVCParameterSets(
    allocator: NULL,
    count: 3,
    ptrs: [vps, sps, pps],
    sizes: [vps_len, sps_len, pps_len],
    nal_header_len: 4,
    extensions: NULL,
    &format_desc
)

// H.264: requires SPS + PPS from SDP a=fmtp sprop-parameter-sets
CMVideoFormatDescriptionCreateFromH264ParameterSets(
    allocator: NULL,
    count: 2,
    ptrs: [sps, pps],
    sizes: [sps_len, pps_len],
    nal_header_len: 4,
    &format_desc
)
```

### Known Issues and Fixes

**segfault on first frame (fixed)**
- Cause: `Arc::into_raw` returns pointer to inner data, not to Arc itself
- Fix: `Box::new(arc)` then `Box::into_raw` — gives pointer to Arc

**malloc: pointer being freed was not allocated (fixed)**
- Cause: `CMBlockBufferCreateWithMemoryBlock` with NULL allocator tries to free Rust memory
- Fix: `data.to_vec()` + `std::mem::forget()` — give CoreMedia malloc-owned memory

**VTDecompressionSessionRelease not found (fixed)**
- Cause: defined as inline function in Apple SDK headers, not a real symbol
- Fix: use `CFRelease(session)` instead

**Video plays 3-4 frames then freezes (fixed)**
- Cause: egui does not repaint unless explicitly requested
- Fix: `ctx.request_repaint_after(Duration::from_millis(33))`

**Frame drops / stutter (fixed)**
- Cause: synchronous VT decode (flag=0) blocked tokio task
- Fix: async decode flag=1 (`kVTDecodeFrame_EnableAsynchronousDecompression`)
- Also: downsample YUV in callback (not in egui thread)

---

## Build

```bash
# debug (slow YUV conversion, use for development)
cargo run --bin nvr

# release (recommended, near-zero CPU)
cargo run --release --bin nvr

# build binary
cargo build --release
```

---

## Dependencies

```toml
egui        = "0.34.2"          # immediate mode UI
eframe      = "0.34.2"          # app framework (winit + wgpu)
wgpu        = "29.0.3"          # Metal/D3D12/Vulkan GPU rendering
tokio       = "1.52.2"          # async runtime
parking_lot = "0.12"            # fast mutex
camproto-ingest = { path = "../camproto-ingest" }

# macOS only
core-foundation     = "0.10.1"
core-foundation-sys = "0.8"
```

---

## Roadmap

### ✅ Phase 1 — Single camera live view (complete)
- [x] egui window (Metal/wgpu backend)
- [x] VideoToolbox H.265 hardware decode
- [x] VideoToolbox H.264 hardware decode
- [x] YUV→RGB (CPU, half-resolution, async decode callback)
- [x] Live video in tile — smooth 25fps display
- [x] Auto-reconnect on camera disconnect

### ✅ Phase 2 — Multi-camera grid (complete)
- [x] 4×4 grid (16 cameras visible)
- [x] 8×8 grid (64 cameras, pagination)
- [x] Grid size selector (1×1, 2×2, 4×4, 8×8)
- [x] Pagination with prev/next
- [x] Mixed H.264 + H.265 cameras simultaneously
- [x] 17 cameras tested — ~0% CPU in release build

### ✅ Phase 3 — Polish (complete)
- [x] Click tile → fullscreen
- [x] ESC / button → back to grid
- [x] Green dot = streaming, yellow = connecting, red = disconnected
- [x] Real measured FPS counter per tile
- [x] Recording badge (REC / EVT / SCH)
- [x] Bottom info bar (codec, resolution, fps, bitrate, audio indicator)
- [x] Camera name + ID display
- [x] Hover highlight on tiles

### 🔲 Phase 4 — Windows port
- [ ] MediaFoundation H.264 decoder (D3D11VA hardware)
- [ ] Software fallback for hardware without GPU
- [ ] cfg(target_os) compile-time platform switch
- [ ] Windows installer (.msi)
- [ ] Tested on Windows 10 hardware without GPU

### 🔲 Phase 5 — Recording integration (after camproto-store)
- [ ] camproto-store integration
- [ ] Timeline scrubber in fullscreen view
- [ ] Playback mode vs live mode
- [ ] Event markers on timeline
- [ ] Clip export

### 🔲 Phase 6 — Production features
- [ ] Camera add/remove at runtime (no restart required)
- [ ] Settings persistence (camera list, grid layout)
- [ ] Multi-monitor support
- [ ] Custom layouts (1+5 PiP, 2+4 sidebar)
- [ ] Audio playback
- [ ] PTZ controls (pan/tilt/zoom over RTSP/ONVIF)
- [ ] Optical zoom controls
- [ ] Image controls (brightness, contrast)
- [ ] Snapshot capture

### 🔲 Phase 7 — GPU YUV conversion (performance)
- [ ] wgpu YUV shader (yuv.wgsl already written)
- [ ] Zero-copy IOSurface → Metal texture (macOS)
- [ ] D3D11 texture → wgpu (Windows)
- [ ] Expected: < 1% CPU for 64 cameras

### 🔲 Phase 8 — Scale testing
- [ ] 32 cameras benchmark
- [ ] 64 cameras benchmark
- [ ] Memory profile
- [ ] Network bandwidth measurement

---

## H.265 Windows Compatibility Notes

H.265 decode on Windows requires either:
- GPU with HEVC hardware decode (Intel 6th gen+, AMD Polaris+, NVIDIA Maxwell+)
- "HEVC Video Extensions" from Microsoft Store ($0.99)

**camproto-nvr uses H.264 substream for live view** to avoid this entirely.
H.265 mainstream is only used by camproto-store for recording.

Browser support for H.265:
- Chrome: NO (without flag)
- Firefox: NO
- Safari: YES
- Edge: sometimes (depends on hardware)

For browser streaming, always use H.264.

---

## Tested Cameras

| Camera | Codec | Resolution | FPS | Notes |
|---|---|---|---|---|
| Sparsh (channel 1-8) | H.265 | 1920×1080 to 3840×2160 | 25 | Mainstream |
| Sparsh (channel 1-8) | H.264 | 1280×720 to 1920×1080 | 25 | Substream |
| Fish-eye camera | H.265 | 2256×1696 | 25 | Circular image |

---

## Related Repos

| Repo | Description | Status |
|---|---|---|
| camproto-spec | Protocol spec + .proto files | ✅ |
| camproto-ingest | RTSP ingest + RTP depacketizer | ✅ |
| **camproto-nvr** | Desktop NVR client | ✅ Phase 1-3 |
| camproto-store | fMP4 recording + PostgreSQL | 🔲 |
| camproto-egress | MSE + WebRTC browser streaming | 🔲 |
| camproto-transport | QUIC + NAT traversal | 🔲 |
| camproto-control | Go HTTP API + dashboard | 🔲 |