use camproto_ingest::rtsp::{RtspClient, RtspConfig};
use camproto_nvr::decode::{new_latest_frame, spawn_decode_task};
use camproto_nvr::ui::app::NvrApp;
use eframe::egui;

struct CameraConfig {
    id: &'static str,
    name: &'static str,
    url: &'static str,
}

const CAMERAS: &[CameraConfig] = &[
    CameraConfig {
        id: "cam_001",
        name: "Front Gate",
        url: "rtsp://admin:admin@192.168.1.240:554/rtsp/streaming?channel=1&subtype=0",
    },
    CameraConfig {
        id: "cam_002",
        name: "Parking Lot",
        url: "rtsp://admin:admin@192.168.1.240:554/rtsp/streaming?channel=2&subtype=0",
    },
    CameraConfig {
        id: "cam_003",
        name: "Side Door",
        url: "rtsp://admin:admin@192.168.1.240:554/rtsp/streaming?channel=3&subtype=0",
    },
    CameraConfig {
        id: "cam_004",
        name: "Roof Cam",
        url: "rtsp://admin:admin@192.168.1.240:554/rtsp/streaming?channel=14&subtype=0",
    },
    CameraConfig {
        id: "cam_005",
        name: "Roof Cam2",
        url: "rtsp://admin:admin@192.168.1.240:554/rtsp/streaming?channel=15&subtype=0",
    },
    CameraConfig {
        id: "cam_006",
        name: "Roof Cam3",
        url: "rtsp://admin:admin@192.168.1.240:554/rtsp/streaming?channel=16&subtype=0",
    },
    CameraConfig {
        id: "cam_007",
        name: "Roof Cam4",
        url: "rtsp://admin:admin@192.168.1.240:554/rtsp/streaming?channel=19&subtype=0",
    },
    CameraConfig {
        id: "cam_008",
        name: "Roof Cam5",
        url: "rtsp://admin:admin@192.168.1.240:554/rtsp/streaming?channel=20&subtype=0",
    },
    CameraConfig {
        id: "cam_009",
        name: "Side Door2",
        url: "rtsp://admin:admin@192.168.1.240:554/rtsp/streaming?channel=5&subtype=0",
    },
    CameraConfig {
        id: "cam_0010",
        name: "Side Door3",
        url: "rtsp://admin:admin@192.168.1.240:554/rtsp/streaming?channel=6&subtype=0",
    },
    CameraConfig {
        id: "cam_0011",
        name: "Side Door",
        url: "rtsp://admin:admin@192.168.1.240:554/rtsp/streaming?channel=8&subtype=0",
    },
    CameraConfig {
        id: "cam_0012",
        name: "Side Door",
        url: "rtsp://admin:admin@192.168.1.240:554/rtsp/streaming?channel=13&subtype=0",
    },
    CameraConfig {
        id: "cam_0013",
        name: "Side Door",
        url: "rtsp://admin:admin@192.168.1.240:554/rtsp/streaming?channel=22&subtype=0",
    },
    CameraConfig {
        id: "cam_0014",
        name: "Side Door",
        url: "rtsp://admin:admin@192.168.1.240:554/rtsp/streaming?channel=24&subtype=0",
    },
    CameraConfig {
        id: "cam_0015",
        name: "Side Door",
        url: "rtsp://admin:admin@192.168.1.240:554/rtsp/streaming?channel=25&subtype=0",
    },
    CameraConfig {
        id: "cam_0016",
        name: "Side Door",
        url: "rtsp://admin:admin@192.168.1.240:554/rtsp/streaming?channel=26&subtype=0",
    },
    CameraConfig {
        id: "cam_0017",
        name: "Side Door",
        url: "rtsp://admin:admin@192.168.1.240:554/rtsp/streaming?channel=27&subtype=0",
    },
];

fn main() {
    // FFmpeg global init — safe to call multiple times, idempotent
    #[cfg(target_os = "windows")]
    unsafe {
        ffmpeg_sys_next::avformat_network_init();
    }

    let rt = tokio::runtime::Runtime::new().unwrap();
    let mut cam_latests = Vec::new();

    for cfg in CAMERAS {
        let latest = new_latest_frame();
        cam_latests.push((cfg.id, cfg.name, latest.clone()));

        rt.spawn({
            let latest = latest.clone();
            async move {
                let config = RtspConfig {
                    url: cfg.url.into(),
                    camera_id: cfg.id.into(),
                };
                let (client, rx) = RtspClient::new(config);
                spawn_decode_task(cfg.id.into(), rx, latest);
                if let Err(e) = client.run().await {
                    eprintln!("ingest error [{}]: {}", cfg.id, e);
                }
            }
        });
    }

    let _ = eframe::run_native(
        "CamProto NVR",
        eframe::NativeOptions {
            viewport: egui::ViewportBuilder::default().with_inner_size([1280.0, 720.0]),
            ..Default::default()
        },
        Box::new(move |cc| Ok(Box::new(NvrApp::new_with_cameras(cc, cam_latests)))),
    );

    drop(rt);

    #[cfg(target_os = "windows")]
    unsafe {
        ffmpeg_sys_next::avformat_network_deinit();
    }
}
