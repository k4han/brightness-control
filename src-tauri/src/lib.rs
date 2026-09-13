mod autostart;
mod brightness;
mod display_listener;
mod tray;

use tauri::tray::{MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::Manager;
use tauri::WindowEvent;

/// Hides the main window to the system tray (invoked from "Hide to tray" button).
#[tauri::command]
async fn hide_main_window(app: tauri::AppHandle) -> Result<(), String> {
    tray::hide_main_window(&app)
}

/// Shows the main window (invoked from the button in the tray flyout panel).
#[tauri::command]
async fn show_main_window(app: tauri::AppHandle) -> Result<(), String> {
    tray::show_main_window(&app)
}

/// Completely terminates the application (invoked from "Quit" button — window close button only minimizes to tray).
#[tauri::command]
async fn quit_app(app: tauri::AppHandle) -> Result<(), String> {
    app.exit(0);
    #[allow(unreachable_code)]
    Ok(())
}

/// Queries "Start with Windows" status.
#[tauri::command]
async fn get_autostart() -> Result<bool, String> {
    autostart::is_autostart_enabled()
}

/// Enables or disables "Start with Windows".
#[tauri::command]
async fn set_autostart(enabled: bool) -> Result<bool, String> {
    autostart::set_autostart(enabled)
}

/// Returns the current application version from package info.
#[tauri::command]
fn get_app_version(app: tauri::AppHandle) -> String {
    app.package_info().version.to_string()
}

/// Updates tray icon dynamically based on theme ("light", "dark", or "system").
#[tauri::command]
async fn set_tray_theme(app: tauri::AppHandle, theme: String) -> Result<(), String> {
    tray::set_tray_icon_theme(&app, &theme);
    Ok(())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .manage(tray::TrayState::default())
        .setup(|app| {
            // Keep autostart registry entry in sync with the current executable path
            autostart::sync_autostart_path();

            let icon = tray::initial_tray_icon()?;

            // No native context menu: left or right click opens the flyout panel
            // (native OS menus cannot host sliders).
            TrayIconBuilder::with_id(tray::TRAY_ID)
                .icon(icon)
                .tooltip("Brightness Control")
                .show_menu_on_left_click(false)
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click {
                        position,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        tray::toggle_flyout_at(tray.app_handle(), position);
                    }
                })
                .build(app)?;

            // If not launched silently on Windows boot (--autostart / --minimized),
            // display the main window on standard user launch.
            let is_autostart = std::env::args().any(|arg| arg == "--autostart" || arg == "--minimized");
            if !is_autostart {
                if let Some(w) = app.get_webview_window(tray::MAIN_LABEL) {
                    let _ = w.show();
                }
            }

            // Background display scan so tooltip and flyout have accurate initial percentages.
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                tray::refresh_from_system(&handle).await;
            });

            // Listen for system display changes (monitor on/off, Win+P, lid open/close)
            let handle_display = app.handle().clone();
            display_listener::start(&handle_display);

            Ok(())
        })
        .on_window_event(|window, event| match event {
            // Window close button (X) hides window rather than quitting.
            WindowEvent::CloseRequested { api, .. } => {
                let _ = window.hide();
                api.prevent_close();
            }
            // Auto-hide flyout on focus loss (matching Windows volume flyout behavior).
            WindowEvent::Focused(false) => {
                if window.label() == tray::FLYOUT_LABEL {
                    let _ = window.hide();
                }
            }
            _ => {}
        })
        .invoke_handler(tauri::generate_handler![
            brightness::list_monitors,
            brightness::get_brightness,
            brightness::set_brightness,
            hide_main_window,
            show_main_window,
            quit_app,
            get_autostart,
            set_autostart,
            get_app_version,
            set_tray_theme
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
