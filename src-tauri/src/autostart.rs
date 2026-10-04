// Manage "Start with Windows" (Autostart on boot) functionality.
// Supports both:
// 1. Packaged MSIX / Microsoft Store app (uses Windows.ApplicationModel StartupTask declared in AppxManifest, avoids writing raw WindowsApps path to HKCU\Run which causes Access Denied).
// 2. Unpackaged Win32 / Standalone installer (uses HKCU\Software\Microsoft\Windows\CurrentVersion\Run).

use std::process::Command;

#[cfg(target_os = "windows")]
use std::os::windows::process::CommandExt;

const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
const APP_NAME: &str = "BrightnessControl";

/// CREATE_NO_WINDOW flag prevents a console window from popping up when reg.exe executes.
#[cfg(target_os = "windows")]
const CREATE_NO_WINDOW: u32 = 0x08000000;

/// Checks if the process is running inside an MSIX / AppX package container (e.g. Microsoft Store).
pub fn is_packaged_app() -> bool {
    #[cfg(target_os = "windows")]
    {
        #[link(name = "kernel32")]
        extern "system" {
            fn GetCurrentPackageFamilyName(
                package_family_name_length: *mut u32,
                package_family_name: *mut u16,
            ) -> i32;
        }
        let mut len: u32 = 0;
        // APPMODEL_ERROR_NO_PACKAGE is 15700
        let res = unsafe { GetCurrentPackageFamilyName(&mut len, std::ptr::null_mut()) };
        if res != 15700 {
            return true;
        }
    }

    if let Ok(current_exe) = std::env::current_exe() {
        if current_exe.to_string_lossy().contains("WindowsApps") {
            return true;
        }
    }

    false
}

/// Returns the age of the current user's desktop shell (explorer.exe) in seconds.
/// This accurately detects user logon time even when Windows Fast Startup preserves kernel uptime.
#[cfg(target_os = "windows")]
fn get_shell_session_age_seconds() -> Option<u64> {
    #[repr(C)]
    struct PROCESSENTRY32W {
        dw_size: u32,
        cnt_usage: u32,
        th32_process_id: u32,
        th32_default_heap_id: usize,
        th32_module_id: u32,
        cnt_threads: u32,
        th32_parent_process_id: u32,
        pc_pri_class_base: i32,
        dw_flags: u32,
        sz_exe_file: [u16; 260],
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn CreateToolhelp32Snapshot(dw_flags: u32, th32_process_id: u32) -> *mut std::ffi::c_void;
        fn Process32FirstW(h_snapshot: *mut std::ffi::c_void, lppe: *mut PROCESSENTRY32W) -> i32;
        fn Process32NextW(h_snapshot: *mut std::ffi::c_void, lppe: *mut PROCESSENTRY32W) -> i32;
        fn CloseHandle(h_object: *mut std::ffi::c_void) -> i32;
        fn OpenProcess(dw_desired_access: u32, b_inherit_handle: i32, dw_process_id: u32) -> *mut std::ffi::c_void;
        fn GetProcessTimes(
            h_process: *mut std::ffi::c_void,
            lp_creation_time: *mut winapi::shared::minwindef::FILETIME,
            lp_exit_time: *mut winapi::shared::minwindef::FILETIME,
            lp_kernel_time: *mut winapi::shared::minwindef::FILETIME,
            lp_user_time: *mut winapi::shared::minwindef::FILETIME,
        ) -> i32;
        fn GetSystemTimeAsFileTime(lp_system_time_as_file_time: *mut winapi::shared::minwindef::FILETIME);
    }

    const TH32CS_SNAPPROCESS: u32 = 0x00000002;
    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
    const INVALID_HANDLE_VALUE: *mut std::ffi::c_void = -1isize as *mut std::ffi::c_void;

    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snapshot == INVALID_HANDLE_VALUE || snapshot.is_null() {
            return None;
        }

        let mut entry = std::mem::zeroed::<PROCESSENTRY32W>();
        entry.dw_size = std::mem::size_of::<PROCESSENTRY32W>() as u32;

        if Process32FirstW(snapshot, &mut entry) == 0 {
            CloseHandle(snapshot);
            return None;
        }

        let mut explorer_creation_time: Option<u64> = None;

        loop {
            let name_len = entry
                .sz_exe_file
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(entry.sz_exe_file.len());
            let name = String::from_utf16_lossy(&entry.sz_exe_file[..name_len]);

            if name.eq_ignore_ascii_case("explorer.exe") {
                let h_proc = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, entry.th32_process_id);
                if !h_proc.is_null() && h_proc != INVALID_HANDLE_VALUE {
                    let mut creation = std::mem::zeroed();
                    let mut exit = std::mem::zeroed();
                    let mut kernel = std::mem::zeroed();
                    let mut user = std::mem::zeroed();

                    if GetProcessTimes(h_proc, &mut creation, &mut exit, &mut kernel, &mut user) != 0 {
                        let creation_u64 =
                            ((creation.dwHighDateTime as u64) << 32) | (creation.dwLowDateTime as u64);
                        explorer_creation_time = match explorer_creation_time {
                            Some(prev) => Some(prev.min(creation_u64)),
                            None => Some(creation_u64),
                        };
                    }
                    CloseHandle(h_proc);
                }
            }

            if Process32NextW(snapshot, &mut entry) == 0 {
                break;
            }
        }

        CloseHandle(snapshot);

        if let Some(creation_u64) = explorer_creation_time {
            let mut now = std::mem::zeroed();
            GetSystemTimeAsFileTime(&mut now);
            let now_u64 = ((now.dwHighDateTime as u64) << 32) | (now.dwLowDateTime as u64);
            if now_u64 >= creation_u64 {
                let diff_secs = (now_u64 - creation_u64) / 10_000_000;
                return Some(diff_secs);
            }
        }
    }

    None
}

/// Checks whether the system has just booted up or user logged in recently (within 3 minutes).
/// Useful for packaged apps launching silently on boot without command-line arguments.
#[cfg(target_os = "windows")]
pub fn is_boot_startup() -> bool {
    // 1. Check kernel uptime (cold boot / restart)
    if unsafe { winapi::um::sysinfoapi::GetTickCount64() < 180_000 } {
        return true;
    }

    // 2. Check user session age (handles Fast Startup / Hibernate resume where kernel uptime is not reset)
    if let Some(session_age_secs) = get_shell_session_age_seconds() {
        if session_age_secs < 180 {
            return true;
        }
    }

    false
}

#[cfg(not(target_os = "windows"))]
pub fn is_boot_startup() -> bool {
    false
}

/// Cleans up any stale or invalid registry entries pointing to `WindowsApps`.
/// Executables inside `C:\Program Files\WindowsApps\` cannot be executed directly
/// by explorer.exe via the Run key and will trigger Access Denied errors on login.
pub fn cleanup_stale_packaged_run_key() {
    if let Ok(Some(reg_path)) = get_registered_exe_path() {
        if reg_path.contains("WindowsApps") {
            let mut cmd = Command::new("reg.exe");
            cmd.args(["delete", RUN_KEY, "/v", APP_NAME, "/f"]);

            #[cfg(target_os = "windows")]
            cmd.creation_flags(CREATE_NO_WINDOW);

            let _ = cmd.output();
        }
    }
}

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

/// Path to local autostart config for packaged MSIX apps
fn packaged_config_path() -> Option<std::path::PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(|local_app_data| {
        let mut path = std::path::PathBuf::from(local_app_data);
        path.push("MultiBrightnessControl");
        let _ = std::fs::create_dir_all(&path);
        path.push("autostart.json");
        path
    })
}

fn get_packaged_autostart_preference() -> bool {
    if let Some(path) = packaged_config_path() {
        if let Ok(content) = std::fs::read_to_string(&path) {
            if let Ok(val) = serde_json::from_str::<serde_json::Value>(&content) {
                if let Some(enabled) = val.get("enabled").and_then(|v| v.as_bool()) {
                    return enabled;
                }
            }
        }
    }
    // Default to true for packaged app since StartupTask is declared enabled in manifest
    true
}

fn set_packaged_autostart_preference(enabled: bool) -> Result<bool, String> {
    if let Some(path) = packaged_config_path() {
        let val = serde_json::json!({ "enabled": enabled });
        std::fs::write(&path, val.to_string())
            .map_err(|e| format!("Failed to save autostart preference: {e}"))?;
    }
    Ok(enabled)
}

/// Ensures the autostart registry entry points to the current executable if autostart was previously enabled.
pub fn sync_autostart_path() {
    if is_packaged_app() {
        // Remove stale registry entry pointing to WindowsApps if any
        cleanup_stale_packaged_run_key();
        return;
    }

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
    if is_packaged_app() {
        cleanup_stale_packaged_run_key();
        return Ok(get_packaged_autostart_preference());
    }

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
    if is_packaged_app() {
        cleanup_stale_packaged_run_key();
        return set_packaged_autostart_preference(enabled);
    }

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
