use crate::audio::{self, AudioDeviceInventory};
use crate::controllers::{
    ControllerDeviceInfo, ControllerEvent, ControllerManager, ControllerSpec,
};
use crate::display;
use crate::hotkeys::{HotkeyEvent, HotkeyManager, HotkeySpec};
use crate::model::{
    AppConfig, AudioDeviceConfig, HotkeyBinding, HotkeyKey, MonitorConfig, MonitorLayout,
};
use crate::storage;
use crate::tray::{TrayEvent, TrayManager};
use crate::updater::{AvailableUpdate, UpdateEvent, UpdateManager};
use eframe::egui;
use std::time::{Duration, Instant};

const SIDEBAR_BREAKPOINT: f32 = 900.0;
const SHORTCUT_CARD_BREAKPOINT: f32 = 820.0;
const DISPLAY_TABLE_BREAKPOINT: f32 = 760.0;

pub struct MonManApp {
    config: AppConfig,
    selected: Option<usize>,
    hotkeys: HotkeyManager,
    controllers: ControllerManager,
    audio_devices: AudioDeviceInventory,
    controller_devices: Vec<ControllerDeviceInfo>,
    controller_capture_layout: Option<usize>,
    controller_capture_status: String,
    tray: Option<TrayManager>,
    updater: UpdateManager,
    available_update: Option<AvailableUpdate>,
    update_in_progress: bool,
    exit_requested: bool,
    initial_window_visibility: Option<bool>,
    status: AppStatus,
    dirty: bool,
    last_persist: Instant,
    /// In-memory rollback point for the most recent apply.
    undo_layout: Option<MonitorLayout>,
}

struct AppStatus {
    message: String,
    is_error: bool,
}

impl AppStatus {
    fn info(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            is_error: false,
        }
    }

    fn error(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            is_error: true,
        }
    }
}

impl MonManApp {
    pub fn new(cc: &eframe::CreationContext<'_>, startup_launch: bool) -> Self {
        configure_ui_style(&cc.egui_ctx);

        let (mut config, mut status) = match storage::load() {
            Ok(config) => (config, AppStatus::info("Ready")),
            Err(err) => (
                AppConfig::default(),
                AppStatus::error(format!("Could not load layouts: {err:#}")),
            ),
        };
        let mut dirty = false;

        if !status.is_error {
            match display::startup_topology_needs_recovery() {
                Ok(true) => {
                    let saved_recovery = config.last_known_working.clone().map(|fallback| {
                        display::ensure_layout_available(&fallback)
                            .and_then(|()| display::apply_layout(&fallback))
                            .map(|()| fallback)
                    });

                    match saved_recovery {
                        Some(Ok(fallback)) => {
                            config.last_known_working = Some(working_snapshot(&fallback));
                            dirty = true;
                            status = AppStatus::info(
                                "Recovered the last known working monitor topology because Windows had no available active display",
                            );
                        }
                        saved_result => match display::restore_connected_topology() {
                            Ok(()) => {
                                match display::capture_layout("Last known working topology") {
                                    Ok(snapshot)
                                        if snapshot
                                            .monitors
                                            .iter()
                                            .any(|monitor| monitor.enabled) =>
                                    {
                                        config.last_known_working =
                                            Some(sanitized_working_snapshot(snapshot));
                                        dirty = true;
                                        status = AppStatus::info(
                                            "Recovered a working topology for the monitors connected now",
                                        );
                                    }
                                    Ok(_) => {
                                        status = AppStatus::error(
                                            "Windows restored the connected-monitor topology, but MonMan could not find an active monitor to record",
                                        );
                                    }
                                    Err(error) => {
                                        status = AppStatus::error(format!(
                                            "Windows restored the connected-monitor topology, but MonMan could not record it: {error:#}"
                                        ));
                                    }
                                }
                            }
                            Err(connected_error) => {
                                status = match saved_result {
                                    Some(Err(saved_error)) => AppStatus::error(format!(
                                        "Startup recovery failed: the saved topology is unavailable ({saved_error:#}), and Windows could not activate the monitors connected now ({connected_error:#})"
                                    )),
                                    None => AppStatus::error(format!(
                                        "Startup recovery failed because Windows could not activate the monitors connected now: {connected_error:#}"
                                    )),
                                    Some(Ok(_)) => {
                                        unreachable!("a successful saved recovery is handled above")
                                    }
                                };
                            }
                        },
                    }
                }
                Ok(false) => match display::capture_layout("Last known working topology") {
                    Ok(snapshot) if snapshot.monitors.iter().any(|monitor| monitor.enabled) => {
                        config.last_known_working = Some(sanitized_working_snapshot(snapshot));
                        dirty = true;
                    }
                    Ok(_) => {}
                    Err(error) => {
                        status = AppStatus::error(format!(
                            "Could not record the current working topology: {error:#}"
                        ));
                    }
                },
                Err(error) => {
                    status = AppStatus::error(format!(
                        "Could not inspect the startup monitor topology: {error:#}"
                    ));
                }
            }
        }

        let selected = (!config.layouts.is_empty()).then_some(0);
        let hotkeys = HotkeyManager::new(cc.egui_ctx.clone());
        let controllers = ControllerManager::new(cc.egui_ctx.clone());
        let tray = match TrayManager::new(cc.egui_ctx.clone()) {
            Ok(tray) => {
                if !status.is_error {
                    status = AppStatus::info(
                        "Ready — closing the window keeps MonMan running in the tray",
                    );
                }
                Some(tray)
            }
            Err(error) => {
                status = AppStatus::error(format!(
                    "System tray is unavailable; closing MonMan will exit: {error:#}"
                ));
                None
            }
        };
        let initial_window_visibility = startup_window_visibility(startup_launch, tray.is_some());
        let updater = UpdateManager::new(cc.egui_ctx.clone());
        let mut app = Self {
            config,
            selected,
            hotkeys,
            controllers,
            audio_devices: AudioDeviceInventory::default(),
            controller_devices: Vec::new(),
            controller_capture_layout: None,
            controller_capture_status: String::new(),
            tray,
            updater,
            available_update: None,
            update_in_progress: false,
            exit_requested: false,
            initial_window_visibility,
            status,
            dirty,
            last_persist: Instant::now(),
            undo_layout: None,
        };
        app.refresh_bindings();
        app.refresh_audio_devices(false);
        app
    }

    fn refresh_bindings(&mut self) {
        self.refresh_hotkeys();
        self.refresh_controller_hotkeys();
    }

    fn refresh_hotkeys(&mut self) {
        let specs = self
            .config
            .layouts
            .iter()
            .enumerate()
            .filter_map(|(layout_index, layout)| {
                layout.hotkey.map(|binding| HotkeySpec {
                    layout_index,
                    binding,
                })
            })
            .collect();

        if let Err(err) = self.hotkeys.replace(specs) {
            self.status = AppStatus::error(format!("Could not update global hotkeys: {err:#}"));
        }
    }

    fn refresh_controller_hotkeys(&mut self) {
        let specs = self
            .config
            .layouts
            .iter()
            .enumerate()
            .filter_map(|(layout_index, layout)| {
                layout
                    .controller_hotkey
                    .clone()
                    .filter(|binding| binding.is_valid())
                    .map(|binding| ControllerSpec {
                        layout_index,
                        binding,
                    })
            })
            .collect();

        if let Err(err) = self.controllers.replace(specs) {
            self.status = AppStatus::error(format!("Could not update controller hotkeys: {err:#}"));
        }
    }

    fn refresh_audio_devices(&mut self, announce: bool) {
        match audio::enumerate_devices() {
            Ok(devices) => {
                let playback_count = devices.playback.len();
                let microphone_count = devices.microphones.len();
                self.audio_devices = devices;
                if announce {
                    self.status = AppStatus::info(format!(
                        "Audio devices refreshed: {playback_count} playback, {microphone_count} microphone"
                    ));
                }
            }
            Err(error) => {
                self.status =
                    AppStatus::error(format!("Could not enumerate audio devices: {error:#}"));
            }
        }
    }

    fn persist(&mut self, ctx: &egui::Context) {
        if !self.dirty {
            return;
        }

        // Debounce saves while keeping a repaint scheduled for the final edit.
        const SAVE_INTERVAL: Duration = Duration::from_millis(500);
        let elapsed = self.last_persist.elapsed();
        if elapsed < SAVE_INTERVAL {
            ctx.request_repaint_after(SAVE_INTERVAL - elapsed);
            return;
        }

        self.last_persist = Instant::now();
        match storage::save(&self.config) {
            Ok(()) => {
                self.dirty = false;
            }
            Err(err) => {
                self.status = AppStatus::error(format!("Could not save layouts: {err:#}"));
            }
        }
    }

    fn save_now(&mut self) {
        match storage::save(&self.config) {
            Ok(()) => {
                self.dirty = false;
                self.status = AppStatus::info("Layouts saved");
            }
            Err(err) => {
                self.status = AppStatus::error(format!("Could not save layouts: {err:#}"));
            }
        }
    }

    fn capture_new(&mut self) {
        let name = format!("Layout {}", self.config.layouts.len() + 1);
        match capture_profile(name) {
            Ok(layout) => self.add_layout(
                layout,
                "Captured current Windows display topology and audio devices",
            ),
            Err(err) => {
                self.status = AppStatus::error(format!("Capture failed: {err:#}"));
            }
        }
    }

    fn create_custom(&mut self) {
        let name = format!("Layout {}", self.config.layouts.len() + 1);
        match display::capture_layout(name) {
            Ok(mut layout) => {
                for monitor in &mut layout.monitors {
                    monitor.enabled = false;
                    monitor.clone_group = None;
                }
                self.add_layout(
                    layout,
                    "Created a custom layout from currently connected monitors",
                );
            }
            Err(err) => {
                self.status =
                    AppStatus::error(format!("Could not enumerate connected monitors: {err:#}"));
            }
        }
    }

    fn add_layout(&mut self, layout: MonitorLayout, status: impl Into<String>) {
        self.config.layouts.push(layout);
        self.selected = Some(self.config.layouts.len() - 1);
        self.dirty = true;
        self.status = AppStatus::info(status);
    }

    fn recapture_selected(&mut self) {
        let Some(index) = self.selected else { return };
        if index >= self.config.layouts.len() {
            return;
        }

        let old_name = self.config.layouts[index].name.clone();
        let old_hotkey = self.config.layouts[index].hotkey;
        let old_controller_hotkey = self.config.layouts[index].controller_hotkey.clone();
        match capture_profile(old_name) {
            Ok(mut layout) => {
                layout.hotkey = old_hotkey;
                layout.controller_hotkey = old_controller_hotkey;
                self.config.layouts[index] = layout;
                self.dirty = true;
                self.status = AppStatus::info(
                    "Replaced this layout with the current display and audio devices",
                );
            }
            Err(err) => {
                self.status = AppStatus::error(format!("Capture failed: {err:#}"));
            }
        }
    }

    fn merge_connected_monitors(&mut self) {
        let Some(index) = self.selected else { return };
        if index >= self.config.layouts.len() {
            return;
        }

        match display::capture_layout("Connected monitors") {
            Ok(current) => {
                let layout = &mut self.config.layouts[index];
                let mut added = 0usize;
                let mut refreshed = 0usize;

                for mut current_monitor in current.monitors {
                    if let Some(existing) = layout
                        .monitors
                        .iter_mut()
                        .find(|saved| saved.identity.matches(&current_monitor.identity))
                    {
                        refresh_monitor(existing, current_monitor);
                        refreshed += 1;
                    } else {
                        current_monitor.enabled = false;
                        current_monitor.clone_group = None;
                        layout.monitors.push(current_monitor);
                        added += 1;
                    }
                }

                layout
                    .monitors
                    .sort_by_key(|m| (m.x, m.y, m.friendly_name.clone()));
                self.dirty |= added > 0 || refreshed > 0;
                self.status = AppStatus::info(format!(
                    "Monitor list refreshed: {added} added, {refreshed} matched"
                ));
            }
            Err(err) => {
                self.status = AppStatus::error(format!("Could not refresh monitors: {err:#}"));
            }
        }
    }

    fn duplicate_selected(&mut self) {
        let Some(index) = self.selected else { return };
        let Some(layout) = self.config.layouts.get(index).cloned() else {
            return;
        };

        let mut copy = layout;
        copy.name = format!("{} copy", copy.name);
        copy.hotkey = None;
        copy.controller_hotkey = None;
        self.config.layouts.insert(index + 1, copy);
        self.selected = Some(index + 1);
        self.dirty = true;
        self.status = AppStatus::info("Duplicated layout (hotkeys cleared on the copy)");
        self.refresh_bindings();
    }

    fn delete_layout(&mut self, index: usize) {
        if self.controller_capture_layout.is_some() {
            self.cancel_controller_capture();
        }
        self.config.layouts.remove(index);
        self.selected =
            (!self.config.layouts.is_empty()).then(|| index.min(self.config.layouts.len() - 1));
        self.dirty = true;
        self.refresh_bindings();
        self.status = AppStatus::info("Deleted layout");
    }

    fn apply_index(&mut self, index: usize) {
        if index >= self.config.layouts.len() {
            return;
        }

        // Applying is staged, so capture a rollback point before changing display or audio.
        let previous = match capture_profile("Previous profile") {
            Ok(layout) => layout,
            Err(err) => {
                self.status = AppStatus::error(format!(
                    "Apply cancelled because the current display and audio settings could not be captured for rollback: {err:#}"
                ));
                return;
            }
        };

        let layout = self.config.layouts[index].clone();
        let name = layout.name.clone();
        match apply_profile(&layout) {
            Ok(()) => {
                self.refresh_audio_devices(false);
                self.undo_layout = Some(previous);
                self.remember_working_layout(&layout);
                self.status = AppStatus::info(format!("Applied '{name}'"));
            }
            Err(apply_err) => match apply_profile(&previous) {
                Ok(()) => {
                    self.refresh_audio_devices(false);
                    self.remember_working_layout(&previous);
                    self.status = AppStatus::error(format!(
                        "Could not apply '{name}': {apply_err:#}. The previous display and audio settings were restored."
                    ));
                }
                Err(rollback_err) => {
                    self.status = AppStatus::error(format!(
                        "Could not apply '{name}': {apply_err:#}. Automatic rollback also failed: {rollback_err:#}"
                    ));
                }
            },
        }
    }

    fn undo_last_apply(&mut self) {
        let Some(previous) = self.undo_layout.clone() else {
            return;
        };

        match apply_profile(&previous) {
            Ok(()) => {
                self.refresh_audio_devices(false);
                self.remember_working_layout(&previous);
                self.undo_layout = None;
                self.status = AppStatus::info(
                    "Restored the display and audio settings from before the last successful apply",
                );
            }
            Err(err) => {
                self.status =
                    AppStatus::error(format!("Could not restore the previous profile: {err:#}"));
            }
        }
    }

    fn handle_hotkeys(&mut self) {
        while let Some(event) = self.hotkeys.try_recv() {
            match event {
                HotkeyEvent::Triggered(index) => self.apply_index(index),
                HotkeyEvent::RegistrationFinished(failures) => {
                    if failures.is_empty() {
                        if self
                            .status
                            .message
                            .starts_with("Some global hotkeys could not be registered:")
                        {
                            self.status = AppStatus::info("Global hotkeys updated");
                        }
                        continue;
                    }

                    let details = failures
                        .iter()
                        .map(|failure| {
                            let layout_name = self
                                .config
                                .layouts
                                .get(failure.layout_index)
                                .map(|layout| layout.name.as_str())
                                .unwrap_or("unknown layout");
                            format!(
                                "{} [{}]: {}",
                                layout_name,
                                failure.binding.label(),
                                failure.reason
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("; ");
                    self.status = AppStatus::error(format!(
                        "Some global hotkeys could not be registered: {details}"
                    ));
                }
            }
        }
    }

    fn handle_controllers(&mut self) {
        while let Some(event) = self.controllers.try_recv() {
            match event {
                ControllerEvent::Triggered(index) => self.apply_index(index),
                ControllerEvent::Captured {
                    layout_index,
                    binding,
                } => {
                    self.clear_controller_capture();
                    if let Some(layout) = self.config.layouts.get_mut(layout_index) {
                        let label = binding.label();
                        layout.controller_hotkey = Some(binding);
                        self.dirty = true;
                        self.refresh_controller_hotkeys();
                        self.status = AppStatus::info(format!("Controller hotkey saved: {label}"));
                    }
                }
                ControllerEvent::CaptureProgress(message) => {
                    self.controller_capture_status = message;
                }
                ControllerEvent::CaptureCancelled(message) => {
                    self.clear_controller_capture();
                    self.status = AppStatus::info(message);
                }
                ControllerEvent::DevicesChanged(devices) => {
                    self.controller_devices = devices;
                }
                ControllerEvent::Error(error) => {
                    self.clear_controller_capture();
                    self.status = AppStatus::error(error);
                }
            }
        }
    }

    fn begin_controller_capture(&mut self, layout_index: usize) {
        match self.controllers.begin_capture(layout_index) {
            Ok(()) => {
                self.controller_capture_layout = Some(layout_index);
                self.controller_capture_status =
                    "Release all controller buttons, then press the desired chord".into();
                self.status = AppStatus::info("Listening for a controller hotkey");
            }
            Err(error) => {
                self.status =
                    AppStatus::error(format!("Could not start controller binding: {error:#}"));
            }
        }
    }

    fn cancel_controller_capture(&mut self) {
        if let Err(error) = self.controllers.cancel_capture() {
            self.status =
                AppStatus::error(format!("Could not cancel controller binding: {error:#}"));
        }
        self.clear_controller_capture();
    }

    fn clear_controller_capture(&mut self) {
        self.controller_capture_layout = None;
        self.controller_capture_status.clear();
    }

    fn remember_working_layout(&mut self, applied: &MonitorLayout) {
        self.config.last_known_working = Some(working_snapshot(applied));
        self.dirty = true;
    }

    fn handle_tray(&mut self, ctx: &egui::Context) {
        let mut tray_failed = false;
        while let Some(event) = self.tray.as_ref().and_then(TrayManager::try_recv) {
            match event {
                TrayEvent::OpenRequested => {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                    ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
                    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                }
                TrayEvent::ExitRequested => self.request_exit(ctx),
                TrayEvent::Error(error) => {
                    self.status = AppStatus::error(format!("System tray stopped working: {error}"));
                    tray_failed = true;
                }
            }
        }
        if tray_failed {
            self.tray = None;
        }
    }

    fn handle_updater(&mut self, ctx: &egui::Context) {
        while let Some(event) = self.updater.try_recv() {
            match event {
                UpdateEvent::Available(update) => {
                    if !self.status.is_error {
                        self.status =
                            AppStatus::info(format!("Update {} is available", update.tag));
                    }
                    self.available_update = Some(update);
                }
                UpdateEvent::InstallerLaunched => {
                    self.status = AppStatus::info(
                        "Update verified and installer launched; MonMan will restart",
                    );
                    self.request_exit(ctx);
                }
                UpdateEvent::InstallFailed(error) => {
                    self.update_in_progress = false;
                    self.status = AppStatus::error(format!("Update failed: {error}"));
                }
            }
        }
    }

    fn hide_to_tray(&mut self, ctx: &egui::Context) {
        if self.tray.is_some() {
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
            self.status = AppStatus::info(
                "MonMan is running in the system tray; keyboard and controller hotkeys remain active",
            );
        }
    }

    fn request_exit(&mut self, ctx: &egui::Context) {
        self.exit_requested = true;
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }

    fn sidebar(&mut self, root_ui: &mut egui::Ui) {
        egui::Panel::left("layout_list")
            .resizable(false)
            .default_size(210.0)
            .min_size(210.0)
            .max_size(210.0)
            .frame(
                egui::Frame::new()
                    .fill(sidebar_fill())
                    .inner_margin(12)
                    .stroke(egui::Stroke::new(1.0_f32, border_color())),
            )
            .show_inside(root_ui, |ui| {
                ui.heading("Layouts");
                ui.add_space(8.0);

                if ui
                    .add_sized(
                        [ui.available_width(), 36.0],
                        egui::Button::new("Capture current"),
                    )
                    .clicked()
                {
                    self.capture_new();
                }
                if ui
                    .add_sized(
                        [ui.available_width(), 36.0],
                        egui::Button::new("+  New layout"),
                    )
                    .clicked()
                {
                    self.create_custom();
                }

                ui.add_space(8.0);
                egui::ScrollArea::vertical()
                    .id_salt("layout_navigation")
                    .show(ui, |ui| {
                        for (index, layout) in self.config.layouts.iter().enumerate() {
                            let label = layout.name.clone();
                            if ui
                                .add_sized(
                                    [ui.available_width(), 40.0],
                                    egui::Button::selectable(self.selected == Some(index), label),
                                )
                                .clicked()
                            {
                                self.selected = Some(index);
                            }
                        }
                    });

                ui.with_layout(egui::Layout::bottom_up(egui::Align::LEFT), |ui| {
                    ui.menu_button("More…", |ui| {
                        ui.label(format!("MonMan v{}", env!("CARGO_PKG_VERSION")));
                        ui.label("Changes save automatically");
                    });
                    if sidebar_footer_button(ui, "Quit").clicked() {
                        self.request_exit(ui.ctx());
                    }
                    if ui
                        .add_enabled_ui(self.tray.is_some(), |ui| {
                            sidebar_footer_button(ui, "Hide to tray")
                        })
                        .inner
                        .on_hover_text(
                            "Hide the window while keeping keyboard and controller hotkeys active",
                        )
                        .clicked()
                    {
                        self.hide_to_tray(ui.ctx());
                    }
                    if sidebar_footer_button(ui, "Undo last apply")
                        .on_hover_text("Restore the display and audio settings captured immediately before the last successful apply")
                        .clicked()
                        && self.undo_layout.is_some()
                    {
                        self.undo_last_apply();
                    }
                    if sidebar_footer_button(ui, "Save").clicked() {
                        self.save_now();
                    }
                    ui.separator();
                    if let Some(update) = self.available_update.clone()
                        && ui
                            .add_enabled_ui(!self.update_in_progress, |ui| {
                                ui.add_sized(
                                    [ui.available_width(), 30.0],
                                    egui::Button::new(if self.update_in_progress {
                                        format!("Installing {}…", update.tag)
                                    } else {
                                        format!("Update to {}", update.tag)
                                    }),
                                )
                            })
                            .inner
                            .on_hover_text(
                                "Download the GitHub release asset, verify its SHA-256 digest, install it, and restart MonMan",
                            )
                            .clicked()
                    {
                        self.update_in_progress = true;
                        self.status =
                            AppStatus::info(format!("Downloading update {}…", update.tag));
                        self.updater.install(update);
                    }
                });
            });
    }

    fn compact_navigation(&mut self, root_ui: &mut egui::Ui) {
        let mut action = None;
        let mut capture_current = false;
        let mut new_layout = false;
        egui::Panel::top("compact_layout_navigation")
            .frame(
                egui::Frame::new()
                    .fill(header_fill())
                    .inner_margin(10)
                    .stroke(egui::Stroke::new(1.0_f32, border_color())),
            )
            .show_inside(root_ui, |ui| {
                ui.horizontal(|ui| {
                    let selected_name = self
                        .selected
                        .and_then(|index| self.config.layouts.get(index))
                        .map(|layout| layout.name.as_str())
                        .unwrap_or("Choose a layout");
                    egui::ComboBox::from_id_salt("compact_layout_selector")
                        .selected_text(selected_name)
                        .width((ui.available_width() - 190.0).clamp(150.0, 260.0))
                        .show_ui(ui, |ui| {
                            for (index, layout) in self.config.layouts.iter().enumerate() {
                                ui.selectable_value(&mut self.selected, Some(index), &layout.name);
                            }
                        });
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.menu_button("•••", |ui| {
                            ui.set_min_width(210.0);
                            if ui.button("Capture current").clicked() {
                                capture_current = true;
                                ui.close();
                            }
                            if ui.button("New layout").clicked() {
                                new_layout = true;
                                ui.close();
                            }
                            if self.selected.is_some() {
                                ui.separator();
                                layout_action_menu(ui, &mut action);
                            }
                        });
                        if ui
                            .add_enabled(
                                self.selected.is_some(),
                                egui::Button::new(egui::RichText::new("Apply layout").strong())
                                    .fill(accent_color())
                                    .min_size(egui::vec2(116.0, 34.0)),
                            )
                            .clicked()
                        {
                            action = Some(LayoutAction::Apply);
                        }
                    });
                });
            });

        if capture_current {
            self.capture_new();
        }
        if new_layout {
            self.create_custom();
        }
        if let Some(index) = self.selected {
            self.perform_layout_action(index, action);
        }
    }

    fn editor(&mut self, root_ui: &mut egui::Ui, show_layout_header: bool) {
        egui::CentralPanel::default().show_inside(root_ui, |ui| {
            egui::ScrollArea::vertical()
                .id_salt("layout_editor_scroll")
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    let Some(index) = self.selected else {
                        ui.vertical_centered(|ui| {
                            ui.add_space(100.0);
                            ui.heading("No layout selected");
                            ui.label(
                                "Capture the current Windows display topology to get started.",
                            );
                        });
                        return;
                    };

                    if index >= self.config.layouts.len() {
                        self.selected = None;
                        return;
                    }

                    let capture_active = self.controller_capture_layout == Some(index);
                    let capture_elsewhere =
                        self.controller_capture_layout.is_some() && !capture_active;
                    let capture_status = self.controller_capture_status.clone();
                    let connected_controllers = self
                        .controller_devices
                        .iter()
                        .map(ControllerDeviceInfo::label)
                        .collect::<Vec<_>>();
                    let audio_devices = self.audio_devices.clone();

                    let (
                        layout_action,
                        hotkeys_changed,
                        controller_edit,
                        layout_changed,
                        refresh_audio,
                    ) = {
                        let layout = &mut self.config.layouts[index];
                        let (name_changed, layout_action) = if show_layout_header {
                            layout_header(ui, layout, index)
                        } else {
                            (false, None)
                        };

                        if show_layout_header {
                            ui.add_space(10.0);
                        }
                        let arrangement_changed = monitor_arrangement_editor(ui, layout, index);
                        ui.add_space(10.0);
                        let (audio_changed, refresh_audio) =
                            audio_device_editor(ui, layout, index, &audio_devices);
                        ui.add_space(10.0);
                        let (hotkeys_changed, controller_edit) = shortcut_editors(
                            ui,
                            layout,
                            index,
                            capture_active,
                            capture_elsewhere,
                            &capture_status,
                            &connected_controllers,
                        );
                        ui.add_space(10.0);
                        let monitor_changed = monitor_list_editor(ui, layout, index);
                        let layout_changed = name_changed
                            || audio_changed
                            || hotkeys_changed
                            || arrangement_changed
                            || monitor_changed
                            || controller_edit == Some(ControllerEdit::Clear);

                        (
                            layout_action,
                            hotkeys_changed,
                            controller_edit,
                            layout_changed,
                            refresh_audio,
                        )
                    };

                    self.dirty |= layout_changed;
                    if refresh_audio {
                        self.refresh_audio_devices(true);
                    }
                    if hotkeys_changed {
                        self.refresh_hotkeys();
                    }
                    match controller_edit {
                        Some(ControllerEdit::Clear) => {
                            self.refresh_controller_hotkeys();
                            self.status = AppStatus::info("Controller hotkey cleared");
                        }
                        Some(ControllerEdit::CancelCapture) => self.cancel_controller_capture(),
                        Some(ControllerEdit::BeginCapture) => self.begin_controller_capture(index),
                        None => {}
                    }

                    self.perform_layout_action(index, layout_action);
                });
        });
    }

    fn perform_layout_action(&mut self, index: usize, action: Option<LayoutAction>) {
        match action {
            Some(LayoutAction::Delete) => self.delete_layout(index),
            Some(LayoutAction::Duplicate) => self.duplicate_selected(),
            Some(LayoutAction::Recapture) => self.recapture_selected(),
            Some(LayoutAction::SyncMonitors) => self.merge_connected_monitors(),
            Some(LayoutAction::Apply) => self.apply_index(index),
            None => {}
        }
    }

    fn status_bar(&mut self, root_ui: &mut egui::Ui) {
        let compact = root_ui.available_width() < SIDEBAR_BREAKPOINT;
        egui::Panel::bottom("status_bar")
            .frame(
                egui::Frame::new()
                    .fill(sidebar_fill())
                    .inner_margin(egui::Margin::symmetric(10, 6))
                    .stroke(egui::Stroke::new(1.0_f32, border_color())),
            )
            .show_inside(root_ui, |ui| {
                ui.columns(2, |columns| {
                    let color = if self.status.is_error {
                        columns[0].visuals().error_fg_color
                    } else {
                        columns[0].visuals().text_color()
                    };
                    columns[0].horizontal(|ui| {
                        let (dot_rect, _) =
                            ui.allocate_exact_size(egui::vec2(9.0, 9.0), egui::Sense::hover());
                        ui.painter().circle_filled(
                            dot_rect.center(),
                            4.5,
                            if self.status.is_error {
                                ui.visuals().error_fg_color
                            } else {
                                egui::Color32::from_rgb(74, 196, 100)
                            },
                        );
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(&self.status.message)
                                    .color(color)
                                    .small(),
                            )
                            .truncate(),
                        )
                        .on_hover_text(&self.status.message);
                    });

                    let config_path = storage::config_path();
                    columns[1].with_layout(
                        egui::Layout::right_to_left(egui::Align::Center),
                        |ui| {
                            ui.label("Config")
                                .on_hover_text(config_path.display().to_string());
                            ui.add(
                                egui::Label::new(
                                    egui::RichText::new(if compact {
                                        String::new()
                                    } else {
                                        config_path.display().to_string()
                                    })
                                    .small(),
                                )
                                .truncate(),
                            )
                            .on_hover_text(config_path.display().to_string());
                        },
                    );
                });
            });
    }
}

#[derive(Clone, Copy)]
enum LayoutAction {
    Apply,
    Recapture,
    SyncMonitors,
    Duplicate,
    Delete,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ControllerEdit {
    BeginCapture,
    CancelCapture,
    Clear,
}

fn section_frame<R>(
    ui: &mut egui::Ui,
    add_contents: impl FnOnce(&mut egui::Ui) -> R,
) -> egui::InnerResponse<R> {
    let available_width = ui.available_width();
    egui::Frame::new()
        .fill(card_fill())
        .stroke(egui::Stroke::new(1.0_f32, border_color()))
        .inner_margin(12)
        .corner_radius(8)
        .show(ui, |ui| {
            ui.set_min_width((available_width - 24.0).max(0.0));
            add_contents(ui)
        })
}

fn sidebar_footer_button(ui: &mut egui::Ui, label: &str) -> egui::Response {
    ui.add(egui::Button::new(label).frame(false))
}

fn layout_action_menu(ui: &mut egui::Ui, action: &mut Option<LayoutAction>) {
    for (label, candidate) in [
        ("Replace with current", LayoutAction::Recapture),
        ("Sync connected monitors", LayoutAction::SyncMonitors),
        ("Duplicate layout", LayoutAction::Duplicate),
    ] {
        if ui.button(label).clicked() {
            *action = Some(candidate);
            ui.close();
        }
    }
    ui.separator();
    if ui
        .button(egui::RichText::new("Delete layout").color(ui.visuals().error_fg_color))
        .clicked()
    {
        *action = Some(LayoutAction::Delete);
        ui.close();
    }
}

fn layout_header(
    ui: &mut egui::Ui,
    layout: &mut MonitorLayout,
    layout_index: usize,
) -> (bool, Option<LayoutAction>) {
    let mut action = None;
    let mut name_changed = false;
    let edit_id = ui.make_persistent_id(("editing_layout_name", layout_index));
    let mut editing_name = ui
        .data(|data| data.get_temp::<bool>(edit_id))
        .unwrap_or(false);
    egui::Frame::new()
        .fill(header_fill())
        .stroke(egui::Stroke::new(1.0_f32, border_color()))
        .inner_margin(10)
        .corner_radius(7)
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                if editing_name {
                    let response = ui.add(
                        egui::TextEdit::singleline(&mut layout.name)
                            .font(egui::TextStyle::Heading)
                            .desired_width((ui.available_width() - 190.0).clamp(180.0, 360.0))
                            .hint_text("Layout name"),
                    );
                    name_changed |= response.changed();
                    if response.lost_focus()
                        && ui.input(|input| input.key_pressed(egui::Key::Enter))
                    {
                        editing_name = false;
                    }
                } else {
                    ui.heading(&layout.name);
                    if pencil_button(ui)
                        .on_hover_text("Rename this layout")
                        .clicked()
                    {
                        editing_name = true;
                    }
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.menu_button("•••", |ui| {
                        ui.set_min_width(210.0);
                        layout_action_menu(ui, &mut action);
                    });
                    if ui
                        .add_sized(
                            [126.0, 34.0],
                            egui::Button::new(egui::RichText::new("Apply layout").strong())
                                .fill(accent_color()),
                        )
                        .clicked()
                    {
                        action = Some(LayoutAction::Apply);
                    }
                });
            });
        });
    ui.data_mut(|data| data.insert_temp(edit_id, editing_name));
    (name_changed, action)
}

fn audio_device_editor(
    ui: &mut egui::Ui,
    layout: &mut MonitorLayout,
    index: usize,
    devices: &AudioDeviceInventory,
) -> (bool, bool) {
    let mut changed = false;
    let mut refresh_requested = false;
    section_frame(ui, |ui| {
        ui.horizontal(|ui| {
            ui.heading("Audio devices");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                refresh_requested = ui.small_button("Refresh devices").clicked();
            });
        });
        ui.small(
            "Optionally set the Windows default playback and microphone endpoints for all audio roles when this profile is applied.",
        );
        ui.add_space(8.0);

        if ui.available_width() >= DISPLAY_TABLE_BREAKPOINT {
            ui.columns(2, |columns| {
                changed |= audio_device_picker(
                    &mut columns[0],
                    "Playback",
                    ("playback_device", index),
                    &mut layout.playback_device,
                    &devices.playback,
                    devices.default_playback_id.as_deref(),
                );
                changed |= audio_device_picker(
                    &mut columns[1],
                    "Microphone",
                    ("microphone_device", index),
                    &mut layout.microphone_device,
                    &devices.microphones,
                    devices.default_microphone_id.as_deref(),
                );
            });
        } else {
            changed |= audio_device_picker(
                ui,
                "Playback",
                ("playback_device", index),
                &mut layout.playback_device,
                &devices.playback,
                devices.default_playback_id.as_deref(),
            );
            ui.add_space(8.0);
            changed |= audio_device_picker(
                ui,
                "Microphone",
                ("microphone_device", index),
                &mut layout.microphone_device,
                &devices.microphones,
                devices.default_microphone_id.as_deref(),
            );
        }
    });
    (changed, refresh_requested)
}

fn audio_device_picker(
    ui: &mut egui::Ui,
    label: &str,
    id_salt: impl std::hash::Hash,
    selected: &mut Option<AudioDeviceConfig>,
    devices: &[AudioDeviceConfig],
    default_id: Option<&str>,
) -> bool {
    let mut changed = false;
    let available = selected
        .as_ref()
        .is_none_or(|saved| devices.iter().any(|device| device.id == saved.id));
    let selected_text = match selected.as_ref() {
        None => "Keep current device".to_string(),
        Some(device) if available => device.label().to_string(),
        Some(device) => format!("{} (unavailable)", device.label()),
    };

    ui.label(egui::RichText::new(label).strong());
    egui::ComboBox::from_id_salt(id_salt)
        .selected_text(selected_text)
        .width(ui.available_width().clamp(180.0, 420.0))
        .show_ui(ui, |ui| {
            changed |= ui
                .selectable_value(selected, None, "Keep current device")
                .changed();
            ui.separator();
            if devices.is_empty() {
                ui.add_enabled(false, egui::Label::new("No active devices found"));
            }
            for device in devices {
                let option_label = if default_id == Some(device.id.as_str()) {
                    format!("{} (current default)", device.label())
                } else {
                    device.label().to_string()
                };
                let response = ui.selectable_value(selected, Some(device.clone()), option_label);
                changed |= response.changed();
                response.on_hover_text(&device.id);
            }
        });
    if !available {
        ui.small(
            "The saved device is not currently available; applying this profile will fail safely.",
        );
    }
    changed
}

fn shortcut_editors(
    ui: &mut egui::Ui,
    layout: &mut MonitorLayout,
    index: usize,
    capture_active: bool,
    capture_elsewhere: bool,
    capture_status: &str,
    connected_controllers: &[String],
) -> (bool, Option<ControllerEdit>) {
    if ui.available_width() >= SHORTCUT_CARD_BREAKPOINT {
        let mut hotkeys_changed = false;
        let mut controller_edit = None;
        ui.columns(2, |columns| {
            hotkeys_changed = global_hotkey_editor(&mut columns[0], layout, index);
            controller_edit = controller_hotkey_editor(
                &mut columns[1],
                layout,
                index,
                capture_active,
                capture_elsewhere,
                capture_status,
                connected_controllers,
            );
        });
        (hotkeys_changed, controller_edit)
    } else {
        let hotkeys_changed = global_hotkey_editor(ui, layout, index);
        ui.add_space(8.0);
        let controller_edit = controller_hotkey_editor(
            ui,
            layout,
            index,
            capture_active,
            capture_elsewhere,
            capture_status,
            connected_controllers,
        );
        (hotkeys_changed, controller_edit)
    }
}

fn global_hotkey_editor(ui: &mut egui::Ui, layout: &mut MonitorLayout, index: usize) -> bool {
    let mut changed = false;
    let edit_id = ui.make_persistent_id(("keyboard_shortcut_edit", index));
    let mut editing = ui
        .data(|data| data.get_temp::<bool>(edit_id))
        .unwrap_or(false);
    section_frame(ui, |ui| {
        ui.strong("⌨  Keyboard shortcut");
        let binding_label = layout
            .hotkey
            .map(|binding| binding.label())
            .unwrap_or_else(|| "No shortcut assigned".to_string());
        shortcut_value_row(ui, &binding_label, &mut editing);

        if editing {
            ui.separator();
            let mut enabled = layout.hotkey.is_some();
            if ui.checkbox(&mut enabled, "Enabled").changed() {
                layout.hotkey = enabled.then_some(HotkeyBinding::default());
                changed = true;
            }

            if let Some(binding) = layout.hotkey.as_mut() {
                ui.horizontal_wrapped(|ui| {
                    changed |= ui.checkbox(&mut binding.ctrl, "Ctrl").changed();
                    changed |= ui.checkbox(&mut binding.alt, "Alt").changed();
                    changed |= ui.checkbox(&mut binding.shift, "Shift").changed();
                    changed |= ui.checkbox(&mut binding.win, "Win").changed();

                    egui::ComboBox::from_id_salt(("hotkey_key", index))
                        .selected_text(binding.key.label())
                        .show_ui(ui, |ui| {
                            for key in HotkeyKey::ALL {
                                changed |= ui
                                    .selectable_value(&mut binding.key, key, key.label())
                                    .changed();
                            }
                        });
                });
                if !binding.has_modifier() {
                    ui.colored_label(
                        ui.visuals().error_fg_color,
                        "Choose at least one modifier for a global hotkey.",
                    );
                }
            }
        }
    });
    ui.data_mut(|data| data.insert_temp(edit_id, editing));
    changed
}

fn shortcut_value_row(ui: &mut egui::Ui, value: &str, editing: &mut bool) {
    ui.horizontal(|ui| {
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if pencil_button(ui).on_hover_text("Edit shortcut").clicked() {
                *editing = !*editing;
            }
            if ui
                .add_sized(
                    [ui.available_width(), 34.0],
                    egui::Button::new(value).fill(input_fill()),
                )
                .clicked()
            {
                *editing = !*editing;
            }
        });
    });
}

fn pencil_button(ui: &mut egui::Ui) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(36.0, 34.0), egui::Sense::click());
    let visuals = ui.style().interact(&response);
    ui.painter().rect_filled(rect, 6.0, visuals.bg_fill);
    paint_rect_outline(ui.painter(), rect, visuals.bg_stroke);
    let center = rect.center();
    ui.painter().line_segment(
        [
            center + egui::vec2(-5.0, 5.0),
            center + egui::vec2(5.0, -5.0),
        ],
        egui::Stroke::new(2.0_f32, visuals.fg_stroke.color),
    );
    ui.painter().line_segment(
        [
            center + egui::vec2(-6.0, 6.0),
            center + egui::vec2(-3.0, 5.0),
        ],
        egui::Stroke::new(2.0_f32, visuals.fg_stroke.color),
    );
    response
}

fn paint_rect_outline(painter: &egui::Painter, rect: egui::Rect, stroke: egui::Stroke) {
    painter.line_segment([rect.left_top(), rect.right_top()], stroke);
    painter.line_segment([rect.right_top(), rect.right_bottom()], stroke);
    painter.line_segment([rect.right_bottom(), rect.left_bottom()], stroke);
    painter.line_segment([rect.left_bottom(), rect.left_top()], stroke);
}

fn controller_hotkey_editor(
    ui: &mut egui::Ui,
    layout: &mut MonitorLayout,
    layout_index: usize,
    capture_active: bool,
    capture_elsewhere: bool,
    capture_status: &str,
    connected_controllers: &[String],
) -> Option<ControllerEdit> {
    let mut action = None;
    let edit_id = ui.make_persistent_id(("controller_shortcut_edit", layout_index));
    let mut editing = ui
        .data(|data| data.get_temp::<bool>(edit_id))
        .unwrap_or(false)
        || capture_active;
    section_frame(ui, |ui| {
        ui.strong("Controller shortcut");
        let binding_label = layout
            .controller_hotkey
            .as_ref()
            .map(|binding| binding.label())
            .unwrap_or_else(|| "No shortcut assigned".to_string());
        shortcut_value_row(ui, &binding_label, &mut editing);

        if editing {
            ui.separator();
            ui.horizontal_wrapped(|ui| {
                if capture_active {
                    ui.spinner();
                    if ui.button("Cancel listening").clicked() {
                        action = Some(ControllerEdit::CancelCapture);
                    }
                    return;
                }

                let text = if layout.controller_hotkey.is_some() {
                    "Rebind controller chord"
                } else {
                    "Bind controller chord"
                };
                if ui
                    .add_enabled(!capture_elsewhere, egui::Button::new(text))
                    .on_hover_text(
                        "Release all buttons, press one button or a chord, then release it to save",
                    )
                    .clicked()
                {
                    action = Some(ControllerEdit::BeginCapture);
                }
                if ui
                    .add_enabled(
                        layout.controller_hotkey.is_some(),
                        egui::Button::new("Clear"),
                    )
                    .clicked()
                {
                    layout.controller_hotkey = None;
                    action = Some(ControllerEdit::Clear);
                }
            });

            if capture_active {
                ui.colored_label(ui.visuals().warn_fg_color, capture_status);
            } else if capture_elsewhere {
                ui.small("Controller listening is active for another layout.");
            }

            if connected_controllers.is_empty() {
                ui.small("No Windows game controller is currently connected.");
            } else {
                ui.small(format!("Connected: {}", connected_controllers.join(", ")));
            }
        }
    });
    ui.data_mut(|data| data.insert_temp(edit_id, editing));
    action
}

struct GeometryUpdate {
    source_index: usize,
    clone_group: Option<u32>,
    x: i32,
    y: i32,
    width: u32,
    height: u32,
}

struct MonitorCardResult {
    changed: bool,
    make_primary: bool,
    geometry_update: Option<GeometryUpdate>,
}

fn monitor_arrangement_editor(ui: &mut egui::Ui, layout: &mut MonitorLayout, index: usize) -> bool {
    let mut changed = false;
    let enabled_count = layout
        .monitors
        .iter()
        .filter(|monitor| monitor.enabled)
        .count();
    section_frame(ui, |ui| {
        ui.horizontal(|ui| {
            ui.strong("Display arrangement");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.horizontal(|ui| {
                    let (dot_rect, _) =
                        ui.allocate_exact_size(egui::vec2(8.0, 8.0), egui::Sense::hover());
                    ui.painter().circle_filled(
                        dot_rect.center(),
                        4.0,
                        if enabled_count == 0 {
                            ui.visuals().error_fg_color
                        } else {
                            egui::Color32::from_rgb(74, 196, 100)
                        },
                    );
                    ui.label(format!("{enabled_count} enabled"));
                });
            });
        });
        if enabled_count == 0 {
            ui.colored_label(
                ui.visuals().error_fg_color,
                "Enable at least one display before applying this layout.",
            );
        }
        ui.add_space(2.0);
        changed |= monitor_layout_canvas(ui, layout, index);
    });
    changed
}

fn monitor_list_editor(ui: &mut egui::Ui, layout: &mut MonitorLayout, index: usize) -> bool {
    let mut changed = false;
    let mut make_primary = None;
    let mut geometry_update = None;
    let show_table = ui.available_width() >= DISPLAY_TABLE_BREAKPOINT;
    section_frame(ui, |ui| {
        ui.horizontal(|ui| {
            ui.strong("Displays");
            ui.weak(format!("{} known", layout.monitors.len()));
        });
        if show_table {
            monitor_table_header(ui);
        }
        for (monitor_index, monitor) in layout.monitors.iter_mut().enumerate() {
            let result = if show_table {
                monitor_table_row(ui, monitor, index, monitor_index)
            } else {
                monitor_summary_card(ui, monitor, index, monitor_index)
            };
            changed |= result.changed;
            if result.make_primary {
                make_primary = Some(monitor_index);
            }
            if result.geometry_update.is_some() {
                geometry_update = result.geometry_update;
            }
            ui.add_space(5.0);
        }
    });

    if let Some(update) = geometry_update
        && let Some(group) = update.clone_group
    {
        for (monitor_index, monitor) in layout.monitors.iter_mut().enumerate() {
            if monitor_index != update.source_index && monitor.clone_group == Some(group) {
                monitor.x = update.x;
                monitor.y = update.y;
                monitor.width = update.width;
                monitor.height = update.height;
            }
        }
    }

    if let Some(primary) = make_primary.and_then(|index| layout.monitors.get(index)) {
        let (offset_x, offset_y) = (primary.x, primary.y);
        for monitor in &mut layout.monitors {
            monitor.x = monitor.x.saturating_sub(offset_x);
            monitor.y = monitor.y.saturating_sub(offset_y);
        }
        changed = true;
    }

    changed
}

fn monitor_table_header(ui: &mut egui::Ui) {
    ui.add_space(2.0);
    ui.horizontal(|ui| {
        ui.add_sized([24.0, 20.0], egui::Label::new("On"));
        ui.add_sized([30.0, 20.0], egui::Label::new("#"));
        let name_width = (ui.available_width() - 474.0).clamp(120.0, 230.0);
        ui.add_sized([name_width, 20.0], egui::Label::new("Name"));
        ui.add_sized([112.0, 20.0], egui::Label::new("Resolution"));
        ui.add_sized([112.0, 20.0], egui::Label::new("Orientation"));
        ui.add_sized([76.0, 20.0], egui::Label::new("Refresh"));
        ui.add_sized([72.0, 20.0], egui::Label::new("Primary"));
    });
    ui.separator();
}

fn monitor_table_row(
    ui: &mut egui::Ui,
    monitor: &mut MonitorConfig,
    layout_index: usize,
    monitor_index: usize,
) -> MonitorCardResult {
    let mut changed = false;
    let mut make_primary = false;
    let mut geometry_changed = false;
    let details_id = ui.make_persistent_id(("monitor_details", layout_index, monitor_index));
    let mut details_open = ui
        .data(|data| data.get_temp::<bool>(details_id))
        .unwrap_or(false);
    let available_width = ui.available_width();
    egui::Frame::new()
        .fill(input_fill())
        .stroke(egui::Stroke::new(1.0_f32, border_color()))
        .inner_margin(8)
        .corner_radius(6)
        .show(ui, |ui| {
            ui.set_min_width((available_width - 16.0).max(0.0));
            let (display_width, display_height) = display_size(monitor);
            let is_primary = monitor.enabled && monitor.x == 0 && monitor.y == 0;
            ui.horizontal(|ui| {
                changed |= ui
                    .add_sized([24.0, 26.0], egui::Checkbox::new(&mut monitor.enabled, ""))
                    .on_hover_text("Include this display in the layout")
                    .changed();
                ui.add_sized(
                    [30.0, 26.0],
                    egui::Button::new((monitor_index + 1).to_string()).fill(header_fill()),
                );
                let name_width = (ui.available_width() - 474.0).clamp(120.0, 230.0);
                ui.add_sized(
                    [name_width, 26.0],
                    egui::Label::new(egui::RichText::new(&monitor.friendly_name).strong())
                        .truncate(),
                )
                .on_hover_text(&monitor.friendly_name);
                ui.add_sized(
                    [112.0, 26.0],
                    egui::Label::new(format!("{display_width} × {display_height}")),
                );
                ui.add_sized(
                    [112.0, 26.0],
                    egui::Label::new(rotation_label(monitor.rotation.unwrap_or(0))),
                );
                ui.add_sized(
                    [76.0, 26.0],
                    egui::Label::new(refresh_rate_label(monitor.refresh_hz)),
                );
                ui.add_sized(
                    [72.0, 26.0],
                    egui::Label::new(if is_primary {
                        egui::RichText::new("Primary").color(accent_color())
                    } else {
                        egui::RichText::new("")
                    }),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .add_sized(
                            [28.0, 26.0],
                            egui::Button::new(if details_open { "^" } else { "v" }),
                        )
                        .on_hover_text("Advanced display details")
                        .clicked()
                    {
                        details_open = !details_open;
                    }
                });
            });
            if details_open {
                ui.separator();
                let result =
                    monitor_details_editor(ui, monitor, layout_index, monitor_index, is_primary);
                changed |= result.changed;
                geometry_changed |= result.geometry_changed;
                make_primary |= result.make_primary;
            }
        });
    ui.data_mut(|data| data.insert_temp(details_id, details_open));
    monitor_card_result(
        monitor,
        monitor_index,
        changed,
        geometry_changed,
        make_primary,
    )
}

fn monitor_summary_card(
    ui: &mut egui::Ui,
    monitor: &mut MonitorConfig,
    layout_index: usize,
    monitor_index: usize,
) -> MonitorCardResult {
    let mut changed = false;
    let mut make_primary = false;
    let mut geometry_changed = false;
    let details_id = ui.make_persistent_id(("monitor_details", layout_index, monitor_index));
    let mut details_open = ui
        .data(|data| data.get_temp::<bool>(details_id))
        .unwrap_or(false);

    let available_width = ui.available_width();
    egui::Frame::new()
        .fill(input_fill())
        .stroke(egui::Stroke::new(1.0_f32, border_color()))
        .inner_margin(10)
        .corner_radius(6)
        .show(ui, |ui| {
            ui.set_min_width((available_width - 20.0).max(0.0));
            let (display_width, display_height) = display_size(monitor);
            let is_primary = monitor.enabled && monitor.x == 0 && monitor.y == 0;
            ui.horizontal(|ui| {
                changed |= ui
                    .checkbox(&mut monitor.enabled, "")
                    .on_hover_text("Include this display in the layout")
                    .changed();
                ui.add_sized(
                    [30.0, 26.0],
                    egui::Button::new((monitor_index + 1).to_string()).fill(header_fill()),
                );
                ui.add(
                    egui::Label::new(egui::RichText::new(&monitor.friendly_name).strong())
                        .truncate(),
                )
                .on_hover_text(&monitor.friendly_name);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .small_button(if details_open { "^" } else { "v" })
                        .on_hover_text("Advanced display details")
                        .clicked()
                    {
                        details_open = !details_open;
                    }
                    if is_primary {
                        ui.label(
                            egui::RichText::new("Primary")
                                .strong()
                                .color(accent_color()),
                        );
                    }
                });
            });
            ui.horizontal(|ui| {
                ui.add_space(62.0);
                ui.weak(format!(
                    "{display_width} × {display_height}   •   {}   •   {}",
                    rotation_label(monitor.rotation.unwrap_or(0)),
                    refresh_rate_label(monitor.refresh_hz)
                ));
            });

            if details_open {
                ui.separator();
                let result =
                    monitor_details_editor(ui, monitor, layout_index, monitor_index, is_primary);
                changed |= result.changed;
                geometry_changed |= result.geometry_changed;
                make_primary |= result.make_primary;
            }
        });
    ui.data_mut(|data| data.insert_temp(details_id, details_open));
    monitor_card_result(
        monitor,
        monitor_index,
        changed,
        geometry_changed,
        make_primary,
    )
}

struct MonitorDetailsResult {
    changed: bool,
    geometry_changed: bool,
    make_primary: bool,
}

fn monitor_details_editor(
    ui: &mut egui::Ui,
    monitor: &mut MonitorConfig,
    layout_index: usize,
    monitor_index: usize,
    is_primary: bool,
) -> MonitorDetailsResult {
    let mut changed = false;
    let mut geometry_changed = false;
    let mut make_primary = false;
    egui::Grid::new(("monitor_details_grid", layout_index, monitor_index))
        .num_columns(2)
        .spacing([18.0, 8.0])
        .show(ui, |ui| {
            ui.label("Position");
            ui.horizontal(|ui| {
                ui.label("X");
                geometry_changed |= ui
                    .add_sized([80.0, 28.0], egui::DragValue::new(&mut monitor.x).speed(10))
                    .changed();
                ui.label("Y");
                geometry_changed |= ui
                    .add_sized([80.0, 28.0], egui::DragValue::new(&mut monitor.y).speed(10))
                    .changed();
            });
            ui.end_row();

            ui.label("Source resolution");
            ui.horizontal(|ui| {
                geometry_changed |= ui
                    .add_sized(
                        [92.0, 28.0],
                        egui::DragValue::new(&mut monitor.width)
                            .range(320..=16384)
                            .speed(10),
                    )
                    .changed();
                ui.label("×");
                geometry_changed |= ui
                    .add_sized(
                        [92.0, 28.0],
                        egui::DragValue::new(&mut monitor.height)
                            .range(200..=16384)
                            .speed(10),
                    )
                    .changed();
            });
            ui.end_row();

            ui.label("Orientation");
            let old_rotation = monitor.rotation.unwrap_or(0);
            let mut rotation = old_rotation;
            egui::ComboBox::from_id_salt(("monitor_rotation", layout_index, monitor_index))
                .selected_text(rotation_label(rotation))
                .width(210.0)
                .show_ui(ui, |ui| {
                    for (value, label) in DISPLAY_ROTATIONS {
                        ui.selectable_value(&mut rotation, value, label);
                    }
                });
            if rotation != old_rotation {
                monitor.rotation = Some(rotation);
                changed = true;
            }
            ui.end_row();

            ui.label("Refresh rate");
            if ui
                .add(
                    egui::DragValue::new(&mut monitor.refresh_hz)
                        .range(1.0..=1000.0)
                        .speed(1.0)
                        .suffix(" Hz"),
                )
                .changed()
            {
                monitor.refresh_numerator = None;
                monitor.refresh_denominator = None;
                changed = true;
            }
            ui.end_row();

            ui.label("Primary display");
            if is_primary {
                ui.strong("Primary");
            } else if ui
                .add_enabled(monitor.enabled, egui::Button::new("Make primary"))
                .clicked()
            {
                make_primary = true;
            }
            ui.end_row();

            ui.label("Clone group");
            ui.label(
                monitor
                    .clone_group
                    .map(|group| format!("#{group}"))
                    .unwrap_or_else(|| "None".to_string()),
            );
            ui.end_row();

            ui.label("Identity");
            ui.add(
                egui::Label::new(egui::RichText::new(monitor.identity.display_label()).small())
                    .truncate(),
            )
            .on_hover_text(&monitor.identity.device_path);
            ui.end_row();
        });
    MonitorDetailsResult {
        changed,
        geometry_changed,
        make_primary,
    }
}

fn monitor_card_result(
    monitor: &MonitorConfig,
    monitor_index: usize,
    changed: bool,
    geometry_changed: bool,
    make_primary: bool,
) -> MonitorCardResult {
    let geometry_update = geometry_changed.then_some(GeometryUpdate {
        source_index: monitor_index,
        clone_group: monitor.clone_group,
        x: monitor.x,
        y: monitor.y,
        width: monitor.width,
        height: monitor.height,
    });

    MonitorCardResult {
        changed: changed || geometry_changed,
        make_primary,
        geometry_update,
    }
}

fn refresh_rate_label(refresh_hz: f32) -> String {
    if (refresh_hz - refresh_hz.round()).abs() < 0.01 {
        format!("{refresh_hz:.0} Hz")
    } else {
        format!("{refresh_hz:.2} Hz")
    }
}

fn startup_window_visibility(startup_launch: bool, tray_available: bool) -> Option<bool> {
    startup_launch.then_some(!tray_available)
}

impl eframe::App for MonManApp {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if let Some(visible) = self.initial_window_visibility.take() {
            // eframe shows the root window after its first rendered frame, even when the
            // native viewport was created hidden. Reassert the startup visibility after
            // that frame so a healthy tray launch remains tray-only.
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(visible));
        }
        self.handle_hotkeys();
        self.handle_controllers();
        self.handle_tray(ctx);
        self.handle_updater(ctx);
        self.persist(ctx);

        if ctx.input(|input| input.viewport().close_requested()) && !self.exit_requested {
            if self.tray.is_some() {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                self.hide_to_tray(ctx);
            } else {
                self.exit_requested = true;
            }
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let show_sidebar = ui.available_width() >= SIDEBAR_BREAKPOINT;
        self.status_bar(ui);
        if show_sidebar {
            self.sidebar(ui);
        } else {
            self.compact_navigation(ui);
        }
        self.editor(ui, show_sidebar);
    }

    fn on_exit(&mut self) {
        if self.dirty {
            let _ = storage::save(&self.config);
        }
    }
}

fn configure_ui_style(ctx: &egui::Context) {
    let mut style = (*ctx.global_style()).clone();
    style.spacing.item_spacing = egui::vec2(8.0, 7.0);
    style.spacing.button_padding = egui::vec2(11.0, 6.0);
    style.spacing.interact_size.y = 30.0;
    style.visuals = egui::Visuals::dark();
    style.visuals.panel_fill = app_fill();
    style.visuals.window_fill = app_fill();
    style.visuals.extreme_bg_color = canvas_fill();
    style.visuals.faint_bg_color = input_fill();
    style.visuals.selection.bg_fill = accent_color();
    style.visuals.selection.stroke = egui::Stroke::new(1.0_f32, egui::Color32::WHITE);
    style.visuals.widgets.noninteractive.bg_fill = card_fill();
    style.visuals.widgets.noninteractive.bg_stroke = egui::Stroke::new(1.0_f32, border_color());
    style.visuals.widgets.inactive.bg_fill = input_fill();
    style.visuals.widgets.inactive.bg_stroke = egui::Stroke::new(1.0_f32, border_color());
    style.visuals.widgets.hovered.bg_fill = egui::Color32::from_rgb(42, 51, 62);
    style.visuals.widgets.hovered.bg_stroke = egui::Stroke::new(1.0_f32, accent_color());
    style.visuals.widgets.active.bg_fill = accent_color();
    style.visuals.widgets.active.bg_stroke = egui::Stroke::new(1.0_f32, accent_color());
    style.visuals.widgets.open.bg_fill = input_fill();
    style.visuals.widgets.open.bg_stroke = egui::Stroke::new(1.0_f32, accent_color());
    let corner_radius = egui::CornerRadius::same(7);
    style.visuals.widgets.noninteractive.corner_radius = corner_radius;
    style.visuals.widgets.inactive.corner_radius = corner_radius;
    style.visuals.widgets.hovered.corner_radius = corner_radius;
    style.visuals.widgets.active.corner_radius = corner_radius;
    style.visuals.widgets.open.corner_radius = corner_radius;
    ctx.set_global_style(style);
}

fn app_fill() -> egui::Color32 {
    egui::Color32::from_rgb(20, 26, 33)
}

fn sidebar_fill() -> egui::Color32 {
    egui::Color32::from_rgb(17, 23, 30)
}

fn header_fill() -> egui::Color32 {
    egui::Color32::from_rgb(23, 30, 38)
}

fn card_fill() -> egui::Color32 {
    egui::Color32::from_rgb(26, 34, 43)
}

fn input_fill() -> egui::Color32 {
    egui::Color32::from_rgb(18, 24, 31)
}

fn canvas_fill() -> egui::Color32 {
    egui::Color32::from_rgb(12, 18, 24)
}

fn border_color() -> egui::Color32 {
    egui::Color32::from_rgb(54, 65, 77)
}

fn accent_color() -> egui::Color32 {
    egui::Color32::from_rgb(8, 103, 216)
}

fn capture_profile(name: impl Into<String>) -> anyhow::Result<MonitorLayout> {
    let mut layout = display::capture_layout(name)?;
    audio::capture_current_devices(&mut layout)?;
    Ok(layout)
}

fn apply_profile(layout: &MonitorLayout) -> anyhow::Result<()> {
    display::apply_layout(layout)?;
    audio::apply_layout(layout)?;
    Ok(())
}

fn working_snapshot(fallback: &MonitorLayout) -> MonitorLayout {
    display::capture_layout("Last known working topology")
        .map(sanitized_working_snapshot)
        .unwrap_or_else(|_| sanitized_working_snapshot(fallback.clone()))
}

fn refresh_monitor(existing: &mut MonitorConfig, current: MonitorConfig) {
    existing.friendly_name = current.friendly_name;
    if existing.identity.device_path.is_empty() && !current.identity.device_path.is_empty() {
        existing.identity = current.identity;
    }
    existing.rotation = existing.rotation.or(current.rotation);
    existing.scaling = existing.scaling.or(current.scaling);

    let has_exact_refresh =
        existing.refresh_numerator.is_some() || existing.refresh_denominator.is_some();
    if !has_exact_refresh && (existing.refresh_hz - current.refresh_hz).abs() < 0.01 {
        existing.refresh_numerator = current.refresh_numerator;
        existing.refresh_denominator = current.refresh_denominator;
    }
}

fn sanitized_working_snapshot(mut layout: MonitorLayout) -> MonitorLayout {
    layout.name = "Last known working topology".into();
    layout.playback_device = None;
    layout.microphone_device = None;
    layout.hotkey = None;
    layout.controller_hotkey = None;
    layout
}

const DISPLAY_ROTATIONS: [(i32, &str); 4] = [
    (1, "Landscape (0°)"),
    (2, "Portrait (90°)"),
    (3, "Landscape flipped (180°)"),
    (4, "Portrait flipped (270°)"),
];

fn rotation_label(rotation: i32) -> &'static str {
    DISPLAY_ROTATIONS
        .iter()
        .find_map(|(value, label)| (*value == rotation).then_some(*label))
        .unwrap_or("Current / unknown")
}

fn is_quarter_turn(rotation: i32) -> bool {
    matches!(rotation, 2 | 4)
}

fn monitor_layout_canvas(
    ui: &mut egui::Ui,
    layout: &mut MonitorLayout,
    layout_index: usize,
) -> bool {
    let width = ui.available_width().max(320.0);
    let height = if width < SHORTCUT_CARD_BREAKPOINT {
        185.0
    } else {
        280.0
    };
    let (response, painter) = ui.allocate_painter(egui::vec2(width, height), egui::Sense::hover());
    let outer = response.rect;
    painter.rect_filled(outer, 6.0, canvas_fill());
    let grid_color = egui::Color32::from_rgb(45, 56, 67);
    let mut grid_x = outer.left() + 12.0;
    while grid_x < outer.right() - 8.0 {
        let mut grid_y = outer.top() + 12.0;
        while grid_y < outer.bottom() - 8.0 {
            painter.circle_filled(egui::pos2(grid_x, grid_y), 0.75, grid_color);
            grid_y += 16.0;
        }
        grid_x += 16.0;
    }

    if layout.monitors.is_empty() {
        painter.text(
            outer.center(),
            egui::Align2::CENTER_CENTER,
            "No monitors in this layout\nUse “Sync connected monitors” to add them.",
            egui::FontId::proportional(14.0),
            ui.visuals().weak_text_color(),
        );
        return false;
    }

    let mut min_x = i64::MAX;
    let mut min_y = i64::MAX;
    let mut max_x = i64::MIN;
    let mut max_y = i64::MIN;
    for monitor in &layout.monitors {
        let (display_width, display_height) = display_size(monitor);
        min_x = min_x.min(monitor.x as i64);
        min_y = min_y.min(monitor.y as i64);
        max_x = max_x.max(monitor.x as i64 + display_width as i64);
        max_y = max_y.max(monitor.y as i64 + display_height as i64);
    }

    let world_width = (max_x - min_x).max(1) as f32;
    let world_height = (max_y - min_y).max(1) as f32;
    let canvas = outer.shrink(22.0);
    let scale =
        ((canvas.width() / world_width).min(canvas.height() / world_height)).clamp(0.01, 0.30);
    let world_center_x = (min_x + max_x) as f32 * 0.5;
    let world_center_y = (min_y + max_y) as f32 * 0.5;

    let to_screen = |x: f32, y: f32| {
        egui::pos2(
            canvas.center().x + (x - world_center_x) * scale,
            canvas.center().y + (y - world_center_y) * scale,
        )
    };

    let mut changed = false;
    let mut snap_guides = Vec::new();
    for monitor_index in 0..layout.monitors.len() {
        let monitor = &layout.monitors[monitor_index];
        let min = to_screen(monitor.x as f32, monitor.y as f32);
        let (display_width, display_height) = display_size(monitor);
        let size = egui::vec2(
            (display_width as f32 * scale).max(34.0),
            (display_height as f32 * scale).max(24.0),
        );
        let monitor_rect = egui::Rect::from_min_size(min, size);
        let enabled = monitor.enabled;
        let clone_group = monitor.clone_group;
        let dimensions = format!("{display_width}×{display_height}");
        let refresh_rate = refresh_rate_label(monitor.refresh_hz);

        let id = ui
            .id()
            .with(("monitor_preview", layout_index, monitor_index));
        let interaction = ui.interact(monitor_rect, id, egui::Sense::drag());
        let drag_state_id = id.with("snap_drag_state");
        if interaction.drag_started() {
            ui.data_mut(|data| {
                data.insert_temp(
                    drag_state_id,
                    MonitorDragState {
                        origins: layout
                            .monitors
                            .iter()
                            .map(|monitor| (monitor.x, monitor.y))
                            .collect(),
                        scale,
                    },
                );
            });
        }
        if interaction.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::Grab);
        }
        if interaction.dragged() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
            let drag_state = ui
                .data(|data| data.get_temp::<MonitorDragState>(drag_state_id))
                .unwrap_or_else(|| MonitorDragState {
                    origins: layout
                        .monitors
                        .iter()
                        .map(|monitor| (monitor.x, monitor.y))
                        .collect(),
                    scale,
                });
            if let Some(&(origin_x, origin_y)) = drag_state.origins.get(monitor_index) {
                let delta = interaction.drag_delta();
                let raw_x = origin_x.saturating_add((delta.x / drag_state.scale).round() as i32);
                let raw_y = origin_y.saturating_add((delta.y / drag_state.scale).round() as i32);
                let snapped = snap_monitor_position(
                    layout,
                    monitor_index,
                    raw_x,
                    raw_y,
                    SNAP_DISTANCE_PX / drag_state.scale,
                );
                let dx = snapped.x.saturating_sub(origin_x);
                let dy = snapped.y.saturating_sub(origin_y);

                for (index, monitor) in layout.monitors.iter_mut().enumerate() {
                    let moves_with_dragged = index == monitor_index
                        || clone_group.is_some() && monitor.clone_group == clone_group;
                    if !moves_with_dragged {
                        continue;
                    }
                    let Some(&(start_x, start_y)) = drag_state.origins.get(index) else {
                        continue;
                    };
                    let next_x = start_x.saturating_add(dx);
                    let next_y = start_y.saturating_add(dy);
                    if monitor.x != next_x || monitor.y != next_y {
                        monitor.x = next_x;
                        monitor.y = next_y;
                        changed = true;
                    }
                }
                snap_guides.extend(snapped.guides);
            }
        }
        if interaction.drag_stopped() {
            ui.data_mut(|data| {
                data.remove_temp::<MonitorDragState>(drag_state_id);
            });
        }

        let drawn_monitor = &layout.monitors[monitor_index];
        let drawn_min = to_screen(drawn_monitor.x as f32, drawn_monitor.y as f32);
        let monitor_rect = egui::Rect::from_min_size(drawn_min, size);
        let fill = if enabled {
            header_fill()
        } else {
            egui::Color32::from_rgb(28, 34, 41)
        };
        let border = if enabled {
            egui::Stroke::new(
                if interaction.dragged() || interaction.hovered() {
                    2.5_f32
                } else {
                    1.5_f32
                },
                accent_color(),
            )
        } else {
            egui::Stroke::new(1.0_f32, border_color())
        };
        painter.rect_filled(monitor_rect, 4.0, fill);
        painter.line_segment([monitor_rect.left_top(), monitor_rect.right_top()], border);
        painter.line_segment(
            [monitor_rect.right_top(), monitor_rect.right_bottom()],
            border,
        );
        painter.line_segment(
            [monitor_rect.right_bottom(), monitor_rect.left_bottom()],
            border,
        );
        painter.line_segment(
            [monitor_rect.left_bottom(), monitor_rect.left_top()],
            border,
        );

        painter.text(
            monitor_rect.center() - egui::vec2(0.0, 17.0),
            egui::Align2::CENTER_CENTER,
            (monitor_index + 1).to_string(),
            egui::FontId::proportional(22.0),
            ui.visuals().text_color(),
        );
        painter.text(
            monitor_rect.center() + egui::vec2(0.0, 16.0),
            egui::Align2::CENTER_CENTER,
            if enabled {
                format!("{dimensions}\n{refresh_rate}")
            } else {
                format!("{dimensions}\nOff")
            },
            egui::FontId::proportional(12.0),
            ui.visuals().text_color(),
        );
    }

    let guide_stroke = egui::Stroke::new(2.5_f32, egui::Color32::from_rgb(56, 189, 248));
    for guide in snap_guides {
        let start = to_screen(guide.start.0, guide.start.1);
        let end = to_screen(guide.end.0, guide.end.1);
        painter.line_segment([start, end], guide_stroke);
        painter.circle_filled(start, 3.5, guide_stroke.color);
        painter.circle_filled(end, 3.5, guide_stroke.color);
        painter.text(
            start.lerp(end, 0.5),
            egui::Align2::CENTER_BOTTOM,
            guide.label,
            egui::FontId::proportional(11.0),
            guide_stroke.color,
        );
    }

    changed
}

const SNAP_DISTANCE_PX: f32 = 12.0;

#[derive(Clone, Default)]
struct MonitorDragState {
    origins: Vec<(i32, i32)>,
    scale: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HorizontalAlignment {
    Top,
    Middle,
    Bottom,
}

impl HorizontalAlignment {
    const ALL: [Self; 3] = [Self::Top, Self::Middle, Self::Bottom];

    fn label(self) -> &'static str {
        match self {
            Self::Top => "TOP",
            Self::Middle => "MIDDLE",
            Self::Bottom => "BOTTOM",
        }
    }

    fn offset(self, height: u32) -> f32 {
        match self {
            Self::Top => 0.0,
            Self::Middle => height as f32 * 0.5,
            Self::Bottom => height as f32,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct SnapGuide {
    start: (f32, f32),
    end: (f32, f32),
    label: &'static str,
}

#[derive(Debug)]
struct SnapResult {
    x: i32,
    y: i32,
    guides: Vec<SnapGuide>,
}

fn snap_monitor_position(
    layout: &MonitorLayout,
    moving_index: usize,
    raw_x: i32,
    raw_y: i32,
    threshold: f32,
) -> SnapResult {
    let moving = &layout.monitors[moving_index];
    if !moving.enabled {
        return SnapResult {
            x: raw_x,
            y: raw_y,
            guides: Vec::new(),
        };
    }

    let (moving_width, moving_height) = display_size(moving);
    let mut best_alignment = None::<(f32, i32, usize, HorizontalAlignment, f32)>;
    let mut best_edge = None::<(f32, i32, usize, f32)>;

    for (target_index, target) in layout.monitors.iter().enumerate() {
        let same_clone_group =
            moving.clone_group.is_some() && moving.clone_group == target.clone_group;
        if target_index == moving_index || same_clone_group || !target.enabled {
            continue;
        }

        let (target_width, target_height) = display_size(target);
        for alignment in HorizontalAlignment::ALL {
            let moving_anchor = raw_y as f32 + alignment.offset(moving_height);
            let target_anchor = target.y as f32 + alignment.offset(target_height);
            let distance = (moving_anchor - target_anchor).abs();
            if distance <= threshold
                && best_alignment.is_none_or(|(best_distance, ..)| distance < best_distance)
            {
                best_alignment = Some((
                    distance,
                    (target_anchor - alignment.offset(moving_height)).round() as i32,
                    target_index,
                    alignment,
                    target_anchor,
                ));
            }
        }

        let moving_left = raw_x as f32;
        let moving_right = moving_left + moving_width as f32;
        let target_left = target.x as f32;
        let target_right = target_left + target_width as f32;
        for (moving_edge, target_edge) in [(moving_left, target_right), (moving_right, target_left)]
        {
            let distance = (moving_edge - target_edge).abs();
            if distance <= threshold
                && best_edge.is_none_or(|(best_distance, ..)| distance < best_distance)
            {
                let snapped_x = (raw_x as f32 + target_edge - moving_edge).round() as i32;
                best_edge = Some((distance, snapped_x, target_index, target_edge));
            }
        }
    }

    let snapped_x = best_edge.map_or(raw_x, |(_, x, _, _)| x);
    let snapped_y = best_alignment.map_or(raw_y, |(_, y, _, _, _)| y);
    let mut guides = Vec::new();
    let margin = threshold * 0.35;

    if let Some((_, _, target_index, alignment, guide_y)) = best_alignment {
        let target = &layout.monitors[target_index];
        let (target_width, _) = display_size(target);
        guides.push(SnapGuide {
            start: ((snapped_x as f32).min(target.x as f32) - margin, guide_y),
            end: (
                (snapped_x as f32 + moving_width as f32).max(target.x as f32 + target_width as f32)
                    + margin,
                guide_y,
            ),
            label: alignment.label(),
        });
    }

    if let Some((_, _, target_index, guide_x)) = best_edge {
        let target = &layout.monitors[target_index];
        let (_, target_height) = display_size(target);
        guides.push(SnapGuide {
            start: (guide_x, (snapped_y as f32).min(target.y as f32) - margin),
            end: (
                guide_x,
                (snapped_y as f32 + moving_height as f32)
                    .max(target.y as f32 + target_height as f32)
                    + margin,
            ),
            label: "EDGE",
        });
    }

    SnapResult {
        x: snapped_x,
        y: snapped_y,
        guides,
    }
}

fn display_size(monitor: &crate::model::MonitorConfig) -> (u32, u32) {
    if monitor.rotation.is_some_and(is_quarter_turn) {
        (monitor.height, monitor.width)
    } else {
        (monitor.width, monitor.height)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::MonitorIdentity;

    #[test]
    fn startup_visibility_tracks_tray_availability() {
        assert_eq!(startup_window_visibility(true, true), Some(false));
        assert_eq!(startup_window_visibility(true, false), Some(true));
        assert_eq!(startup_window_visibility(false, true), None);
    }

    fn monitor(
        name: &str,
        x: i32,
        y: i32,
        width: u32,
        height: u32,
        rotation: i32,
    ) -> crate::model::MonitorConfig {
        crate::model::MonitorConfig {
            identity: MonitorIdentity {
                device_path: name.to_string(),
                adapter_low: 0,
                adapter_high: 0,
                target_id: 0,
            },
            friendly_name: name.into(),
            enabled: true,
            source_adapter_low: 0,
            source_adapter_high: 0,
            source_id: 0,
            clone_group: None,
            rotation: Some(rotation),
            scaling: None,
            x,
            y,
            width,
            height,
            refresh_hz: 60.0,
            refresh_numerator: None,
            refresh_denominator: None,
        }
    }

    #[test]
    fn portrait_monitor_is_drawn_with_vertical_dimensions() {
        let monitor = monitor("portrait", 0, 0, 3840, 2160, 2);

        assert_eq!(display_size(&monitor), (2160, 3840));
    }

    #[test]
    fn snapping_uses_rotated_monitor_top_middle_and_bottom() {
        let target = monitor("portrait", 0, 0, 1920, 1080, 2);
        let moving = monitor("landscape", 4000, 0, 1920, 1080, 1);
        let layout = MonitorLayout {
            name: "snap test".into(),
            monitors: vec![target, moving],
            playback_device: None,
            microphone_device: None,
            hotkey: None,
            controller_hotkey: None,
        };

        let top = snap_monitor_position(&layout, 1, 1090, 10, 20.0);
        assert_eq!((top.x, top.y), (1080, 0));
        assert!(top.guides.iter().any(|guide| guide.label == "TOP"));

        let middle = snap_monitor_position(&layout, 1, 4000, 430, 20.0);
        assert_eq!(middle.y, 420);
        assert!(middle.guides.iter().any(|guide| guide.label == "MIDDLE"));

        let bottom = snap_monitor_position(&layout, 1, 4000, 850, 20.0);
        assert_eq!(bottom.y, 840);
        assert!(bottom.guides.iter().any(|guide| guide.label == "BOTTOM"));
    }
}
