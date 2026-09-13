// Multi-monitor screen brightness control on Windows 10/11.
//
// - Laptop / Internal display: WMI ROOT\WMI (WmiMonitorBrightness / WmiMonitorBrightnessMethods)
//   executed via powershell.exe (Get-CimInstance + Invoke-CimMethod) to avoid complex COM
//   dependencies while remaining reliable across Windows 10/11 versions.
// - External displays: Hardware DDC/CI standard via Dxva2.dll (EnumDisplayMonitors ->
//   GetPhysicalMonitorsFromHMONITOR -> Get/SetMonitorBrightness).
//
// Each operation performs fresh enumeration and immediately destroys physical monitor handles
// (no persistent HANDLE caching; only high-level/VCP capability modes and min/max ranges are cached
// to allow 1 DDC call per slider adjustment).

use serde::Serialize;
use std::collections::HashMap;
use std::process::Command;
use std::sync::{Mutex, OnceLock};

#[cfg(target_os = "windows")]
use std::os::windows::process::CommandExt;

/// CREATE_NO_WINDOW prevents Windows from opening terminal/console windows when running powershell.exe.
#[cfg(target_os = "windows")]
const CREATE_NO_WINDOW: u32 = 0x08000000;

/// Brightness percentage range for UI.
/// Clamped to a minimum floor > 0 to prevent a completely black screen
/// (which would prevent users from seeing the interface to readjust).
pub const MIN_BRIGHTNESS_PERCENT: u32 = 10;
pub const MAX_BRIGHTNESS_PERCENT: u32 = 100;

#[derive(Debug, Clone, Serialize)]
pub struct MonitorInfo {
    pub id: String,
    /// Display name, e.g., "Laptop Display (CMN14D4)" or "MSI MP251 E2".
    pub name: String,
    /// "wmi" | "ddc"
    pub kind: String,
    /// UI percentage scale: min = MIN_BRIGHTNESS_PERCENT, max = 100.
    pub min: u32,
    pub max: u32,
    /// Current percentage (converted from native monitor range).
    pub current: u32,
    /// Set to false when display is detected but brightness cannot be controlled (e.g. DDC/CI disabled, dock blocking).
    pub capable: bool,
    /// Descriptive detail / hint for UI when capable = false.
    pub detail: String,
}

/// Clamps brightness to safe percentage boundaries (prevents 0% black screen).
pub fn clamp_brightness_percent(v: u32) -> u32 {
    v.max(MIN_BRIGHTNESS_PERCENT).min(MAX_BRIGHTNESS_PERCENT)
}

/// Helper function maintaining compatibility with existing clamped calls.
fn clamp_brightness(v: u32) -> u32 {
    clamp_brightness_percent(v)
}

fn normalize_native_max(max_native: u32) -> u32 {
    if max_native == 0 {
        100
    } else {
        max_native
    }
}

/// Converts UI percentage (MIN..100) -> native monitor range (min_native..max_native).
/// Used when setting brightness. Handles monitors with non-standard native ranges (e.g., max=255).
pub fn percent_to_native(percent: u32, min_native: u32, max_native: u32) -> u32 {
    let max_n = normalize_native_max(max_native);
    let p = clamp_brightness_percent(percent);
    if max_n <= min_native {
        return p.min(100);
    }
    let span = max_n - min_native;
    min_native + ((p as u64 * span as u64 + 50) / 100) as u32
}

/// Converts native monitor value -> UI percentage (0..100).
/// Used when reading brightness. Intentionally does NOT clamp floor so hardware OSD adjustments
/// below MIN are accurately reflected in the UI.
pub fn native_to_percent(current_native: u32, min_native: u32, max_native: u32) -> u32 {
    let max_n = normalize_native_max(max_native);
    if max_n <= min_native {
        return current_native.min(100);
    }
    let cur = current_native.max(min_native).min(max_n);
    (((cur - min_native) as u64 * 100 + (max_n - min_native) as u64 / 2)
        / (max_n - min_native) as u64) as u32
}

/// Parses "ddc:{index}" -> index. Shared between get/set for consistent error handling.
pub fn parse_ddc_index(id: &str) -> Result<usize, String> {
    id.strip_prefix("ddc:")
        .ok_or_else(|| format!("Invalid monitor ID: {id}"))?
        .parse::<usize>()
        .map_err(|_| format!("Invalid monitor ID: {id}"))
}

/// Extracts model token "MSI30D2" from InstanceName "DISPLAY\\MSI30D2\\5&..._0".
/// Upper-cased for case-insensitive matching with HMONITOR DeviceID.
pub fn extract_token_from_instance(instance: &str) -> Option<String> {
    let token = instance.split('\\').nth(1)?.trim().to_uppercase();
    if token.is_empty() {
        None
    } else {
        Some(token)
    }
}

// ---------------------------------------------------------------------------
// WMI (Internal laptop display) via powershell.exe
// ---------------------------------------------------------------------------

fn run_powershell(script: &str) -> Result<String, String> {
    let mut cmd = Command::new("powershell.exe");
    cmd.args(["-NoProfile", "-NonInteractive", "-Command", script]);

    #[cfg(target_os = "windows")]
    cmd.creation_flags(CREATE_NO_WINDOW);

    let output = cmd
        .output()
        .map_err(|e| format!("Failed to execute powershell.exe: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let msg = if !stderr.is_empty() { stderr } else { stdout };
        return Err(format!("PowerShell error (WMI): {msg}"));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Reads brightness from internal display. Returns 0-100 (actual value, without clamping floor
/// to ensure UI reflects physical key adjustments below minimum).
fn query_wmi_brightness() -> Result<u32, String> {
    // Fast path: Native Windows COM API reads in ~2ms
    #[cfg(target_os = "windows")]
    {
        if let Ok(b) = wmi_native::query_wmi_brightness() {
            return Ok(b);
        }
    }

    // Fallback: PowerShell script. Select first active instance. Desktop PCs do not have WMI brightness -> outputs empty.
    let script = "(Get-CimInstance -Namespace root/wmi -ClassName WmiMonitorBrightness | Where-Object { $_.Active } | Select-Object -First 1).CurrentBrightness";
    let out = run_powershell(script)?;
    if out.is_empty() || out.eq_ignore_ascii_case("null") {
        return Err(
            "WMI brightness not found (this machine may be a desktop PC without an internal display)."
                .to_string(),
        );
    }
    out.parse::<u32>()
        .map_err(|_| format!("Failed to parse WMI brightness: '{out}'"))
}

/// Extracts internal token (e.g. "CMN14D4") from WmiMonitorBrightness InstanceName.
/// Used to identify laptop panels in DDC listings to suppress duplicate entries.
#[allow(dead_code)]
fn query_wmi_internal_token() -> Option<String> {
    #[cfg(target_os = "windows")]
    {
        if let Ok((_, Some(token))) = wmi_native::query_wmi_brightness_and_token() {
            return Some(token);
        }
    }
    query_wmi_scan().internal_token
}

fn set_wmi_brightness(value: u32) -> Result<(), String> {
    let v = clamp_brightness(value);

    // Fast path: Native Windows COM API directly communicates with WMI in ~15-20ms
    // without spawning powershell.exe, eliminating slider adjustment latency.
    #[cfg(target_os = "windows")]
    {
        match wmi_native::set_wmi_brightness(v) {
            Ok(()) => return Ok(()),
            Err(e) => {
                eprintln!("[wmi_native] direct set failed ({e}), falling back to powershell");
            }
        }
    }

    let script = format!(
        "$m = Get-CimInstance -Namespace root/wmi -ClassName WmiMonitorBrightnessMethods | Select-Object -First 1; \
         if ($null -eq $m) {{ Write-Error 'No WmiMonitorBrightnessMethods'; exit 1 }}; \
         $r = Invoke-CimMethod -InputObject $m -MethodName WmiSetBrightness -Arguments @{{ Timeout = 1; Brightness = {v} }}; \
         if ($r.ReturnValue -ne 0) {{ Write-Error ('WmiSetBrightness failed: ' + $r.ReturnValue); exit $r.ReturnValue }}; \
         exit 0"
    );
    run_powershell(&script)?;
    Ok(())
}

/// Represents one monitor identified from WMI WmiMonitorID (friendly EDID names).
/// `token` is the segment following "DISPLAY\" in InstanceName, e.g. "MSI30D2",
/// used to match HMONITOR DeviceID ("MONITOR\\MSI30D2\\...").
#[derive(Debug, Clone)]
pub(crate) struct WmiMonitorId {
    pub(crate) token: String,
    pub(crate) manufacturer: String,
    pub(crate) friendly: String,
    #[allow(dead_code)] // Preserved to differentiate identical models or for debugging
    pub(crate) serial: String,
}

#[derive(Debug, Default, Clone)]
pub(crate) struct WmiScan {
    pub(crate) ids: Vec<WmiMonitorId>,
    pub(crate) internal_token: Option<String>,
    pub(crate) current_brightness: Option<u32>,
}

/// Runs a single batch PowerShell command to fetch all WMI brightness and monitor ID info at once.
/// This avoids multiple powershell.exe spawns, speeding up scan by ~3-4x.
pub(crate) fn query_wmi_scan() -> WmiScan {
    let script = r#"$b = Get-CimInstance -Namespace root/wmi -ClassName WmiMonitorBrightness -ErrorAction SilentlyContinue | Where-Object { $_.Active } | Select-Object -First 1; if ($b) { Write-Output ('INTERNAL|{0}|{1}' -f $b.InstanceName, $b.CurrentBrightness) }; $mons = Get-CimInstance -Namespace root/wmi -ClassName WmiMonitorID -ErrorAction SilentlyContinue | Where-Object { $_.Active }; foreach ($m in $mons) { $fn = ($m.UserFriendlyName | Where-Object { $_ -ne 0 } | ForEach-Object { [char]$_ }) -join ''; $mf = ($m.ManufacturerName | Where-Object { $_ -ne 0 } | ForEach-Object { [char]$_ }) -join ''; $se = ($m.SerialNumberID | Where-Object { $_ -ne 0 } | ForEach-Object { [char]$_ }) -join ''; Write-Output ('ID|{0}|{1}|{2}|{3}' -f $m.InstanceName, $mf, $fn, $se) }"#;
    let out = match run_powershell(script) {
        Ok(o) => o,
        Err(_) => return WmiScan::default(),
    };

    let mut scan = WmiScan::default();
    for line in out.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("INTERNAL|") {
            let parts: Vec<&str> = trimmed.splitn(3, '|').collect();
            if parts.len() >= 2 {
                scan.internal_token = extract_token_from_instance(parts[1]);
            }
            if parts.len() >= 3 {
                scan.current_brightness = parts[2].trim().parse::<u32>().ok();
            }
        } else if trimmed.starts_with("ID|") {
            let parts: Vec<&str> = trimmed.splitn(5, '|').collect();
            if parts.len() >= 5 {
                let token = extract_token_from_instance(parts[1]).unwrap_or_default();
                if !token.is_empty() {
                    scan.ids.push(WmiMonitorId {
                        token,
                        manufacturer: parts[2].trim().to_string(),
                        friendly: parts[3].trim().to_string(),
                        serial: parts[4].trim().to_string(),
                    });
                }
            }
        }
    }
    scan
}

/// Queries EDID friendly names of all active monitors. Errors fallback to empty vector.
#[allow(dead_code)]
pub(crate) fn query_wmi_monitor_ids() -> Vec<WmiMonitorId> {
    query_wmi_scan().ids
}

// ---------------------------------------------------------------------------
// DDC/CI (External monitors) via Dxva2 — compiled on Windows
// ---------------------------------------------------------------------------

/// Cached communication mode of a physical monitor.
/// Caches high-level or VCP capability so dragging sliders only takes 1 DDC call.
#[derive(Debug, Clone, Copy)]
enum CachedMode {
    HighLevel { min_native: u32, max_native: u32 },
    Vcp { max_native: u32 },
}

static DDC_CACHE: OnceLock<Mutex<HashMap<usize, CachedMode>>> = OnceLock::new();

fn ddc_cache() -> &'static Mutex<HashMap<usize, CachedMode>> {
    DDC_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn ddc_cache_get(index: usize) -> Option<CachedMode> {
    ddc_cache().lock().ok()?.get(&index).copied()
}

fn ddc_cache_insert(index: usize, mode: CachedMode) {
    if let Ok(mut m) = ddc_cache().lock() {
        m.insert(index, mode);
    }
}

fn ddc_cache_remove(index: usize) {
    if let Ok(mut m) = ddc_cache().lock() {
        m.remove(&index);
    }
}

/// Short-lived cache deduplicating simultaneous full display scans (e.g. main
/// window + flyout requesting at once on startup).
static SCAN_CACHE: OnceLock<Mutex<Option<(std::time::Instant, Vec<MonitorInfo>)>>> =
    OnceLock::new();

fn scan_cache() -> &'static Mutex<Option<(std::time::Instant, Vec<MonitorInfo>)>> {
    SCAN_CACHE.get_or_init(|| Mutex::new(None))
}

#[cfg(target_os = "windows")]
mod ddc {
    use std::ptr::{null, null_mut};
    use winapi::shared::minwindef::{BOOL, DWORD, LPARAM, TRUE};
    use winapi::shared::windef::{HDC, HMONITOR, LPRECT};
    use winapi::um::highlevelmonitorconfigurationapi::{
        GetMonitorBrightness, SetMonitorBrightness,
    };
    use winapi::um::lowlevelmonitorconfigurationapi::{
        GetVCPFeatureAndVCPFeatureReply, SetVCPFeature,
    };
    use winapi::um::physicalmonitorenumerationapi::{
        DestroyPhysicalMonitors, GetNumberOfPhysicalMonitorsFromHMONITOR,
        GetPhysicalMonitorsFromHMONITOR, PHYSICAL_MONITOR,
    };
    use winapi::um::wingdi::DISPLAY_DEVICEW;
    use winapi::um::winnt::HANDLE;
    use winapi::um::winuser::{
        EnumDisplayDevicesW, EnumDisplayMonitors, GetMonitorInfoW, LPMONITORINFO, MONITORINFOEXW,
    };

    pub struct DdcMonitor {
        pub index: usize,
        /// EDID matching token (e.g. "MSI30D2"), None if DeviceID cannot be retrieved.
        pub token: Option<String>,
        pub name: String,
        pub min_native: u32,
        pub current_native: u32,
        pub max_native: u32,
        pub capable: bool,
        /// True when fallback to VCP 0x10 is required instead of high-level API.
        pub vcp_fallback: bool,
        /// Dxva2 error code when incapable.
        pub error: String,
    }

    /// MCCS VCP code for luminance / brightness.
    const VCP_BRIGHTNESS: u8 = 0x10;

    /// Result of probing a physical monitor.
    enum Probe {
        HighLevel {
            min: u32,
            current: u32,
            max: u32,
        },
        Vcp { current: u32, max: u32 },
        Dead { le_bright: u32, le_vcp: u32 },
    }

    unsafe extern "system" fn enum_cb(
        hmon: HMONITOR,
        _hdc: HDC,
        _rect: LPRECT,
        lparam: LPARAM,
    ) -> BOOL {
        let list = &mut *(lparam as *mut Vec<HMONITOR>);
        list.push(hmon);
        TRUE
    }

    fn last_error_msg(ctx: &str) -> String {
        unsafe {
            let code = winapi::um::errhandlingapi::GetLastError();
            format!("{ctx} (GetLastError={code})")
        }
    }

    fn last_error() -> u32 {
        unsafe { winapi::um::errhandlingapi::GetLastError() }
    }

    /// Probes a physical monitor: tests high-level API first, then falls back to low-level VCP 0x10.
    fn probe(h: HANDLE) -> Probe {
        unsafe {
            let mut min: DWORD = 0;
            let mut cur: DWORD = 0;
            let mut max: DWORD = 0;
            if GetMonitorBrightness(h, &mut min, &mut cur, &mut max) != 0 {
                return Probe::HighLevel {
                    min: min as u32,
                    current: cur as u32,
                    max: max as u32,
                };
            }
            let le_bright = last_error();
            let mut vct = std::mem::zeroed();
            let mut vcur: DWORD = 0;
            let mut vmax: DWORD = 0;
            if GetVCPFeatureAndVCPFeatureReply(h, VCP_BRIGHTNESS, &mut vct, &mut vcur, &mut vmax)
                != 0
            {
                return Probe::Vcp {
                    current: vcur as u32,
                    max: vmax as u32,
                };
            }
            Probe::Dead {
                le_bright,
                le_vcp: last_error(),
            }
        }
    }

    /// PHYSICAL_MONITOR is #[repr(packed)], so references to its fields are disallowed.
    /// Safely copy the struct using read_unaligned before accessing fields.
    fn copy_physical(pm_ptr: *const PHYSICAL_MONITOR) -> PHYSICAL_MONITOR {
        unsafe { std::ptr::read_unaligned(pm_ptr) }
    }

    fn physical_description(pm_ptr: *const PHYSICAL_MONITOR) -> String {
        let raw = copy_physical(pm_ptr).szPhysicalMonitorDescription;
        let len = raw.iter().position(|&c| c == 0).unwrap_or(raw.len());
        String::from_utf16_lossy(&raw[..len]).trim().to_string()
    }

    fn physical_handle(pm_ptr: *const PHYSICAL_MONITOR) -> HANDLE {
        copy_physical(pm_ptr).hPhysicalMonitor
    }

    fn wide_to_string(raw: &[u16]) -> String {
        let len = raw.iter().position(|&c| c == 0).unwrap_or(raw.len());
        String::from_utf16_lossy(&raw[..len]).trim().to_string()
    }

    /// Extracts token (e.g. "MSI30D2") from HMONITOR DeviceID "MONITOR\\MSI30D2\\...".
    fn hmonitor_device_token(hmon: HMONITOR) -> Option<String> {
        unsafe {
            let mut mi: MONITORINFOEXW = std::mem::zeroed();
            mi.cbSize = std::mem::size_of::<MONITORINFOEXW>() as DWORD;
            if GetMonitorInfoW(hmon, &mut mi as *mut MONITORINFOEXW as LPMONITORINFO) == 0 {
                return None;
            }
            let mut dd: DISPLAY_DEVICEW = std::mem::zeroed();
            dd.cb = std::mem::size_of::<DISPLAY_DEVICEW>() as DWORD;
            if EnumDisplayDevicesW(mi.szDevice.as_ptr(), 0, &mut dd, 0) == 0 {
                return None;
            }
            let id = wide_to_string(&dd.DeviceID);
            let token = id.split('\\').nth(1)?.trim().to_uppercase();
            if token.is_empty() {
                None
            } else {
                Some(token)
            }
        }
    }

    /// Resolves the most user-friendly display name: EDID friendly name > manufacturer > Dxva2 description.
    fn resolve_name(
        token: Option<&str>,
        ids: &[super::WmiMonitorId],
        desc_raw: String,
        fallback_idx: usize,
    ) -> String {
        let clean = |s: &str| {
            s.chars()
                .filter(|c| !c.is_control())
                .collect::<String>()
                .trim()
                .to_string()
        };
        if let Some(t) = token {
            if let Some(w) = ids.iter().find(|id| id.token.eq_ignore_ascii_case(t)) {
                let friendly = clean(&w.friendly);
                if !friendly.is_empty() {
                    return friendly;
                }
                let mf = clean(&w.manufacturer);
                let desc = clean(&desc_raw);
                if !mf.is_empty() && (desc.is_empty() || desc == "Generic PnP Monitor") {
                    return mf;
                }
            }
        }
        let desc = clean(&desc_raw);
        if !desc.is_empty() {
            return desc;
        }
        format!("External Display {}", fallback_idx + 1)
    }

    fn enumerate_hmonitors() -> Result<Vec<HMONITOR>, String> {
        unsafe {
            let mut list: Vec<HMONITOR> = Vec::new();
            let ok = EnumDisplayMonitors(
                null_mut(),
                null(),
                Some(enum_cb),
                &mut list as *mut Vec<HMONITOR> as LPARAM,
            );
            if ok == 0 {
                return Err(last_error_msg("EnumDisplayMonitors failed"));
            }
            Ok(list)
        }
    }

    pub fn get_active_hmonitor_tokens() -> Vec<String> {
        let hmons = match enumerate_hmonitors() {
            Ok(h) => h,
            Err(_) => return Vec::new(),
        };
        let mut tokens = Vec::new();
        for hmon in hmons {
            if let Some(t) = hmonitor_device_token(hmon) {
                if !tokens.contains(&t) {
                    tokens.push(t);
                }
            }
        }
        tokens
    }

    pub fn get_active_hmonitor_count() -> usize {
        enumerate_hmonitors().map(|h| h.len()).unwrap_or(0)
    }

    /// Lists all physical monitors. Monitors with unsupported brightness
    /// are returned with capable=false so the UI can explain why.
    pub fn list_ddc_monitors(ids: &[super::WmiMonitorId]) -> Result<Vec<DdcMonitor>, String> {
        unsafe {
            let hmons = enumerate_hmonitors()?;
            let mut out: Vec<DdcMonitor> = Vec::new();

            for hmon in hmons {
                let token = hmonitor_device_token(hmon);
                let mut count: DWORD = 0;
                if GetNumberOfPhysicalMonitorsFromHMONITOR(hmon, &mut count) == 0 {
                    continue;
                }
                if count == 0 {
                    continue;
                }
                let mut phys: Vec<PHYSICAL_MONITOR> = vec![std::mem::zeroed(); count as usize];
                if GetPhysicalMonitorsFromHMONITOR(hmon, count, phys.as_mut_ptr()) == 0 {
                    continue;
                }

                for i in 0..(count as usize) {
                    let pm_ptr = phys.as_ptr().add(i);
                    let h: HANDLE = physical_handle(pm_ptr);
                    let desc_raw = physical_description(pm_ptr);
                    let idx = out.len();

                    let name = resolve_name(token.as_deref(), ids, desc_raw, idx);

                    match probe(h) {
                        Probe::HighLevel { min, current, max } => {
                            super::ddc_cache_insert(
                                idx,
                                super::CachedMode::HighLevel {
                                    min_native: min,
                                    max_native: max,
                                },
                            );
                            out.push(DdcMonitor {
                                index: idx,
                                token: token.clone(),
                                name,
                                min_native: min,
                                current_native: current,
                                max_native: max,
                                capable: true,
                                vcp_fallback: false,
                                error: String::new(),
                            })
                        }
                        Probe::Vcp { current, max } => {
                            super::ddc_cache_insert(
                                idx,
                                super::CachedMode::Vcp { max_native: max },
                            );
                            out.push(DdcMonitor {
                                index: idx,
                                token: token.clone(),
                                name,
                                min_native: 0,
                                current_native: current,
                                max_native: max,
                                capable: true,
                                vcp_fallback: true,
                                error: String::new(),
                            })
                        }
                        Probe::Dead { le_bright, le_vcp } => {
                            super::ddc_cache_remove(idx);
                            out.push(DdcMonitor {
                                index: idx,
                                token: token.clone(),
                                name,
                                min_native: 0,
                                current_native: 50,
                                max_native: 100,
                                capable: false,
                                vcp_fallback: false,
                                error: format!("brightness le={le_bright}, VCP 0x10 le={le_vcp}"),
                            })
                        }
                    }
                }

                // Always release physical monitor handles in the same loop.
                DestroyPhysicalMonitors(count, phys.as_mut_ptr());
            }

            for (i, m) in out.iter_mut().enumerate() {
                m.index = i;
            }
            Ok(out)
        }
    }

    fn with_physical<F, T>(target_index: usize, f: F) -> Result<T, String>
    where
        F: Fn(HANDLE) -> Result<T, String>,
    {
        unsafe {
            let hmons = enumerate_hmonitors()?;
            let mut flat: usize = 0;
            for hmon in hmons {
                let mut count: DWORD = 0;
                if GetNumberOfPhysicalMonitorsFromHMONITOR(hmon, &mut count) == 0 {
                    continue;
                }
                if count == 0 {
                    continue;
                }
                let mut phys: Vec<PHYSICAL_MONITOR> = vec![std::mem::zeroed(); count as usize];
                if GetPhysicalMonitorsFromHMONITOR(hmon, count, phys.as_mut_ptr()) == 0 {
                    continue;
                }

                let mut result: Option<Result<T, String>> = None;
                for i in 0..(count as usize) {
                    if flat == target_index {
                        let pm_ptr = phys.as_ptr().add(i);
                        result = Some(f(physical_handle(pm_ptr)));
                        break;
                    }
                    flat += 1;
                }
                DestroyPhysicalMonitors(count, phys.as_mut_ptr());

                if let Some(r) = result {
                    return r;
                }
            }
            Err(format!("DDC monitor at index {target_index} not found"))
        }
    }

    pub fn get_ddc_brightness(index: usize) -> Result<u32, String> {
        with_physical(index, |h| match probe(h) {
            Probe::HighLevel { min, current, max } => {
                Ok(super::native_to_percent(current, min, max))
            }
            Probe::Vcp { current, max } => {
                Ok(super::native_to_percent(current, 0, max))
            }
            Probe::Dead { le_bright, le_vcp } => Err(format!(
                "Failed to read brightness (brightness le={le_bright}, VCP 0x10 le={le_vcp}). Please check if DDC/CI is enabled in monitor OSD."
            )),
        })
    }

    pub fn set_ddc_brightness(index: usize, percent: u32) -> Result<(), String> {
        let p = super::clamp_brightness(percent);
        // Fast path: mode and range are already known from previous probe,
        // so we perform direct Set with only 1 DDC call (crucial when dragging sliders).
        if let Some(cached) = super::ddc_cache_get(index) {
            let native = match cached {
                super::CachedMode::HighLevel {
                    min_native,
                    max_native,
                } => super::percent_to_native(p, min_native, max_native),
                super::CachedMode::Vcp { max_native } => {
                    super::percent_to_native(p, 0, max_native)
                }
            };
            let fast: Result<(), String> = with_physical(index, |h| unsafe {
                match cached {
                    super::CachedMode::HighLevel { .. } => {
                        if SetMonitorBrightness(h, native as DWORD) == 0 {
                            return Err(last_error_msg("SetMonitorBrightness failed"));
                        }
                        Ok(())
                    }
                    super::CachedMode::Vcp { .. } => {
                        if SetVCPFeature(h, VCP_BRIGHTNESS, native as DWORD) == 0 {
                            return Err(last_error_msg("SetVCPFeature(0x10) failed"));
                        }
                        Ok(())
                    }
                }
            });
            if fast.is_ok() {
                return Ok(());
            }
            super::ddc_cache_remove(index);
        }

        // Full path: probe capabilities first, then Set (2-3 DDC calls).
        with_physical(index, |h| unsafe {
            match probe(h) {
                Probe::HighLevel { min, max, .. } => {
                    let native = super::percent_to_native(p, min, max);
                    super::ddc_cache_insert(
                        index,
                        super::CachedMode::HighLevel {
                            min_native: min,
                            max_native: max,
                        },
                    );
                    if SetMonitorBrightness(h, native as DWORD) == 0 {
                        super::ddc_cache_remove(index);
                        return Err(last_error_msg(
                            "SetMonitorBrightness failed (check if DDC/CI is enabled in monitor OSD)",
                        ));
                    }
                    Ok(())
                }
                Probe::Vcp { max, .. } => {
                    let native = super::percent_to_native(p, 0, max);
                    super::ddc_cache_insert(index, super::CachedMode::Vcp { max_native: max });
                    if SetVCPFeature(h, VCP_BRIGHTNESS, native as DWORD) == 0 {
                        super::ddc_cache_remove(index);
                        return Err(last_error_msg(
                            "SetVCPFeature(0x10) failed (check if DDC/CI is enabled in monitor OSD)",
                        ));
                    }
                    Ok(())
                }
                Probe::Dead { le_bright, le_vcp } => Err(format!(
                    "Failed to set brightness (brightness le={le_bright}, VCP 0x10 le={le_vcp}). Please check if DDC/CI is enabled in monitor OSD."
                )),
            }
        })
    }

}

#[cfg(not(target_os = "windows"))]
mod ddc {
    #[allow(dead_code)]
    pub struct DdcMonitor {
        pub index: usize,
        pub token: Option<String>,
        pub name: String,
        pub min_native: u32,
        pub current_native: u32,
        pub max_native: u32,
        pub capable: bool,
        pub vcp_fallback: bool,
        pub error: String,
    }
    pub fn list_ddc_monitors(
        _ids: &[super::WmiMonitorId],
    ) -> Result<Vec<DdcMonitor>, String> {
        Ok(Vec::new())
    }
    pub fn get_ddc_brightness(_index: usize) -> Result<u32, String> {
        Err("DDC/CI is only supported on Windows.".to_string())
    }
    pub fn set_ddc_brightness(_index: usize, _percent: u32) -> Result<(), String> {
        Err("DDC/CI is only supported on Windows.".to_string())
    }
    pub fn get_active_hmonitor_tokens() -> Vec<String> {
        Vec::new()
    }
    pub fn get_active_hmonitor_count() -> usize {
        0
    }
}

pub(crate) fn get_active_hmonitor_tokens() -> Vec<String> {
    ddc::get_active_hmonitor_tokens()
}

pub(crate) fn get_active_hmonitor_count() -> usize {
    ddc::get_active_hmonitor_count()
}

/// Checks if the internal laptop display is currently active in Windows desktop.
/// Prevents displaying a brightness slider when laptop lid is closed or project mode is "Second screen only".
pub(crate) fn is_internal_display_active(
    internal_token: Option<&str>,
    active_tokens: &[String],
    active_hmonitor_count: usize,
    ddc_capable_count: usize,
) -> bool {
    // If no active HMONITOR exists on the desktop, screen is definitely off
    if active_hmonitor_count == 0 {
        return false;
    }

    // If we know the internal token (e.g. "CMN14D4"), verify it exists among active HMONITORs
    if let Some(it) = internal_token {
        if !it.is_empty() {
            return active_tokens.iter().any(|t| t.eq_ignore_ascii_case(it));
        }
    }

    // Fallback: If internal_token was unknown, but active desktop HMONITORs exceed detected DDC monitors,
    // the remaining non-DDC HMONITOR is the internal panel.
    active_hmonitor_count > ddc_capable_count
}

// ---------------------------------------------------------------------------
// Tauri commands (synchronous logic + async wrappers to prevent UI blocking)
// ---------------------------------------------------------------------------

/// Enumerates all monitors: 1 WMI entry (internal laptop display, only when active)
/// + all physical DDC monitors.
/// Suppresses duplicate internal panel entries matching WMI token so two sliders don't control
/// the same screen.
pub(crate) fn list_monitors_blocking() -> Result<Vec<MonitorInfo>, String> {
    // Short-lived cache to deduplicate simultaneous scans during application startup.
    let cache_lock = scan_cache();
    if let Ok(guard) = cache_lock.lock() {
        if let Some((time, ref list)) = *guard {
            if time.elapsed() < std::time::Duration::from_millis(1500) {
                return Ok(list.clone());
            }
        }
    }

    let mut out: Vec<MonitorInfo> = Vec::new();

    let wmi = query_wmi_scan();
    let ids = wmi.ids;
    let internal_token = wmi.internal_token;
    let wmi_current = wmi.current_brightness;

    let active_tokens = get_active_hmonitor_tokens();
    let active_hmonitor_count = get_active_hmonitor_count();

    // Check external DDC monitors first to know how many external displays are active
    let ddc_result = ddc::list_ddc_monitors(&ids);
    let ddc_capable_count = match &ddc_result {
        Ok(list) => list.iter().filter(|m| m.capable).count(),
        Err(_) => 0,
    };

    let is_internal_on = is_internal_display_active(
        internal_token.as_deref(),
        &active_tokens,
        active_hmonitor_count,
        ddc_capable_count,
    );

    if let Some(cur) = wmi_current {
        // Skip the internal panel when it is not active (lid closed or
        // "Second screen only" project mode) — no brightness control possible.
        if is_internal_on {
            let name = match &internal_token {
                Some(t) => match ids.iter().find(|id| id.token.eq_ignore_ascii_case(t)) {
                    Some(w) => {
                        let friendly: String = w
                            .friendly
                            .chars()
                            .filter(|c| !c.is_control())
                            .collect::<String>()
                            .trim()
                            .to_string();
                        if friendly.is_empty() {
                            format!("Laptop Display ({})", w.token)
                        } else {
                            format!("Laptop Display ({friendly})")
                        }
                    }
                    None => "Laptop Display (Internal)".to_string(),
                },
                None => "Laptop Display (Internal)".to_string(),
            };
            out.push(MonitorInfo {
                id: "wmi:internal".to_string(),
                name,
                kind: "wmi".to_string(),
                min: MIN_BRIGHTNESS_PERCENT,
                max: MAX_BRIGHTNESS_PERCENT,
                current: cur.min(100),
                capable: true,
                detail: "Controlled via WMI".to_string(),
            });
        }
    }
    let has_wmi_active = is_internal_on && wmi_current.is_some();

    match ddc_result {
        Ok(ddc_list) => {
            for m in ddc_list {
                // Hide duplicate internal panel if already controlled via WMI
                if has_wmi_active {
                    if let (Some(it), Some(mt)) = (internal_token.as_deref(), m.token.as_deref())
                    {
                        if it.eq_ignore_ascii_case(mt) {
                            continue;
                        }
                    }
                }
                let percent = native_to_percent(m.current_native, m.min_native, m.max_native);
                let id = format!("ddc:{}", m.index);
                out.push(MonitorInfo {
                    id,
                    name: m.name.clone(),
                    kind: "ddc".to_string(),
                    min: MIN_BRIGHTNESS_PERCENT,
                    max: MAX_BRIGHTNESS_PERCENT,
                    current: if m.capable { percent } else { 0 },
                    capable: m.capable,
                    detail: if m.capable {
                        if m.vcp_fallback {
                            "Controlled via DDC/CI (VCP mode)".to_string()
                        } else {
                            "Controlled via DDC/CI".to_string()
                        }
                    } else if m.error.is_empty() {
                        "Display is turned off or DDC/CI is disabled in monitor OSD.".to_string()
                    } else {
                        format!(
                            "Display is turned off or DDC/CI is disabled ({}).",
                            m.error
                        )
                    },
                });
            }
        }
        Err(e) => {
            if !has_wmi_active && out.is_empty() {
                return Err(format!("Failed to list displays: {e}"));
            }
        }
    }

    if out.is_empty() {
        return Err("No controllable displays found. Please check: ensure DDC/CI is enabled in monitor OSD and use a direct HDMI/DP cable.".to_string());
    }
    if let Ok(mut guard) = cache_lock.lock() {
        *guard = Some((std::time::Instant::now(), out.clone()));
    }
    Ok(out)
}

pub(crate) fn get_brightness_blocking(id: String) -> Result<u32, String> {
    if id.starts_with("wmi:") {
        return query_wmi_brightness();
    }
    if id.starts_with("ddc:") {
        let i = parse_ddc_index(&id)?;
        return ddc::get_ddc_brightness(i);
    }
    Err(format!("Invalid monitor ID: {id}"))
}

pub(crate) fn set_brightness_blocking(id: String, value: u32) -> Result<(), String> {
    let v = clamp_brightness(value);
    if id.starts_with("wmi:") {
        return set_wmi_brightness(v);
    }
    if id.starts_with("ddc:") {
        let i = parse_ddc_index(&id)?;
        return ddc::set_ddc_brightness(i, v);
    }
    Err(format!("Invalid monitor ID: {id}"))
}

/// Tauri commands are asynchronous and use spawn_blocking because PowerShell/DDC are blocking I/O.
/// This prevents UI stuttering while sliding controls.
#[tauri::command]
pub async fn list_monitors(app: tauri::AppHandle) -> Result<Vec<MonitorInfo>, String> {
    let list = tauri::async_runtime::spawn_blocking(list_monitors_blocking)
        .await
        .map_err(|e| format!("Worker thread error while listing monitors: {e}"))?;

    // Sync state and tooltip in system tray
    if let Ok(monitors) = list.as_ref() {
        crate::tray::replace_state(&app, monitors.clone());
        crate::tray::sync_tray(&app);
    }
    list
}

#[tauri::command]
pub async fn get_brightness(id: String) -> Result<u32, String> {
    tauri::async_runtime::spawn_blocking(move || get_brightness_blocking(id))
        .await
        .map_err(|e| format!("Worker thread error while reading brightness: {e}"))?
}

#[tauri::command]
pub async fn set_brightness(
    app: tauri::AppHandle,
    id: String,
    value: u32,
) -> Result<(), String> {
    let id2 = id.clone();
    let result = tauri::async_runtime::spawn_blocking(move || set_brightness_blocking(id2, value))
        .await
        .map_err(|e| format!("Worker thread error while setting brightness: {e}"))?;

    if result.is_ok() {
        crate::tray::update_one(&app, &id, clamp_brightness_percent(value));
        crate::tray::emit_changed(&app);
    }
    result
}

#[cfg(target_os = "windows")]
pub(crate) mod wmi_native {
    use std::ptr::null_mut;
    use winapi::shared::minwindef::{DWORD, ULONG};
    use winapi::shared::rpcdce::{
        RPC_C_AUTHN_LEVEL_CALL, RPC_C_AUTHN_WINNT, RPC_C_AUTHZ_NONE,
        RPC_C_IMP_LEVEL_IMPERSONATE,
    };
    use winapi::shared::winerror::SUCCEEDED;
    use winapi::shared::wtypes::{BSTR, VT_BSTR, VT_I4, VT_UI1, VT_UI4};
    use winapi::um::combaseapi::{
        CoCreateInstance, CoInitializeEx, CoSetProxyBlanket, CoUninitialize,
    };
    use winapi::um::oaidl::VARIANT;
    use winapi::um::objbase::COINIT_MULTITHREADED;
    use winapi::um::oleauto::{
        SysAllocString, SysFreeString, VariantClear, VariantInit,
    };
    use winapi::um::wbemcli::{
        CLSID_WbemLocator, IEnumWbemClassObject, IID_IWbemLocator, IWbemClassObject, IWbemLocator,
        IWbemServices, WBEM_FLAG_FORWARD_ONLY, WBEM_FLAG_RETURN_IMMEDIATELY, WBEM_INFINITE,
    };

    const CLSCTX_INPROC_SERVER: DWORD = 1;
    const EOAC_NONE: DWORD = 0;

    fn to_wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    struct Bstr(BSTR);
    impl Bstr {
        fn new(s: &str) -> Self {
            let wide = to_wide(s);
            unsafe { Self(SysAllocString(wide.as_ptr())) }
        }
        fn as_ptr(&self) -> BSTR {
            self.0
        }
    }
    impl Drop for Bstr {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe { SysFreeString(self.0) };
            }
        }
    }

    struct ComScope;
    impl ComScope {
        fn new() -> Self {
            unsafe {
                CoInitializeEx(null_mut(), COINIT_MULTITHREADED);
            }
            Self
        }
    }
    impl Drop for ComScope {
        fn drop(&mut self) {
            unsafe {
                CoUninitialize();
            }
        }
    }

    struct AutoVariant(pub VARIANT);
    impl AutoVariant {
        fn new() -> Self {
            let mut v: VARIANT = unsafe { std::mem::zeroed() };
            unsafe { VariantInit(&mut v) };
            Self(v)
        }
        unsafe fn vt(&self) -> u32 {
            self.0.n1.n2().vt as u32
        }
        unsafe fn set_vt(&mut self, vt: u32) {
            self.0.n1.n2_mut().vt = vt as u16;
        }
    }
    impl Drop for AutoVariant {
        fn drop(&mut self) {
            unsafe {
                VariantClear(&mut self.0);
            }
        }
    }

    unsafe fn connect_wmi() -> Result<(*mut IWbemLocator, *mut IWbemServices), String> {
        let mut loc: *mut IWbemLocator = null_mut();
        let hr = CoCreateInstance(
            &CLSID_WbemLocator,
            null_mut(),
            CLSCTX_INPROC_SERVER,
            &IID_IWbemLocator,
            &mut loc as *mut *mut _ as *mut *mut _,
        );
        if !SUCCEEDED(hr) || loc.is_null() {
            return Err(format!("CoCreateInstance(CLSID_WbemLocator) failed: 0x{hr:08X}"));
        }

        let namespace = Bstr::new(r"ROOT\WMI");
        let mut svc: *mut IWbemServices = null_mut();
        let hr = (*loc).ConnectServer(
            namespace.as_ptr(),
            null_mut(),
            null_mut(),
            null_mut(),
            0,
            null_mut(),
            null_mut(),
            &mut svc,
        );
        if !SUCCEEDED(hr) || svc.is_null() {
            (*loc).Release();
            return Err(format!("ConnectServer(ROOT\\WMI) failed: 0x{hr:08X}"));
        }

        let hr = CoSetProxyBlanket(
            svc as *mut _,
            RPC_C_AUTHN_WINNT,
            RPC_C_AUTHZ_NONE,
            null_mut(),
            RPC_C_AUTHN_LEVEL_CALL,
            RPC_C_IMP_LEVEL_IMPERSONATE,
            null_mut(),
            EOAC_NONE,
        );
        if !SUCCEEDED(hr) {
            (*svc).Release();
            (*loc).Release();
            return Err(format!("CoSetProxyBlanket failed: 0x{hr:08X}"));
        }

        Ok((loc, svc))
    }

    pub fn set_wmi_brightness(brightness: u32) -> Result<(), String> {
        let _com = ComScope::new();
        unsafe {
            let (loc, svc) = connect_wmi()?;

            // Query active WmiMonitorBrightnessMethods
            let wql = Bstr::new("WQL");
            let query = Bstr::new("SELECT * FROM WmiMonitorBrightnessMethods WHERE Active = True");
            let mut enumerator: *mut IEnumWbemClassObject = null_mut();
            let hr = (*svc).ExecQuery(
                wql.as_ptr(),
                query.as_ptr(),
                (WBEM_FLAG_FORWARD_ONLY | WBEM_FLAG_RETURN_IMMEDIATELY) as i32,
                null_mut(),
                &mut enumerator,
            );
            if !SUCCEEDED(hr) || enumerator.is_null() {
                (*svc).Release();
                (*loc).Release();
                return Err(format!("ExecQuery failed: 0x{hr:08X}"));
            }

            let mut obj: *mut IWbemClassObject = null_mut();
            let mut returned: ULONG = 0;
            let hr = (*enumerator).Next(WBEM_INFINITE as i32, 1, &mut obj, &mut returned);
            (*enumerator).Release();

            if !SUCCEEDED(hr) || returned == 0 || obj.is_null() {
                (*svc).Release();
                (*loc).Release();
                return Err("No active WmiMonitorBrightnessMethods instance found".to_string());
            }

            // Get __RELPATH
            let mut path_var = AutoVariant::new();
            let prop_path = to_wide("__RELPATH");
            let hr = (*obj).Get(prop_path.as_ptr(), 0, &mut path_var.0, null_mut(), null_mut());
            (*obj).Release();

            if !SUCCEEDED(hr) || path_var.vt() != VT_BSTR {
                (*svc).Release();
                (*loc).Release();
                return Err(format!("Failed to retrieve __RELPATH: 0x{hr:08X}"));
            }
            let obj_path_bstr = *path_var.0.n1.n2().n3.bstrVal();

            // Get class and in-params definition
            let class_bstr = Bstr::new("WmiMonitorBrightnessMethods");
            let mut class_obj: *mut IWbemClassObject = null_mut();
            let hr = (*svc).GetObject(class_bstr.as_ptr(), 0, null_mut(), &mut class_obj, null_mut());
            if !SUCCEEDED(hr) || class_obj.is_null() {
                (*svc).Release();
                (*loc).Release();
                return Err(format!("GetObject(WmiMonitorBrightnessMethods) failed: 0x{hr:08X}"));
            }

            let method_name = to_wide("WmiSetBrightness");
            let mut in_params_def: *mut IWbemClassObject = null_mut();
            let hr = (*class_obj).GetMethod(method_name.as_ptr(), 0, &mut in_params_def, null_mut());
            (*class_obj).Release();

            if !SUCCEEDED(hr) || in_params_def.is_null() {
                (*svc).Release();
                (*loc).Release();
                return Err(format!("GetMethod(WmiSetBrightness) failed: 0x{hr:08X}"));
            }

            let mut in_params: *mut IWbemClassObject = null_mut();
            let hr = (*in_params_def).SpawnInstance(0, &mut in_params);
            (*in_params_def).Release();

            if !SUCCEEDED(hr) || in_params.is_null() {
                (*svc).Release();
                (*loc).Release();
                return Err(format!("SpawnInstance failed: 0x{hr:08X}"));
            }

            // Put Timeout = 1 (VT_I4)
            let prop_timeout = to_wide("Timeout");
            let mut var_timeout = AutoVariant::new();
            var_timeout.set_vt(VT_I4);
            *var_timeout.0.n1.n2_mut().n3.lVal_mut() = 1;
            let hr = (*in_params).Put(prop_timeout.as_ptr(), 0, &mut var_timeout.0, 0);
            if !SUCCEEDED(hr) {
                (*in_params).Release();
                (*svc).Release();
                (*loc).Release();
                return Err(format!("Put(Timeout) failed: 0x{hr:08X}"));
            }

            // Put Brightness = brightness (VT_UI1)
            let prop_brightness = to_wide("Brightness");
            let mut var_bright = AutoVariant::new();
            var_bright.set_vt(VT_UI1);
            *var_bright.0.n1.n2_mut().n3.bVal_mut() = brightness as u8;
            let hr = (*in_params).Put(prop_brightness.as_ptr(), 0, &mut var_bright.0, 0);
            if !SUCCEEDED(hr) {
                (*in_params).Release();
                (*svc).Release();
                (*loc).Release();
                return Err(format!("Put(Brightness) failed: 0x{hr:08X}"));
            }

            // ExecMethod
            let method_bstr = Bstr::new("WmiSetBrightness");
            let mut out_params: *mut IWbemClassObject = null_mut();
            let hr = (*svc).ExecMethod(
                obj_path_bstr,
                method_bstr.as_ptr(),
                0,
                null_mut(),
                in_params,
                &mut out_params,
                null_mut(),
            );
            (*in_params).Release();

            if !out_params.is_null() {
                (*out_params).Release();
            }
            (*svc).Release();
            (*loc).Release();

            if !SUCCEEDED(hr) {
                return Err(format!("ExecMethod(WmiSetBrightness) failed: 0x{hr:08X}"));
            }
            Ok(())
        }
    }

    pub fn query_wmi_brightness_and_token() -> Result<(u32, Option<String>), String> {
        let _com = ComScope::new();
        unsafe {
            let (loc, svc) = connect_wmi()?;

            let wql = Bstr::new("WQL");
            let query = Bstr::new("SELECT * FROM WmiMonitorBrightness WHERE Active = True");
            let mut enumerator: *mut IEnumWbemClassObject = null_mut();
            let hr = (*svc).ExecQuery(
                wql.as_ptr(),
                query.as_ptr(),
                (WBEM_FLAG_FORWARD_ONLY | WBEM_FLAG_RETURN_IMMEDIATELY) as i32,
                null_mut(),
                &mut enumerator,
            );
            if !SUCCEEDED(hr) || enumerator.is_null() {
                (*svc).Release();
                (*loc).Release();
                return Err(format!("ExecQuery failed: 0x{hr:08X}"));
            }

            let mut obj: *mut IWbemClassObject = null_mut();
            let mut returned: ULONG = 0;
            let hr = (*enumerator).Next(WBEM_INFINITE as i32, 1, &mut obj, &mut returned);
            (*enumerator).Release();

            if !SUCCEEDED(hr) || returned == 0 || obj.is_null() {
                (*svc).Release();
                (*loc).Release();
                return Err("No active WmiMonitorBrightness instance found".to_string());
            }

            let mut cur_var = AutoVariant::new();
            let prop_curr = to_wide("CurrentBrightness");
            let hr = (*obj).Get(prop_curr.as_ptr(), 0, &mut cur_var.0, null_mut(), null_mut());

            let mut inst_var = AutoVariant::new();
            let prop_inst = to_wide("InstanceName");
            let _ = (*obj).Get(prop_inst.as_ptr(), 0, &mut inst_var.0, null_mut(), null_mut());

            (*obj).Release();
            (*svc).Release();
            (*loc).Release();

            if !SUCCEEDED(hr) {
                return Err(format!("Failed to read CurrentBrightness: 0x{hr:08X}"));
            }

            let brightness = match cur_var.vt() {
                VT_UI1 => *cur_var.0.n1.n2().n3.bVal() as u32,
                VT_UI4 => *cur_var.0.n1.n2().n3.ulVal(),
                VT_I4 => *cur_var.0.n1.n2().n3.lVal() as u32,
                other => return Err(format!("Unexpected CurrentBrightness VARIANT type: {other}")),
            };

            let token = if inst_var.vt() == VT_BSTR {
                let bstr = *inst_var.0.n1.n2().n3.bstrVal();
                let len = winapi::um::oleauto::SysStringLen(bstr) as usize;
                let slice = std::slice::from_raw_parts(bstr, len);
                let s = String::from_utf16_lossy(slice);
                super::extract_token_from_instance(&s)
            } else {
                None
            };

            Ok((brightness, token))
        }
    }

    pub fn query_wmi_brightness() -> Result<u32, String> {
        query_wmi_brightness_and_token().map(|(b, _)| b)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamp_never_allows_black_screen() {
        assert_eq!(clamp_brightness_percent(0), MIN_BRIGHTNESS_PERCENT);
        assert_eq!(clamp_brightness_percent(1), MIN_BRIGHTNESS_PERCENT);
        assert_eq!(clamp_brightness_percent(MIN_BRIGHTNESS_PERCENT), MIN_BRIGHTNESS_PERCENT);
        assert_eq!(clamp_brightness_percent(50), 50);
        assert_eq!(clamp_brightness_percent(100), 100);
        assert_eq!(clamp_brightness_percent(150), 100);
        assert_eq!(clamp_brightness_percent(u32::MAX), 100);
    }

    #[test]
    fn percent_native_known_values() {
        assert_eq!(percent_to_native(100, 0, 100), 100);
        assert_eq!(percent_to_native(50, 0, 100), 50);
        assert_eq!(
            percent_to_native(MIN_BRIGHTNESS_PERCENT, 0, 100),
            MIN_BRIGHTNESS_PERCENT
        );
        // Monitors with VCP range 0-255 scale accurately
        assert_eq!(percent_to_native(100, 0, 255), 255);
        assert_eq!(percent_to_native(50, 0, 255), 128);
        assert_eq!(native_to_percent(255, 0, 255), 100);
        assert_eq!(native_to_percent(0, 0, 255), 0);
    }

    #[test]
    fn percent_native_roundtrip_within_1() {
        for (min, max) in [(0u32, 100u32), (0, 255), (20, 80), (0, 0)] {
            for p in [MIN_BRIGHTNESS_PERCENT, 25, 50, 75, 100] {
                let native = percent_to_native(p, min, max);
                let back = native_to_percent(native, min, max);
                assert!(
                    (back as i32 - p as i32).abs() <= 1,
                    "roundtrip p={p} min={min} max={max} native={native} back={back}"
                );
            }
        }
    }

    #[test]
    fn parse_ddc_index_ok_and_err() {
        assert_eq!(parse_ddc_index("ddc:0").unwrap(), 0);
        assert_eq!(parse_ddc_index("ddc:12").unwrap(), 12);
        assert!(parse_ddc_index("wmi:internal").is_err());
        assert!(parse_ddc_index("ddc:").is_err());
        assert!(parse_ddc_index("ddc:abc").is_err());
        assert!(parse_ddc_index("").is_err());
    }

    #[test]
    fn extract_token_handles_real_formats() {
        assert_eq!(
            extract_token_from_instance("DISPLAY\\MSI30D2\\5&1f9d8a86&0&UID4352_0"),
            Some("MSI30D2".to_string())
        );
        // Lowercase is still converted to uppercase to match DeviceID
        assert_eq!(
            extract_token_from_instance("DISPLAY\\cmn14d4\\5&1234&0_0"),
            Some("CMN14D4".to_string())
        );
        assert_eq!(extract_token_from_instance(""), None);
        assert_eq!(extract_token_from_instance("NO-BACKSLASH"), None);
        assert_eq!(extract_token_from_instance("DISPLAY\\\\5&x"), None);
    }

    /// Run `cargo test -- --nocapture` to inspect detected monitors on the active machine.
    #[test]
    fn debug_list_monitors() {
        println!("WMI IDs:");
        for id in query_wmi_monitor_ids() {
            println!(
                "  token={} manufacturer='{}' friendly='{}' serial='{}'",
                id.token, id.manufacturer, id.friendly, id.serial
            );
        }
        println!("WMI internal token: {:?}", query_wmi_internal_token());
        match list_monitors_blocking() {
            Ok(list) => {
                println!("FOUND {} monitor(s)", list.len());
                for m in &list {
                    println!(
                        "MONITOR id={} name='{}' kind={} current={} capable={} detail='{}'",
                        m.id, m.name, m.kind, m.current, m.capable, m.detail
                    );
                }
            }
            Err(e) => println!("LIST ERROR: {e}"),
        }
    }

    #[test]
    fn test_native_wmi_get_and_set() {
        let (cur, token) = wmi_native::query_wmi_brightness_and_token().expect("native WMI read failed");
        println!("NATIVE WMI current brightness: {cur}%, token: {token:?}");
        let start = std::time::Instant::now();
        wmi_native::set_wmi_brightness(cur).expect("native WMI set failed");
        let elapsed = start.elapsed();
        println!("NATIVE WMI set brightness took: {elapsed:?}");
    }

    #[test]
    fn test_active_hmonitors() {
        #[cfg(target_os = "windows")]
        {
            use winapi::shared::minwindef::{BOOL, DWORD, LPARAM, TRUE};
            use winapi::shared::windef::{HDC, HMONITOR, LPRECT};
            use winapi::um::wingdi::DISPLAY_DEVICEW;
            use winapi::um::winuser::{
                EnumDisplayDevicesW, EnumDisplayMonitors, GetMonitorInfoW, LPMONITORINFO, MONITORINFOEXW,
            };

            unsafe extern "system" fn cb(hmon: HMONITOR, _hdc: HDC, _rect: LPRECT, lparam: LPARAM) -> BOOL {
                let list = &mut *(lparam as *mut Vec<HMONITOR>);
                list.push(hmon);
                TRUE
            }

            unsafe {
                let mut list: Vec<HMONITOR> = Vec::new();
                EnumDisplayMonitors(std::ptr::null_mut(), std::ptr::null(), Some(cb), &mut list as *mut _ as LPARAM);
                println!("ACTIVE HMONITORS COUNT: {}", list.len());
                for (i, hmon) in list.iter().enumerate() {
                    let mut mi: MONITORINFOEXW = std::mem::zeroed();
                    mi.cbSize = std::mem::size_of::<MONITORINFOEXW>() as DWORD;
                    if GetMonitorInfoW(*hmon, &mut mi as *mut _ as LPMONITORINFO) != 0 {
                        let sz_device = String::from_utf16_lossy(&mi.szDevice);
                        let clean_dev = sz_device.trim_matches('\0');
                        let mut dd: DISPLAY_DEVICEW = std::mem::zeroed();
                        dd.cb = std::mem::size_of::<DISPLAY_DEVICEW>() as DWORD;
                        if EnumDisplayDevicesW(mi.szDevice.as_ptr(), 0, &mut dd, 0) != 0 {
                            let dev_id = String::from_utf16_lossy(&dd.DeviceID);
                            let clean_id = dev_id.trim_matches('\0');
                            println!("HMONITOR #{i}: device='{clean_dev}', id='{clean_id}', StateFlags=0x{:X}", dd.StateFlags);
                        } else {
                            println!("HMONITOR #{i}: device='{clean_dev}' (EnumDisplayDevicesW failed)");
                        }
                    }
                }

                println!("--- ALL ADAPTERS & MONITORS VIA EnumDisplayDevicesW ---");
                let mut dev_idx = 0;
                loop {
                    let mut adapter: DISPLAY_DEVICEW = std::mem::zeroed();
                    adapter.cb = std::mem::size_of::<DISPLAY_DEVICEW>() as DWORD;
                    if EnumDisplayDevicesW(std::ptr::null(), dev_idx, &mut adapter, 0) == 0 {
                        break;
                    }
                    let ad_name = String::from_utf16_lossy(&adapter.DeviceName);
                    let clean_ad = ad_name.trim_matches('\0');
                    println!("Adapter #{dev_idx}: name='{clean_ad}', flags=0x{:X}", adapter.StateFlags);

                    let mut mon_idx = 0;
                    loop {
                        let mut mon: DISPLAY_DEVICEW = std::mem::zeroed();
                        mon.cb = std::mem::size_of::<DISPLAY_DEVICEW>() as DWORD;
                        if EnumDisplayDevicesW(adapter.DeviceName.as_ptr(), mon_idx, &mut mon, 0) == 0 {
                            break;
                        }
                        let mon_id = String::from_utf16_lossy(&mon.DeviceID);
                        let clean_mid = mon_id.trim_matches('\0');
                        let mon_str = String::from_utf16_lossy(&mon.DeviceString);
                        let clean_str = mon_str.trim_matches('\0');
                        println!("  Mon #{mon_idx}: name='{clean_str}', id='{clean_mid}', flags=0x{:X}", mon.StateFlags);
                        mon_idx += 1;
                    }
                    dev_idx += 1;
                }
            }
        }
    }

}
