use crate::model::HotkeyBinding;
use std::sync::mpsc::{self, Receiver, Sender};

#[derive(Debug, Clone)]
pub struct HotkeySpec {
    pub layout_index: usize,
    pub binding: HotkeyBinding,
}

#[derive(Debug, Clone)]
pub struct HotkeyRegistrationFailure {
    pub layout_index: usize,
    pub binding: HotkeyBinding,
    pub reason: String,
}

#[derive(Debug, Clone)]
pub enum HotkeyEvent {
    Triggered(usize),
    RegistrationFinished(Vec<HotkeyRegistrationFailure>),
}

pub struct HotkeyManager {
    #[cfg(windows)]
    command_tx: Sender<Command>,
    #[cfg(windows)]
    thread_id: u32,
    event_rx: Receiver<HotkeyEvent>,
}

#[cfg(windows)]
#[derive(Debug)]
enum Command {
    Replace(Vec<HotkeySpec>),
    Stop,
}

impl HotkeyManager {
    pub fn new(ctx: eframe::egui::Context) -> Self {
        #[cfg(windows)]
        {
            windows_impl::start(ctx)
        }

        #[cfg(not(windows))]
        {
            let (_tx, rx) = mpsc::channel();
            let _ = ctx;
            Self { event_rx: rx }
        }
    }

    pub fn replace(&self, specs: Vec<HotkeySpec>) -> anyhow::Result<()> {
        #[cfg(windows)]
        {
            self.command_tx.send(Command::Replace(specs))?;
            windows_impl::wake(self.thread_id)?;
            Ok(())
        }

        #[cfg(not(windows))]
        {
            let _ = specs;
            Ok(())
        }
    }

    pub fn try_recv(&self) -> Option<HotkeyEvent> {
        self.event_rx.try_recv().ok()
    }
}

impl Drop for HotkeyManager {
    fn drop(&mut self) {
        #[cfg(windows)]
        {
            let _ = self.command_tx.send(Command::Stop);
            let _ = windows_impl::wake(self.thread_id);
        }
    }
}

#[cfg(windows)]
mod windows_impl {
    use super::*;
    use anyhow::{Result, anyhow};
    use windows::Win32::Foundation::{LPARAM, WPARAM};
    use windows::Win32::System::Threading::GetCurrentThreadId;
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        HOT_KEY_MODIFIERS, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT, MOD_WIN, RegisterHotKey,
        UnregisterHotKey,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        GetMessageW, MSG, PostThreadMessageW, WM_APP, WM_HOTKEY,
    };

    const WM_MONMAN_COMMAND: u32 = WM_APP + 0x41;

    pub(super) fn start(ctx: eframe::egui::Context) -> HotkeyManager {
        let (command_tx, command_rx) = mpsc::channel::<Command>();
        let (event_tx, event_rx) = mpsc::channel::<HotkeyEvent>();
        let (thread_id_tx, thread_id_rx) = mpsc::sync_channel::<u32>(1);

        std::thread::Builder::new()
            .name("monman-hotkeys".into())
            .spawn(move || hotkey_thread(ctx, command_rx, event_tx, thread_id_tx))
            .expect("failed to start hotkey thread");

        let thread_id = thread_id_rx
            .recv()
            .expect("hotkey thread exited during startup");
        HotkeyManager {
            command_tx,
            thread_id,
            event_rx,
        }
    }

    pub(super) fn wake(thread_id: u32) -> Result<()> {
        unsafe { PostThreadMessageW(thread_id, WM_MONMAN_COMMAND, WPARAM(0), LPARAM(0)) }
            .map_err(|e| anyhow!("failed to wake hotkey thread: {e}"))
    }

    fn hotkey_thread(
        ctx: eframe::egui::Context,
        command_rx: Receiver<Command>,
        event_tx: Sender<HotkeyEvent>,
        thread_id_tx: mpsc::SyncSender<u32>,
    ) {
        let thread_id = unsafe { GetCurrentThreadId() };

        // Create the message queue before publishing the thread ID.
        let mut bootstrap = MSG::default();
        unsafe {
            let _ = windows::Win32::UI::WindowsAndMessaging::PeekMessageW(
                &mut bootstrap,
                None,
                0,
                0,
                windows::Win32::UI::WindowsAndMessaging::PM_NOREMOVE,
            );
        }
        let _ = thread_id_tx.send(thread_id);

        let mut registered_ids = Vec::<i32>::new();
        let mut id_to_layout = std::collections::HashMap::<i32, usize>::new();
        let mut msg = MSG::default();

        loop {
            let got = unsafe { GetMessageW(&mut msg, None, 0, 0) };
            if got.0 <= 0 {
                break;
            }

            if msg.message == WM_MONMAN_COMMAND {
                // Register only the newest edit to avoid transient global conflicts.
                let mut newest_specs = None::<Vec<HotkeySpec>>;
                while let Ok(command) = command_rx.try_recv() {
                    match command {
                        Command::Replace(specs) => newest_specs = Some(specs),
                        Command::Stop => {
                            unregister_all(&mut registered_ids);
                            return;
                        }
                    }
                }

                if let Some(specs) = newest_specs {
                    unregister_all(&mut registered_ids);
                    id_to_layout.clear();

                    let mut failures = Vec::new();
                    for (slot, spec) in specs.into_iter().enumerate() {
                        if !spec.binding.has_modifier() {
                            failures.push(HotkeyRegistrationFailure {
                                layout_index: spec.layout_index,
                                binding: spec.binding,
                                reason: "choose at least one modifier (Ctrl/Alt/Shift/Win)"
                                    .to_string(),
                            });
                            continue;
                        }

                        let id = 0x4D00 + slot as i32;
                        let modifiers = modifiers(spec.binding);
                        match unsafe { RegisterHotKey(None, id, modifiers, spec.binding.key.vk()) }
                        {
                            Ok(()) => {
                                registered_ids.push(id);
                                id_to_layout.insert(id, spec.layout_index);
                            }
                            Err(err) => failures.push(HotkeyRegistrationFailure {
                                layout_index: spec.layout_index,
                                binding: spec.binding,
                                reason: err.to_string(),
                            }),
                        }
                    }

                    let _ = event_tx.send(HotkeyEvent::RegistrationFinished(failures));
                    ctx.request_repaint();
                }
                continue;
            }

            if msg.message == WM_HOTKEY {
                let id = msg.wParam.0 as i32;
                if let Some(&layout_index) = id_to_layout.get(&id) {
                    let _ = event_tx.send(HotkeyEvent::Triggered(layout_index));
                    ctx.request_repaint();
                }
            }
        }

        unregister_all(&mut registered_ids);
    }

    fn unregister_all(ids: &mut Vec<i32>) {
        for id in ids.drain(..) {
            unsafe {
                let _ = UnregisterHotKey(None, id);
            }
        }
    }

    fn modifiers(binding: HotkeyBinding) -> HOT_KEY_MODIFIERS {
        let mut bits = MOD_NOREPEAT.0;
        if binding.ctrl {
            bits |= MOD_CONTROL.0;
        }
        if binding.alt {
            bits |= MOD_ALT.0;
        }
        if binding.shift {
            bits |= MOD_SHIFT.0;
        }
        if binding.win {
            bits |= MOD_WIN.0;
        }
        HOT_KEY_MODIFIERS(bits)
    }
}
