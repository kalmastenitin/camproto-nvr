use crate::ui::app::{CameraState, ConnectionStatus, RecordingStatus};
use eframe::egui;

// ── Colors ────────────────────────────────────────────────────────────────────

const COL_BG: egui::Color32 = egui::Color32::from_rgb(12, 14, 18);
const COL_BG_HOVER: egui::Color32 = egui::Color32::from_rgb(20, 24, 32);
const COL_BORDER: egui::Color32 = egui::Color32::from_rgb(40, 44, 54);
const COL_BORDER_FOCUS: egui::Color32 = egui::Color32::from_rgb(80, 140, 255);
const COL_BAR: egui::Color32 = egui::Color32::from_rgba_premultiplied(0, 0, 0, 180);
const COL_TEXT: egui::Color32 = egui::Color32::from_rgb(220, 220, 225);
const COL_TEXT_DIM: egui::Color32 = egui::Color32::from_rgb(130, 135, 145);
const COL_GREEN: egui::Color32 = egui::Color32::from_rgb(52, 211, 99);
const COL_YELLOW: egui::Color32 = egui::Color32::from_rgb(251, 191, 36);
const COL_RED: egui::Color32 = egui::Color32::from_rgb(239, 68, 68);
const COL_BLUE: egui::Color32 = egui::Color32::from_rgb(96, 165, 250);

// ── Public entry point ────────────────────────────────────────────────────────

/// Render one camera tile.
/// Returns true if the tile was clicked (caller should focus this camera).
pub fn render_tile(ui: &mut egui::Ui, cam: &mut CameraState, tile_size: egui::Vec2) -> bool {
    let (rect, response) = ui.allocate_exact_size(tile_size, egui::Sense::click());

    if !ui.is_rect_visible(rect) {
        return response.clicked();
    }

    let painter = ui.painter_at(rect);
    let hovered = response.hovered();

    // ── Background ────────────────────────────────────────────────────────────
    let bg = if hovered { COL_BG_HOVER } else { COL_BG };
    painter.rect_filled(rect, 4.0, bg);

    // ── Border ───────────────────────────────────────────────────────────────
    let border_col = if hovered {
        COL_BORDER_FOCUS
    } else {
        COL_BORDER
    };
    painter.rect_stroke(
        rect,
        4.0,
        egui::Stroke::new(1.0, border_col),
        egui::StrokeKind::Outside,
    );

    // ── Top bar (semi-transparent) ────────────────────────────────────────────
    let bar_h = 22.0;
    let top_bar = egui::Rect::from_min_size(
        rect.min,
        egui::Vec2 {
            x: rect.width(),
            y: bar_h,
        },
    );
    painter.rect_filled(
        top_bar,
        egui::CornerRadius {
            nw: 4,
            ne: 4,
            sw: 0,
            se: 0,
        },
        COL_BAR,
    );

    // ── Status dot ───────────────────────────────────────────────────────────
    let dot_radius = 4.0;
    let dot_center = egui::pos2(rect.min.x + 10.0, rect.min.y + bar_h / 2.0);
    let dot_color = connection_color(&cam.connection);
    painter.circle_filled(dot_center, dot_radius, dot_color);

    // ── Camera name ───────────────────────────────────────────────────────────
    let name_pos = egui::pos2(rect.min.x + 20.0, rect.min.y + 4.0);
    let display_name = if cam.name.is_empty() {
        &cam.camera_id
    } else {
        &cam.name
    };
    painter.text(
        name_pos,
        egui::Align2::LEFT_TOP,
        display_name,
        egui::FontId::proportional(11.0),
        COL_TEXT,
    );

    // ── Recording badge (top-right) ───────────────────────────────────────────
    if let Some((label, color)) = recording_badge(&cam.recording) {
        painter.text(
            egui::pos2(rect.max.x - 6.0, rect.min.y + 4.0),
            egui::Align2::RIGHT_TOP,
            format!("● {}", label),
            egui::FontId::proportional(10.0),
            color,
        );
    }

    // ── No-signal placeholder (when no texture yet) ───────────────────────────
    let inner = egui::Rect::from_min_max(
        egui::pos2(rect.min.x, rect.min.y + bar_h),
        egui::pos2(rect.max.x, rect.max.y - 20.0),
    );

    match &cam.connection {
        ConnectionStatus::Connecting => {
            painter.text(
                inner.center(),
                egui::Align2::CENTER_CENTER,
                "Connecting…",
                egui::FontId::proportional(11.0),
                COL_TEXT_DIM,
            );
        }
        ConnectionStatus::Disconnected(reason) => {
            painter.text(
                inner.center() - egui::vec2(0.0, 8.0),
                egui::Align2::CENTER_CENTER,
                "No Signal",
                egui::FontId::proportional(12.0),
                COL_RED,
            );
            // truncate long reasons
            let short_reason = if reason.len() > 30 {
                format!("{}…", &reason[..30])
            } else {
                reason.clone()
            };
            painter.text(
                inner.center() + egui::vec2(0.0, 10.0),
                egui::Align2::CENTER_CENTER,
                short_reason,
                egui::FontId::proportional(9.0),
                COL_TEXT_DIM,
            );
        }
        ConnectionStatus::Streaming => {
            // video texture will go here later
            if let Some(ref texture) = cam.texture {
                // render video texture
                painter.image(
                    texture.id(),
                    inner,
                    egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                    egui::Color32::WHITE,
                );
            } else {
                painter.text(
                    inner.center(),
                    egui::Align2::CENTER_CENTER,
                    &cam.camera_id,
                    egui::FontId::proportional(13.0),
                    egui::Color32::from_rgb(30, 35, 45),
                );
            }
        }
    }

    // ── Bottom info bar ───────────────────────────────────────────────────────
    let bot_bar = egui::Rect::from_min_size(
        egui::pos2(rect.min.x, rect.max.y - 20.0),
        egui::vec2(rect.width(), 20.0),
    );
    painter.rect_filled(
        bot_bar,
        egui::CornerRadius {
            nw: 0,
            ne: 0,
            sw: 4,
            se: 4,
        },
        COL_BAR,
    );

    let info = build_info_string(cam);
    painter.text(
        egui::pos2(rect.min.x + 6.0, rect.max.y - 18.0),
        egui::Align2::LEFT_TOP,
        info,
        egui::FontId::proportional(9.5),
        COL_TEXT_DIM,
    );

    // ── FPS indicator (top-right corner of inner area) ────────────────────────
    if matches!(cam.connection, ConnectionStatus::Streaming) && cam.displayed_fps > 0.0 {
        painter.text(
            egui::pos2(rect.max.x - 5.0, rect.min.y + bar_h + 3.0),
            egui::Align2::RIGHT_TOP,
            format!("{:.0}fps", cam.displayed_fps),
            egui::FontId::proportional(9.0),
            COL_BLUE,
        );
    }

    response.clicked()
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn connection_color(status: &ConnectionStatus) -> egui::Color32 {
    match status {
        ConnectionStatus::Connecting => COL_YELLOW,
        ConnectionStatus::Streaming => COL_GREEN,
        ConnectionStatus::Disconnected(_) => COL_RED,
    }
}

fn recording_badge(status: &RecordingStatus) -> Option<(&'static str, egui::Color32)> {
    match status {
        RecordingStatus::Idle => None,
        RecordingStatus::Recording { .. } => Some(("REC", COL_RED)),
        RecordingStatus::EventRecording { .. } => Some(("EVT", COL_RED)),
        RecordingStatus::Scheduled { .. } => Some(("SCH", COL_YELLOW)),
    }
}

fn build_info_string(cam: &CameraState) -> String {
    let mut parts: Vec<String> = Vec::new();

    if !cam.codec.is_empty() {
        parts.push(cam.codec.clone());
    }
    if cam.resolution != (0, 0) {
        parts.push(format!("{}×{}", cam.resolution.0, cam.resolution.1));
    }
    if cam.framerate > 0.0 {
        parts.push(format!("{}fps", cam.framerate as u32));
    }
    if cam.bitrate_kbps > 0 {
        if cam.bitrate_kbps >= 1000 {
            parts.push(format!("{:.1}Mbps", cam.bitrate_kbps as f32 / 1000.0));
        } else {
            parts.push(format!("{}Kbps", cam.bitrate_kbps));
        }
    }
    if cam.has_audio {
        parts.push("🔊".to_string());
    }

    if parts.is_empty() {
        cam.camera_id.clone()
    } else {
        parts.join("  ")
    }
}
