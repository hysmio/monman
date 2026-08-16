#![cfg_attr(
    all(target_os = "windows", not(debug_assertions)),
    windows_subsystem = "windows"
)]

mod app;
mod controllers;
mod display;
mod hotkeys;
mod model;
mod storage;
mod tray;

use app::MonManApp;

fn main() -> eframe::Result {
    let app_icon = eframe::icon_data::from_png_bytes(include_bytes!("../assets/egui-icon.png"))
        .expect("bundled egui icon must be a valid PNG");
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title("MonMan")
            .with_icon(app_icon)
            .with_inner_size([1040.0, 700.0])
            .with_min_inner_size([820.0, 560.0]),
        ..Default::default()
    };

    eframe::run_native(
        "MonMan",
        options,
        Box::new(|cc| Ok(Box::new(MonManApp::new(cc)))),
    )
}
