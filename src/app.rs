use crate::controllers::{
    ControllerDeviceInfo, ControllerEvent, ControllerManager, ControllerSpec,
};
use crate::display;
use crate::hotkeys::{HotkeyEvent, HotkeyManager, HotkeySpec};
use crate::model::{AppConfig, HotkeyBinding, HotkeyKey, MonitorConfig, MonitorLayout};
use crate::storage;
use crate::tray::{TrayEvent, TrayManager};
use crate::updater::{AvailableUpdate, UpdateEvent, UpdateManager};
use eframe::egui;
use std::time::{Duration, Instant};

pub struct MonManApp {
    config: AppConfig,
    selected: Option<usize>,
    hotkeys: HotkeyManager,
    controllers: ControllerManager,
    controller_devices: Vec<ControllerDeviceInfo>,
    controller_capture_layout: Option<usize>,
    controller_capture_status: String,
    tray: Option<TrayManager>,
    updater: UpdateManager,
    available_update: Option<AvailableUpdate>,
    update_in_progress: bool,
    exit_requested: bool,
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
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
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
        let updater = UpdateManager::new(cc.egui_ctx.clone());
        let mut app = Self {
            config,
            selected,
            hotkeys,
            controllers,
            controller_devices: Vec::new(),
            controller_capture_layout: None,
            controller_capture_status: String::new(),
            tray,
            updater,
            available_update: None,
            update_in_progress: false,
            exit_requested: false,
            status,
            dirty,
            last_persist: Instant::now(),
            undo_layout: None,
        };
        app.refresh_bindings();
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
        match display::capture_layout(name) {
            Ok(layout) => self.add_layout(layout, "Captured current Windows display topology"),
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
        match display::capture_layout(old_name) {
            Ok(mut layout) => {
                layout.hotkey = old_hotkey;
                layout.controller_hotkey = old_controller_hotkey;
                self.config.layouts[index] = layout;
                self.dirty = true;
                self.status =
                    AppStatus::info("Replaced this layout with the current desktop topology");
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

        // Applying is staged, so capture a rollback point before changing topology.
        let previous = match display::capture_layout("Previous topology") {
            Ok(layout) => layout,
            Err(err) => {
                self.status = AppStatus::error(format!(
                    "Apply cancelled because the current topology could not be captured for rollback: {err:#}"
                ));
                return;
            }
        };

        let layout = self.config.layouts[index].clone();
        let name = layout.name.clone();
        match display::apply_layout(&layout) {
            Ok(()) => {
                self.undo_layout = Some(previous);
                self.remember_working_layout(&layout);
                self.status = AppStatus::info(format!("Applied '{name}'"));
            }
            Err(apply_err) => match display::apply_layout(&previous) {
                Ok(()) => {
                    self.remember_working_layout(&previous);
                    self.status = AppStatus::error(format!(
                        "Could not apply '{name}': {apply_err:#}. The previous topology was restored."
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

        match display::apply_layout(&previous) {
            Ok(()) => {
                self.remember_working_layout(&previous);
                self.undo_layout = None;
                self.status =
                    AppStatus::info("Restored the topology from before the last successful apply");
            }
            Err(err) => {
                self.status =
                    AppStatus::error(format!("Could not restore the previous topology: {err:#}"));
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
            .resizable(true)
            .default_size(245.0)
            .show_inside(root_ui, |ui| {
                ui.heading("Layouts");
                ui.add_space(6.0);

                if ui.button("＋ Capture current layout").clicked() {
                    self.capture_new();
                }
                if ui.button("＋ New custom layout").clicked() {
                    self.create_custom();
                }

                ui.separator();
                egui::ScrollArea::vertical().show(ui, |ui| {
                    for (index, layout) in self.config.layouts.iter().enumerate() {
                        let hotkey = layout
                            .hotkey
                            .map(|h| format!("  [{}]", h.label()))
                            .unwrap_or_default();
                        let controller = layout
                            .controller_hotkey
                            .as_ref()
                            .map(|_| "  [controller]")
                            .unwrap_or_default();
                        if ui
                            .selectable_label(
                                self.selected == Some(index),
                                format!("{}{}{}", layout.name, hotkey, controller),
                            )
                            .clicked()
                        {
                            self.selected = Some(index);
                        }
                    }
                });

                ui.with_layout(egui::Layout::bottom_up(egui::Align::LEFT), |ui| {
                    if ui.button("Quit MonMan").clicked() {
                        self.request_exit(ui.ctx());
                    }
                    if ui
                        .add_enabled(self.tray.is_some(), egui::Button::new("Hide to tray"))
                        .on_hover_text(
                            "Hide the window while keeping keyboard and controller hotkeys active",
                        )
                        .clicked()
                    {
                        self.hide_to_tray(ui.ctx());
                    }
                    ui.separator();
                    if ui.button("Save now").clicked() {
                        self.save_now();
                    }
                    ui.small("Changes are also saved automatically.");
                    ui.add_space(4.0);
                    if ui
                        .add_enabled(self.undo_layout.is_some(), egui::Button::new("Undo last apply"))
                        .on_hover_text("Restore the Windows topology captured immediately before the last successful apply")
                        .clicked()
                    {
                        self.undo_last_apply();
                    }
                    ui.separator();
                    ui.small(format!("MonMan v{}", env!("CARGO_PKG_VERSION")));
                    if let Some(update) = self.available_update.clone()
                        && ui
                            .add_enabled(
                                !self.update_in_progress,
                                egui::Button::new(if self.update_in_progress {
                                    format!("Installing {}…", update.tag)
                                } else {
                                    format!("Update to {}", update.tag)
                                }),
                            )
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

    fn editor(&mut self, root_ui: &mut egui::Ui) {
        egui::CentralPanel::default().show_inside(root_ui, |ui| {
            let Some(index) = self.selected else {
                ui.vertical_centered(|ui| {
                    ui.add_space(100.0);
                    ui.heading("No layout selected");
                    ui.label("Capture the current Windows display topology to get started.");
                });
                return;
            };

            if index >= self.config.layouts.len() {
                self.selected = None;
                return;
            }

            let capture_active = self.controller_capture_layout == Some(index);
            let capture_elsewhere = self.controller_capture_layout.is_some() && !capture_active;
            let capture_status = self.controller_capture_status.clone();
            let connected_controllers = self
                .controller_devices
                .iter()
                .map(ControllerDeviceInfo::label)
                .collect::<Vec<_>>();

            let (layout_action, hotkeys_changed, controller_edit, layout_changed) = {
                let layout = &mut self.config.layouts[index];
                let (name_changed, layout_action) = layout_toolbar(ui, layout);

                ui.add_space(10.0);
                let hotkeys_changed = global_hotkey_editor(ui, layout, index);

                ui.add_space(8.0);
                let controller_edit = controller_hotkey_editor(
                    ui,
                    layout,
                    capture_active,
                    capture_elsewhere,
                    &capture_status,
                    &connected_controllers,
                );
                let monitor_changed = monitor_editor(ui, layout, index);
                let layout_changed = name_changed
                    || hotkeys_changed
                    || monitor_changed
                    || controller_edit == Some(ControllerEdit::Clear);

                (
                    layout_action,
                    hotkeys_changed,
                    controller_edit,
                    layout_changed,
                )
            };

            self.dirty |= layout_changed;
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

            match layout_action {
                Some(LayoutAction::Delete) => {
                    self.delete_layout(index);
                }
                Some(LayoutAction::Duplicate) => {
                    self.duplicate_selected();
                }
                Some(LayoutAction::Recapture) => self.recapture_selected(),
                Some(LayoutAction::SyncMonitors) => self.merge_connected_monitors(),
                Some(LayoutAction::Apply) => self.apply_index(index),
                None => {}
            }
        });
    }
    fn status_bar(&mut self, root_ui: &mut egui::Ui) {
        egui::Panel::bottom("status_bar").show_inside(root_ui, |ui| {
            ui.horizontal(|ui| {
                let color = if self.status.is_error {
                    ui.visuals().error_fg_color
                } else {
                    ui.visuals().text_color()
                };
                ui.colored_label(color, &self.status.message);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(
                        egui::RichText::new(format!(
                            "Config: {}",
                            storage::config_path().display()
                        ))
                        .small(),
                    );
                });
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

fn layout_toolbar(ui: &mut egui::Ui, layout: &mut MonitorLayout) -> (bool, Option<LayoutAction>) {
    let mut action = None;
    let name_changed = ui
        .horizontal_wrapped(|ui| {
            ui.label("Name");
            let changed = ui.text_edit_singleline(&mut layout.name).changed();
            for (label, candidate) in [
                ("Apply", LayoutAction::Apply),
                ("Capture current into this", LayoutAction::Recapture),
                ("Sync connected monitors", LayoutAction::SyncMonitors),
                ("Duplicate", LayoutAction::Duplicate),
                ("Delete", LayoutAction::Delete),
            ] {
                if ui.button(label).clicked() {
                    action = Some(candidate);
                }
            }
            changed
        })
        .inner;
    (name_changed, action)
}

fn global_hotkey_editor(ui: &mut egui::Ui, layout: &mut MonitorLayout, index: usize) -> bool {
    let mut changed = false;
    ui.group(|ui| {
        ui.horizontal(|ui| {
            ui.strong("Global hotkey");
            let mut enabled = layout.hotkey.is_some();
            if ui.checkbox(&mut enabled, "Enabled").changed() {
                layout.hotkey = enabled.then_some(HotkeyBinding::default());
                changed = true;
            }
        });

        let Some(binding) = layout.hotkey.as_mut() else {
            return;
        };
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
            ui.label(format!("→ {}", binding.label()));
        });
        if !binding.has_modifier() {
            ui.colored_label(
                ui.visuals().error_fg_color,
                "Choose at least one modifier for a global hotkey.",
            );
        }
    });
    changed
}

fn controller_hotkey_editor(
    ui: &mut egui::Ui,
    layout: &mut MonitorLayout,
    capture_active: bool,
    capture_elsewhere: bool,
    capture_status: &str,
    connected_controllers: &[String],
) -> Option<ControllerEdit> {
    let mut action = None;
    ui.group(|ui| {
        ui.strong("Controller hotkey");
        match layout.controller_hotkey.as_ref() {
            Some(binding) => ui.label(binding.label()),
            None => ui.label("No controller chord assigned to this layout."),
        };

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
        ui.small("Controller hotkeys keep working while MonMan is hidden in the system tray.");
    });
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

fn monitor_editor(ui: &mut egui::Ui, layout: &mut MonitorLayout, index: usize) -> bool {
    let mut changed = false;
    ui.add_space(10.0);
    let enabled_count = layout
        .monitors
        .iter()
        .filter(|monitor| monitor.enabled)
        .count();
    ui.horizontal(|ui| {
        ui.heading("Monitor layout");
        ui.label(format!(
            "{} enabled / {} known",
            enabled_count,
            layout.monitors.len()
        ));
    });
    ui.label(
        "Drag monitors in the preview to edit their desktop coordinates. Enabled monitors snap to adjacent edges and matching top, middle, or bottom axes; the active snap is highlighted.",
    );
    if enabled_count == 0 {
        ui.colored_label(
            ui.visuals().error_fg_color,
            "This layout cannot be applied until at least one monitor is enabled.",
        );
    }

    changed |= monitor_layout_canvas(ui, layout, index);

    let mut make_primary = None;
    let mut geometry_update = None;
    ui.add_space(8.0);
    egui::ScrollArea::both()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            egui::Grid::new(("monitor_grid", index))
                .striped(true)
                .num_columns(11)
                .spacing([12.0, 8.0])
                .show(ui, |ui| {
                    for heading in [
                        "On",
                        "Monitor",
                        "X",
                        "Y",
                        "Width",
                        "Height",
                        "Orientation",
                        "Hz",
                        "Primary",
                        "Clone",
                        "Identity",
                    ] {
                        ui.strong(heading);
                    }
                    ui.end_row();

                    for (monitor_index, monitor) in layout.monitors.iter_mut().enumerate() {
                        changed |= ui.checkbox(&mut monitor.enabled, "").changed();
                        ui.label(&monitor.friendly_name);

                        let mut geometry_changed = false;
                        geometry_changed |= ui
                            .add(egui::DragValue::new(&mut monitor.x).speed(10))
                            .changed();
                        geometry_changed |= ui
                            .add(egui::DragValue::new(&mut monitor.y).speed(10))
                            .changed();
                        geometry_changed |= ui
                            .add(
                                egui::DragValue::new(&mut monitor.width)
                                    .range(320..=16384)
                                    .speed(10),
                            )
                            .changed();
                        geometry_changed |= ui
                            .add(
                                egui::DragValue::new(&mut monitor.height)
                                    .range(200..=16384)
                                    .speed(10),
                            )
                            .changed();

                        let old_rotation = monitor.rotation.unwrap_or(0);
                        let mut rotation = old_rotation;
                        egui::ComboBox::from_id_salt(("monitor_rotation", index, monitor_index))
                            .selected_text(rotation_label(rotation))
                            .show_ui(ui, |ui| {
                                for (value, label) in DISPLAY_ROTATIONS {
                                    ui.selectable_value(&mut rotation, value, label);
                                }
                            });
                        if rotation != old_rotation {
                            monitor.rotation = Some(rotation);
                            changed = true;
                        }
                        if geometry_changed {
                            changed = true;
                            geometry_update = Some(GeometryUpdate {
                                source_index: monitor_index,
                                clone_group: monitor.clone_group,
                                x: monitor.x,
                                y: monitor.y,
                                width: monitor.width,
                                height: monitor.height,
                            });
                        }

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

                        if monitor.enabled && monitor.x == 0 && monitor.y == 0 {
                            ui.strong("Primary");
                        } else if ui
                            .add_enabled(monitor.enabled, egui::Button::new("Make primary"))
                            .clicked()
                        {
                            make_primary = Some(monitor_index);
                        }

                        ui.label(
                            monitor
                                .clone_group
                                .map(|group| format!("#{group}"))
                                .unwrap_or_else(|| "—".to_string()),
                        );
                        ui.label(egui::RichText::new(monitor.identity.display_label()).small())
                            .on_hover_text(&monitor.identity.device_path);
                        ui.end_row();
                    }
                });
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

impl eframe::App for MonManApp {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
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
        self.sidebar(ui);
        self.status_bar(ui);
        self.editor(ui);
    }

    fn on_exit(&mut self) {
        if self.dirty {
            let _ = storage::save(&self.config);
        }
    }
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
    let (response, painter) = ui.allocate_painter(egui::vec2(width, 285.0), egui::Sense::hover());
    let outer = response.rect;
    painter.rect_filled(outer, 6.0, ui.visuals().extreme_bg_color);

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

    // Desktop origin marks the primary source.
    let origin = to_screen(0.0, 0.0);
    let origin_stroke = egui::Stroke::new(1.0_f32, ui.visuals().weak_text_color());
    if canvas.left() <= origin.x && origin.x <= canvas.right() {
        painter.line_segment(
            [
                egui::pos2(origin.x, canvas.top()),
                egui::pos2(origin.x, canvas.bottom()),
            ],
            origin_stroke,
        );
    }
    if canvas.top() <= origin.y && origin.y <= canvas.bottom() {
        painter.line_segment(
            [
                egui::pos2(canvas.left(), origin.y),
                egui::pos2(canvas.right(), origin.y),
            ],
            origin_stroke,
        );
    }

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
        let is_primary = enabled && monitor.x == 0 && monitor.y == 0;
        let name = monitor.friendly_name.clone();
        let dimensions = format!("{display_width}×{display_height}");

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
            ui.visuals().widgets.inactive.bg_fill
        } else {
            ui.visuals().widgets.noninteractive.bg_fill
        };
        let border = if interaction.dragged() || interaction.hovered() {
            ui.visuals().widgets.active.fg_stroke
        } else {
            ui.visuals().widgets.inactive.fg_stroke
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

        let label = if is_primary {
            format!("{name}\n{dimensions}\nPRIMARY")
        } else if enabled {
            format!("{name}\n{dimensions}")
        } else {
            format!("{name}\n{dimensions}\nOFF")
        };
        painter.text(
            monitor_rect.center(),
            egui::Align2::CENTER_CENTER,
            label,
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
