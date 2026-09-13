// System tray integration and flyout panel.
//
// Because the Windows native context menu only accommodates text items (sliders cannot be embedded),
// this application follows the Windows volume flyout design: clicking the tray icon (left or right)
// opens a frameless flyout window right beside the icon, containing sliders for all detected monitors.
//
// - Click tray icon: toggles flyout at click position.
// - Lost focus (click outside): automatically hides flyout.
// - Window Close (X): hides window instead of quitting; complete exit is handled via "Quit".
// - Brightness modifications emit "tray-brightness-changed" with updated monitor state so all windows sync.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use tauri::{AppHandle, Emitter, Manager, Runtime};

use crate::brightness::MonitorInfo;

pub const TRAY_ID: &str = "main-tray";
pub const EVENT_CHANGED: &str = "tray-brightness-changed";
pub const MAIN_LABEL: &str = "main";
pub const FLYOUT_LABEL: &str = "flyout";

/// Logical flyout dimensions matching `tauri.conf.json`.
const FLYOUT_W: f64 = 350.0;
const _FLYOUT_H: f64 = 325.0;

fn calculate_flyout_height(monitors: &[MonitorInfo]) -> f64 {
    let active_capable = monitors.iter().filter(|m| m.capable).count();
    let has_master = active_capable >= 2;
    let master_count = if has_master { 1 } else { 0 };

    if active_capable == 0 {
        return 155.0;
    }

    let total = 95.0 + ((active_capable + master_count) as f64 * 72.0);

    total.max(160.0)
}

#[derive(Default)]
pub struct TrayState {
    monitors: Mutex<Vec<MonitorInfo>>,
}

pub fn snapshot(app: &AppHandle<impl Runtime>) -> Vec<MonitorInfo> {
    app.state::<TrayState>()
        .monitors
        .lock()
        .map(|m| m.clone())
        .unwrap_or_default()
}

/// Replaces current state (after scanning) and updates tooltip. Does not emit
/// (caller from frontend command already has the data).
pub fn replace_state(app: &AppHandle<impl Runtime>, monitors: Vec<MonitorInfo>) {
    if let Ok(mut slot) = app.state::<TrayState>().monitors.lock() {
        *slot = monitors;
    }
    sync_tray(app);
}

/// Updates brightness of a specific monitor and refreshes tray tooltip.
pub fn update_one(app: &AppHandle<impl Runtime>, id: &str, value: u32) {
    if let Ok(mut slot) = app.state::<TrayState>().monitors.lock() {
        if let Some(m) = slot.iter_mut().find(|m| m.id == id) {
            m.current = value;
        }
    }
    sync_tray(app);
}

fn short_name(name: &str) -> String {
    const MAX: usize = 24;
    let chars: Vec<char> = name.chars().collect();
    if chars.len() <= MAX {
        name.to_string()
    } else {
        chars[..MAX].iter().collect::<String>() + "…"
    }
}

fn tooltip_text(monitors: &[MonitorInfo]) -> String {
    let capable: Vec<&MonitorInfo> = monitors.iter().filter(|m| m.capable).collect();
    if capable.is_empty() {
        return "Brightness Control".to_string();
    }
    let parts: Vec<String> = capable
        .iter()
        .take(3)
        .map(|m| format!("{} {}%", short_name(&m.name), m.current))
        .collect();
    let mut s = parts.join(" • ");
    if capable.len() > 3 {
        s.push_str(&format!(" (+{} more)", capable.len() - 3));
    }
    // Windows tray tooltip length is capped at ~127 characters.
    if s.chars().count() > 120 {
        s.chars().take(120).collect()
    } else {
        s
    }
}

/// Synchronizes tray tooltip with current state.
pub fn sync_tray<R: Runtime>(app: &AppHandle<R>) {
    if let Some(tray) = app.tray_by_id(TRAY_ID) {
        let _ = tray.set_tooltip(Some(tooltip_text(&snapshot(app))));
    }
}

/// Updates tray icon based on theme ("light", "dark", or "default").
pub fn set_tray_icon_theme<R: Runtime>(app: &AppHandle<R>, theme: &str) {
    if let Some(tray) = app.tray_by_id(TRAY_ID) {
        let bytes: &[u8] = match theme {
            "light" => include_bytes!("../icons/tray-light.png"),
            "dark" => include_bytes!("../icons/tray-dark.png"),
            _ => {
                if is_system_light_theme() {
                    include_bytes!("../icons/tray-light.png")
                } else {
                    include_bytes!("../icons/tray-dark.png")
                }
            }
        };
        if let Ok(icon) = tauri::image::Image::from_bytes(bytes) {
            let _ = tray.set_icon(Some(icon));
        }
    }
}

/// Checks Windows registry to see if the Windows Taskbar uses Light theme.
pub fn is_system_light_theme() -> bool {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        let mut cmd = std::process::Command::new("reg.exe");
        cmd.args([
            "query",
            r"HKCU\Software\Microsoft\Windows\CurrentVersion\Themes\Personalize",
            "/v",
            "SystemUsesLightTheme",
        ]);
        cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
        if let Ok(output) = cmd.output() {
            let s = String::from_utf8_lossy(&output.stdout);
            return s.contains("0x1");
        }
    }
    false
}

pub fn initial_tray_icon() -> Result<tauri::image::Image<'static>, String> {
    let bytes: &[u8] = if is_system_light_theme() {
        include_bytes!("../icons/tray-light.png")
    } else {
        include_bytes!("../icons/tray-dark.png")
    };
    tauri::image::Image::from_bytes(bytes).map_err(|e| format!("Failed to load tray icon: {e}"))
}

pub fn emit_changed<R: Runtime>(app: &AppHandle<R>) {
    let _ = app.emit(EVENT_CHANGED, snapshot(app));
}

/// Rescans displays from system (blocking) and synchronizes state, tooltip, and open windows.
///
/// Concurrent rescans are skipped: DDC speaks over a serial I2C bus, so two
/// simultaneous scans contend and each takes seconds instead of milliseconds.
/// The scan already in flight emits fresh state when it finishes, so skipping
/// is lossless.
static REFRESHING: AtomicBool = AtomicBool::new(false);

pub async fn refresh_from_system<R: Runtime>(app: &AppHandle<R>) {
    if REFRESHING.swap(true, Ordering::AcqRel) {
        return;
    }
    let app2 = app.clone();
    let result = tauri::async_runtime::spawn_blocking(crate::brightness::list_monitors_blocking)
        .await
        .map_err(|e| format!("Worker thread error while listing monitors: {e}"));
    match result {
        Ok(Ok(monitors)) => {
            replace_state(&app2, monitors);
            emit_changed(&app2);
        }
        Ok(Err(e)) | Err(e) => {
            eprintln!("[tray] rescan failed: {e}");
            sync_tray(&app2);
        }
    }
    REFRESHING.store(false, Ordering::Release);
}

/// Toggles flyout window near the tray icon position (physical screen coordinates).
/// Positions flyout above taskbar, centered horizontally on tray icon; flips downward if space is limited.
pub fn toggle_flyout_at<R: Runtime>(app: &AppHandle<R>, pos: tauri::PhysicalPosition<f64>) {
    let Some(w) = app.get_webview_window(FLYOUT_LABEL) else {
        return;
    };
    if w.is_visible().unwrap_or(false) {
        let _ = w.hide();
        return;
    }

    // Refresh displays in background when opening flyout so state is always up-to-date
    let app_handle = app.clone();
    tauri::async_runtime::spawn(async move {
        refresh_from_system(&app_handle).await;
    });

    let monitors = snapshot(app);
    let flyout_h = calculate_flyout_height(&monitors);
    let scale = w.scale_factor().unwrap_or(1.0);
    let _ = w.set_size(tauri::Size::Logical(tauri::LogicalSize {
        width: FLYOUT_W,
        height: flyout_h,
    }));
    let (pw, ph) = (FLYOUT_W * scale, flyout_h * scale);
    let mut x = (pos.x - pw / 2.0) as i32;
    let mut y = (pos.y - ph - 12.0 * scale) as i32;

    // Constrain within right edge of current monitor
    if let Ok(Some(monitor)) = w.current_monitor() {
        let screen_size = monitor.size();
        let max_x = (screen_size.width as f64 - pw - 12.0 * scale) as i32;
        if x > max_x {
            x = max_x;
        }
    }

    if x < 0 {
        x = 0;
    }
    if y < 0 {
        // Taskbar is at top or screen boundary exceeded: place below click position.
        y = (pos.y + 12.0 * scale) as i32;
    }
    let _ = w.set_position(tauri::Position::Physical(tauri::PhysicalPosition {
        x,
        y,
    }));
    let _ = w.show();
    let _ = w.set_focus();
    emit_changed(app);
}

pub fn hide_main_window<R: Runtime>(app: &AppHandle<R>) -> Result<(), String> {
    app.get_webview_window(MAIN_LABEL)
        .ok_or_else(|| "Main window not found.".to_string())
        .and_then(|w| w.hide().map_err(|e| format!("Failed to hide window: {e}")))
}

pub fn show_main_window<R: Runtime>(app: &AppHandle<R>) -> Result<(), String> {
    let w = app
        .get_webview_window(MAIN_LABEL)
        .ok_or_else(|| "Main window not found.".to_string())?;
    w.show()
        .and_then(|_| w.unminimize())
        .and_then(|_| w.set_focus())
        .map_err(|e| format!("Failed to show window: {e}"))
}
