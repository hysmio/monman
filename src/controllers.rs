use crate::model::ControllerBinding;
use std::sync::mpsc::{self, Receiver, Sender};

#[derive(Debug, Clone)]
pub struct ControllerSpec {
    pub layout_index: usize,
    pub binding: ControllerBinding,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControllerDeviceInfo {
    pub display_name: String,
    pub vendor_id: u16,
    pub product_id: u16,
    pub wireless: bool,
}

impl ControllerDeviceInfo {
    pub fn label(&self) -> String {
        let connection = if self.wireless { "Bluetooth" } else { "USB" };
        format!(
            "{} ({:04X}:{:04X}, {connection})",
            self.display_name, self.vendor_id, self.product_id
        )
    }
}

#[derive(Debug, Clone)]
pub enum ControllerEvent {
    Triggered(usize),
    Captured {
        layout_index: usize,
        binding: ControllerBinding,
    },
    CaptureProgress(String),
    CaptureCancelled(String),
    DevicesChanged(Vec<ControllerDeviceInfo>),
    Error(String),
}

#[derive(Debug)]
enum Command {
    Replace(Vec<ControllerSpec>),
    BeginCapture(usize),
    CancelCapture,
    Stop,
}

pub struct ControllerManager {
    command_tx: Sender<Command>,
    event_rx: Receiver<ControllerEvent>,
}

impl ControllerManager {
    pub fn new(ctx: eframe::egui::Context) -> Self {
        let (command_tx, command_rx) = mpsc::channel();
        let (event_tx, event_rx) = mpsc::channel();

        #[cfg(windows)]
        std::thread::Builder::new()
            .name("monman-controllers".into())
            .spawn(move || windows_impl::controller_thread(ctx, command_rx, event_tx))
            .expect("failed to start controller thread");

        #[cfg(not(windows))]
        {
            let _ = ctx;
            std::thread::Builder::new()
                .name("monman-controllers".into())
                .spawn(move || {
                    while let Ok(command) = command_rx.recv() {
                        match command {
                            Command::BeginCapture(_) => {
                                let _ = event_tx.send(ControllerEvent::Error(
                                    "Controller hotkeys are only supported on Windows".into(),
                                ));
                            }
                            Command::Stop => break,
                            Command::Replace(_) | Command::CancelCapture => {}
                        }
                    }
                })
                .expect("failed to start controller stub thread");
        }

        Self {
            command_tx,
            event_rx,
        }
    }

    pub fn replace(&self, specs: Vec<ControllerSpec>) -> anyhow::Result<()> {
        self.send(Command::Replace(specs))
    }

    pub fn begin_capture(&self, layout_index: usize) -> anyhow::Result<()> {
        self.send(Command::BeginCapture(layout_index))
    }

    pub fn cancel_capture(&self) -> anyhow::Result<()> {
        self.send(Command::CancelCapture)
    }

    fn send(&self, command: Command) -> anyhow::Result<()> {
        self.command_tx.send(command)?;
        Ok(())
    }

    pub fn try_recv(&self) -> Option<ControllerEvent> {
        self.event_rx.try_recv().ok()
    }
}

impl Drop for ControllerManager {
    fn drop(&mut self) {
        let _ = self.command_tx.send(Command::Stop);
    }
}

#[cfg(windows)]
mod windows_impl {
    use super::*;
    use anyhow::{Context, Result, anyhow};
    use hidapi::{BusType, HidApi, HidDevice};
    use std::collections::{BTreeSet, HashMap};
    use std::time::{Duration, Instant};

    const SONY_VENDOR_ID: u16 = 0x054c;
    const DUALSENSE_PRODUCT_ID: u16 = 0x0ce6;
    const DUALSENSE_EDGE_PRODUCT_ID: u16 = 0x0df2;
    const POLL_INTERVAL: Duration = Duration::from_millis(16);
    const DEVICE_REFRESH_INTERVAL: Duration = Duration::from_secs(1);
    const CAPTURE_TIMEOUT: Duration = Duration::from_secs(15);

    // Persisted button IDs; keep these values and names in order.
    const CROSS: u32 = 0;
    const CIRCLE: u32 = 1;
    const SQUARE: u32 = 2;
    const TRIANGLE: u32 = 3;
    const DPAD_UP: u32 = 4;
    const DPAD_DOWN: u32 = 5;
    const DPAD_LEFT: u32 = 6;
    const DPAD_RIGHT: u32 = 7;
    const L1: u32 = 8;
    const R1: u32 = 9;
    const L2: u32 = 10;
    const R2: u32 = 11;
    const CREATE: u32 = 12;
    const OPTIONS: u32 = 13;
    const L3: u32 = 14;
    const R3: u32 = 15;
    const PS: u32 = 16;
    const TOUCHPAD: u32 = 17;
    const MUTE: u32 = 18;
    const BUTTON_NAMES: [&str; 19] = [
        "Cross",
        "Circle",
        "Square",
        "Triangle",
        "D-pad Up",
        "D-pad Down",
        "D-pad Left",
        "D-pad Right",
        "L1",
        "R1",
        "L2",
        "R2",
        "Create",
        "Options",
        "L3",
        "R3",
        "PS",
        "Touchpad",
        "Mute",
    ];

    struct Device {
        id: String,
        info: ControllerDeviceInfo,
        device: HidDevice,
        pressed: BTreeSet<u32>,
    }

    enum CaptureState {
        AwaitingRelease {
            layout_index: usize,
            deadline: Instant,
            armed: bool,
        },
        Collecting {
            layout_index: usize,
            deadline: Instant,
            device_id: String,
            buttons: BTreeSet<u32>,
        },
    }

    impl CaptureState {
        fn deadline(&self) -> Instant {
            match self {
                Self::AwaitingRelease { deadline, .. } | Self::Collecting { deadline, .. } => {
                    *deadline
                }
            }
        }
    }

    pub(super) fn controller_thread(
        ctx: eframe::egui::Context,
        command_rx: Receiver<Command>,
        event_tx: Sender<ControllerEvent>,
    ) {
        let mut specs = Vec::<ControllerSpec>::new();
        let mut active = HashMap::<usize, bool>::new();
        let mut capture = None::<CaptureState>;
        let mut devices = Vec::<Device>::new();
        let mut published_devices = None::<Vec<ControllerDeviceInfo>>;
        let mut refresh_at = Instant::now();
        let mut last_refresh_error = None::<String>;

        loop {
            match command_rx.recv_timeout(POLL_INTERVAL) {
                Ok(command) => {
                    if handle_command(
                        command,
                        &mut specs,
                        &mut active,
                        &mut capture,
                        &ctx,
                        &event_tx,
                    ) {
                        break;
                    }
                    while let Ok(command) = command_rx.try_recv() {
                        if handle_command(
                            command,
                            &mut specs,
                            &mut active,
                            &mut capture,
                            &ctx,
                            &event_tx,
                        ) {
                            return;
                        }
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }

            if Instant::now() >= refresh_at {
                match refresh_devices(&mut devices) {
                    Ok(()) => last_refresh_error = None,
                    Err(error) => {
                        let message =
                            format!("Could not open DualSense controller input: {error:#}");
                        if last_refresh_error.as_ref() != Some(&message) {
                            send(&ctx, &event_tx, ControllerEvent::Error(message.clone()));
                            last_refresh_error = Some(message);
                        }
                    }
                }
                let infos = devices.iter().map(|device| device.info.clone()).collect();
                if published_devices.as_ref() != Some(&infos) {
                    published_devices = Some(infos.clone());
                    send(&ctx, &event_tx, ControllerEvent::DevicesChanged(infos));
                }
                refresh_at = Instant::now() + DEVICE_REFRESH_INTERVAL;
            }

            let mut read_failed = None::<String>;
            for device in &mut devices {
                if let Err(error) = poll_device(device) {
                    read_failed = Some(format!(
                        "Lost controller input from {}: {error}",
                        device.info.display_name
                    ));
                    break;
                }
            }
            if let Some(error) = read_failed {
                devices.clear();
                active.clear();
                refresh_at = Instant::now();
                send(&ctx, &event_tx, ControllerEvent::Error(error));
                continue;
            }

            if capture.is_some() {
                update_capture(&mut capture, &devices, &ctx, &event_tx);
            } else {
                update_hotkeys(&specs, &mut active, &devices, &ctx, &event_tx);
            }
        }
    }

    fn handle_command(
        command: Command,
        specs: &mut Vec<ControllerSpec>,
        active: &mut HashMap<usize, bool>,
        capture: &mut Option<CaptureState>,
        ctx: &eframe::egui::Context,
        event_tx: &Sender<ControllerEvent>,
    ) -> bool {
        match command {
            Command::Replace(new_specs) => {
                *specs = new_specs
                    .into_iter()
                    .filter(|spec| spec.binding.is_valid())
                    .collect();
                active.clear();
            }
            Command::BeginCapture(layout_index) => {
                *capture = Some(CaptureState::AwaitingRelease {
                    layout_index,
                    deadline: Instant::now() + CAPTURE_TIMEOUT,
                    armed: false,
                });
                send(
                    ctx,
                    event_tx,
                    ControllerEvent::CaptureProgress(
                        "Release all controller buttons, then press the desired chord".into(),
                    ),
                );
            }
            Command::CancelCapture => {
                if capture.take().is_some() {
                    send(
                        ctx,
                        event_tx,
                        ControllerEvent::CaptureCancelled("Controller binding cancelled".into()),
                    );
                }
            }
            Command::Stop => return true,
        }
        false
    }

    fn refresh_devices(devices: &mut Vec<Device>) -> Result<()> {
        let mut existing = std::mem::take(devices)
            .into_iter()
            .map(|device| (device.id.clone(), device))
            .collect::<HashMap<_, _>>();
        let api = HidApi::new().context("initializing Windows HID")?;
        let mut refreshed = Vec::new();
        let mut matching_devices = 0usize;
        let mut open_errors = Vec::new();

        for device_info in api.device_list().filter(|info| is_dualsense(info)) {
            matching_devices += 1;
            let id = device_info.path().to_string_lossy().into_owned();
            let info = ControllerDeviceInfo {
                display_name: device_info
                    .product_string()
                    .unwrap_or("DualSense controller")
                    .to_string(),
                vendor_id: device_info.vendor_id(),
                product_id: device_info.product_id(),
                wireless: matches!(device_info.bus_type(), BusType::Bluetooth),
            };

            if let Some(mut device) = existing.remove(&id) {
                device.info = info;
                refreshed.push(device);
                continue;
            }

            match api.open_path(device_info.path()) {
                Ok(device) => refreshed.push(Device {
                    id,
                    info,
                    device,
                    pressed: BTreeSet::new(),
                }),
                Err(error) => open_errors.push(error.to_string()),
            }
        }

        *devices = refreshed;
        if devices.is_empty() && matching_devices > 0 {
            Err(anyhow!(open_errors.join("; ")))
        } else {
            Ok(())
        }
    }

    fn is_dualsense(info: &hidapi::DeviceInfo) -> bool {
        info.vendor_id() == SONY_VENDOR_ID
            && info.usage_page() == 0x01
            && info.usage() == 0x05
            && (matches!(
                info.product_id(),
                DUALSENSE_PRODUCT_ID | DUALSENSE_EDGE_PRODUCT_ID
            ) || info
                .product_string()
                .is_some_and(|name| name.to_ascii_lowercase().contains("dualsense")))
    }

    fn poll_device(device: &mut Device) -> Result<()> {
        let mut report = [0_u8; 128];
        loop {
            let size = device.device.read_timeout(&mut report, 0)?;
            if size == 0 {
                return Ok(());
            }
            if let Some(data) = extract_report(&report[..size]) {
                device.pressed = parse_buttons(data);
            }
        }
    }

    // Strip the USB/Bluetooth headers to expose their shared button payload.
    fn extract_report(report: &[u8]) -> Option<&[u8]> {
        match report.len() {
            63 => Some(report),
            64 if report[0] == 0x01 => Some(&report[1..]),
            64 => Some(report),
            77 => Some(&report[2..]),
            78 if report[0] == 0x01 => Some(&report[3..]),
            78 => Some(&report[2..]),
            _ => None,
        }
    }

    fn parse_buttons(report: &[u8]) -> BTreeSet<u32> {
        let mut pressed = BTreeSet::new();
        if report.len() < 10 {
            return pressed;
        }
        let button0 = report[7];
        let button1 = report[8];
        let button2 = report[9];

        for (bit, button) in [(4, SQUARE), (5, CROSS), (6, CIRCLE), (7, TRIANGLE)] {
            add_if(&mut pressed, button0 & (1 << bit) != 0, button);
        }
        match button0 & 0x0f {
            0 => pressed.extend([DPAD_UP]),
            1 => pressed.extend([DPAD_UP, DPAD_RIGHT]),
            2 => pressed.extend([DPAD_RIGHT]),
            3 => pressed.extend([DPAD_DOWN, DPAD_RIGHT]),
            4 => pressed.extend([DPAD_DOWN]),
            5 => pressed.extend([DPAD_DOWN, DPAD_LEFT]),
            6 => pressed.extend([DPAD_LEFT]),
            7 => pressed.extend([DPAD_UP, DPAD_LEFT]),
            _ => {}
        }

        for (bit, button) in [
            (0, L1),
            (1, R1),
            (2, L2),
            (3, R2),
            (4, CREATE),
            (5, OPTIONS),
            (6, L3),
            (7, R3),
        ] {
            add_if(&mut pressed, button1 & (1 << bit) != 0, button);
        }
        for (bit, button) in [(0, PS), (1, TOUCHPAD), (2, MUTE)] {
            add_if(&mut pressed, button2 & (1 << bit) != 0, button);
        }
        pressed
    }

    fn add_if(buttons: &mut BTreeSet<u32>, condition: bool, button: u32) {
        if condition {
            buttons.insert(button);
        }
    }

    fn update_capture(
        capture: &mut Option<CaptureState>,
        devices: &[Device],
        ctx: &eframe::egui::Context,
        event_tx: &Sender<ControllerEvent>,
    ) {
        let Some(state) = capture.take() else { return };
        if Instant::now() >= state.deadline() {
            send(
                ctx,
                event_tx,
                ControllerEvent::CaptureCancelled(
                    "Controller binding timed out after 15 seconds".into(),
                ),
            );
            return;
        }

        match state {
            CaptureState::AwaitingRelease {
                layout_index,
                deadline,
                mut armed,
            } => {
                if devices.iter().all(|device| device.pressed.is_empty()) {
                    armed = true;
                }
                if armed
                    && let Some(device) = devices.iter().find(|device| !device.pressed.is_empty())
                {
                    send(
                        ctx,
                        event_tx,
                        ControllerEvent::CaptureProgress(format!(
                            "Recording {} — release the chord to save it",
                            buttons_label(&device.pressed)
                        )),
                    );
                    *capture = Some(CaptureState::Collecting {
                        layout_index,
                        deadline,
                        device_id: device.id.clone(),
                        buttons: device.pressed.clone(),
                    });
                } else {
                    *capture = Some(CaptureState::AwaitingRelease {
                        layout_index,
                        deadline,
                        armed,
                    });
                }
            }
            CaptureState::Collecting {
                layout_index,
                deadline,
                device_id,
                mut buttons,
            } => {
                let Some(device) = devices.iter().find(|device| device.id == device_id) else {
                    send(
                        ctx,
                        event_tx,
                        ControllerEvent::CaptureCancelled(
                            "The controller disconnected while recording the binding".into(),
                        ),
                    );
                    return;
                };
                buttons.extend(&device.pressed);
                if device.pressed.is_empty() {
                    let button_vec = buttons.iter().copied().collect::<Vec<_>>();
                    let button_labels = button_vec
                        .iter()
                        .map(|button| button_name(*button).to_string())
                        .collect();
                    send(
                        ctx,
                        event_tx,
                        ControllerEvent::Captured {
                            layout_index,
                            binding: ControllerBinding {
                                vendor_id: device.info.vendor_id,
                                product_id: device.info.product_id,
                                device_name: device.info.display_name.clone(),
                                buttons: button_vec,
                                button_labels,
                            },
                        },
                    );
                } else {
                    *capture = Some(CaptureState::Collecting {
                        layout_index,
                        deadline,
                        device_id,
                        buttons,
                    });
                }
            }
        }
    }

    fn update_hotkeys(
        specs: &[ControllerSpec],
        active: &mut HashMap<usize, bool>,
        devices: &[Device],
        ctx: &eframe::egui::Context,
        event_tx: &Sender<ControllerEvent>,
    ) {
        let mut triggered = None;
        for spec in specs {
            let is_active = devices.iter().any(|device| {
                device.info.vendor_id == spec.binding.vendor_id
                    && device.info.product_id == spec.binding.product_id
                    && spec
                        .binding
                        .buttons
                        .iter()
                        .all(|button| device.pressed.contains(button))
            });
            let was_active = active.insert(spec.layout_index, is_active).unwrap_or(false);
            if is_active && !was_active && triggered.is_none() {
                triggered = Some(spec.layout_index);
            }
        }
        if let Some(layout_index) = triggered {
            send(ctx, event_tx, ControllerEvent::Triggered(layout_index));
        }
    }

    fn buttons_label(buttons: &BTreeSet<u32>) -> String {
        buttons
            .iter()
            .map(|button| button_name(*button))
            .collect::<Vec<_>>()
            .join(" + ")
    }

    fn button_name(button: u32) -> &'static str {
        BUTTON_NAMES
            .get(button as usize)
            .copied()
            .unwrap_or("Unknown button")
    }

    fn send(
        ctx: &eframe::egui::Context,
        event_tx: &Sender<ControllerEvent>,
        event: ControllerEvent,
    ) {
        let _ = event_tx.send(event);
        ctx.request_repaint();
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn parses_usb_dualsense_face_button_and_released_dpad() {
            let mut raw = [0_u8; 64];
            raw[0] = 0x01;
            raw[8] = 0x28;
            let parsed = parse_buttons(extract_report(&raw).unwrap());
            assert_eq!(parsed, BTreeSet::from([CROSS]));
        }

        #[test]
        fn parses_dpad_diagonal_as_a_chord() {
            let mut report = [0_u8; 63];
            report[7] = 0x01;
            assert_eq!(
                parse_buttons(&report),
                BTreeSet::from([DPAD_UP, DPAD_RIGHT])
            );
        }

        #[test]
        fn persisted_button_ids_still_match_their_names() {
            assert_eq!(button_name(CROSS), "Cross");
            assert_eq!(button_name(MUTE), "Mute");
            assert_eq!(button_name(MUTE + 1), "Unknown button");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn controller_binding_falls_back_to_one_based_button_names() {
        let binding = ControllerBinding {
            vendor_id: 0x054c,
            product_id: 0x0df2,
            device_name: "DualSense Edge".into(),
            buttons: vec![0, 12],
            button_labels: Vec::new(),
        };
        assert_eq!(binding.button_label(), "Button 1 + Button 13");
    }
}
