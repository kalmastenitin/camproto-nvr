use eframe::egui;

pub struct TopBarState {
    pub grid_cols: Option<usize>,
    pub page_changed: Option<usize>,
}

pub struct TopBarResponse {
    pub grid_cols_changed: Option<usize>,
}

pub fn render_topbar(
    ui: &mut egui::Ui,
    camera_count: usize,
    cols: usize,
    current_page: usize,
) -> TopBarState {
    let mut state = TopBarState {
        grid_cols: None,
        page_changed: None,
    };

    let cameras_per_page = cols * cols;
    let total_pages = (camera_count + cameras_per_page - 1) / cameras_per_page;
    let total_pages = total_pages.max(1);

    ui.horizontal(|ui| {
        ui.label(egui::RichText::new("CamProto NVR").strong().size(15.0));
        ui.separator();
        ui.label(
            egui::RichText::new(format!("{} cameras", camera_count))
                .color(egui::Color32::from_rgb(130, 135, 145)),
        );
        ui.separator();

        ui.label("Grid:");
        for (label, c) in [("1×1", 1usize), ("2×2", 2), ("4×4", 4), ("8×8", 8)] {
            if ui.selectable_label(cols == c, label).clicked() {
                state.grid_cols = Some(c);
            }
        }

        ui.separator();

        // pagination
        ui.add_enabled_ui(current_page > 0, |ui| {
            if ui.button("◀").clicked() {
                state.page_changed = Some(current_page - 1);
            }
        });

        ui.label(format!("{} / {}", current_page + 1, total_pages));

        ui.add_enabled_ui(current_page + 1 < total_pages, |ui| {
            if ui.button("▶").clicked() {
                state.page_changed = Some(current_page + 1);
            }
        });
    });

    state
}
