use std::sync::mpsc::{self, Receiver};

#[derive(Debug)]
pub enum TrayEvent {
    OpenRequested,
    ExitRequested,
    Error(String),
}

pub struct TrayManager {
    event_rx: Receiver<TrayEvent>,
    #[cfg(windows)]
    thread_id: u32,
    #[cfg(windows)]
    join_handle: Option<std::thread::JoinHandle<()>>,
}

impl TrayManager {
    pub fn new(ctx: eframe::egui::Context) -> anyhow::Result<Self> {
        #[cfg(windows)]
        {
            windows_impl::start(ctx)
        }

        #[cfg(not(windows))]
        {
            let _ = ctx;
            anyhow::bail!("the MonMan tray icon is only available on Windows")
        }
    }

    pub fn try_recv(&self) -> Option<TrayEvent> {
        self.event_rx.try_recv().ok()
    }
}

impl Drop for TrayManager {
    fn drop(&mut self) {
        #[cfg(windows)]
        {
            if windows_impl::stop(self.thread_id).is_ok()
                && let Some(handle) = self.join_handle.take()
            {
                let _ = handle.join();
            }
        }
    }
}

#[cfg(windows)]
mod windows_impl {
    use super::*;
    use anyhow::{Context, Result, anyhow, bail};
    use std::cell::RefCell;
    use std::mem::size_of;
    use std::sync::mpsc::{Sender, SyncSender};
    use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, WPARAM};
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::System::Threading::GetCurrentThreadId;
    use windows::Win32::UI::Shell::{
        NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW, Shell_NotifyIconW,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        AppendMenuW, CreateIconFromResourceEx, CreatePopupMenu, CreateWindowExW, DefWindowProcW,
        DestroyIcon, DestroyMenu, DestroyWindow, DispatchMessageW, GetCursorPos, GetMessageW,
        GetSystemMetrics, LR_DEFAULTCOLOR, MF_STRING, MSG, PostQuitMessage, PostThreadMessageW,
        RegisterClassW, SM_CXSMICON, SetForegroundWindow, TPM_RETURNCMD, TPM_RIGHTBUTTON,
        TrackPopupMenu, TranslateMessage, UnregisterClassW, WINDOW_EX_STYLE, WINDOW_STYLE, WM_APP,
        WM_DESTROY, WM_LBUTTONDBLCLK, WM_RBUTTONUP, WNDCLASSW,
    };
    use windows::core::{Error as WindowsError, PCWSTR, w};

    const TRAY_ICON_ID: u32 = 1;
    const WM_MONMAN_TRAY: u32 = WM_APP + 0x52;
    const WM_MONMAN_TRAY_STOP: u32 = WM_APP + 0x53;
    const MENU_OPEN: usize = 1;
    const MENU_QUIT: usize = 2;
    const CLASS_NAME: PCWSTR = w!("MonManTrayWindow");

    thread_local! {
        static CALLBACKS: RefCell<Option<(Sender<TrayEvent>, eframe::egui::Context)>> =
            const { RefCell::new(None) };
    }

    struct TrayResources {
        hwnd: HWND,
        instance: HINSTANCE,
        icon: windows::Win32::UI::WindowsAndMessaging::HICON,
        icon_data: NOTIFYICONDATAW,
    }

    pub(super) fn start(ctx: eframe::egui::Context) -> Result<TrayManager> {
        let (event_tx, event_rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let join_handle = std::thread::Builder::new()
            .name("monman-tray".into())
            .spawn(move || tray_thread(ctx, event_tx, ready_tx))
            .context("failed to start tray thread")?;

        let thread_id = ready_rx
            .recv()
            .context("tray thread exited during startup")?
            .map_err(|message| anyhow!(message))?;

        Ok(TrayManager {
            event_rx,
            thread_id,
            join_handle: Some(join_handle),
        })
    }

    pub(super) fn stop(thread_id: u32) -> windows::core::Result<()> {
        unsafe { PostThreadMessageW(thread_id, WM_MONMAN_TRAY_STOP, WPARAM(0), LPARAM(0)) }
    }

    fn tray_thread(
        ctx: eframe::egui::Context,
        event_tx: Sender<TrayEvent>,
        ready_tx: SyncSender<Result<u32, String>>,
    ) {
        CALLBACKS.with(|callbacks| {
            *callbacks.borrow_mut() = Some((event_tx, ctx));
        });

        let resources = match unsafe { create_tray_icon() } {
            Ok(resources) => resources,
            Err(error) => {
                let _ = ready_tx.send(Err(format!("{error:#}")));
                CALLBACKS.with(|callbacks| callbacks.borrow_mut().take());
                return;
            }
        };

        let thread_id = unsafe { GetCurrentThreadId() };
        if ready_tx.send(Ok(thread_id)).is_err() {
            unsafe { destroy_tray_icon(&resources) };
            CALLBACKS.with(|callbacks| callbacks.borrow_mut().take());
            return;
        }

        let mut message = MSG::default();
        loop {
            let result = unsafe { GetMessageW(&mut message, None, 0, 0) };
            if result.0 == -1 {
                emit(TrayEvent::Error(format!(
                    "tray message loop failed: {}",
                    WindowsError::from_thread()
                )));
                break;
            }
            if result.0 == 0 || message.message == WM_MONMAN_TRAY_STOP {
                break;
            }

            unsafe {
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }

        unsafe { destroy_tray_icon(&resources) };
        CALLBACKS.with(|callbacks| callbacks.borrow_mut().take());
    }

    unsafe fn create_tray_icon() -> Result<TrayResources> {
        let module = unsafe { GetModuleHandleW(None) }.context("GetModuleHandleW failed")?;
        let instance = HINSTANCE(module.0);
        let window_class = WNDCLASSW {
            lpfnWndProc: Some(window_proc),
            hInstance: instance,
            lpszClassName: CLASS_NAME,
            ..Default::default()
        };
        if unsafe { RegisterClassW(&window_class) } == 0 {
            bail!("RegisterClassW failed: {}", WindowsError::from_thread());
        }

        let hwnd = match unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                CLASS_NAME,
                w!("MonMan tray"),
                WINDOW_STYLE::default(),
                0,
                0,
                0,
                0,
                None,
                None,
                Some(instance),
                None,
            )
        } {
            Ok(hwnd) => hwnd,
            Err(error) => {
                let _ = unsafe { UnregisterClassW(CLASS_NAME, Some(instance)) };
                return Err(error).context("CreateWindowExW failed");
            }
        };

        let icon_size = unsafe { GetSystemMetrics(SM_CXSMICON) };
        let icon = unsafe {
            CreateIconFromResourceEx(
                include_bytes!("../assets/egui-icon.png"),
                true,
                0x0003_0000,
                icon_size,
                icon_size,
                LR_DEFAULTCOLOR,
            )
        }
        .context("could not create the tray icon from the bundled egui PNG")?;
        let mut icon_data = NOTIFYICONDATAW {
            cbSize: size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: hwnd,
            uID: TRAY_ICON_ID,
            uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP,
            uCallbackMessage: WM_MONMAN_TRAY,
            hIcon: icon,
            ..Default::default()
        };
        copy_wide("MonMan — monitor layout hotkeys", &mut icon_data.szTip);

        if !unsafe { Shell_NotifyIconW(NIM_ADD, &icon_data) }.as_bool() {
            let error = WindowsError::from_thread();
            let _ = unsafe { DestroyIcon(icon) };
            let _ = unsafe { DestroyWindow(hwnd) };
            let _ = unsafe { UnregisterClassW(CLASS_NAME, Some(instance)) };
            bail!("Shell_NotifyIconW(NIM_ADD) failed: {error}");
        }

        Ok(TrayResources {
            hwnd,
            instance,
            icon,
            icon_data,
        })
    }

    unsafe fn destroy_tray_icon(resources: &TrayResources) {
        let _ = unsafe { Shell_NotifyIconW(NIM_DELETE, &resources.icon_data) };
        let _ = unsafe { DestroyIcon(resources.icon) };
        let _ = unsafe { DestroyWindow(resources.hwnd) };
        let _ = unsafe { UnregisterClassW(CLASS_NAME, Some(resources.instance)) };
    }

    unsafe extern "system" fn window_proc(
        hwnd: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        if message == WM_MONMAN_TRAY {
            match lparam.0 as u32 {
                WM_LBUTTONDBLCLK => emit(TrayEvent::OpenRequested),
                WM_RBUTTONUP => {
                    if let Err(error) = unsafe { show_context_menu(hwnd) } {
                        emit(TrayEvent::Error(format!(
                            "could not show tray menu: {error:#}"
                        )));
                    }
                }
                _ => {}
            }
            return LRESULT(0);
        }

        if message == WM_DESTROY {
            unsafe { PostQuitMessage(0) };
            return LRESULT(0);
        }

        unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
    }

    unsafe fn show_context_menu(hwnd: HWND) -> Result<()> {
        let menu = unsafe { CreatePopupMenu() }.context("CreatePopupMenu failed")?;
        let menu_result = (|| -> Result<()> {
            unsafe { AppendMenuW(menu, MF_STRING, MENU_OPEN, w!("Open MonMan")) }
                .context("could not add Open to tray menu")?;
            unsafe { AppendMenuW(menu, MF_STRING, MENU_QUIT, w!("Quit MonMan")) }
                .context("could not add Quit to tray menu")?;

            let mut cursor = POINT::default();
            unsafe { GetCursorPos(&mut cursor) }.context("GetCursorPos failed")?;
            let _ = unsafe { SetForegroundWindow(hwnd) };
            let command = unsafe {
                TrackPopupMenu(
                    menu,
                    TPM_RIGHTBUTTON | TPM_RETURNCMD,
                    cursor.x,
                    cursor.y,
                    None,
                    hwnd,
                    None,
                )
            };
            match command.0 as usize {
                MENU_OPEN => emit(TrayEvent::OpenRequested),
                MENU_QUIT => emit(TrayEvent::ExitRequested),
                _ => {}
            }
            Ok(())
        })();
        let _ = unsafe { DestroyMenu(menu) };
        menu_result
    }

    fn emit(event: TrayEvent) {
        CALLBACKS.with(|callbacks| {
            if let Some((event_tx, ctx)) = callbacks.borrow().as_ref() {
                let _ = event_tx.send(event);
                ctx.request_repaint();
            }
        });
    }

    fn copy_wide(text: &str, destination: &mut [u16]) {
        destination.fill(0);
        let content_length = destination.len().saturating_sub(1);
        for (slot, value) in destination
            .iter_mut()
            .take(content_length)
            .zip(text.encode_utf16())
        {
            *slot = value;
        }
    }
}
