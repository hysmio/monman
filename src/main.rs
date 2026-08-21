#![cfg_attr(
    all(target_os = "windows", not(debug_assertions)),
    windows_subsystem = "windows"
)]

mod app;
mod controllers;
mod display;
mod hotkeys;
mod model;
mod single_instance;
mod storage;
mod tray;
mod updater;

use app::MonManApp;
use std::ffi::OsStr;

const STARTUP_ARGUMENT: &str = "--startup";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let startup_launch = is_startup_launch(std::env::args_os().skip(1));
    let Some(_instance_guard) = single_instance::acquire()? else {
        if !startup_launch {
            single_instance::show_existing_window();
        }
        return Ok(());
    };

    let app_icon = eframe::icon_data::from_png_bytes(include_bytes!("../assets/egui-icon.png"))
        .expect("bundled egui icon must be a valid PNG");
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title("MonMan")
            .with_icon(app_icon)
            .with_inner_size([1360.0, 850.0])
            .with_min_inner_size([680.0, 560.0])
            .with_visible(!startup_launch),
        ..Default::default()
    };

    eframe::run_native(
        "MonMan",
        options,
        Box::new(move |cc| Ok(Box::new(MonManApp::new(cc, startup_launch)))),
    )?;
    Ok(())
}

fn is_startup_launch<I, S>(arguments: I) -> bool
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    arguments
        .into_iter()
        .any(|argument| argument.as_ref() == OsStr::new(STARTUP_ARGUMENT))
}

#[cfg(test)]
mod tests {
    use super::is_startup_launch;

    #[test]
    fn detects_startup_argument() {
        assert!(is_startup_launch(["--startup"]));
        assert!(is_startup_launch(["--other", "--startup"]));
    }

    #[test]
    fn normal_launch_is_not_treated_as_startup() {
        assert!(!is_startup_launch(std::iter::empty::<&str>()));
        assert!(!is_startup_launch(["--other"]));
    }
}
