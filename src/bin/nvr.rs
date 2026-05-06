use camproto_nvr::ui::app::NvrApp;

fn main() {
    let native_options = eframe::NativeOptions::default();
    let _ = eframe::run_native(
        "Camproto NVR",
        native_options,
        Box::new(|cc| Ok(Box::new(NvrApp::new(cc)))),
    );
}
