use camproto_ingest::rtsp::{RtspClient, RtspConfig};
use camproto_nvr::decode::{new_latest_frame, spawn_decode_task};
use camproto_nvr::ui::app::NvrApp;
use eframe::egui;

fn main() {
    // build tokio runtime for ingest
    let rt = tokio::runtime::Runtime::new().unwrap();

    // create app with one real camera
    let latest = new_latest_frame();

    // spawn ingest + decode
    rt.spawn({
        let latest = latest.clone();
        async move {
            let config = RtspConfig {
                url: "rtsp://admin:admin@192.168.1.240:554/rtsp/streaming?channel=2&subtype=0"
                    .into(),
                camera_id: "cam_001".into(),
            };

            let (client, rx) = RtspClient::new(config);

            // spawn decode task — reads from broadcast, feeds VideoToolbox
            spawn_decode_task("cam_001".into(), rx, latest);

            // run ingest loop
            if let Err(e) = client.run().await {
                eprintln!("ingest error: {}", e);
            }
        }
    });

    // run egui on main thread
    let _ = eframe::run_native(
        "CamProto NVR",
        eframe::NativeOptions {
            viewport: egui::ViewportBuilder::default().with_inner_size([1280.0, 720.0]),
            ..Default::default()
        },
        Box::new(move |cc| Ok(Box::new(NvrApp::new_with_camera(cc, "cam_001", latest)))),
    );
}
