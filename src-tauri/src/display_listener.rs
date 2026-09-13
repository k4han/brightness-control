// Windows display topology event listener.
// Listens for WM_DISPLAYCHANGE / WM_SETTINGCHANGE broadcasts so that
// changing project modes (Win+P), or closing/opening laptop lid
// automatically triggers rescan and updates tray flyout and main window in real time.

use tauri::{AppHandle, Runtime};

#[cfg(target_os = "windows")]
use std::sync::OnceLock;

#[cfg(target_os = "windows")]
static NOTIFY_CALLBACK: OnceLock<Box<dyn Fn() + Send + Sync + 'static>> = OnceLock::new();

/// Last accepted display-change notification. Windows fires WM_DISPLAYCHANGE /
/// WM_SETTINGCHANGE in bursts (one topology switch = several messages), and
/// WM_SETTINGCHANGE also fires for unrelated reasons (theme, env, audio).
/// Without debouncing, each message spawned a full rescan (powershell.exe +
/// DDC enumeration), stalling the DDC bus for seconds.
#[cfg(target_os = "windows")]
static LAST_NOTIFY: OnceLock<std::sync::Mutex<std::time::Instant>> = OnceLock::new();

#[cfg(target_os = "windows")]
fn should_notify() -> bool {
    let lock = LAST_NOTIFY.get_or_init(|| {
        std::sync::Mutex::new(
            std::time::Instant::now() - std::time::Duration::from_secs(10),
        )
    });
    if let Ok(mut last) = lock.lock() {
        if last.elapsed() < std::time::Duration::from_millis(1200) {
            return false;
        }
        *last = std::time::Instant::now();
        true
    } else {
        true
    }
}

#[cfg(target_os = "windows")]
pub fn start<R: Runtime + 'static>(app: &AppHandle<R>) {
    let handle = app.clone();
    NOTIFY_CALLBACK.get_or_init(|| {
        Box::new(move || {
            if !should_notify() {
                return;
            }
            let h = handle.clone();
            tauri::async_runtime::spawn(async move {
                // Short wait to allow Windows display topology to settle
                let _ = tauri::async_runtime::spawn_blocking(|| {
                    std::thread::sleep(std::time::Duration::from_millis(350));
                })
                .await;
                crate::tray::refresh_from_system(&h).await;
            });
        })
    });

    let _ = std::thread::Builder::new()
        .name("display-listener".to_string())
        .spawn(|| unsafe {
            use std::ptr::null_mut;
            use winapi::shared::minwindef::{LPARAM, LRESULT, UINT, WPARAM};
            use winapi::shared::windef::HWND;
            use winapi::um::winuser::{
                CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW,
                PostQuitMessage, RegisterClassExW, TranslateMessage, MSG, WNDCLASSEXW,
                WM_DESTROY, WM_DISPLAYCHANGE,
            };

            const WM_SETTINGCHANGE: UINT = 0x001A;

            unsafe extern "system" fn wnd_proc(
                hwnd: HWND,
                msg: UINT,
                wparam: WPARAM,
                lparam: LPARAM,
            ) -> LRESULT {
                match msg {
                    WM_DISPLAYCHANGE | WM_SETTINGCHANGE => {
                        if let Some(cb) = NOTIFY_CALLBACK.get() {
                            cb();
                        }
                        0
                    }
                    WM_DESTROY => {
                        PostQuitMessage(0);
                        0
                    }
                    _ => DefWindowProcW(hwnd, msg, wparam, lparam),
                }
            }

            let class_name: Vec<u16> = "BrightnessDisplayListener\0"
                .encode_utf16()
                .collect();
            let mut wc: WNDCLASSEXW = std::mem::zeroed();
            wc.cbSize = std::mem::size_of::<WNDCLASSEXW>() as u32;
            wc.lpfnWndProc = Some(wnd_proc);
            wc.lpszClassName = class_name.as_ptr();

            RegisterClassExW(&wc);

            // Create top-level message window (NULL parent) to receive WM_DISPLAYCHANGE broadcasts
            let hwnd = CreateWindowExW(
                0,
                class_name.as_ptr(),
                class_name.as_ptr(),
                0,
                0,
                0,
                0,
                0,
                null_mut(),
                null_mut(),
                null_mut(),
                null_mut(),
            );

            if hwnd.is_null() {
                eprintln!("[display_listener] Failed to create listener window");
                return;
            }

            let mut msg: MSG = std::mem::zeroed();
            while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        });
}

#[cfg(not(target_os = "windows"))]
pub fn start<R: Runtime>(_app: &AppHandle<R>) {}
