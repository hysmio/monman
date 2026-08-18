#[cfg(windows)]
mod win {
    use std::time::Duration;
    use windows::Win32::Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE};
    use windows::Win32::System::Threading::CreateMutexW;
    use windows::Win32::UI::WindowsAndMessaging::{
        FindWindowW, SW_RESTORE, SetForegroundWindow, ShowWindow,
    };
    use windows::core::{PCWSTR, w};

    const WINDOW_LOOKUP_ATTEMPTS: usize = 100;

    pub struct InstanceGuard(HANDLE);

    impl Drop for InstanceGuard {
        fn drop(&mut self) {
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }

    pub fn acquire() -> std::io::Result<Option<InstanceGuard>> {
        let handle = unsafe {
            CreateMutexW(
                None,
                false,
                w!("Local\\hysmio.monman.single-instance.4E813E13-4D3A-4D93-9E62-7131A480496E"),
            )
        }
        .map_err(|error| std::io::Error::other(error.to_string()))?;

        let already_running = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
        if already_running {
            unsafe {
                let _ = CloseHandle(handle);
            }
            Ok(None)
        } else {
            Ok(Some(InstanceGuard(handle)))
        }
    }

    pub fn show_existing_window() {
        // A duplicate can be launched while the first process is still creating
        // its native window, so allow that short startup race to settle.
        for _ in 0..WINDOW_LOOKUP_ATTEMPTS {
            if let Ok(window) = unsafe { FindWindowW(PCWSTR::null(), w!("MonMan")) } {
                unsafe {
                    let _ = ShowWindow(window, SW_RESTORE);
                    let _ = SetForegroundWindow(window);
                }
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

#[cfg(windows)]
pub use win::{acquire, show_existing_window};

#[cfg(not(windows))]
pub struct InstanceGuard;

#[cfg(not(windows))]
pub fn acquire() -> std::io::Result<Option<InstanceGuard>> {
    Ok(Some(InstanceGuard))
}

#[cfg(not(windows))]
pub fn show_existing_window() {}
