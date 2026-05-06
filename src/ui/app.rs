use crate::decode::LatestFrame;
use crate::ui::tile;
use crate::ui::topbar;

pub struct NvrApp {
    pub cameras: Vec<CameraState>,
    pub focused: Option<usize>,
    pub grid_cols: usize,
    pub current_page: usize,
}

impl Default for NvrApp {
    fn default() -> Self {
        Self {
            cameras: Vec::new(),
            focused: None,
            grid_cols: 4,
            current_page: 0,
        }
    }
}

pub struct CameraState {
    pub camera_id: String,
    pub name: String,

    pub connection: ConnectionStatus,
    pub fps: f32,               // measured at runtime
    pub bitrate_kbps: u32,      // measured at runtime
    pub resolution: (u32, u32), // from SDP

    // recording status
    pub recording: RecordingStatus,
    pub record_secs: u64, // seconds recording so far
    pub storage_mb: f32,  // MB written this session

    // stream info from probe()
    pub codec: String,
    pub framerate: f32,
    pub has_audio: bool,

    // future — reserve now, use later
    pub ptz_capable: bool,
    pub zoom_capable: bool,

    pub latest: Option<LatestFrame>,
    pub texture: Option<egui::TextureHandle>,
}

pub struct RgbFrame {
    pub data:   Vec<u8>,
    pub width:  usize,
    pub height: usize,
}

pub enum ConnectionStatus {
    Connecting,
    Streaming,
    Disconnected(String), // Disconnected(reason)
}

pub enum RecordingStatus {
    Idle,
    Recording {
        started_secs: u64,
    }, // when did it start
    EventRecording {
        reason: String, // "Motion" / "Alarm" / "Manual"
        started_secs: u64,
        post_secs: u64, // record N more seconds after event
    },
    Scheduled {
        ends_secs: u64,
    }, // scheduled end time
}

impl CameraState {
    pub fn new(camera_id: &str, name: &str) -> Self {
        Self {
            camera_id: camera_id.to_string(),
            name: name.to_string(),
            connection: ConnectionStatus::Connecting,
            fps: 0.0,
            bitrate_kbps: 0,
            resolution: (0, 0),
            recording: RecordingStatus::Idle,
            record_secs: 0,
            storage_mb: 0.0,
            codec: String::new(),
            framerate: 0.0,
            has_audio: false,
            ptz_capable: false,
            zoom_capable: false,
            latest: None,
            texture: None,
        }
    }
}

impl NvrApp {
    pub fn new(_cc: &eframe::CreationContext<'_>) -> Self {
        let mut app = Self::default();

        let mut cam1 = CameraState::new("cam_001", "Front Gate");
        cam1.connection = ConnectionStatus::Streaming;
        cam1.codec = "H265".into();
        cam1.resolution = (1920, 1080);
        cam1.framerate = 25.0;
        cam1.fps = 24.8;
        cam1.bitrate_kbps = 2048;
        cam1.has_audio = true;
        app.cameras.push(cam1);

        // recording camera
        let mut cam2 = CameraState::new("cam_002", "Parking Lot");
        cam2.connection = ConnectionStatus::Streaming;
        cam2.recording = RecordingStatus::Recording { started_secs: 120 };
        cam2.codec = "H264".into();
        cam2.resolution = (1280, 720);
        cam2.framerate = 25.0;
        cam2.fps = 25.0;
        app.cameras.push(cam2);

        // event recording
        let mut cam3 = CameraState::new("cam_003", "Side Door");
        cam3.connection = ConnectionStatus::Streaming;
        cam3.recording = RecordingStatus::EventRecording {
            reason: "Motion".into(),
            started_secs: 30,
            post_secs: 60,
        };
        app.cameras.push(cam3);

        // disconnected
        let mut cam4 = CameraState::new("cam_004", "Roof Cam");
        cam4.connection = ConnectionStatus::Disconnected("connection timed out".into());
        app.cameras.push(cam4);

        for i in 5..=16 {
            app.cameras.push(CameraState::new(
                &format!("cam_{:03}", i),
                &format!("Camera {}", i),
            ));
        }
        app
    }

    pub fn new_with_cameras(
        _cc: &eframe::CreationContext<'_>,
        cameras: Vec<(&str, &str, LatestFrame)>,
    ) -> Self {
        let mut app = Self::default();
        for (id, name, latest) in cameras {
            let mut cam = CameraState::new(id, name);
            cam.latest = Some(latest);
            app.cameras.push(cam);
        }
        app
    }
}

impl eframe::App for NvrApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // 1. top panel FIRST
        egui::Panel::top("topbar").show_inside(ui, |ui| {
            let resp =
                topbar::render_topbar(ui, self.cameras.len(), self.grid_cols, self.current_page);
            if let Some(cols) = resp.grid_cols {
                self.grid_cols = cols;
                self.current_page = 0;
            }
            if let Some(page) = resp.page_changed {
                self.current_page = page;
            }
        });

        egui::CentralPanel::default().show_inside(ui, |ui| {
            let cols = self.grid_cols;
            let spacing = 4.0;
            let cameras_per_page = cols * cols;
            let start = self.current_page * cameras_per_page;
            let end = (start + cameras_per_page).min(self.cameras.len());

            let available = ui.available_width();
            let tile_w = (available - spacing * (cols as f32 - 1.0)) / cols as f32;
            let tile_h = tile_w * 9.0 / 16.0;
            let tile_size = egui::vec2(tile_w, tile_h);
            let ctx = ui.ctx().clone();

            egui::Grid::new("camera_grid")
                .spacing([spacing, spacing])
                .show(ui, |ui| {
                    for (i, cam) in self.cameras[start..end].iter_mut().enumerate() {
                        if i > 0 && i % cols == 0 {
                            ui.end_row();
                        }

                        // ── poll for new decoded frame ────────────────────────────
                        if let Some(ref latest) = cam.latest {
                            if let Ok(mut guard) = latest.try_lock() {
                                if let Some(frame) = guard.take() {
                                    let (rgb, dw, dh) = yuv_to_rgb(&frame);
                                    let texture = ctx.load_texture(
                                        &cam.camera_id,
                                        egui::ColorImage::from_rgb([dw, dh], &rgb),
                                        egui::TextureOptions::LINEAR,
                                    );
                                    cam.texture = Some(texture);
                                    cam.connection = ConnectionStatus::Streaming;
                                    cam.resolution = (frame.width, frame.height);
                                }
                            }
                        }
                        if tile::render_tile(ui, cam, tile_size) {
                            self.focused = Some(start + i);
                        }
                    }
                });
        });
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_millis(33));
    }
}

fn yuv_to_rgb(frame: &crate::decode::YuvFrame) -> (Vec<u8>, usize, usize) {
    let w = frame.width as usize;
    let h = frame.height as usize;
    let mut rgb = Vec::with_capacity(w * h * 3);

    for row in 0..h {
        for col in 0..w {
            let y = frame.y_plane[row * w + col] as f32;
            let u = frame.u_plane[(row / 2) * (w / 2) + col / 2] as f32 - 128.0;
            let v = frame.v_plane[(row / 2) * (w / 2) + col / 2] as f32 - 128.0;

            rgb.push((y + 1.5748 * v).clamp(0.0, 255.0) as u8);
            rgb.push((y - 0.1873 * u - 0.4681 * v).clamp(0.0, 255.0) as u8);
            rgb.push((y + 1.8556 * u).clamp(0.0, 255.0) as u8);
        }
    }
    (rgb, w, h)
}
