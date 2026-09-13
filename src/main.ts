import { invoke } from "@tauri-apps/api/core";
import { LogicalSize, PhysicalPosition } from "@tauri-apps/api/dpi";
import { emit, listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";

/** Backend event emitted when brightness changes via tray menu, flyout, or another window */
const TRAY_CHANGED_EVENT = "tray-brightness-changed";

const isFlyout = getCurrentWindow().label === "flyout";
if (isFlyout) {
  document.documentElement.classList.add("flyout");
}

interface MonitorInfo {
  id: string;
  name: string;
  kind: string;
  min: number;
  max: number;
  current: number;
  capable: boolean;
  detail: string;
}

let monitors: MonitorInfo[] = [];
const pendingTimers = new Map<string, number>();
let trayStatusTimer: number | null = null;
let mainStatusTimer: number | null = null;

const STEP = 5;
const PRESETS = [25, 50, 75, 100];

function el<T extends HTMLElement>(id: string): T | null {
  return document.querySelector<T>(`#${id}`);
}

/** Native WMI (COM) and DDC calls both respond in ~15ms, so 80ms debounce ensures immediate response while dragging sliders. */
function debounceFor(_m: MonitorInfo): number {
  return 80;
}

function clampUi(v: number, min: number, max: number): number {
  if (Number.isNaN(v)) return min;
  return Math.min(max, Math.max(min, Math.round(v)));
}

// =========================================================
// THEME PREFERENCES (AUTO / LIGHT / DARK)
// =========================================================

const THEME_CHANGED_EVENT = "app-theme-changed";

type ThemePref = "system" | "light" | "dark";
const THEME_STORAGE_KEY = "brightness_control_theme";

function getSavedTheme(): ThemePref {
  try {
    const saved = localStorage.getItem(THEME_STORAGE_KEY);
    if (saved === "light" || saved === "dark" || saved === "system") {
      return saved;
    }
  } catch {
    // Ignore storage errors
  }
  return "system";
}

function updateFlyoutThemeBtn(theme: ThemePref, effectiveTheme: "light" | "dark"): void {
  const btn = el<HTMLButtonElement>("tray-theme-btn");
  if (!btn) return;

  const modeName =
    theme === "system"
      ? `Auto (${effectiveTheme === "dark" ? "Dark" : "Light"})`
      : theme === "light"
      ? "Light"
      : "Dark";
  btn.title = `Theme: ${modeName}. Click to change.`;

  if (effectiveTheme === "dark") {
    // In dark mode, show sun icon to allow toggling to light
    btn.innerHTML = `
      <svg viewBox="0 0 24 24" width="14" height="14" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
        <circle cx="12" cy="12" r="4"></circle>
        <line x1="12" y1="2" x2="12" y2="4"></line>
        <line x1="12" y1="20" x2="12" y2="22"></line>
        <line x1="4.93" y1="4.93" x2="6.34" y2="6.34"></line>
        <line x1="17.66" y1="17.66" x2="19.07" y2="19.07"></line>
        <line x1="2" y1="12" x2="4" y2="12"></line>
        <line x1="20" y1="12" x2="22" y2="12"></line>
        <line x1="6.34" y1="17.66" x2="4.93" y2="19.07"></line>
        <line x1="19.07" y1="4.93" x2="17.66" y2="6.34"></line>
      </svg>
    `;
  } else {
    // In light mode, show moon icon to allow toggling to dark
    btn.innerHTML = `
      <svg viewBox="0 0 24 24" width="14" height="14" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
        <path d="M21 12.79A9 9 0 1 1 11.21 3 7 7 0 0 0 21 12.79z"></path>
      </svg>
    `;
  }
}

function applyTheme(theme: ThemePref, broadcast = true): void {
  const isSystemDark = window.matchMedia("(prefers-color-scheme: dark)").matches;
  const effectiveTheme: "light" | "dark" = theme === "system" ? (isSystemDark ? "dark" : "light") : theme;

  document.documentElement.setAttribute("data-theme", effectiveTheme);

  // Update theme pill buttons in main window
  document.querySelectorAll<HTMLButtonElement>(".theme-pill").forEach((pill) => {
    const val = pill.getAttribute("data-theme-val");
    if (val === theme) {
      pill.classList.add("active");
    } else {
      pill.classList.remove("active");
    }
  });

  // Update flyout theme toggle button
  updateFlyoutThemeBtn(theme, effectiveTheme);

  try {
    localStorage.setItem(THEME_STORAGE_KEY, theme);
  } catch {
    // Ignore storage errors
  }

  if (broadcast) {
    void emit(THEME_CHANGED_EVENT, { theme, effectiveTheme });
    void invoke("set_tray_theme", { theme: effectiveTheme }).catch(() => {});
  }
}

// React to OS dark/light changes when set to auto/system
window.matchMedia("(prefers-color-scheme: dark)").addEventListener("change", () => {
  if (getSavedTheme() === "system") {
    applyTheme("system", true);
  }
});

// React to cross-window theme changes (e.g. settings changed in main window, sync to flyout)
void listen<{ theme: ThemePref; effectiveTheme: string }>(THEME_CHANGED_EVENT, (event) => {
  if (event.payload && event.payload.theme) {
    applyTheme(event.payload.theme, false);
  }
});

// React to storage events across webview processes
window.addEventListener("storage", (e) => {
  if (e.key === THEME_STORAGE_KEY) {
    applyTheme(getSavedTheme(), false);
  }
});

// Automatically re-verify theme whenever window regains focus
window.addEventListener("focus", () => {
  applyTheme(getSavedTheme(), false);
});

// Apply saved theme immediately on script load
applyTheme(getSavedTheme());

// =========================================================
// CUSTOM MONITOR NAMES (LOCAL PERSISTENCE)
// =========================================================

const NAMES_STORAGE_KEY = "brightness_control_monitor_names";

function getCustomNames(): Record<string, string> {
  try {
    const raw = localStorage.getItem(NAMES_STORAGE_KEY);
    return raw ? JSON.parse(raw) : {};
  } catch {
    return {};
  }
}

function setCustomName(id: string, name: string): void {
  try {
    const names = getCustomNames();
    const trimmed = name.trim();
    if (!trimmed) {
      delete names[id];
    } else {
      names[id] = trimmed;
    }
    localStorage.setItem(NAMES_STORAGE_KEY, JSON.stringify(names));
  } catch (e) {
    console.error("Failed to save custom monitor name:", e);
  }
}

function getMonitorDisplayName(m: MonitorInfo): string {
  const names = getCustomNames();
  return names[m.id]?.trim() || m.name;
}

// =========================================================
// MASTER SLIDER (ALL DISPLAYS) PREFERENCE
// =========================================================

const MASTER_SLIDER_STORAGE_KEY = "brightness_control_show_master";

function isMasterSliderEnabled(): boolean {
  try {
    return localStorage.getItem(MASTER_SLIDER_STORAGE_KEY) === "true";
  } catch {
    return false;
  }
}

function setMasterSliderEnabled(enabled: boolean): void {
  try {
    localStorage.setItem(MASTER_SLIDER_STORAGE_KEY, enabled ? "true" : "false");
  } catch (e) {
    console.error("Failed to save master slider preference:", e);
  }
}

function syncMasterSliderUi(enabled: boolean): void {
  const checkbox = el<HTMLInputElement>("master-slider-checkbox");
  if (checkbox && checkbox.checked !== enabled) {
    checkbox.checked = enabled;
  }
}

// =========================================================
// STATUS NOTIFICATIONS
// =========================================================

function showMainError(msg: string): void {
  const banner = el<HTMLElement>("error-banner");
  const errText = el<HTMLElement>("error");
  if (!banner || !errText) return;

  if (msg) {
    errText.textContent = msg;
    banner.style.display = "flex";
  } else {
    errText.textContent = "";
    banner.style.display = "none";
  }
}

function setMainStatus(msg: string, type: "ready" | "busy" | "error" = "ready"): void {
  const st = el<HTMLElement>("status");
  const dot = el<HTMLElement>("status-indicator");
  if (st) st.textContent = msg;
  if (dot) {
    dot.className = `status-indicator ${type}`;
  }

  if (mainStatusTimer !== null) {
    window.clearTimeout(mainStatusTimer);
    mainStatusTimer = null;
  }

  // Automatically restore ready state after 3s if reporting a successful action
  if (type === "ready" && msg.includes("✓")) {
    mainStatusTimer = window.setTimeout(() => {
      const capableCount = monitors.filter((m) => m.capable).length;
      if (st) st.textContent = `Ready (${capableCount} active display${capableCount !== 1 ? "s" : ""})`;
      if (dot) dot.className = "status-indicator ready";
    }, 3000);
  }
}

function setTrayStatus(msg: string, autoClearMs = 2500): void {
  const st = el<HTMLElement>("tray-status");
  if (!st) return;
  st.textContent = msg;
  if (trayStatusTimer !== null) {
    window.clearTimeout(trayStatusTimer);
    trayStatusTimer = null;
  }
  if (autoClearMs > 0 && msg) {
    trayStatusTimer = window.setTimeout(() => {
      if (st.textContent === msg) st.textContent = "";
    }, autoClearMs);
  }
}

// =========================================================
// BRIGHTNESS ADJUSTMENTS (APPLY / SCHEDULE)
// =========================================================

function scheduleSet(m: MonitorInfo, value: number): void {
  const prev = pendingTimers.get(m.id);
  if (prev !== undefined) window.clearTimeout(prev);
  const timer = window.setTimeout(
    () => {
      pendingTimers.delete(m.id);
      void applyBrightness(m, value);
    },
    debounceFor(m),
  );
  pendingTimers.set(m.id, timer);
}

function flushSet(m: MonitorInfo, value: number): void {
  const prev = pendingTimers.get(m.id);
  if (prev !== undefined) {
    window.clearTimeout(prev);
    pendingTimers.delete(m.id);
    void applyBrightness(m, value);
  } else if (value !== m.current) {
    void applyBrightness(m, value);
  }
}

async function applyBrightness(m: MonitorInfo, value: number): Promise<void> {
  const v = clampUi(value, m.min, m.max);
  if (isFlyout) {
    setTrayStatus(`Setting ${v}%…`, 0);
  } else {
    setMainStatus(`Setting ${m.name} → ${v}%…`, "busy");
    showMainError("");
  }

  try {
    await invoke("set_brightness", { id: m.id, value: v });
    m.current = v;
    if (isFlyout) {
      setTrayStatus(`${v}% ✓`);
      refreshTrayMaster();
    } else {
      setMainStatus(`${m.name}: ${v}% ✓`, "ready");
      refreshMaster();
      updateActivePresetChips(m.id, v);
    }
  } catch (e) {
    const msg = e instanceof Error ? e.message : String(e);
    if (isFlyout) {
      setTrayStatus(`Error: ${msg}`);
    } else {
      showMainError(`Error adjusting ${m.name}: ${msg}`);
      setMainStatus("Error adjusting brightness", "error");
    }
    await refreshOne(m.id);
  }
}

async function refreshOne(id: string): Promise<void> {
  try {
    const v = await invoke<number>("get_brightness", { id });
    const m = monitors.find((x) => x.id === id);
    if (m) {
      m.current = v;
      if (isFlyout) {
        syncTrayInputs(m);
        refreshTrayMaster();
      } else {
        syncMainInputs(m);
        refreshMaster();
      }
    }
  } catch {
    // Ignore error on single refresh
  }
}

function updateActivePresetChips(monitorId: string, currentVal: number): void {
  const chips = document.querySelectorAll<HTMLButtonElement>(
    `[data-preset-monitor="${monitorId}"]`,
  );
  chips.forEach((chip) => {
    const val = Number(chip.getAttribute("data-preset-val"));
    if (val === currentVal) {
      chip.classList.add("active");
    } else {
      chip.classList.remove("active");
    }
  });
}

function updateSliderVisual(
  slider: HTMLInputElement | null,
  value: number,
  min = 10,
  max = 100,
): void {
  if (!slider) return;
  const clamped = Math.max(min, Math.min(max, value));
  const pct = max <= min ? clamped : ((clamped - min) / (max - min)) * 100;
  slider.style.setProperty("--slider-pct", `${pct}%`);
}

function attachWheelListener(
  target: HTMLElement,
  onAdjust: (delta: number) => void,
): void {
  target.addEventListener(
    "wheel",
    (e: WheelEvent) => {
      e.preventDefault();
      // Holding Shift allows fine 2% adjustment; normal wheel is 5%
      const step = e.shiftKey ? 2 : STEP;
      const delta = e.deltaY < 0 ? step : -step;
      onAdjust(delta);
    },
    { passive: false },
  );
}

function syncMainInputs(m: MonitorInfo): void {
  const slider = document.querySelector<HTMLInputElement>(
    `input[data-monitor-id="${m.id}"]`,
  );
  const pct = document.querySelector<HTMLElement>(
    `[data-monitor-pct="${m.id}"]`,
  );
  if (slider && document.activeElement !== slider) slider.value = String(m.current);
  if (slider) updateSliderVisual(slider, m.current, m.min, m.max);
  if (pct) pct.textContent = m.capable ? `${m.current}%` : "Unsupported";
  if (m.capable) updateActivePresetChips(m.id, m.current);

  const nameSpan = document.querySelector<HTMLElement>(`[data-rename-name="${m.id}"]`);
  const nameInput = document.querySelector<HTMLInputElement>(`[data-rename-input="${m.id}"]`);
  if (nameSpan && nameInput && nameInput.style.display === "none") {
    const displayName = getMonitorDisplayName(m);
    nameSpan.textContent = displayName;
    nameInput.value = displayName;
  }
}

function syncTrayInputs(m: MonitorInfo): void {
  if (!m.capable) return;
  const slider = document.querySelector<HTMLInputElement>(
    `input[data-tray-id="${m.id}"]`,
  );
  const pct = document.querySelector<HTMLElement>(
    `[data-tray-pct="${m.id}"]`,
  );
  if (slider && document.activeElement !== slider) slider.value = String(m.current);
  if (slider) updateSliderVisual(slider, m.current, m.min, m.max);
  if (pct) pct.textContent = `${m.current}%`;

  const nameSpan = document.querySelector<HTMLElement>(`[data-item-id="${m.id}"] .tray-item-name`);
  if (nameSpan) {
    const displayName = getMonitorDisplayName(m);
    nameSpan.textContent = displayName;
    nameSpan.title = displayName;
  }
}

// =========================================================
// COMMON SVG ICONS FOR MAIN WINDOW & TRAY FLYOUT
// =========================================================

const SVG_LAPTOP = `<svg class="device-icon" viewBox="0 0 24 24" width="16" height="16" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect x="3" y="4" width="18" height="12" rx="2"></rect><line x1="2" y1="20" x2="22" y2="20"></line></svg>`;
const SVG_MONITOR = `<svg class="device-icon" viewBox="0 0 24 24" width="16" height="16" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect x="2" y="3" width="20" height="14" rx="2"></rect><line x1="8" y1="21" x2="16" y2="21"></line><line x1="12" y1="17" x2="12" y2="21"></line></svg>`;
const SVG_ALL_MONITORS = `<svg class="device-icon" viewBox="0 0 24 24" width="16" height="16" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect width="18" height="12" x="4" y="3" rx="2"></rect><path d="M2 17h20"></path><path d="M6 21h12"></path></svg>`;

const SVG_SUN_DIM = `<svg class="sun-dim" viewBox="0 0 24 24" width="14" height="14" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="3"></circle><line x1="12" y1="3" x2="12" y2="5"></line><line x1="12" y1="19" x2="12" y2="21"></line><line x1="3" y1="12" x2="5" y2="12"></line><line x1="19" y1="12" x2="21" y2="12"></line></svg>`;
const SVG_SUN_BRIGHT = `<svg class="sun-bright" viewBox="0 0 24 24" width="16" height="16" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="4"></circle><line x1="12" y1="2" x2="12" y2="4"></line><line x1="12" y1="20" x2="12" y2="22"></line><line x1="4.22" y1="4.22" x2="5.64" y2="5.64"></line><line x1="18.36" y1="18.36" x2="19.78" y2="19.78"></line><line x1="2" y1="12" x2="4" y2="12"></line><line x1="20" y1="12" x2="22" y2="12"></line><line x1="4.22" y1="19.78" x2="5.64" y2="18.36"></line><line x1="18.36" y1="5.64" x2="19.78" y2="4.22"></line></svg>`;

// =========================================================
// SYSTEM TRAY FLYOUT VIEW
// =========================================================

function refreshTrayMaster(): void {
  if (!isMasterSliderEnabled()) return;
  const activeCapable = monitors.filter((m) => m.capable);
  const master = el<HTMLInputElement>("tray-master-slider");
  const label = el<HTMLElement>("tray-master-val");
  if (!master || activeCapable.length < 2) return;
  const avg = Math.round(
    activeCapable.reduce((s, m) => s + m.current, 0) / activeCapable.length,
  );
  if (document.activeElement !== master) master.value = String(avg);
  updateSliderVisual(master, avg, Number(master.min), 100);
  if (label) label.textContent = `${avg}%`;
}

function renderTrayMaster(host: HTMLElement): void {
  const activeCapable = monitors.filter((m) => m.capable);
  if (activeCapable.length < 2) return;

  const min = Math.max(...activeCapable.map((m) => m.min));
  const avg = Math.round(
    activeCapable.reduce((s, m) => s + m.current, 0) / activeCapable.length,
  );

  const card = document.createElement("div");
  card.className = "tray-item tray-master-item";

  card.innerHTML = `
    <div class="tray-item-header">
      <div class="tray-item-info">
        ${SVG_ALL_MONITORS}
        <span class="tray-item-name">All Displays</span>
      </div>
      <span class="tray-item-pct" id="tray-master-val">${avg}%</span>
    </div>
    <div class="tray-slider-row">
      ${SVG_SUN_DIM}
      <input type="range" id="tray-master-slider" class="tray-slider" min="${min}" max="100" step="1" value="${avg}" aria-label="Brightness for all displays" />
      ${SVG_SUN_BRIGHT}
    </div>
  `;

  const slider = card.querySelector<HTMLInputElement>("#tray-master-slider")!;
  const valLabel = card.querySelector<HTMLElement>("#tray-master-val")!;
  updateSliderVisual(slider, avg, min, 100);

  const applyToAll = (v: number, immediate: boolean): void => {
    valLabel.textContent = `${v}%`;
    updateSliderVisual(slider, v, min, 100);
    for (const m of activeCapable) {
      const target = clampUi(v, m.min, m.max);
      const s = document.querySelector<HTMLInputElement>(
        `input[data-tray-id="${m.id}"]`,
      );
      const p = document.querySelector<HTMLElement>(
        `[data-tray-pct="${m.id}"]`,
      );
      if (s) {
        s.value = String(target);
        updateSliderVisual(s, target, m.min, m.max);
      }
      if (p) p.textContent = `${target}%`;
      if (immediate) flushSet(m, target);
      else scheduleSet(m, target);
    }
  };

  slider.addEventListener("input", () => {
    applyToAll(Number(slider.value), false);
  });
  slider.addEventListener("change", () => {
    applyToAll(Number(slider.value), true);
  });

  attachWheelListener(card, (delta) => {
    const cur = Number(slider.value);
    const target = clampUi(cur + delta, min, 100);
    slider.value = String(target);
    applyToAll(target, true);
  });

  host.appendChild(card);
}

function renderTrayMonitors(): void {
  const list = el<HTMLElement>("tray-monitor-list");
  if (!list) return;
  list.innerHTML = "";

  if (monitors.length === 0) {
    list.innerHTML = `<p class="tray-loading">No displays found.</p>`;
    autoSizeFlyout(0);
    return;
  }

  const activeMonitors = monitors.filter((m) => m.capable);

  if (isMasterSliderEnabled() && activeMonitors.length >= 2) {
    renderTrayMaster(list);
  }

  // Render monitors with brightness sliders
  for (const m of activeMonitors) {
    const card = document.createElement("div");
    card.className = "tray-item";
    card.setAttribute("data-item-id", m.id);

    const deviceIcon = m.kind === "wmi" ? SVG_LAPTOP : SVG_MONITOR;
    const displayName = getMonitorDisplayName(m);

    card.innerHTML = `
      <div class="tray-item-header">
        <div class="tray-item-info">
          ${deviceIcon}
          <span class="tray-item-name" title="${displayName}">${displayName}</span>
        </div>
        <span class="tray-item-pct" data-tray-pct="${m.id}">${m.current}%</span>
      </div>
      <div class="tray-slider-row">
        ${SVG_SUN_DIM}
        <input
          type="range"
          class="tray-slider"
          min="${m.min}"
          max="${m.max}"
          step="1"
          value="${clampUi(m.current, m.min, m.max)}"
          data-tray-id="${m.id}"
          aria-label="${m.name} brightness"
        />
        ${SVG_SUN_BRIGHT}
      </div>
    `;

    const slider = card.querySelector<HTMLInputElement>(`input[data-tray-id="${m.id}"]`)!;
    const pct = card.querySelector<HTMLElement>(`[data-tray-pct="${m.id}"]`)!;
    updateSliderVisual(slider, clampUi(m.current, m.min, m.max), m.min, m.max);

    slider.addEventListener("input", () => {
      const v = Number(slider.value);
      pct.textContent = `${v}%`;
      updateSliderVisual(slider, v, m.min, m.max);
      scheduleSet(m, v);
      refreshTrayMaster();
    });

    slider.addEventListener("change", () => {
      const v = Number(slider.value);
      pct.textContent = `${v}%`;
      updateSliderVisual(slider, v, m.min, m.max);
      flushSet(m, v);
      refreshTrayMaster();
    });

    attachWheelListener(card, (delta) => {
      const cur = Number(slider.value);
      const target = clampUi(cur + delta, m.min, m.max);
      slider.value = String(target);
      pct.textContent = `${target}%`;
      updateSliderVisual(slider, target, m.min, m.max);
      flushSet(m, target);
      refreshTrayMaster();
    });

    list.appendChild(card);
  }

  const hasMaster = isMasterSliderEnabled() && activeMonitors.length >= 2;
  autoSizeFlyout(activeMonitors.length + (hasMaster ? 1 : 0));
}

async function autoSizeFlyout(activeItems: number): Promise<void> {
  if (!isFlyout) return;
  if (activeItems === 0) {
    void getCurrentWindow().setSize(new LogicalSize(350, 155)).catch(() => {});
    return;
  }
  const h = Math.max(160, Math.round(95 + activeItems * 72));

  try {
    const win = getCurrentWindow();
    const currentPos = await win.outerPosition();
    const currentSize = await win.outerSize();
    const scale = await win.scaleFactor();

    const newPhysicalH = Math.round(h * scale);
    const deltaH = newPhysicalH - currentSize.height;

    if (Math.abs(deltaH) > 2) {
      const newY = currentPos.y - deltaH;
      await win.setSize(new LogicalSize(350, h));
      await win.setPosition(new PhysicalPosition(currentPos.x, newY));
    }
  } catch {
    void getCurrentWindow().setSize(new LogicalSize(350, h)).catch(() => {});
  }
}

// =========================================================
// MAIN WINDOW VIEW
// =========================================================

function kindBadgeTag(kind: string): string {
  if (kind === "wmi") {
    return `<span class="badge-tag laptop">Laptop</span>`;
  }
  return `<span class="badge-tag ddc">DDC/CI</span>`;
}

function refreshMaster(): void {
  if (!isMasterSliderEnabled()) return;
  const activeCapable = monitors.filter((m) => m.capable);
  const master = el<HTMLInputElement>("master-slider");
  const label = el<HTMLElement>("master-val");
  if (!master || activeCapable.length < 2) return;
  const avg = Math.round(
    activeCapable.reduce((s, m) => s + m.current, 0) / activeCapable.length,
  );
  if (document.activeElement !== master) master.value = String(avg);
  updateSliderVisual(master, avg, Number(master.min), 100);
  if (label) label.textContent = `${avg}%`;
  updateActivePresetChips("master", avg);
}

function renderMaster(host: HTMLElement): void {
  const activeCapable = monitors.filter((m) => m.capable);
  if (activeCapable.length < 2) return;

  const min = Math.max(...activeCapable.map((m) => m.min));
  const avg = Math.round(
    activeCapable.reduce((s, m) => s + m.current, 0) / activeCapable.length,
  );

  const card = document.createElement("section");
  card.className = "monitor-card master-card";
  card.id = "card-master";

  const presetChipsHtml = PRESETS.map(
    (p) =>
      `<button type="button" class="preset-chip ${p === avg ? "active" : ""}" data-preset-monitor="master" data-preset-val="${p}">${p}%</button>`,
  ).join("");

  card.innerHTML = `
    <div class="monitor-card-header">
      <div class="monitor-identity">
        ${SVG_ALL_MONITORS}
        <span class="monitor-name">All Displays</span>
        <span class="badge-tag master">${activeCapable.length} displays</span>
      </div>
      <div class="percent-stepper">
        <button type="button" class="step-btn" id="master-step-down" title="Decrease 5%" aria-label="Decrease 5%">−</button>
        <span class="percent-val" id="master-val">${avg}%</span>
        <button type="button" class="step-btn" id="master-step-up" title="Increase 5%" aria-label="Increase 5%">+</button>
        <button type="button" class="master-hide-btn" id="master-hide-btn" title="Hide All Displays slider (Re-enable in Settings ⚙️)" aria-label="Hide All Displays slider">✕</button>
      </div>
    </div>
    <div class="slider-container">
      ${SVG_SUN_DIM}
      <input
        type="range"
        id="master-slider"
        class="main-slider"
        min="${min}"
        max="100"
        step="1"
        value="${avg}"
        aria-label="Brightness for all displays"
      />
      ${SVG_SUN_BRIGHT}
    </div>
    <div class="monitor-card-footer">
      <span class="monitor-detail-text">Adjust all displays simultaneously</span>
      <div class="preset-chips">
        ${presetChipsHtml}
      </div>
    </div>
  `;

  const slider = card.querySelector<HTMLInputElement>("#master-slider")!;
  const label = card.querySelector<HTMLElement>("#master-val")!;
  updateSliderVisual(slider, avg, min, 100);

  const applyToAll = (v: number, immediate: boolean): void => {
    label.textContent = `${v}%`;
    updateSliderVisual(slider, v, min, 100);
    updateActivePresetChips("master", v);
    for (const m of activeCapable) {
      const target = clampUi(v, m.min, m.max);
      const s = document.querySelector<HTMLInputElement>(
        `input[data-monitor-id="${m.id}"]`,
      );
      const p = document.querySelector<HTMLElement>(
        `[data-monitor-pct="${m.id}"]`,
      );
      if (s) {
        s.value = String(target);
        updateSliderVisual(s, target, m.min, m.max);
      }
      if (p) p.textContent = `${target}%`;
      updateActivePresetChips(m.id, target);

      if (immediate) flushSet(m, target);
      else scheduleSet(m, target);
    }
  };

  slider.addEventListener("input", () => {
    applyToAll(Number(slider.value), false);
  });
  slider.addEventListener("change", () => {
    applyToAll(Number(slider.value), true);
  });

  // Quick step buttons ±5%
  card.querySelector("#master-step-down")?.addEventListener("click", () => {
    const cur = Number(slider.value);
    const target = clampUi(cur - STEP, min, 100);
    slider.value = String(target);
    applyToAll(target, true);
  });

  card.querySelector("#master-step-up")?.addEventListener("click", () => {
    const cur = Number(slider.value);
    const target = clampUi(cur + STEP, min, 100);
    slider.value = String(target);
    applyToAll(target, true);
  });

  // Hide button on master card
  card.querySelector("#master-hide-btn")?.addEventListener("click", () => {
    setMasterSliderEnabled(false);
    syncMasterSliderUi(false);
    renderMonitors();
    setMainStatus("All Displays slider hidden (re-enable in Settings ⚙️)", "ready");
  });

  // Preset chips (25%, 50%, 75%, 100%)
  card.querySelectorAll<HTMLButtonElement>('[data-preset-monitor="master"]').forEach((chip) => {
    chip.addEventListener("click", () => {
      const target = Number(chip.getAttribute("data-preset-val"));
      slider.value = String(target);
      applyToAll(target, true);
    });
  });

  // Wheel listener: scroll anywhere on Master Card to adjust all screens
  attachWheelListener(card, (delta) => {
    const cur = Number(slider.value);
    const target = clampUi(cur + delta, min, 100);
    slider.value = String(target);
    applyToAll(target, true);
  });

  host.appendChild(card);
}

function renderMonitors(): void {
  const list = el<HTMLElement>("monitor-list");
  if (!list) return;
  list.innerHTML = "";

  if (monitors.length === 0) {
    list.innerHTML = `
      <div class="loading-state">
        <p class="loading-text">No controllable displays found.</p>
      </div>
    `;
    return;
  }

  const activeMonitors = monitors.filter((m) => m.capable);
  const activeCount = activeMonitors.length;

  // Master card if >= 2 controllable displays AND enabled by user
  if (isMasterSliderEnabled() && activeCount >= 2) {
    renderMaster(list);
  }

  for (const m of monitors) {
    const card = document.createElement("section");
    card.className = "monitor-card";
    card.id = `card-${m.id.replace(":", "-")}`;
    card.setAttribute("data-card-id", m.id);

    const deviceIcon = m.kind === "wmi" ? SVG_LAPTOP : SVG_MONITOR;
    const displayName = getMonitorDisplayName(m);

    let badgeTag = "";
    if (m.capable) {
      badgeTag = kindBadgeTag(m.kind);
    } else {
      badgeTag = `<span class="badge-tag incapable">Unsupported</span>`;
    }

    if (m.capable) {
      const presetChipsHtml = PRESETS.map(
        (p) =>
          `<button type="button" class="preset-chip ${p === m.current ? "active" : ""}" data-preset-monitor="${m.id}" data-preset-val="${p}">${p}%</button>`,
      ).join("");

      card.innerHTML = `
        <div class="monitor-card-header">
          <div class="monitor-identity">
            ${deviceIcon}
            <div class="monitor-title-box">
              <span class="monitor-name" data-rename-name="${m.id}" title="Click to rename display">${displayName}</span>
              <input type="text" class="monitor-name-input" data-rename-input="${m.id}" value="${displayName}" maxlength="35" style="display: none;" spellcheck="false" />
              <button type="button" class="rename-btn" data-rename-btn="${m.id}" title="Rename display" aria-label="Rename display">
                <svg viewBox="0 0 24 24" width="12" height="12" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
                  <path d="M17 3a2.828 2.828 0 1 1 4 4L7.5 20.5 2 22l1.5-5.5L17 3z"></path>
                </svg>
              </button>
            </div>
            ${badgeTag}
          </div>
          <div class="card-header-actions">
            <div class="percent-stepper">
              <button type="button" class="step-btn" data-step-down="${m.id}" title="Decrease 5%" aria-label="Decrease 5%">−</button>
              <span class="percent-val" data-monitor-pct="${m.id}">${m.current}%</span>
              <button type="button" class="step-btn" data-step-up="${m.id}" title="Increase 5%" aria-label="Increase 5%">+</button>
            </div>
          </div>
        </div>
        <div class="slider-container">
          ${SVG_SUN_DIM}
          <input
            type="range"
            class="main-slider"
            min="${m.min}"
            max="${m.max}"
            step="1"
            value="${clampUi(m.current, m.min, m.max)}"
            data-monitor-id="${m.id}"
            aria-label="${m.name} brightness"
          />
          ${SVG_SUN_BRIGHT}
        </div>
        <div class="monitor-card-footer">
          <span class="monitor-detail-text" title="${m.detail}">
            ${m.detail}
          </span>
          <div class="preset-chips">${presetChipsHtml}</div>
        </div>
      `;

      const slider = card.querySelector<HTMLInputElement>(`input[data-monitor-id="${m.id}"]`)!;
      const pct = card.querySelector<HTMLElement>(`[data-monitor-pct="${m.id}"]`)!;
      updateSliderVisual(slider, clampUi(m.current, m.min, m.max), m.min, m.max);

      slider.addEventListener("input", () => {
        const v = Number(slider.value);
        pct.textContent = `${v}%`;
        updateActivePresetChips(m.id, v);
        updateSliderVisual(slider, v, m.min, m.max);
        scheduleSet(m, v);
        refreshMaster();
      });

      slider.addEventListener("change", () => {
        const v = Number(slider.value);
        pct.textContent = `${v}%`;
        updateActivePresetChips(m.id, v);
        updateSliderVisual(slider, v, m.min, m.max);
        flushSet(m, v);
        refreshMaster();
      });

      // Quick step buttons ±5%
      card.querySelector(`[data-step-down="${m.id}"]`)?.addEventListener("click", () => {
        const cur = Number(slider.value);
        const target = clampUi(cur - STEP, m.min, m.max);
        slider.value = String(target);
        pct.textContent = `${target}%`;
        updateActivePresetChips(m.id, target);
        updateSliderVisual(slider, target, m.min, m.max);
        flushSet(m, target);
        refreshMaster();
      });

      card.querySelector(`[data-step-up="${m.id}"]`)?.addEventListener("click", () => {
        const cur = Number(slider.value);
        const target = clampUi(cur + STEP, m.min, m.max);
        slider.value = String(target);
        pct.textContent = `${target}%`;
        updateActivePresetChips(m.id, target);
        updateSliderVisual(slider, target, m.min, m.max);
        flushSet(m, target);
        refreshMaster();
      });

      // Preset chips
      card.querySelectorAll<HTMLButtonElement>(`[data-preset-monitor="${m.id}"]`).forEach((chip) => {
        chip.addEventListener("click", () => {
          const target = clampUi(Number(chip.getAttribute("data-preset-val")), m.min, m.max);
          slider.value = String(target);
          pct.textContent = `${target}%`;
          updateActivePresetChips(m.id, target);
          updateSliderVisual(slider, target, m.min, m.max);
          flushSet(m, target);
          refreshMaster();
        });
      });

      // Wheel listener: scroll anywhere on the Monitor Card to adjust brightness
      attachWheelListener(card, (delta) => {
        const cur = Number(slider.value);
        const target = clampUi(cur + delta, m.min, m.max);
        slider.value = String(target);
        pct.textContent = `${target}%`;
        updateActivePresetChips(m.id, target);
        updateSliderVisual(slider, target, m.min, m.max);
        flushSet(m, target);
        refreshMaster();
      });

    } else {
      card.innerHTML = `
        <div class="monitor-card-header">
          <div class="monitor-identity">
            ${deviceIcon}
            <div class="monitor-title-box">
              <span class="monitor-name" data-rename-name="${m.id}">${displayName}</span>
              <input type="text" class="monitor-name-input" data-rename-input="${m.id}" value="${displayName}" maxlength="35" style="display: none;" spellcheck="false" />
              <button type="button" class="rename-btn" data-rename-btn="${m.id}" title="Rename display" aria-label="Rename display">
                <svg viewBox="0 0 24 24" width="12" height="12" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
                  <path d="M17 3a2.828 2.828 0 1 1 4 4L7.5 20.5 2 22l1.5-5.5L17 3z"></path>
                </svg>
              </button>
            </div>
            ${badgeTag}
          </div>
          <div class="percent-stepper disabled">
            <span class="percent-val inactive">Unsupported</span>
          </div>
        </div>
        <div class="slider-container-disabled">
          <span class="incapable-info-text">${m.detail}</span>
        </div>
        <div class="monitor-card-footer">
          <span class="monitor-detail-text warn">${m.detail}</span>
        </div>
      `;
    }

    // Inline rename interaction
    const nameSpan = card.querySelector<HTMLElement>(`[data-rename-name="${m.id}"]`);
    const nameInput = card.querySelector<HTMLInputElement>(`[data-rename-input="${m.id}"]`);
    const renameBtn = card.querySelector<HTMLButtonElement>(`[data-rename-btn="${m.id}"]`);

    if (nameSpan && nameInput && renameBtn) {
      const startEditing = (): void => {
        nameSpan.style.display = "none";
        renameBtn.style.display = "none";
        nameInput.style.display = "inline-block";
        nameInput.value = getMonitorDisplayName(m);
        nameInput.focus();
        nameInput.select();
      };

      const commitEditing = (): void => {
        if (nameInput.style.display === "none") return;
        const newName = nameInput.value.trim();
        if (newName && newName !== m.name) {
          setCustomName(m.id, newName);
        } else {
          setCustomName(m.id, "");
        }
        const updated = getMonitorDisplayName(m);
        nameSpan.textContent = updated;
        nameInput.value = updated;
        nameInput.style.display = "none";
        nameSpan.style.display = "inline-block";
        renameBtn.style.display = "inline-flex";
        setMainStatus(`Renamed display to "${updated}" ✓`, "ready");
      };

      const cancelEditing = (): void => {
        nameInput.value = getMonitorDisplayName(m);
        nameInput.style.display = "none";
        nameSpan.style.display = "inline-block";
        renameBtn.style.display = "inline-flex";
      };

      renameBtn.addEventListener("click", (e) => {
        e.stopPropagation();
        startEditing();
      });

      nameSpan.addEventListener("click", () => {
        startEditing();
      });

      nameInput.addEventListener("keydown", (e) => {
        if (e.key === "Enter") {
          e.preventDefault();
          commitEditing();
        } else if (e.key === "Escape") {
          e.preventDefault();
          cancelEditing();
        }
      });

      nameInput.addEventListener("blur", () => {
        commitEditing();
      });
    }

    list.appendChild(card);
  }
}

// =========================================================
// DATA MANAGEMENT & REAL-TIME SYNCHRONIZATION
// =========================================================

/**
 * Checks if the current monitor list has the same structure as new data.
 * If matching, updates slider values/text without re-rendering DOM,
 * ensuring smooth slider dragging without flickering or lost focus.
 */
function isSameStructure(data: MonitorInfo[]): boolean {
  if (data.length !== monitors.length) return false;
  for (let i = 0; i < data.length; i++) {
    if (
      data[i].id !== monitors[i].id ||
      data[i].capable !== monitors[i].capable
    ) {
      return false;
    }
  }
  return true;
}

function applyMonitors(data: MonitorInfo[]): void {
  const structureMatches = isSameStructure(data);
  monitors = data;

  if (isFlyout) {
    const activeCapableCount = monitors.filter((m) => m.capable).length;
    const trayMasterExists = !!document.querySelector(".tray-master-item");
    const trayMasterDesired = isMasterSliderEnabled() && activeCapableCount >= 2;
    if (structureMatches && trayMasterExists === trayMasterDesired) {
      for (const m of monitors) {
        syncTrayInputs(m);
      }
      refreshTrayMaster();
    } else {
      renderTrayMonitors();
    }
    setTrayStatus("");
  } else {
    // Update header subtitle
    const subtitle = el<HTMLElement>("header-subtitle");
    const activeCount = monitors.filter((m) => m.capable).length;
    if (subtitle) {
      if (monitors.length === 0) {
        subtitle.textContent = "No displays found";
      } else {
        subtitle.textContent = `${monitors.length} display(s) (${activeCount} active) • System tray ready`;
      }
    }

    const masterExists = !!el<HTMLElement>("card-master");
    const masterDesired = isMasterSliderEnabled() && activeCount >= 2;
    if (structureMatches && masterExists === masterDesired) {
      for (const m of monitors) {
        syncMainInputs(m);
      }
      refreshMaster();
    } else {
      renderMonitors();
    }

    setMainStatus(`Connected to ${monitors.length} display(s)`, "ready");
  }
}

async function loadMonitors(): Promise<void> {
  const refreshBtn = isFlyout
    ? el<HTMLButtonElement>("tray-refresh-btn")
    : el<HTMLButtonElement>("refresh-btn");

  if (refreshBtn) refreshBtn.classList.add("spin");

  if (isFlyout) {
    setTrayStatus("Scanning…", 0);
  } else {
    setMainStatus("Scanning display configurations…", "busy");
    showMainError("");
  }

  try {
    const list = await invoke<MonitorInfo[]>("list_monitors");
    applyMonitors(list);
  } catch (e) {
    const msg = e instanceof Error ? e.message : String(e);
    monitors = [];
    if (isFlyout) {
      setTrayStatus(`Scan error: ${msg}`);
      renderTrayMonitors();
    } else {
      const list = el<HTMLElement>("monitor-list");
      if (list) {
        list.innerHTML = `
          <div class="loading-state">
            <p class="loading-text">Failed to load displays.</p>
          </div>
        `;
      }
      showMainError(msg);
      setMainStatus("Display scan failed", "error");
    }
  } finally {
    if (refreshBtn) refreshBtn.classList.remove("spin");
  }
}

// =========================================================
// START ON BOOT (AUTOSTART) MANAGEMENT
// =========================================================

let isAutostartEnabled = false;

function syncAutostartUi(enabled: boolean): void {
  const btn = el<HTMLButtonElement>("autostart-toggle-btn");
  const checkbox = el<HTMLInputElement>("autostart-checkbox");
  if (btn) {
    if (enabled) {
      btn.classList.add("active");
      btn.title = "Start with Windows: ON (Click to disable)";
    } else {
      btn.classList.remove("active");
      btn.title = "Start with Windows: OFF (Click to enable)";
    }
  }
  if (checkbox && checkbox.checked !== enabled) {
    checkbox.checked = enabled;
  }
}

async function loadAutostart(): Promise<void> {
  try {
    const enabled = await invoke<boolean>("get_autostart");
    isAutostartEnabled = enabled;
    syncAutostartUi(enabled);
  } catch (e) {
    console.error("Failed to retrieve autostart status:", e);
  }
}

async function toggleAutostart(target?: boolean): Promise<void> {
  const desired = target !== undefined ? target : !isAutostartEnabled;
  try {
    const result = await invoke<boolean>("set_autostart", { enabled: desired });
    isAutostartEnabled = result;
    syncAutostartUi(result);
    if (result) {
      setMainStatus("Start with Windows enabled ✓", "ready");
    } else {
      setMainStatus("Start with Windows disabled", "ready");
    }
  } catch (e) {
    const msg = e instanceof Error ? e.message : String(e);
    showMainError(`Failed to update Start with Windows: ${msg}`);
    syncAutostartUi(isAutostartEnabled);
  }
}

// =========================================================
// APPLICATION VERSION
// =========================================================

/**
 * Loads and displays current application version from backend.
 */
async function initAppVersion(): Promise<void> {
  let version = "0.1.0";
  try {
    const backendVer = await invoke<string>("get_app_version");
    if (backendVer) {
      version = backendVer;
    }
  } catch {
    // Fallback to default version
  }

  const displayVer = version.startsWith("v") ? version : `v${version}`;

  const headerBadge = el<HTMLElement>("app-version-badge");
  if (headerBadge) headerBadge.textContent = displayVer;

  const settingsBadge = el<HTMLElement>("settings-app-version");
  if (settingsBadge) settingsBadge.textContent = displayVer;

  const trayBadge = el<HTMLElement>("tray-version-badge");
  if (trayBadge) trayBadge.textContent = displayVer;
}

// =========================================================
// INITIALIZATION AND EVENT LISTENERS
// =========================================================

window.addEventListener("DOMContentLoaded", () => {
  void initAppVersion();

  if (isFlyout) {
    document.documentElement.classList.add("flyout");
    document.body.classList.add("flyout");
    const mainView = el<HTMLElement>("main-view");
    const flyoutView = el<HTMLElement>("flyout-view");
    if (mainView) mainView.style.display = "none";
    if (flyoutView) flyoutView.style.display = "flex";

    el<HTMLButtonElement>("tray-theme-btn")?.addEventListener("click", () => {
      const current = getSavedTheme();
      // Cycle: system -> light -> dark -> system
      const next: ThemePref = current === "system" ? "light" : current === "light" ? "dark" : "system";
      applyTheme(next, true);
      const label = next === "system" ? "Auto" : next.charAt(0).toUpperCase() + next.slice(1);
      setTrayStatus(`Theme: ${label} ✓`);
    });

    el<HTMLButtonElement>("tray-refresh-btn")?.addEventListener("click", () => {
      void loadMonitors();
    });

    el<HTMLButtonElement>("tray-open-main-btn")?.addEventListener("click", async () => {
      try {
        await invoke("show_main_window");
        await getCurrentWindow().hide();
      } catch (e) {
        setTrayStatus(String(e));
      }
    });

    el<HTMLButtonElement>("tray-quit-btn")?.addEventListener("click", () => {
      void invoke("quit_app");
    });
  } else {
    const mainView = el<HTMLElement>("main-view");
    const flyoutView = el<HTMLElement>("flyout-view");
    if (mainView) mainView.style.display = "flex";
    if (flyoutView) flyoutView.style.display = "none";

    // Rescan displays button
    el<HTMLButtonElement>("refresh-btn")?.addEventListener("click", () => {
      void loadMonitors();
    });

    // Hide to system tray button
    el<HTMLButtonElement>("tray-btn")?.addEventListener("click", () => {
      void invoke("hide_main_window").catch((e: unknown) => {
        showMainError(String(e));
      });
    });

    // Settings & Preferences panel
    const settingsBtn = el<HTMLButtonElement>("settings-toggle-btn");
    const settingsPanel = el<HTMLElement>("settings-panel");
    const settingsCloseBtn = el<HTMLButtonElement>("settings-close-btn");

    const toggleSettings = (): void => {
      if (!settingsPanel) return;
      const isShowing = settingsPanel.style.display !== "none";
      settingsPanel.style.display = isShowing ? "none" : "block";
      if (settingsBtn) {
        if (isShowing) settingsBtn.classList.remove("active");
        else settingsBtn.classList.add("active");
      }
    };

    settingsBtn?.addEventListener("click", toggleSettings);
    settingsCloseBtn?.addEventListener("click", toggleSettings);

    // Theme appearance segmented controls
    document.querySelectorAll<HTMLButtonElement>(".theme-pill").forEach((pill) => {
      pill.addEventListener("click", () => {
        const val = pill.getAttribute("data-theme-val") as ThemePref;
        if (val) {
          applyTheme(val, true);
          const label = val === "system" ? "Auto" : val.charAt(0).toUpperCase() + val.slice(1);
          setMainStatus(`Theme: ${label} (Synchronized with System Tray) ✓`, "ready");
        }
      });
    });

    // Master slider (All Displays) checkbox inside settings panel
    syncMasterSliderUi(isMasterSliderEnabled());
    el<HTMLInputElement>("master-slider-checkbox")?.addEventListener("change", (e) => {
      const enabled = (e.target as HTMLInputElement).checked;
      setMasterSliderEnabled(enabled);
      renderMonitors();
      if (enabled) {
        setMainStatus("All Displays master slider enabled ✓", "ready");
      } else {
        setMainStatus("All Displays master slider hidden", "ready");
      }
    });

    // Start with Windows checkbox inside settings panel
    el<HTMLInputElement>("autostart-checkbox")?.addEventListener("change", (e) => {
      const target = (e.target as HTMLInputElement).checked;
      void toggleAutostart(target);
    });

    // Quit application button inside settings panel
    el<HTMLButtonElement>("settings-quit-btn")?.addEventListener("click", () => {
      void invoke("quit_app").catch((e: unknown) => {
        showMainError(String(e));
      });
    });

    // Keyboard shortcuts
    window.addEventListener("keydown", (e) => {
      if (e.key === "F5") {
        e.preventDefault();
        void loadMonitors();
      } else if (e.key === "Escape") {
        e.preventDefault();
        if (settingsPanel && settingsPanel.style.display !== "none") {
          toggleSettings();
        } else {
          void invoke("hide_main_window");
        }
      }
    });

    // Ensure initial theme UI state is synchronized
    applyTheme(getSavedTheme());

    // Load autostart status
    void loadAutostart();
  }

  // Listen for synchronization events from backend (when flyout or tray changes brightness)
  void listen<MonitorInfo[]>(TRAY_CHANGED_EVENT, (event) => {
    applyTheme(getSavedTheme(), false);
    if (Array.isArray(event.payload)) {
      applyMonitors(event.payload);
    } else {
      void loadMonitors();
    }
  });

  // Initial display load
  void loadMonitors();
});
