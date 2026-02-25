use std::{
    collections::{HashMap, HashSet},
    fs,
    mem,
    path::{Path, PathBuf},
    ptr,
    sync::{mpsc, OnceLock},
    time::{Duration, Instant},
};

use serde::Deserialize;
use serde_json::Value;
use webview2_com::Microsoft::Web::WebView2::Win32::*;
use image::{Rgba, RgbaImage};
use windows::{
    core::{w, BOOL, PCWSTR},
    Win32::{
        Foundation::{E_POINTER, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM},
        Graphics::Gdi::{
            BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject,
            EnumDisplayMonitors, GetDC, GetDIBits, GetMonitorInfoW, HDC, HGDIOBJ, HMONITOR, MonitorFromWindow,
            MONITORINFOEXW, MONITOR_DEFAULTTONEAREST, ReleaseDC, SelectObject, BI_RGB, BITMAPINFO, BITMAPINFOHEADER,
            DIB_RGB_COLORS, SRCCOPY,
        },
        Media::Audio::{
            eCommunications, eConsole, eMultimedia, eRender, IMMDeviceEnumerator,
            MMDeviceEnumerator,
        },
        Media::Audio::Endpoints::IAudioMeterInformation,
        Storage::Xps::{PrintWindow, PRINT_WINDOW_FLAGS},
        System::{Com::*, LibraryLoader::GetModuleHandleW},
        UI::Input::KeyboardAndMouse::GetAsyncKeyState,
        UI::WindowsAndMessaging::{
            CreateWindowExW, DefWindowProcW, DestroyWindow, EnumWindows, FindWindowExW, FindWindowW,
            GetCursorPos, GetForegroundWindow, GetWindowLongW, GetWindowRect, IsZoomed, RegisterClassW, SendMessageTimeoutW,
            SetWindowLongW,
            SetWindowPos, GWL_EXSTYLE, GWL_STYLE, HWND_BOTTOM, HWND_NOTOPMOST, HWND_TOP, HWND_TOPMOST,
            SMTO_NORMAL, SWP_FRAMECHANGED,
            SWP_NOACTIVATE, SWP_SHOWWINDOW, WINDOW_EX_STYLE,
            WINDOW_STYLE, WNDCLASSW, WS_CAPTION, WS_CHILD, WS_CLIPCHILDREN, WS_CLIPSIBLINGS,
            WS_EX_APPWINDOW, WS_EX_DLGMODALFRAME, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
            WS_EX_WINDOWEDGE, WS_MAXIMIZEBOX, WS_MINIMIZEBOX, WS_SYSMENU, WS_THICKFRAME, WS_VISIBLE,
            SystemParametersInfoW, SPI_SETDESKWALLPAPER, SPIF_SENDCHANGE, SPIF_UPDATEINIFILE,
        },
    },
};

use crate::{
    data_loaders::config::{AddonConfig, PauseMode, WallpaperConfig},
    error,
    ipc_connector::{request, request_quick},
    utility::{sentinel_assets_dir, to_wstring},
    warn,
};

const HOST_CLASS_NAME: PCWSTR = w!("SentinelWallpaperHostWindow");

#[derive(Debug, Deserialize, Clone)]
struct RegistryAsset {
    id: String,
    #[serde(default)]
    category: String,
    #[serde(default)]
    metadata: Value,
    #[serde(default)]
    path: PathBuf,
}

#[derive(Debug, Clone)]
struct MonitorArea {
    index: usize,
    primary: bool,
    rect: RECT,
}

struct HostedWallpaper {
    hwnd: HWND,
    controller: ICoreWebView2Controller,
    webview: ICoreWebView2,
    monitor_rect: RECT,
    monitor_id: Option<String>,
    pause_focus_mode: PauseMode,
    pause_maximized_mode: PauseMode,
    pause_fullscreen_mode: PauseMode,
    paused: bool,
    asset_dir: PathBuf,
}

impl Drop for HostedWallpaper {
    fn drop(&mut self) {
        unsafe {
            let _ = self.controller.Close();
            let _ = DestroyWindow(self.hwnd);
        }
    }
}

/// Virtual-key codes we poll each tick for `native_key` events.
const TRACKED_KEYS: &[(i32, &str)] = &[
    (0x08, "Backspace"), (0x09, "Tab"), (0x0D, "Enter"), (0x10, "Shift"),
    (0x11, "Control"), (0x12, "Alt"), (0x14, "CapsLock"), (0x1B, "Escape"),
    (0x20, "Space"),
    (0x25, "ArrowLeft"), (0x26, "ArrowUp"), (0x27, "ArrowRight"), (0x28, "ArrowDown"),
    // digits 0-9
    (0x30, "0"), (0x31, "1"), (0x32, "2"), (0x33, "3"), (0x34, "4"),
    (0x35, "5"), (0x36, "6"), (0x37, "7"), (0x38, "8"), (0x39, "9"),
    // letters A-Z
    (0x41, "A"), (0x42, "B"), (0x43, "C"), (0x44, "D"), (0x45, "E"),
    (0x46, "F"), (0x47, "G"), (0x48, "H"), (0x49, "I"), (0x4A, "J"),
    (0x4B, "K"), (0x4C, "L"), (0x4D, "M"), (0x4E, "N"), (0x4F, "O"),
    (0x50, "P"), (0x51, "Q"), (0x52, "R"), (0x53, "S"), (0x54, "T"),
    (0x55, "U"), (0x56, "V"), (0x57, "W"), (0x58, "X"), (0x59, "Y"),
    (0x5A, "Z"),
    // F1-F12
    (0x70, "F1"), (0x71, "F2"), (0x72, "F3"), (0x73, "F4"), (0x74, "F5"),
    (0x75, "F6"), (0x76, "F7"), (0x77, "F8"), (0x78, "F9"), (0x79, "F10"),
    (0x7A, "F11"), (0x7B, "F12"),
];

pub struct WallpaperRuntime {
    hosted: Vec<HostedWallpaper>,
    last_cursor: Option<(i32, i32)>,
    last_left_down: bool,
    pressed_keys: HashSet<i32>,
    audio_meter: Option<SystemAudioMeter>,
    last_audio_tick: Instant,
    last_audio_retry: Instant,
    last_audio_refresh: Instant,
    last_registry_tick: Instant,
    last_registry_payload: Option<String>,
    last_pause_tick: Instant,
    pause_check_interval: Duration,
    log_pause_state_changes: bool,
    last_pause_snapshot_path: Option<PathBuf>,
    cached_sysdata: Value,
    cached_appdata: Value,
    last_editable_tick: Instant,
    editable_cache: HashMap<PathBuf, String>,
    /// Whether the last registry IPC call succeeded.
    /// When false, ALL data delivery to webviews is suppressed.
    registry_connected: bool,
}

impl WallpaperRuntime {
    pub fn new() -> Self {
        let _ = ensure_host_class();
        unsafe {
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        }

        let audio_meter = match SystemAudioMeter::new() {
            Ok(meter) => {
                warn!("[WALLPAPER][AUDIO] System output meter initialized");
                Some(meter)
            }
            Err(e) => {
                warn!("[WALLPAPER][AUDIO] System output meter unavailable: {}", e);
                None
            }
        };

        Self {
            hosted: Vec::new(),
            last_cursor: None,
            last_left_down: false,
            pressed_keys: HashSet::new(),
            audio_meter,
            last_audio_tick: Instant::now(),
            last_audio_retry: Instant::now(),
            last_audio_refresh: Instant::now(),
            last_registry_tick: Instant::now(),
            last_registry_payload: None,
            last_pause_tick: Instant::now(),
            pause_check_interval: Duration::from_millis(500),
            log_pause_state_changes: true,
            last_pause_snapshot_path: None,
            cached_sysdata: Value::Null,
            cached_appdata: Value::Null,
            last_editable_tick: Instant::now(),
            editable_cache: HashMap::new(),
            registry_connected: false,
        }
    }

    pub fn apply(&mut self, config: &AddonConfig) {
        self.hosted.clear();
        self.last_cursor = None;
        self.last_left_down = false;
        self.pressed_keys.clear();
        self.last_audio_tick = Instant::now();
        self.last_audio_retry = Instant::now();
        self.last_audio_refresh = Instant::now();
        self.last_registry_tick = Instant::now();
        self.last_registry_payload = None;
        self.last_pause_tick = Instant::now();
        self.pause_check_interval =
            Duration::from_millis(config.settings.performance.pausing.check_interval_ms.max(100));
        self.log_pause_state_changes = config.settings.diagnostics.log_pause_state_changes;
        self.last_pause_snapshot_path = None;
        self.cached_sysdata = Value::Null;
        self.cached_appdata = Value::Null;
        self.last_editable_tick = Instant::now();
        self.editable_cache.clear();
        self.registry_connected = false;
        warn!("[WALLPAPER][APPLY] Cleared previous hosted wallpapers");

        if config.wallpapers.is_empty() {
            warn!("[WALLPAPER] No wallpaper sections found in config");
            return;
        }

        let assets = fetch_wallpaper_assets();
        if assets.is_empty() {
            warn!("[WALLPAPER] No wallpaper assets found from IPC or local Assets/wallpaper");
        }

        let monitors = enumerate_monitors();
        if monitors.is_empty() {
            error!("[WALLPAPER] No monitors detected, aborting runtime apply");
            return;
        }
        warn!(
            "[WALLPAPER][APPLY] {} asset(s), {} monitor(s), {} enabled profile(s)",
            assets.len(),
            monitors.len(),
            config.enabled_wallpapers().len()
        );

        let mut assigned_monitors = HashSet::<usize>::new();
        let enabled_profiles = config.enabled_wallpapers();

        for priority in [0u8, 1u8, 2u8] {
            for profile in enabled_profiles.iter().copied() {
                if profile_priority(profile) != priority {
                    continue;
                }
                self.launch_profile(profile, &assets, &monitors, &mut assigned_monitors);
            }
        }
    }

    fn launch_profile(
        &mut self,
        profile: &WallpaperConfig,
        assets: &[RegistryAsset],
        monitors: &[MonitorArea],
        assigned_monitors: &mut HashSet<usize>,
    ) {
        warn!(
            "[WALLPAPER][PROFILE] section='{}' wallpaper_id='{}' monitor_index={:?} mode='{}' z_index='{}'",
            profile.section,
            profile.wallpaper_id,
            profile.monitor_index,
            profile.mode,
            profile.z_index
        );

        let Some(asset) = resolve_asset(assets, &profile.wallpaper_id) else {
            warn!(
                "[WALLPAPER] Section '{}' references missing wallpaper_id '{}'",
                profile.section,
                profile.wallpaper_id
            );
            return;
        };

        let Some(url) = resolve_asset_url(asset) else {
            warn!(
                "[WALLPAPER] Asset '{}' has no 'url' and no local index.html",
                asset.id
            );
            return;
        };

        warn!(
            "[WALLPAPER][PROFILE] asset='{}' resolved url='{}'",
            asset.id,
            url
        );

        let targets = resolve_target_monitors(monitors, &profile.monitor_index, assigned_monitors);
        if targets.is_empty() {
            warn!(
                "[WALLPAPER] Section '{}' has no resolved monitor targets",
                profile.section
            );
            return;
        }

        for target in &targets {
            assigned_monitors.insert(target.index);
        }

        if profile.mode.eq_ignore_ascii_case("span") && targets.len() > 1 {
            let span_target = make_span_monitor_area(&targets);
            match self.launch_into_monitor(profile, &span_target, &url, &asset.path) {
                Ok(()) => warn!(
                    "[WALLPAPER] Embedded '{}' as span across {} monitor(s)",
                    profile.wallpaper_id,
                    targets.len(),
                ),
                Err(e) => warn!(
                    "[WALLPAPER] Failed to embed span '{}' ({} monitor(s)): {}",
                    profile.wallpaper_id,
                    targets.len(),
                    e
                ),
            }
            return;
        }

        for monitor in targets {
            match self.launch_into_monitor(profile, monitor, &url, &asset.path) {
                Ok(()) => warn!(
                    "[WALLPAPER] Embedded '{}' into desktop host on monitor {}",
                    profile.wallpaper_id,
                    monitor.index + 1,
                ),
                Err(e) => warn!(
                    "[WALLPAPER] Failed to embed '{}' for monitor {}: {}",
                    profile.wallpaper_id,
                    monitor.index + 1,
                    e
                ),
            }
        }
    }

    fn launch_into_monitor(
        &mut self,
        profile: &WallpaperConfig,
        monitor: &MonitorArea,
        url: &str,
        asset_dir: &Path,
    ) -> std::result::Result<(), String> {
        warn!(
            "[WALLPAPER][EMBED] monitor={} primary={} rect=[l={},t={},r={},b={}]",
            monitor.index + 1,
            monitor.primary,
            monitor.rect.left,
            monitor.rect.top,
            monitor.rect.right,
            monitor.rect.bottom
        );

        let desktop = ensure_desktop_host()
            .ok_or_else(|| "Failed to locate WorkerW desktop host window".to_string())?;
        warn!("[WALLPAPER][EMBED] parent desktop host resolved: {:?}", desktop);

        let parent_rect = window_rect(desktop)
            .ok_or_else(|| "Failed to read desktop host window rect".to_string())?;
        warn!(
            "[WALLPAPER][EMBED] parent rect=[l={},t={},r={},b={}]",
            parent_rect.left,
            parent_rect.top,
            parent_rect.right,
            parent_rect.bottom
        );

        let hwnd = create_desktop_child_window(desktop, parent_rect, monitor.rect)?;
        warn!("[WALLPAPER][EMBED] desktop child created: {:?}", hwnd);

        apply_host_style(hwnd, &profile.z_index)?;
        warn!(
            "[WALLPAPER][EMBED] host style applied: hwnd={:?} z_index='{}'",
            hwnd,
            profile.z_index
        );

        let controller = create_webview_controller(hwnd, monitor.rect, url)?;
        warn!("[WALLPAPER][EMBED] WebView2 controller attached to hwnd={:?}", hwnd);

        let webview = unsafe {
            controller
                .CoreWebView2()
                .map_err(|e| format!("WebView2 CoreWebView2 unavailable: {e:?}"))?
        };

        self.hosted.push(HostedWallpaper {
            hwnd,
            controller,
            webview,
            monitor_rect: monitor.rect,
            monitor_id: None,
            pause_focus_mode: profile.pause_focus_mode,
            pause_maximized_mode: profile.pause_maximized_mode,
            pause_fullscreen_mode: profile.pause_fullscreen_mode,
            paused: false,
            asset_dir: asset_dir.to_path_buf(),
        });
        warn!("[WALLPAPER][EMBED] host committed into runtime state");
        Ok(())
    }

    pub fn tick_interactions(&mut self) -> bool {
        if self.hosted.is_empty() {
            return false;
        }

        let mut unpaused_transition = false;

        let all_paused = self.hosted.iter().all(|h| h.paused);
        let registry_interval = if all_paused {
            self.pause_check_interval
        } else {
            Duration::from_millis(100)
        };

        // ── Registry snapshot (determines connectivity) ─────────────
        if self.last_registry_tick.elapsed() >= registry_interval {
            self.last_registry_tick = Instant::now();

            if let Some((sysdata, appdata, payload)) = build_registry_snapshot_and_payload() {
                if !self.registry_connected {
                    warn!("[WALLPAPER][REGISTRY] Connection established");
                }
                self.registry_connected = true;
                self.cached_sysdata = sysdata;
                self.cached_appdata = appdata;
                let has_active_hosts = self.hosted.iter().any(|h| !h.paused);
                let should_send = self
                    .last_registry_payload
                    .as_ref()
                    .map(|prev| prev != &payload)
                    .unwrap_or(true);

                if has_active_hosts && should_send {
                    self.last_registry_payload = Some(payload.clone());
                    for hosted in &self.hosted {
                        if hosted.paused {
                            continue;
                        }
                        let _ = post_webview_json(&hosted.webview, &payload);
                    }
                }
            } else {
                if self.registry_connected {
                    warn!("[WALLPAPER][REGISTRY] Connection lost — suppressing all data delivery");
                }
                self.registry_connected = false;
            }
        }

        // ── All interaction data gated behind registry connection ───
        if !self.registry_connected {
            // Still evaluate pausing even without registry,
            // but skip mouse/keyboard/audio delivery.
            if self.last_pause_tick.elapsed() >= self.pause_check_interval {
                self.last_pause_tick = Instant::now();
            }
            return false;
        }

        let mut point = POINT::default();
        if !all_paused {
            unsafe {
                if GetCursorPos(&mut point).is_err() {
                    return false;
                }
            }

            let cursor = (point.x, point.y);
            let left_down = unsafe { (GetAsyncKeyState(0x01_i32) as u16 & 0x8000) != 0 };
            let moved = self.last_cursor.map(|p| p != cursor).unwrap_or(true);
            let just_pressed = left_down && !self.last_left_down;

            if moved || just_pressed {
                for hosted in &self.hosted {
                    if hosted.paused || !point_in_rect(cursor, hosted.monitor_rect) {
                        continue;
                    }

                    let width = (hosted.monitor_rect.right - hosted.monitor_rect.left).max(1);
                    let height = (hosted.monitor_rect.bottom - hosted.monitor_rect.top).max(1);
                    let local_x = (cursor.0 - hosted.monitor_rect.left).clamp(0, width);
                    let local_y = (cursor.1 - hosted.monitor_rect.top).clamp(0, height);
                    let norm_x = local_x as f64 / width as f64;
                    let norm_y = local_y as f64 / height as f64;

                    if moved {
                        let payload = format!(
                            "{{\"type\":\"native_move\",\"x\":{},\"y\":{},\"nx\":{:.6},\"ny\":{:.6}}}",
                            local_x, local_y, norm_x, norm_y
                        );
                        let _ = post_webview_json(&hosted.webview, &payload);
                    }

                    if just_pressed {
                        let payload = format!(
                            "{{\"type\":\"native_click\",\"x\":{},\"y\":{},\"nx\":{:.6},\"ny\":{:.6}}}",
                            local_x, local_y, norm_x, norm_y
                        );
                        let _ = post_webview_json(&hosted.webview, &payload);
                    }
                }
            }

            self.last_cursor = Some(cursor);
            self.last_left_down = left_down;

            // ── Keyboard tracking ──────────────────────────────────
            for &(vk, label) in TRACKED_KEYS {
                let down = unsafe { (GetAsyncKeyState(vk) as u16 & 0x8000) != 0 };
                let was_down = self.pressed_keys.contains(&vk);
                if down && !was_down {
                    self.pressed_keys.insert(vk);
                    let payload = format!(
                        "{{\"type\":\"native_key\",\"key\":\"{}\",\"vk\":{},\"state\":\"down\"}}",
                        label, vk
                    );
                    for hosted in &self.hosted {
                        if !hosted.paused {
                            let _ = post_webview_json(&hosted.webview, &payload);
                        }
                    }
                } else if !down && was_down {
                    self.pressed_keys.remove(&vk);
                    let payload = format!(
                        "{{\"type\":\"native_key\",\"key\":\"{}\",\"vk\":{},\"state\":\"up\"}}",
                        label, vk
                    );
                    for hosted in &self.hosted {
                        if !hosted.paused {
                            let _ = post_webview_json(&hosted.webview, &payload);
                        }
                    }
                }
            }
        }

        if !all_paused && self.last_audio_tick.elapsed() >= Duration::from_millis(33) {
            self.last_audio_tick = Instant::now();

            if self.last_audio_refresh.elapsed() >= Duration::from_millis(1200) {
                self.last_audio_refresh = Instant::now();
                if let Some(meter) = self.audio_meter.as_mut() {
                    if let Err(e) = meter.refresh() {
                        warn!("[WALLPAPER][AUDIO] Endpoint refresh failed: {}", e);
                        self.audio_meter = None;
                    }
                }
            }

            if self.audio_meter.is_none() && self.last_audio_retry.elapsed() >= Duration::from_secs(2) {
                self.last_audio_retry = Instant::now();
                match SystemAudioMeter::new() {
                    Ok(meter) => {
                        warn!("[WALLPAPER][AUDIO] System output meter restored");
                        self.audio_meter = Some(meter);
                    }
                    Err(e) => {
                        warn!("[WALLPAPER][AUDIO] Retry failed: {}", e);
                    }
                }
            }

            if let Some(meter) = self.audio_meter.as_mut() {
                match meter.peak() {
                    Ok(level) => {
                        let payload = format!("{{\"type\":\"native_audio\",\"level\":{:.6}}}", level);
                        for hosted in &self.hosted {
                            if hosted.paused {
                                continue;
                            }
                            let _ = post_webview_json(&hosted.webview, &payload);
                        }
                    }
                    Err(e) => {
                        warn!("[WALLPAPER][AUDIO] Peak read failed, resetting meter: {}", e);
                        self.audio_meter = None;
                    }
                }
            }
        }

        // ── Live editable CSS var updates (manifest.json watch) ──
        if self.last_editable_tick.elapsed() >= Duration::from_millis(250) {
            self.last_editable_tick = Instant::now();
            self.check_editable_updates();
        }

        if self.last_pause_tick.elapsed() >= self.pause_check_interval {
            self.last_pause_tick = Instant::now();
            let cached_sysdata = self.cached_sysdata.clone();
            let cached_appdata = self.cached_appdata.clone();
            let states_changed = self.evaluate_and_apply_pause(&cached_sysdata, &cached_appdata);
            if states_changed {
                let all_paused_now = self.hosted.iter().all(|h| h.paused);
                if !all_paused && all_paused_now {
                    if let Err(e) = self.capture_and_set_paused_wallpaper_snapshot() {
                        warn!("[WALLPAPER][PAUSE] Snapshot capture/apply failed: {}", e);
                    }
                }
                if all_paused && !all_paused_now {
                    unpaused_transition = true;
                }
                self.apply_host_visibility();
            }
        }

        unpaused_transition
    }

    fn evaluate_and_apply_pause(&mut self, sysdata: &Value, appdata: &Value) -> bool {
        if self.hosted.is_empty() {
            return false;
        }

        let mut states_changed = false;

        for hosted in &mut self.hosted {
            hosted.monitor_id = resolve_monitor_id_for_rect(sysdata, hosted.monitor_rect);
        }

        let global_states = global_window_states(appdata).unwrap_or_else(foreground_window_states);

        for hosted in &mut self.hosted {
            let local_states = hosted
                .monitor_id
                .as_deref()
                .map(|id| monitor_window_states(appdata, id))
                .unwrap_or_default();

            let should_pause = mode_triggered(hosted.pause_focus_mode, local_states.focused, global_states.focused)
                || mode_triggered(
                    hosted.pause_maximized_mode,
                    local_states.maximized,
                    global_states.maximized,
                )
                || mode_triggered(
                    hosted.pause_fullscreen_mode,
                    local_states.fullscreen,
                    global_states.fullscreen,
                );

            if should_pause != hosted.paused {
                hosted.paused = should_pause;
                states_changed = true;
                let payload = format!("{{\"type\":\"native_pause\",\"paused\":{}}}", should_pause);
                let _ = post_webview_json(&hosted.webview, &payload);
                if self.log_pause_state_changes {
                    warn!(
                        "[WALLPAPER][PAUSE] monitor={:?} paused={} (local: focused={} maximized={} fullscreen={}; global: focused={} maximized={} fullscreen={})",
                        hosted.monitor_id,
                        should_pause,
                        local_states.focused,
                        local_states.maximized,
                        local_states.fullscreen,
                        global_states.focused,
                        global_states.maximized,
                        global_states.fullscreen
                    );
                }
            }
        }

        states_changed
    }

    fn apply_host_visibility(&mut self) {
        for hosted in &mut self.hosted {
            unsafe {
                let _ = hosted.controller.SetIsVisible(!hosted.paused);
            }
        }
    }

    /// Check each hosted wallpaper's manifest.json for editable changes.
    /// When the editable section changes, push a `native_css_vars` message
    /// containing all CSS variable updates to the affected WebView2 instances.
    fn check_editable_updates(&mut self) {
        // Collect unique asset dirs
        let mut seen = HashSet::new();
        let mut dirs: Vec<PathBuf> = Vec::new();
        for h in &self.hosted {
            if seen.insert(h.asset_dir.clone()) {
                dirs.push(h.asset_dir.clone());
            }
        }

        for dir in &dirs {
            let manifest_path = dir.join("manifest.json");
            let content = match fs::read_to_string(&manifest_path) {
                Ok(c) => c,
                Err(_) => continue,
            };

            let manifest: Value = match serde_json::from_str(&content) {
                Ok(v) => v,
                Err(_) => continue,
            };

            let editable = match manifest.get("editable") {
                Some(e) => e,
                None => continue,
            };

            let editable_json = serde_json::to_string(editable).unwrap_or_default();

            // Skip if nothing changed since last check
            if self.editable_cache.get(dir).map(|prev| *prev == editable_json).unwrap_or(false) {
                continue;
            }
            self.editable_cache.insert(dir.clone(), editable_json);

            // Extract CSS variable → value pairs from the editable tree
            let vars = extract_css_vars(editable);
            if vars.is_empty() {
                continue;
            }

            let vars_obj = Value::Object(vars);
            let payload = format!(
                "{{\"type\":\"native_css_vars\",\"vars\":{}}}",
                serde_json::to_string(&vars_obj).unwrap_or_else(|_| "{}".to_string())
            );

            for hosted in &self.hosted {
                if hosted.asset_dir == *dir {
                    let _ = post_webview_json(&hosted.webview, &payload);
                }
            }
        }
    }

    fn capture_and_set_paused_wallpaper_snapshot(&mut self) -> std::result::Result<(), String> {
        if self.hosted.is_empty() {
            return Ok(());
        }

        let min_left = self
            .hosted
            .iter()
            .map(|h| h.monitor_rect.left)
            .min()
            .ok_or_else(|| "No hosted monitor bounds".to_string())?;
        let min_top = self
            .hosted
            .iter()
            .map(|h| h.monitor_rect.top)
            .min()
            .ok_or_else(|| "No hosted monitor bounds".to_string())?;
        let max_right = self
            .hosted
            .iter()
            .map(|h| h.monitor_rect.right)
            .max()
            .ok_or_else(|| "No hosted monitor bounds".to_string())?;
        let max_bottom = self
            .hosted
            .iter()
            .map(|h| h.monitor_rect.bottom)
            .max()
            .ok_or_else(|| "No hosted monitor bounds".to_string())?;

        let virtual_width = (max_right - min_left).max(1);
        let virtual_height = (max_bottom - min_top).max(1);
        let mut stitched = RgbaImage::from_pixel(virtual_width as u32, virtual_height as u32, Rgba([0, 0, 0, 255]));
        let mut has_non_black_pixel = false;

        for hosted in &self.hosted {
            let width = (hosted.monitor_rect.right - hosted.monitor_rect.left).max(1);
            let height = (hosted.monitor_rect.bottom - hosted.monitor_rect.top).max(1);
            let pixels = capture_window_bgra(hosted.hwnd, width, height)?;
            let offset_x = (hosted.monitor_rect.left - min_left).max(0);
            let offset_y = (hosted.monitor_rect.top - min_top).max(0);

            for y in 0..height {
                for x in 0..width {
                    let src = ((y * width + x) * 4) as usize;
                    if src + 3 >= pixels.len() {
                        continue;
                    }
                    let b = pixels[src];
                    let g = pixels[src + 1];
                    let r = pixels[src + 2];
                    if r != 0 || g != 0 || b != 0 {
                        has_non_black_pixel = true;
                    }
                    let dst_x = (offset_x + x) as u32;
                    let dst_y = (offset_y + y) as u32;
                    if dst_x < stitched.width() && dst_y < stitched.height() {
                        stitched.put_pixel(dst_x, dst_y, Rgba([r, g, b, 255]));
                    }
                }
            }
        }

        if !has_non_black_pixel {
            return Err("Captured wallpaper frame is fully black; refusing to apply snapshot wallpaper".to_string());
        }

        let snapshot_dir = sentinel_assets_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("wallpaper")
            .join("snapshots");
        let _ = fs::create_dir_all(&snapshot_dir);
        let snapshot_path = snapshot_dir.join("paused_wallpaper_snapshot.bmp");
        stitched
            .save(&snapshot_path)
            .map_err(|e| format!("Failed to save snapshot bitmap: {e}"))?;

        apply_windows_wallpaper(&snapshot_path)?;
        self.last_pause_snapshot_path = Some(snapshot_path.clone());
        if self.log_pause_state_changes {
            warn!(
                "[WALLPAPER][PAUSE] Applied snapshot wallpaper: {}",
                snapshot_path.display()
            );
        }
        Ok(())
    }
}

fn capture_window_bgra(hwnd: HWND, width: i32, height: i32) -> std::result::Result<Vec<u8>, String> {
    unsafe {
        let src_dc = GetDC(Some(hwnd));
        if src_dc.0.is_null() {
            return Err("GetDC failed".to_string());
        }

        let mem_dc = CreateCompatibleDC(Some(src_dc));
        if mem_dc.0.is_null() {
            let _ = ReleaseDC(Some(hwnd), src_dc);
            return Err("CreateCompatibleDC failed".to_string());
        }

        let bitmap = CreateCompatibleBitmap(src_dc, width, height);
        if bitmap.0.is_null() {
            let _ = DeleteDC(mem_dc);
            let _ = ReleaseDC(Some(hwnd), src_dc);
            return Err("CreateCompatibleBitmap failed".to_string());
        }

        let old = SelectObject(mem_dc, HGDIOBJ(bitmap.0));
        let printed = PrintWindow(hwnd, mem_dc, PRINT_WINDOW_FLAGS(2)).as_bool();
        if !printed {
            let _ = BitBlt(mem_dc, 0, 0, width, height, Some(src_dc), 0, 0, SRCCOPY)
                .map_err(|e| format!("BitBlt fallback failed: {e:?}"));
        }

        let mut bmi = BITMAPINFO::default();
        bmi.bmiHeader.biSize = mem::size_of::<BITMAPINFOHEADER>() as u32;
        bmi.bmiHeader.biWidth = width;
        bmi.bmiHeader.biHeight = -height;
        bmi.bmiHeader.biPlanes = 1;
        bmi.bmiHeader.biBitCount = 32;
        bmi.bmiHeader.biCompression = BI_RGB.0;

        let mut pixels = vec![0u8; (width * height * 4) as usize];
        let lines = GetDIBits(
            mem_dc,
            bitmap,
            0,
            height as u32,
            Some(pixels.as_mut_ptr() as *mut core::ffi::c_void),
            &mut bmi,
            DIB_RGB_COLORS,
        );

        let _ = SelectObject(mem_dc, old);
        let _ = DeleteObject(HGDIOBJ(bitmap.0));
        let _ = DeleteDC(mem_dc);
        let _ = ReleaseDC(Some(hwnd), src_dc);

        if lines == 0 {
            return Err("GetDIBits failed".to_string());
        }

        Ok(pixels)
    }
}

fn apply_windows_wallpaper(path: &Path) -> std::result::Result<(), String> {
    let wide = to_wstring(path.to_string_lossy().as_ref());
    unsafe {
        SystemParametersInfoW(
            SPI_SETDESKWALLPAPER,
            0,
            Some(wide.as_ptr() as *mut core::ffi::c_void),
            SPIF_UPDATEINIFILE | SPIF_SENDCHANGE,
        )
        .map_err(|e| format!("SystemParametersInfoW(SPI_SETDESKWALLPAPER) failed: {e:?}"))
    }
}

#[derive(Default, Clone, Copy)]
struct MonitorWindowStates {
    focused: bool,
    maximized: bool,
    fullscreen: bool,
}

fn mode_triggered(mode: PauseMode, local_triggered: bool, any_triggered: bool) -> bool {
    match mode {
        PauseMode::Off => false,
        PauseMode::PerMonitor => local_triggered,
        PauseMode::AllMonitors => any_triggered,
    }
}

fn monitor_window_states(appdata: &Value, monitor_id: &str) -> MonitorWindowStates {
    let windows = appdata
        .get(monitor_id)
        .and_then(|v| v.get("windows"))
        .and_then(|v| v.as_array());

    let mut states = MonitorWindowStates::default();
    let Some(windows) = windows else {
        return states;
    };

    for window in windows {
        let focused = window
            .get("focused")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let state = window
            .get("window_state")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_lowercase();

        if focused {
            states.focused = true;
            if state == "maximized" {
                states.maximized = true;
            }
            if state == "fullscreen" {
                states.fullscreen = true;
            }
        }
    }

    states
}

fn foreground_window_states() -> MonitorWindowStates {
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.0.is_null() {
            return MonitorWindowStates::default();
        }

        let mut states = MonitorWindowStates {
            focused: true,
            maximized: IsZoomed(hwnd).0 != 0,
            fullscreen: false,
        };

        let mut rect = RECT::default();
        if GetWindowRect(hwnd, &mut rect).is_err() {
            return states;
        }

        let monitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
        if monitor.0.is_null() {
            return states;
        }

        let mut mi_ex: MONITORINFOEXW = std::mem::zeroed();
        mi_ex.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
        if GetMonitorInfoW(monitor, &mut mi_ex.monitorInfo).0 == 0 {
            return states;
        }

        let monitor_rc = mi_ex.monitorInfo.rcMonitor;
        let epsilon = 1i32;
        let covers_monitor = (rect.left - monitor_rc.left).abs() <= epsilon
            && (rect.top - monitor_rc.top).abs() <= epsilon
            && (rect.right - monitor_rc.right).abs() <= epsilon
            && (rect.bottom - monitor_rc.bottom).abs() <= epsilon;

        let style = GetWindowLongW(hwnd, GWL_STYLE) as u32;
        let has_frame = (style & (WS_CAPTION.0 | WS_THICKFRAME.0)) != 0;
        states.fullscreen = covers_monitor && !states.maximized && !has_frame;

        states
    }
}

fn resolve_monitor_id_for_rect(sysdata: &Value, rect: RECT) -> Option<String> {
    let displays = sysdata.get("displays")?.as_array()?;

    for display in displays {
        let metadata = display.get("metadata")?;
        let x = metadata.get("x").and_then(|v| v.as_i64())? as i32;
        let y = metadata.get("y").and_then(|v| v.as_i64())? as i32;
        let width = metadata.get("width").and_then(|v| v.as_i64())? as i32;
        let height = metadata.get("height").and_then(|v| v.as_i64())? as i32;

        if x == rect.left && y == rect.top && (x + width) == rect.right && (y + height) == rect.bottom {
            if let Some(id) = display.get("id").and_then(|v| v.as_str()) {
                return Some(id.to_string());
            }
            if let Some(id) = metadata.get("id").and_then(|v| v.as_str()) {
                return Some(id.to_string());
            }
        }
    }

    None
}

fn build_registry_snapshot_and_payload() -> Option<(Value, Value, String)> {
    // Single IPC round-trip using the combined `snapshot` command.
    // Uses request_quick (no retries) so the tick loop never blocks for seconds.
    let snapshot_raw = request_quick("registry", "snapshot", None)?;
    let snapshot: Value = serde_json::from_str(&snapshot_raw).ok()?;

    let sysdata = snapshot.get("sysdata").cloned().unwrap_or(Value::Null);
    let appdata = snapshot.get("appdata").cloned().unwrap_or(Value::Null);

    let payload = serde_json::json!({
        "type": "native_registry",
        "sysdata": sysdata,
        "appdata": appdata,
    })
    .to_string();

    Some((sysdata, appdata, payload))
}

struct SystemAudioMeter {
    enumerator: IMMDeviceEnumerator,
    meters: Vec<IAudioMeterInformation>,
}

impl SystemAudioMeter {
    fn new() -> std::result::Result<Self, String> {
        unsafe {
            let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
                .map_err(|e| format!("CoCreateInstance(MMDeviceEnumerator) failed: {e:?}"))?;

            let mut meter = Self {
                enumerator,
                meters: Vec::new(),
            };
            meter.refresh()?;
            Ok(meter)
        }
    }

    fn refresh(&mut self) -> std::result::Result<(), String> {
        unsafe {
            let mut meters = Vec::<IAudioMeterInformation>::new();
            for role in [eConsole, eMultimedia, eCommunications] {
                if let Ok(device) = self.enumerator.GetDefaultAudioEndpoint(eRender, role) {
                    if let Ok(meter) = device.Activate::<IAudioMeterInformation>(CLSCTX_ALL, None) {
                        meters.push(meter);
                    }
                }
            }

            if meters.is_empty() {
                return Err("No usable default render endpoint audio meters found".to_string());
            }

            self.meters = meters;
            Ok(())
        }
    }

    fn peak(&self) -> std::result::Result<f32, String> {
        unsafe {
            let mut best = 0.0f32;
            let mut had_success = false;

            for meter in &self.meters {
                match meter.GetPeakValue() {
                    Ok(peak) => {
                        had_success = true;
                        if peak > best {
                            best = peak;
                        }
                    }
                    Err(_) => {}
                }
            }

            if !had_success {
                return Err("All audio meter reads failed".to_string());
            }

            Ok(best.clamp(0.0, 1.0))
        }
    }
}

fn point_in_rect(point: (i32, i32), rect: RECT) -> bool {
    point.0 >= rect.left && point.0 < rect.right && point.1 >= rect.top && point.1 < rect.bottom
}

fn post_webview_json(webview: &ICoreWebView2, payload: &str) -> std::result::Result<(), String> {
    let payload_wide = to_wstring(payload);
    unsafe {
        webview
            .PostWebMessageAsJson(PCWSTR(payload_wide.as_ptr()))
            .map_err(|e| format!("WebView2 PostWebMessageAsJson failed: {e:?}"))
    }
}

/// Walk the editable tree from manifest.json and collect { "--css-var": "value" } pairs.
fn extract_css_vars(editable: &Value) -> serde_json::Map<String, Value> {
    let mut vars = serde_json::Map::new();
    let obj = match editable.as_object() {
        Some(o) => o,
        None => return vars,
    };

    for (_key, entry) in obj {
        if let Some(variable) = entry.get("variable").and_then(|v| v.as_str()) {
            // Direct editable with a variable
            if let Some(value) = entry.get("value") {
                vars.insert(variable.to_string(), Value::String(value_to_css_string(value)));
            }
        } else if let Some(sub_obj) = entry.as_object() {
            // Group — iterate sub-entries (skip non-object fields like "name", "description")
            for (_sub_key, sub) in sub_obj {
                if let Some(variable) = sub.get("variable").and_then(|v| v.as_str()) {
                    if let Some(value) = sub.get("value") {
                        vars.insert(variable.to_string(), Value::String(value_to_css_string(value)));
                    }
                }
            }
        }
    }

    vars
}

/// Convert a serde_json Value to a CSS-appropriate string.
fn value_to_css_string(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        _ => value.to_string(),
    }
}

fn ensure_host_class() -> std::result::Result<(), String> {
    static CLASS_ONCE: OnceLock<bool> = OnceLock::new();
    if CLASS_ONCE.get().is_some() {
        return Ok(());
    }

    let hinstance = unsafe {
        GetModuleHandleW(None)
            .map(|h| HINSTANCE(h.0))
            .map_err(|e| format!("GetModuleHandleW failed: {e:?}"))?
    };

    let wc = WNDCLASSW {
        lpfnWndProc: Some(host_window_proc),
        hInstance: hinstance,
        lpszClassName: HOST_CLASS_NAME,
        ..Default::default()
    };

    unsafe {
        let _ = RegisterClassW(&wc);
    }

    let _ = CLASS_ONCE.set(true);
    Ok(())
}

unsafe extern "system" fn host_window_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

fn create_desktop_child_window(worker: HWND, parent_rect: RECT, rect: RECT) -> std::result::Result<HWND, String> {
    let x = rect.left - parent_rect.left;
    let y = rect.top - parent_rect.top;
    let width = rect.right - rect.left;
    let height = rect.bottom - rect.top;
    warn!(
        "[WALLPAPER][HOST] creating child window parent={:?} pos=({}, {}) size={}x{}",
        worker,
        x,
        y,
        width,
        height
    );

    let style = WINDOW_STYLE((WS_CHILD | WS_VISIBLE | WS_CLIPSIBLINGS | WS_CLIPCHILDREN).0);
    let ex_style = WINDOW_EX_STYLE((WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE).0);

    let hinstance = unsafe {
        GetModuleHandleW(None)
            .map(|h| HINSTANCE(h.0))
            .map_err(|e| format!("GetModuleHandleW failed: {e:?}"))?
    };

    let hwnd = unsafe {
        CreateWindowExW(
            ex_style,
            HOST_CLASS_NAME,
            PCWSTR::null(),
            style,
            x,
            y,
            width,
            height,
            Some(worker),
            None,
            Some(hinstance),
            Some(ptr::null()),
        )
    }
    .map_err(|e| format!("CreateWindowExW failed: {e:?}"))?;

    Ok(hwnd)
}

fn window_rect(hwnd: HWND) -> Option<RECT> {
    unsafe {
        let mut rect = RECT::default();
        if GetWindowRect(hwnd, &mut rect).is_ok() {
            Some(rect)
        } else {
            None
        }
    }
}

fn apply_host_style(hwnd: HWND, z_index: &str) -> std::result::Result<(), String> {
    unsafe {
        let style = GetWindowLongW(hwnd, GWL_STYLE) as u32;
        let mut new_style = style
            & !(WS_CAPTION.0 | WS_THICKFRAME.0 | WS_MINIMIZEBOX.0 | WS_MAXIMIZEBOX.0 | WS_SYSMENU.0);
        new_style |= WS_VISIBLE.0 | WS_CHILD.0;
        let _ = SetWindowLongW(hwnd, GWL_STYLE, new_style as i32);

        let ex_style = GetWindowLongW(hwnd, GWL_EXSTYLE) as u32;
        let mut new_ex = ex_style & !(WS_EX_APPWINDOW.0 | WS_EX_WINDOWEDGE.0 | WS_EX_DLGMODALFRAME.0);
        new_ex |= WS_EX_TOOLWINDOW.0 | WS_EX_NOACTIVATE.0;
        let _ = SetWindowLongW(hwnd, GWL_EXSTYLE, new_ex as i32);

        let insert_after = match z_index.to_lowercase().as_str() {
            "desktop" => HWND_TOPMOST,
            "bottom" => HWND_BOTTOM,
            "normal" => HWND_NOTOPMOST,
            "top" => HWND_TOP,
            "topmost" | "overlay" => HWND_TOPMOST,
            _ => HWND_BOTTOM,
        };
        warn!(
            "[WALLPAPER][STYLE] hwnd={:?} old_style=0x{:X} new_style=0x{:X} old_ex=0x{:X} new_ex=0x{:X}",
            hwnd,
            style,
            new_style,
            ex_style,
            new_ex
        );
        warn!("[WALLPAPER][STYLE] insert_after={:?}", insert_after);

        if SetWindowPos(
            hwnd,
            Some(insert_after),
            0,
            0,
            0,
            0,
            SWP_NOACTIVATE | SWP_SHOWWINDOW | SWP_FRAMECHANGED,
        )
        .is_err()
        {
            return Err("SetWindowPos failed for host style".to_string());
        }
    }

    Ok(())
}

fn create_webview_controller(
    hwnd: HWND,
    rect: RECT,
    url: &str,
) -> std::result::Result<ICoreWebView2Controller, String> {
    warn!("[WALLPAPER][WEBVIEW] creating environment for hwnd={:?}", hwnd);
    let environment = {
        let (tx, rx) = mpsc::channel();

        webview2_com::CreateCoreWebView2EnvironmentCompletedHandler::wait_for_async_operation(
            Box::new(|handler| unsafe {
                CreateCoreWebView2Environment(&handler).map_err(webview2_com::Error::WindowsError)
            }),
            Box::new(move |error_code, environment| {
                error_code?;
                tx.send(environment.ok_or_else(|| windows::core::Error::from(E_POINTER)))
                    .expect("send WebView2 environment");
                Ok(())
            }),
        )
        .map_err(|e| format!("CreateCoreWebView2Environment failed: {e:?}"))?;

        rx.recv()
            .map_err(|_| "Failed to receive WebView2 environment".to_string())?
            .map_err(|e| format!("WebView2 environment unavailable: {e:?}"))?
    };
    warn!("[WALLPAPER][WEBVIEW] environment ready for hwnd={:?}", hwnd);

    let controller = {
        let (tx, rx) = mpsc::channel();

        webview2_com::CreateCoreWebView2ControllerCompletedHandler::wait_for_async_operation(
            Box::new(move |handler| unsafe {
                environment
                    .CreateCoreWebView2Controller(hwnd, &handler)
                    .map_err(webview2_com::Error::WindowsError)
            }),
            Box::new(move |error_code, controller| {
                error_code?;
                tx.send(controller.ok_or_else(|| windows::core::Error::from(E_POINTER)))
                    .expect("send WebView2 controller");
                Ok(())
            }),
        )
        .map_err(|e| format!("CreateCoreWebView2Controller failed: {e:?}"))?;

        rx.recv()
            .map_err(|_| "Failed to receive WebView2 controller".to_string())?
            .map_err(|e| format!("WebView2 controller unavailable: {e:?}"))?
    };
    warn!("[WALLPAPER][WEBVIEW] controller ready for hwnd={:?}", hwnd);

    unsafe {
        let width = rect.right - rect.left;
        let height = rect.bottom - rect.top;
        warn!(
            "[WALLPAPER][WEBVIEW] setting bounds {}x{} and navigating to '{}'",
            width,
            height,
            url
        );
        controller
            .SetBounds(RECT {
                left: 0,
                top: 0,
                right: width,
                bottom: height,
            })
            .map_err(|e| format!("WebView2 SetBounds failed: {e:?}"))?;

        controller
            .SetIsVisible(true)
            .map_err(|e| format!("WebView2 SetIsVisible failed: {e:?}"))?;

        let webview = controller
            .CoreWebView2()
            .map_err(|e| format!("WebView2 CoreWebView2 unavailable: {e:?}"))?;

        let url_wide = to_wstring(url);
        webview
            .Navigate(PCWSTR(url_wide.as_ptr()))
            .map_err(|e| format!("WebView2 Navigate failed for '{}': {e:?}", url))?;
    }
    warn!("[WALLPAPER][WEBVIEW] navigation submitted successfully");

    Ok(controller)
}

fn fetch_wallpaper_assets() -> Vec<RegistryAsset> {
    if let Some(raw) = request("registry", "list_assets", None) {
        if let Ok(entries) = serde_json::from_str::<Vec<RegistryAsset>>(&raw) {
            let filtered: Vec<RegistryAsset> = entries
                .into_iter()
                .filter(|e| e.category.eq_ignore_ascii_case("wallpaper"))
                .collect();

            if !filtered.is_empty() {
                return filtered;
            }
        } else {
            warn!("[WALLPAPER] Failed to parse registry list_assets payload");
        }
    } else {
        warn!("[WALLPAPER] IPC list_assets request failed");
    }

    let local_assets = fetch_local_wallpaper_assets();
    if local_assets.is_empty() {
        warn!("[WALLPAPER] Local fallback found no wallpaper assets");
    } else {
        warn!("[WALLPAPER] Using local fallback: loaded {} wallpaper asset(s)", local_assets.len());
    }

    local_assets
}

fn fetch_local_wallpaper_assets() -> Vec<RegistryAsset> {
    let Some(root) = sentinel_assets_dir().map(|p| p.join("wallpaper")) else {
        return Vec::new();
    };

    let Ok(read_dir) = fs::read_dir(&root) else {
        return Vec::new();
    };

    let mut results = Vec::<RegistryAsset>::new();

    for entry in read_dir.flatten() {
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }

        let manifest_path = dir.join("manifest.json");
        if !manifest_path.exists() {
            continue;
        }

        let Ok(raw) = fs::read_to_string(&manifest_path) else {
            continue;
        };

        let Ok(metadata) = serde_json::from_str::<Value>(&raw) else {
            continue;
        };

        let id = metadata
            .get("id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .or_else(|| dir.file_name().and_then(|n| n.to_str()).map(|s| s.to_string()));

        let Some(id) = id else {
            continue;
        };

        results.push(RegistryAsset {
            id,
            category: "wallpaper".to_string(),
            metadata,
            path: dir,
        });
    }

    results
}

fn resolve_asset<'a>(assets: &'a [RegistryAsset], wallpaper_id: &str) -> Option<&'a RegistryAsset> {
    assets.iter().find(|a| a.id == wallpaper_id)
}

fn resolve_asset_url(asset: &RegistryAsset) -> Option<String> {
    if let Some(url) = asset.metadata.get("url").and_then(|v| v.as_str()) {
        return Some(url.to_string());
    }

    let local_html = asset.path.join("index.html");
    if local_html.exists() {
        return Some(path_to_file_url(&local_html));
    }

    None
}

fn resolve_target_monitors<'a>(
    monitors: &'a [MonitorArea],
    keys: &[String],
    assigned_monitors: &HashSet<usize>,
) -> Vec<&'a MonitorArea> {
    let mut result = Vec::<&MonitorArea>::new();

    if keys.iter().any(|key| key.eq_ignore_ascii_case("p")) {
        if let Some(primary) = monitors.iter().find(|monitor| monitor.primary) {
            result.push(primary);
        }
    }

    for key in keys {
        if key == "*" || key.eq_ignore_ascii_case("p") {
            continue;
        }

        if let Ok(index) = key.parse::<usize>() {
            if let Some(monitor) = monitors.get(index) {
                if assigned_monitors.contains(&monitor.index) {
                    continue;
                }
                if !result.iter().any(|m| m.index == monitor.index) {
                    result.push(monitor);
                }
            }
        }
    }

    if keys.iter().any(|key| key == "*") {
        for monitor in monitors {
            if assigned_monitors.contains(&monitor.index) {
                continue;
            }
            if !result.iter().any(|m| m.index == monitor.index) {
                result.push(monitor);
            }
        }
    }

    result
}

fn path_to_file_url(path: &Path) -> String {
    let normalized = path.to_string_lossy().replace('\\', "/");
    format!("file:///{normalized}")
}

fn enumerate_monitors() -> Vec<MonitorArea> {
    unsafe extern "system" fn enum_monitor_proc(
        monitor: HMONITOR,
        _hdc: HDC,
        _rect: *mut RECT,
        lparam: LPARAM,
    ) -> BOOL {
        let vec = &mut *(lparam.0 as *mut Vec<MonitorArea>);

        let mut info: MONITORINFOEXW = mem::zeroed();
        info.monitorInfo.cbSize = mem::size_of::<MONITORINFOEXW>() as u32;

        if GetMonitorInfoW(monitor, &mut info as *mut MONITORINFOEXW as *mut _).as_bool() {
            vec.push(MonitorArea {
                index: vec.len(),
                primary: info.monitorInfo.dwFlags != 0,
                rect: info.monitorInfo.rcMonitor,
            });
        }

        BOOL(1)
    }

    let mut monitors = Vec::<MonitorArea>::new();
    unsafe {
        let _ = EnumDisplayMonitors(
            None,
            None,
            Some(enum_monitor_proc),
            LPARAM((&mut monitors as *mut Vec<MonitorArea>) as isize),
        );
    }

    if monitors.len() > 1 {
        let min_height = monitors
            .iter()
            .map(|m| (m.rect.bottom - m.rect.top).max(1))
            .min()
            .unwrap_or(1);
        let row_tolerance = (min_height / 4).max(80);

        monitors.sort_by(|a, b| b.rect.top.cmp(&a.rect.top));

        let mut rows: Vec<(i32, Vec<MonitorArea>)> = Vec::new();
        for monitor in monitors.into_iter() {
            if let Some((_, row)) = rows
                .iter_mut()
                .find(|(anchor_y, _)| (monitor.rect.top - *anchor_y).abs() <= row_tolerance)
            {
                row.push(monitor);
            } else {
                rows.push((monitor.rect.top, vec![monitor]));
            }
        }

        rows.sort_by(|(ay, _), (by, _)| by.cmp(ay));

        let mut flattened = Vec::<MonitorArea>::new();
        for (_, mut row) in rows {
            row.sort_by(|a, b| a.rect.left.cmp(&b.rect.left));
            flattened.extend(row);
        }

        monitors = flattened;
    }

    for (index, monitor) in monitors.iter_mut().enumerate() {
        monitor.index = index;
    }

    monitors
}

fn profile_priority(profile: &WallpaperConfig) -> u8 {
    if profile
        .monitor_index
        .iter()
        .any(|key| key.eq_ignore_ascii_case("p"))
    {
        return 0;
    }

    if profile.monitor_index.iter().any(|key| key == "*") {
        return 2;
    }

    1
}

fn make_span_monitor_area(monitors: &[&MonitorArea]) -> MonitorArea {
    let left = monitors.iter().map(|m| m.rect.left).min().unwrap_or(0);
    let top = monitors.iter().map(|m| m.rect.top).min().unwrap_or(0);
    let right = monitors.iter().map(|m| m.rect.right).max().unwrap_or(0);
    let bottom = monitors.iter().map(|m| m.rect.bottom).max().unwrap_or(0);
    let primary = monitors.iter().any(|m| m.primary);
    let index = monitors.iter().map(|m| m.index).min().unwrap_or(0);

    MonitorArea {
        index,
        primary,
        rect: RECT {
            left,
            top,
            right,
            bottom,
        },
    }
}

fn ensure_desktop_host() -> Option<HWND> {
    unsafe {
        let progman = FindWindowW(w!("Progman"), None).ok()?;
        warn!("[WALLPAPER][HOSTSEL] Progman={:?}", progman);

        let mut spawn_result = 0usize;
        let _ = SendMessageTimeoutW(
            progman,
            0x052C,
            WPARAM(0),
            LPARAM(0),
            SMTO_NORMAL,
            1000,
            Some(&mut spawn_result),
        );

        let mut defview_host: Option<HWND> = None;
        unsafe extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
            let out = (lparam.0 as *mut Option<HWND>).as_mut().unwrap();
            if FindWindowExW(Some(hwnd), None, w!("SHELLDLL_DefView"), None).ok().is_some() {
                *out = Some(hwnd);
                return BOOL(0);
            }
            BOOL(1)
        }
        let _ = EnumWindows(
            Some(enum_proc),
            LPARAM((&mut defview_host) as *mut Option<HWND> as isize),
        );

        if let Some(host) = defview_host {
            warn!("[WALLPAPER][HOSTSEL] DefView host={:?}", host);

            if let Some(workerw) = FindWindowExW(None, Some(host), w!("WorkerW"), None).ok() {
                warn!("[WALLPAPER][HOSTSEL] WorkerW sibling selected={:?}", workerw);
                return Some(workerw);
            }

            if let Some(workerw) = FindWindowExW(Some(progman), None, w!("WorkerW"), None).ok() {
                warn!("[WALLPAPER][HOSTSEL] WorkerW under Progman selected={:?}", workerw);
                return Some(workerw);
            }

            warn!("[WALLPAPER][HOSTSEL] No WorkerW found; using DefView host as fallback");
            return Some(host);
        }

        if let Some(workerw) = FindWindowExW(Some(progman), None, w!("WorkerW"), None).ok() {
            warn!("[WALLPAPER][HOSTSEL] Fallback WorkerW selected={:?}", workerw);
            return Some(workerw);
        }

        warn!("[WALLPAPER][HOSTSEL] Final fallback to Progman");
        Some(progman)
    }
}

fn global_window_states(appdata: &Value) -> Option<MonitorWindowStates> {
    let app_map = appdata.as_object()?;
    let mut states = MonitorWindowStates::default();
    let mut found_focused = false;

    for monitor_entry in app_map.values() {
        let windows = monitor_entry
            .get("windows")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();

        for window in windows {
            let focused = window
                .get("focused")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if !focused {
                continue;
            }

            found_focused = true;
            states.focused = true;
            let state = window
                .get("window_state")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_lowercase();
            if state == "maximized" {
                states.maximized = true;
            }
            if state == "fullscreen" {
                states.fullscreen = true;
            }
        }
    }

    if found_focused {
        Some(states)
    } else {
        None
    }
}