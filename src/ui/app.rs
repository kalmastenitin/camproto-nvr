use crate::ui::tile;

pub struct NvrApp {
    pub cameras: Vec<CameraState>,
    pub focused: Option<usize>,
}

impl Default for NvrApp {
    fn default() -> Self {
        Self {
            cameras: Vec::new(),
            focused: None,
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
        }
    }
}

impl NvrApp {
    pub fn new(_cc: &eframe::CreationContext<'_>) -> Self {
        let mut app = Self::default();

        for i in 1..=16 {
            app.cameras.push(CameraState::new(
                &format!("cam_{:03}", i),
                &format!("Camera {}", i),
            ));
        }
        app
    }
}

impl eframe::App for NvrApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        egui::CentralPanel::default().show_inside(ui, |ui| {
            let cols = 4usize;
            let spacing = 4.0;
            let available = ui.available_width();
            let tile_w = (available - spacing * (cols as f32 - 1.0)) / cols as f32;
            let tile_h = tile_w * 9.0 / 16.0;
            let tile_size = egui::vec2(tile_w, tile_h);

            egui::Grid::new("camera_grid")
                .spacing([spacing, spacing])
                .show(ui, |ui| {
                    for (i, cam) in self.cameras.iter().enumerate() {
                        if i > 0 && i % cols == 0 {
                            ui.end_row();
                        }
                        if tile::render_tile(ui, cam, tile_size) {
                            self.focused = Some(i);
                        }
                    }
                });
        });
    }
}
