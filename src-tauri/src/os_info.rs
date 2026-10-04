// Windows OS version and style detection module.
// Distinguishes between Windows 10 (Build < 22000) and Windows 11 (Build >= 22000)
// to automatically apply authentic native visual styles (Fluent 1.0 vs Fluent 2.0).

use serde::Serialize;

#[derive(Serialize, Clone, Debug)]
pub struct OsInfo {
    /// "win10" or "win11"
    pub detected_style: String,
    /// Exact Windows build number, e.g. 19045 (Win 10 22H2) or 22631 (Win 11 23H2)
    pub build_number: u32,
    /// Marketing product name, e.g. "Windows 10 Pro" or "Windows 11 Home"
    pub product_name: String,
    /// Display version release identifier, e.g. "22H2" or "24H2"
    pub display_version: String,
    /// True if running on Windows 11 or higher
    pub is_windows_11: bool,
}

#[cfg(target_os = "windows")]
pub fn get_windows_build_number() -> u32 {
    #[repr(C)]
    #[allow(non_snake_case)]
    struct OSVERSIONINFOEXW {
        dwOSVersionInfoSize: u32,
        dwMajorVersion: u32,
        dwMinorVersion: u32,
        dwBuildNumber: u32,
        dwPlatformId: u32,
        szCSDVersion: [u16; 128],
        wServicePackMajor: u16,
        wServicePackMinor: u16,
        wSuiteMask: u16,
        wProductType: u8,
        wReserved: u8,
    }

    #[link(name = "ntdll")]
    extern "system" {
        fn RtlGetVersion(lpVersionInformation: *mut OSVERSIONINFOEXW) -> i32;
    }

    unsafe {
        let mut info: OSVERSIONINFOEXW = std::mem::zeroed();
        info.dwOSVersionInfoSize = std::mem::size_of::<OSVERSIONINFOEXW>() as u32;
        if RtlGetVersion(&mut info) == 0 && info.dwBuildNumber > 0 {
            return info.dwBuildNumber;
        }
    }

    // Fallback: Registry query HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\CurrentBuild
    use std::os::windows::process::CommandExt;
    let mut cmd = std::process::Command::new("reg.exe");
    cmd.args([
        "query",
        r"HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion",
        "/v",
        "CurrentBuild",
    ]);
    cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
    if let Ok(output) = cmd.output() {
        let text = String::from_utf8_lossy(&output.stdout);
        for line in text.lines() {
            if line.contains("CurrentBuild") {
                if let Some(part) = line.split_whitespace().last() {
                    if let Ok(num) = part.parse::<u32>() {
                        return num;
                    }
                }
            }
        }
    }

    19045
}

#[cfg(not(target_os = "windows"))]
pub fn get_windows_build_number() -> u32 {
    22000
}

#[cfg(target_os = "windows")]
pub fn get_product_details() -> (String, String) {
    use std::os::windows::process::CommandExt;
    let mut prod_name = String::new();
    let mut disp_ver = String::new();

    let mut cmd = std::process::Command::new("reg.exe");
    cmd.args([
        "query",
        r"HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion",
    ]);
    cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
    if let Ok(output) = cmd.output() {
        let text = String::from_utf8_lossy(&output.stdout);
        for line in text.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with("ProductName") {
                let parts: Vec<&str> = trimmed.split_whitespace().collect();
                if parts.len() >= 3 {
                    prod_name = parts[2..].join(" ");
                }
            } else if trimmed.starts_with("DisplayVersion") {
                let parts: Vec<&str> = trimmed.split_whitespace().collect();
                if parts.len() >= 3 {
                    disp_ver = parts[2..].join(" ");
                }
            }
        }
    }

    if prod_name.is_empty() {
        prod_name = "Windows".to_string();
    }
    (prod_name, disp_ver)
}

#[cfg(not(target_os = "windows"))]
pub fn get_product_details() -> (String, String) {
    ("Windows".to_string(), "".to_string())
}

pub fn detect_os() -> OsInfo {
    let build = get_windows_build_number();
    let is_win11 = build >= 22000;
    let detected_style = if is_win11 { "win11" } else { "win10" }.to_string();
    let (product_name, display_version) = get_product_details();

    OsInfo {
        detected_style,
        build_number: build,
        product_name,
        display_version,
        is_windows_11: is_win11,
    }
}
