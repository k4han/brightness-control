// Manage "Start with Windows" (Autostart on boot) functionality.
// Uses Windows Registry key HKCU\Software\Microsoft\Windows\CurrentVersion\Run.
// Does not require Administrator privileges because it resides in HKEY_CURRENT_USER.

use std::process::Command;

#[cfg(target_os = "windows")]
use std::os::windows::process::CommandExt;

const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
const APP_NAME: &str = "BrightnessControl";

/// CREATE_NO_WINDOW flag prevents a console window from popping up when reg.exe executes.
#[cfg(target_os = "windows")]
const CREATE_NO_WINDOW: u32 = 0x08000000;

/// Retrieves the registered executable path from registry, if present.
pub fn get_registered_exe_path() -> Result<Option<String>, String> {
    let mut cmd = Command::new("reg.exe");
    cmd.args(["query", RUN_KEY, "/v", APP_NAME]);

    #[cfg(target_os = "windows")]
    cmd.creation_flags(CREATE_NO_WINDOW);

    let output = cmd
        .output()
        .map_err(|e| format!("Failed to query registry: {e}"))?;

    if !output.status.success() {
        return Ok(None);
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with(APP_NAME) {
            // Format: BrightnessControl    REG_SZ    "..." --autostart
            if let Some(idx) = trimmed.find("REG_SZ") {
                let val = trimmed[idx + "REG_SZ".len()..].trim();
                let exe_path = if val.starts_with('"') {
                    val.trim_start_matches('"')
                        .split('"')
                        .next()
                        .unwrap_or("")
                        .to_string()
                } else {
                    val.split_whitespace()
                        .next()
                        .unwrap_or("")
                        .to_string()
                };
                if !exe_path.is_empty() {
                    return Ok(Some(exe_path));
                }
            }
        }
    }

    Ok(None)
}

/// Ensures the autostart registry entry points to the current executable if autostart was previously enabled.
pub fn sync_autostart_path() {
    if let Ok(Some(reg_path)) = get_registered_exe_path() {
        if let Ok(current_exe) = std::env::current_exe() {
            let current_str = current_exe.to_string_lossy();
            if !current_str.eq_ignore_ascii_case(&reg_path) {
                // Outdated path in registry (e.g. previous dev build or moved folder). Auto-update it!
                let _ = set_autostart(true);
            }
        }
    }
}

pub fn is_autostart_enabled() -> Result<bool, String> {
    match get_registered_exe_path()? {
        Some(reg_path) => {
            if let Ok(current_exe) = std::env::current_exe() {
                let current_str = current_exe.to_string_lossy();
                if !current_str.eq_ignore_ascii_case(&reg_path) {
                    // Outdated path in registry, heal it automatically
                    let _ = set_autostart(true);
                }
            }
            Ok(true)
        }
        None => Ok(false),
    }
}

pub fn set_autostart(enabled: bool) -> Result<bool, String> {
    if enabled {
        let exe_path = std::env::current_exe()
            .map_err(|e| format!("Failed to retrieve current executable path: {e}"))?;

        // Append --autostart so that on boot, the app starts minimized to the system tray.
        let val = format!("\"{}\" --autostart", exe_path.display());

        let mut cmd = Command::new("reg.exe");
        cmd.args([
            "add",
            RUN_KEY,
            "/v",
            APP_NAME,
            "/t",
            "REG_SZ",
            "/d",
            &val,
            "/f",
        ]);

        #[cfg(target_os = "windows")]
        cmd.creation_flags(CREATE_NO_WINDOW);

        let output = cmd
            .output()
            .map_err(|e| format!("Failed to execute reg add command: {e}"))?;

        if !output.status.success() {
            let err = String::from_utf8_lossy(&output.stderr);
            return Err(format!("Failed to enable start on boot: {err}"));
        }
    } else {
        let mut cmd = Command::new("reg.exe");
        cmd.args(["delete", RUN_KEY, "/v", APP_NAME, "/f"]);

        #[cfg(target_os = "windows")]
        cmd.creation_flags(CREATE_NO_WINDOW);

        let _ = cmd.output();
    }

    is_autostart_enabled()
}
