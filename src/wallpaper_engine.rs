use std::{
    collections::{HashMap, HashSet},
    fs,
    mem,
    path::{Path, PathBuf},
    ptr,
    sync::{mpsc, OnceLock},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use serde::Deserialize;
use serde_json::Value;
use webview2_com::Microsoft::Web::WebView2::Win32::*;
use image::{Rgba, RgbaImage};
use windows::{
    core::{w, BOOL, PCWSTR},
    Win32::{
        Foundation::{E_POINTER, HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM},
        Graphics::Gdi::{
            BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject,
            EnumDisplayMonitors, GetDC, GetDIBits, GetMonitorInfoW, HDC, HGDIOBJ, HMONITOR, MonitorFromWindow,
            MONITORINFOEXW, MONITOR_DEFAULTTONEAREST, ReleaseDC, SelectObject, BI_RGB, BITMAPINFO, BITMAPINFOHEADER,
            DIB_RGB_COLORS, SRCCOPY,
        },
        Storage::Xps::{PrintWindow, PRINT_WINDOW_FLAGS},
        System::{Com::*, LibraryLoader::GetModuleHandleW},
        UI::WindowsAndMessaging::{
            CreateWindowExW, DefWindowProcW, DestroyWindow, EnumWindows, FindWindowExW, FindWindowW,
            GetClassNameW, GetForegroundWindow, GetWindowLongW, GetWindowRect, IsZoomed, RegisterClassW, SendMessageTimeoutW,
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
    source_url: String,
    monitor_rect: RECT,
    monitor_id: Option<String>,
    pause_focus_mode: PauseMode,
    pause_maximized_mode: PauseMode,
    pause_fullscreen_mode: PauseMode,
    pause_battery_mode: PauseMode,
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

/// Data shipped to the snapshot background thread for stitching + disk save.
struct SnapshotJob {
    captures: Vec<(RECT, Vec<u8>)>,
    virtual_width: i32,
    virtual_height: i32,
    min_left: i32,
    min_top: i32,
}

pub struct WallpaperRuntime {
    hosted: Vec<HostedWallpaper>,
    last_registry_tick: Instant,
    last_registry_payload: Option<String>,
    last_pause_tick: Instant,
    pause_check_interval: Duration,
    idle_pause_after: Option<Duration>,
    log_pause_state_changes: bool,
    last_pause_snapshot_path: Option<PathBuf>,
    cached_sysdata: Value,
    cached_appdata: Value,
    last_editable_tick: Instant,
    editable_cache: HashMap<PathBuf, String>,
    /// Whether the last registry IPC call succeeded.
    /// When false, ALL data delivery to webviews is suppressed.
    registry_connected: bool,
    last_sent_demands: HashSet<String>,
    /// Snapshot of monitor RECTs from the last apply(), used to detect layout changes.
    last_monitor_rects: Vec<RECT>,
    /// Timer for periodic BMP saves (no SPI call — just keeps the file fresh).
    last_snapshot_tick: Instant,
    /// Channel to the background stitching/save thread.
    snapshot_tx: Option<mpsc::SyncSender<SnapshotJob>>,
}

impl WallpaperRuntime {
    pub fn new() -> Self {
        let _ = ensure_host_class();
        unsafe {
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        }

        Self {
            hosted: Vec::new(),
            last_registry_tick: Instant::now(),
            last_registry_payload: None,
            last_pause_tick: Instant::now(),
            pause_check_interval: Duration::from_millis(500),
            idle_pause_after: None,
            log_pause_state_changes: true,
            last_pause_snapshot_path: None,
            cached_sysdata: Value::Null,
            cached_appdata: Value::Null,
            last_editable_tick: Instant::now(),
            editable_cache: HashMap::new(),
            registry_connected: false,
            last_sent_demands: HashSet::new(),
            last_monitor_rects: Vec::new(),
            last_snapshot_tick: Instant::now(),
            snapshot_tx: {
                let (tx, rx) = mpsc::sync_channel::<SnapshotJob>(1);
                thread::Builder::new()
                    .name("snapshot-worker".into())
                    .spawn(move || snapshot_worker(rx))
                    .ok();
                Some(tx)
            },
        }
    }

    pub fn apply(&mut self, config: &AddonConfig) {
        self.hosted.clear();
        self.last_registry_tick = Instant::now();
        self.last_registry_payload = None;
        self.last_pause_tick = Instant::now();
        self.pause_check_interval =
            Duration::from_millis(config.settings.performance.pausing.check_interval_ms.max(100));
        self.idle_pause_after = if config.settings.performance.pausing.idle_timeout_ms == 0 {
            None
        } else {
            Some(Duration::from_millis(
                config.settings.performance.pausing.idle_timeout_ms,
            ))
        };
        self.log_pause_state_changes = config.settings.diagnostics.log_pause_state_changes;
        self.last_pause_snapshot_path = None;
        self.cached_sysdata = Value::Null;
        self.cached_appdata = Value::Null;
        self.last_editable_tick = Instant::now();
        self.editable_cache.clear();
        self.registry_connected = false;
        self.last_sent_demands.clear();
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
        // Snapshot current layout so monitors_changed() can detect rearrangements
        self.last_monitor_rects = monitors.iter().map(|m| m.rect).collect();
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
            source_url: url.to_string(),
            monitor_rect: monitor.rect,
            monitor_id: None,
            pause_focus_mode: profile.pause_focus_mode,
            pause_maximized_mode: profile.pause_maximized_mode,
            pause_fullscreen_mode: profile.pause_fullscreen_mode,
            pause_battery_mode: profile.pause_battery_mode,
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

        let demanded_sections = self.current_demanded_sections();
        if demanded_sections != self.last_sent_demands {
            self.send_tracking_demands(&demanded_sections);
            self.last_sent_demands = demanded_sections.clone();
        }

        // ── Registry snapshot (determines connectivity) ─────────────
        self.last_registry_tick = Instant::now();

        if let Some((sysdata, appdata, payload)) = build_registry_snapshot_and_payload(&demanded_sections) {
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
                    // Send per-monitor bounds BEFORE registry data so cursor
                    // → local coordinate mapping is already set when the
                    // wallpaper's mouse subscription fires.
                    let r = hosted.monitor_rect;
                    let bounds_payload = serde_json::json!({
                        "type": "native_monitor_bounds",
                        "left": r.left,
                        "top": r.top,
                        "width": r.right - r.left,
                        "height": r.bottom - r.top,
                    }).to_string();
                    let _ = post_webview_json(&hosted.webview, &bounds_payload);
                    let _ = post_webview_json(&hosted.webview, &payload);
                }
            }
        } else {
            if self.registry_connected {
                warn!("[WALLPAPER][REGISTRY] Connection lost — suppressing all data delivery");
            }
            self.registry_connected = false;
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

        if !all_paused {
            // Addons do not generate independent runtime telemetry.
        }

        // ── Live editable CSS var updates (manifest.json watch) ──
        if self.last_editable_tick.elapsed() >= Duration::from_millis(250) {
            self.last_editable_tick = Instant::now();
            self.check_editable_updates();
        }

        if self.last_pause_tick.elapsed() >= self.pause_check_interval {
            self.last_pause_tick = Instant::now();
            unpaused_transition = self.sync_pause_state_now(all_paused);
        }

        // ── Periodic BMP save (no SPI call) ────────────────────────
        // Keeps the snapshot file on disk fresh so that:
        //   • The next startup SPI call shows a recent frame
        //   • A Task-Manager kill leaves a recent fallback file
        // Uses PrintWindow on wallpaper HWNDs (correct content, no app
        // windows) and ships pixel buffers to a background thread for
        // the expensive stitching + disk write.
        if !all_paused && self.last_snapshot_tick.elapsed() >= Duration::from_secs(5) {
            self.last_snapshot_tick = Instant::now();
            self.save_snapshot_to_disk();
        }

        unpaused_transition
    }

    pub fn hosted_all_paused(&self) -> bool {
        self.hosted.iter().all(|h| h.paused)
    }

    /// Capture each hosted wallpaper via `PrintWindow` on the main thread,
    /// then ship the raw pixel buffers to a background thread for stitching
    /// + BMP save.  Does NOT call `SPI_SETDESKWALLPAPER`.
    ///
    /// The main-thread work is only `PrintWindow` + `GetDIBits` per monitor
    /// (fast GDI calls).  Skips silently if the worker is still busy.
    pub fn save_snapshot_to_disk(&mut self) {
        if self.hosted.is_empty() || self.hosted.iter().all(|h| h.paused) {
            return;
        }

        let min_left = self.hosted.iter().map(|h| h.monitor_rect.left).min().unwrap_or(0);
        let min_top = self.hosted.iter().map(|h| h.monitor_rect.top).min().unwrap_or(0);
        let max_right = self.hosted.iter().map(|h| h.monitor_rect.right).max().unwrap_or(1);
        let max_bottom = self.hosted.iter().map(|h| h.monitor_rect.bottom).max().unwrap_or(1);

        let virtual_width = (max_right - min_left).max(1);
        let virtual_height = (max_bottom - min_top).max(1);

        let mut captures: Vec<(RECT, Vec<u8>)> = Vec::with_capacity(self.hosted.len());
        for hosted in &self.hosted {
            let width = (hosted.monitor_rect.right - hosted.monitor_rect.left).max(1);
            let height = (hosted.monitor_rect.bottom - hosted.monitor_rect.top).max(1);
            match capture_window_bgra(hosted.hwnd, width, height) {
                Ok(pixels) => captures.push((hosted.monitor_rect, pixels)),
                Err(e) => {
                    warn!("[WALLPAPER][SNAP] PrintWindow capture failed: {}", e);
                }
            }
        }
        if captures.is_empty() {
            return;
        }

        let job = SnapshotJob { captures, virtual_width, virtual_height, min_left, min_top };
        if let Some(tx) = &self.snapshot_tx {
            let _ = tx.try_send(job);
        }
    }

    /// Capture + save + apply as Windows wallpaper.  For shutdown only.
    pub fn shutdown_snapshot(&mut self) {
        match self.capture_paused_wallpaper_snapshot(true) {
            Ok(()) => {
                warn!("[WALLPAPER][SHUTDOWN] Captured and applied shutdown snapshot");
            }
            Err(e) => {
                warn!("[WALLPAPER][SHUTDOWN] Live capture failed ({}), falling back to saved BMP", e);
                self.apply_snapshot_as_wallpaper();
            }
        }
    }

    /// Apply the saved snapshot BMP as the Windows desktop wallpaper via
    /// `SPI_SETDESKWALLPAPER`.  Safe to call before WorkerW children exist
    /// (startup) or after they've been destroyed (shutdown).
    pub fn apply_snapshot_as_wallpaper(&self) {
        let snapshot_dir = sentinel_assets_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("wallpaper")
            .join("snapshots");
        let snapshot_path = snapshot_dir.join("paused_wallpaper_snapshot.bmp");
        if snapshot_path.exists() {
            match apply_windows_wallpaper(&snapshot_path) {
                Ok(()) => {
                    warn!(
                        "[WALLPAPER][SHUTDOWN] Applied snapshot wallpaper: {}",
                        snapshot_path.display()
                    );
                }
                Err(e) => {
                    warn!("[WALLPAPER][SHUTDOWN] Failed to apply snapshot wallpaper: {}", e);
                }
            }
        }
    }

    /// Re-enumerate monitors and return `true` if the layout (count or any
    /// RECT) has changed since the last `apply()`.  This is cheap to call
    /// periodically (a single Win32 `EnumDisplayMonitors` round-trip).
    pub fn monitors_changed(&self) -> bool {
        let current = enumerate_monitors();
        let current_rects: Vec<RECT> = current.iter().map(|m| m.rect).collect();
        if current_rects.len() != self.last_monitor_rects.len() {
            return true;
        }
        current_rects.iter().zip(self.last_monitor_rects.iter()).any(|(a, b)| {
            a.left != b.left || a.top != b.top || a.right != b.right || a.bottom != b.bottom
        })
    }

    pub fn active_asset_dirs(&self) -> Vec<PathBuf> {
        let mut seen = HashSet::new();
        let mut dirs = Vec::new();
        for hosted in &self.hosted {
            if seen.insert(hosted.asset_dir.clone()) {
                dirs.push(hosted.asset_dir.clone());
            }
        }
        dirs
    }

    pub fn reload_wallpapers_for_asset_dir(&mut self, asset_dir: &Path) -> usize {
        let mut reloaded = 0usize;
        for hosted in &mut self.hosted {
            if hosted.asset_dir != asset_dir {
                continue;
            }

            let url = add_reload_nonce(&hosted.source_url);
            let wide = to_wstring(&url);
            let result = unsafe { hosted.webview.Navigate(PCWSTR(wide.as_ptr())) };
            match result {
                Ok(_) => {
                    reloaded += 1;
                }
                Err(e) => {
                    warn!(
                        "[WALLPAPER][WATCHER] Failed to reload wallpaper for '{}' via '{}': {:?}",
                        hosted.asset_dir.display(),
                        hosted.source_url,
                        e
                    );
                }
            }
        }

        reloaded
    }

    pub fn has_registry_snapshot(&self) -> bool {
        !self.cached_sysdata.is_null() && !self.cached_appdata.is_null()
    }

    fn current_demanded_sections(&self) -> HashSet<String> {
        if !self.hosted.iter().any(|h| !h.paused) {
            return HashSet::new();
        }

        [
            "time", "cpu", "gpu", "ram", "storage", "displays", "network", "wifi",
            "bluetooth", "audio", "keyboard", "mouse", "power", "idle", "system",
            "processes", "appdata",
        ]
        .into_iter()
        .map(|s| s.to_string())
        .collect()
    }

    fn send_tracking_demands(&self, demanded_sections: &HashSet<String>) {
        let mut sections: Vec<String> = demanded_sections.iter().cloned().collect();
        sections.sort();
        let args = serde_json::json!({ "sections": sections });
        let _ = request_quick("backend", "set_tracking_demands", Some(args));
    }

    pub fn sync_pause_state_now(&mut self, all_paused_before: bool) -> bool {
        let paused_before: Vec<bool> = self.hosted.iter().map(|h| h.paused).collect();
        let cached_sysdata = self.cached_sysdata.clone();
        let cached_appdata = self.cached_appdata.clone();
        let states_changed = self.evaluate_and_apply_pause(&cached_sysdata, &cached_appdata);
        if !states_changed {
            return false;
        }

        let any_new_paused = self
            .hosted
            .iter()
            .zip(paused_before.iter())
            .any(|(hosted, was_paused)| !*was_paused && hosted.paused);
        let all_paused_now = self.hosted.iter().all(|h| h.paused);
        if any_new_paused {
            if let Err(e) = self.capture_paused_wallpaper_snapshot(all_paused_now) {
                warn!("[WALLPAPER][PAUSE] Snapshot capture/apply failed: {}", e);
            }
        }
        self.apply_host_visibility();
        all_paused_before && !all_paused_now
    }

    fn evaluate_and_apply_pause(&mut self, sysdata: &Value, appdata: &Value) -> bool {
        if self.hosted.is_empty() {
            return false;
        }

        let mut states_changed = false;

        for hosted in &mut self.hosted {
            hosted.monitor_id = resolve_monitor_id_for_rect(sysdata, hosted.monitor_rect);
        }

        let foreground_snapshot = foreground_window_snapshot();
        let shell_foreground = is_shell_foreground_active();
        let mut global_states = global_window_states(appdata).unwrap_or_default();

        if let Some(snapshot) = foreground_snapshot {
            global_states.focused |= snapshot.states.focused;
            global_states.maximized |= snapshot.states.maximized;
            global_states.fullscreen |= snapshot.states.fullscreen;
        }

        if shell_foreground {
            global_states.focused = false;
        }

        let idle_triggered = self
            .idle_pause_after
            .and_then(|threshold| {
                sysdata
                    .get("idle")
                    .and_then(|idle| idle.get("idle_ms"))
                    .and_then(|value| value.as_u64())
                    .map(|idle_ms| idle_ms >= threshold.as_millis() as u64)
            })
            .unwrap_or(false);

        let on_battery = power_on_battery(sysdata);

        for hosted in &mut self.hosted {
            let mut local_states = hosted
                .monitor_id
                .as_deref()
                .map(|id| monitor_window_states(appdata, id))
                .unwrap_or_default();

            if let Some(snapshot) = foreground_snapshot {
                if rect_matches_monitor(hosted.monitor_rect, snapshot.monitor_rect) {
                    local_states.focused |= snapshot.states.focused;
                    local_states.maximized |= snapshot.states.maximized;
                    local_states.fullscreen |= snapshot.states.fullscreen;
                }
            }

            if shell_foreground {
                local_states.focused = false;
            }

            let should_pause = idle_triggered
                || mode_triggered(
                    hosted.pause_focus_mode,
                    local_states.focused,
                    global_states.focused,
                )
                || mode_triggered(
                    hosted.pause_maximized_mode,
                    local_states.maximized,
                    global_states.maximized,
                )
                || mode_triggered(
                    hosted.pause_fullscreen_mode,
                    local_states.fullscreen,
                    global_states.fullscreen,
                )
                || mode_triggered(
                    hosted.pause_battery_mode,
                    on_battery,
                    on_battery,
                );

            if should_pause != hosted.paused {
                hosted.paused = should_pause;
                states_changed = true;
                let payload = format!("{{\"type\":\"native_pause\",\"paused\":{}}}", should_pause);
                let _ = post_webview_json(&hosted.webview, &payload);
                if self.log_pause_state_changes {
                    warn!(
                        "[WALLPAPER][PAUSE] monitor={:?} paused={} idle_triggered={} on_battery={} (local: focused={} maximized={} fullscreen={}; global: focused={} maximized={} fullscreen={})",
                        hosted.monitor_id,
                        should_pause,
                        idle_triggered,
                        on_battery,
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

            // Cache latest editable JSON for change tracking diagnostics; we still
            // rebroadcast vars every tick so late-loading WebViews do not miss
            // updates and fall back to default values.
            let unchanged = self
                .editable_cache
                .get(dir)
                .map(|prev| *prev == editable_json)
                .unwrap_or(false);
            if !unchanged {
                self.editable_cache.insert(dir.clone(), editable_json);
            }

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

    fn capture_paused_wallpaper_snapshot(
        &mut self,
        apply_to_desktop: bool,
    ) -> std::result::Result<(), String> {
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

        if apply_to_desktop {
            apply_windows_wallpaper(&snapshot_path)?;
            self.last_pause_snapshot_path = Some(snapshot_path.clone());
            if self.log_pause_state_changes {
                warn!(
                    "[WALLPAPER][PAUSE] Applied snapshot wallpaper: {}",
                    snapshot_path.display()
                );
            }
        } else if self.log_pause_state_changes {
            warn!(
                "[WALLPAPER][PAUSE] Captured snapshot only (desktop unchanged): {}",
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

/// Background thread that stitches raw pixel captures into an RgbaImage
/// and saves the BMP to disk.  No SPI call — just keeps the file fresh.
fn snapshot_worker(rx: mpsc::Receiver<SnapshotJob>) {
    while let Ok(job) = rx.recv() {
        let mut stitched = RgbaImage::from_pixel(
            job.virtual_width as u32,
            job.virtual_height as u32,
            Rgba([0, 0, 0, 255]),
        );
        let mut has_non_black_pixel = false;

        for (r, pixels) in &job.captures {
            let width = (r.right - r.left).max(1);
            let height = (r.bottom - r.top).max(1);
            let offset_x = (r.left - job.min_left).max(0);
            let offset_y = (r.top - job.min_top).max(0);

            for y in 0..height {
                for x in 0..width {
                    let src = ((y * width + x) * 4) as usize;
                    if src + 3 >= pixels.len() { continue; }
                    let b = pixels[src];
                    let g = pixels[src + 1];
                    let r = pixels[src + 2];
                    if r != 0 || g != 0 || b != 0 { has_non_black_pixel = true; }
                    let dst_x = (offset_x + x) as u32;
                    let dst_y = (offset_y + y) as u32;
                    if dst_x < stitched.width() && dst_y < stitched.height() {
                        stitched.put_pixel(dst_x, dst_y, Rgba([r, g, b, 255]));
                    }
                }
            }
        }

        if !has_non_black_pixel {
            continue;
        }

        let snapshot_dir = sentinel_assets_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("wallpaper")
            .join("snapshots");
        let _ = fs::create_dir_all(&snapshot_dir);
        let snapshot_path = snapshot_dir.join("paused_wallpaper_snapshot.bmp");
        if let Err(e) = stitched.save(&snapshot_path) {
            warn!("[WALLPAPER][SNAP] Failed to save snapshot: {}", e);
        }
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

#[derive(Clone, Copy)]
struct ForegroundWindowSnapshot {
    monitor_rect: RECT,
    states: MonitorWindowStates,
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
        let (focused, maximized, fullscreen) = window_flags(window);

        if focused {
            states.focused = true;
        }
        if maximized {
            states.maximized = true;
        }
        if fullscreen {
            states.fullscreen = true;
        }
    }

    states
}

fn foreground_window_snapshot() -> Option<ForegroundWindowSnapshot> {
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.0.is_null() {
            return None;
        }

        if is_shell_foreground_window(hwnd) {
            return None;
        }

        let mut states = MonitorWindowStates {
            focused: true,
            maximized: IsZoomed(hwnd).0 != 0,
            fullscreen: false,
        };

        let mut rect = RECT::default();
        if GetWindowRect(hwnd, &mut rect).is_err() {
            return Some(ForegroundWindowSnapshot {
                monitor_rect: RECT::default(),
                states,
            });
        }

        let monitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
        if monitor.0.is_null() {
            return Some(ForegroundWindowSnapshot {
                monitor_rect: RECT::default(),
                states,
            });
        }

        let mut mi_ex: MONITORINFOEXW = std::mem::zeroed();
        mi_ex.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
        if GetMonitorInfoW(monitor, &mut mi_ex.monitorInfo).0 == 0 {
            return Some(ForegroundWindowSnapshot {
                monitor_rect: RECT::default(),
                states,
            });
        }

        let monitor_rc = mi_ex.monitorInfo.rcMonitor;
        let epsilon = 1i32;
        let covers_monitor = (rect.left - monitor_rc.left).abs() <= epsilon
            && (rect.top - monitor_rc.top).abs() <= epsilon
            && (rect.right - monitor_rc.right).abs() <= epsilon
            && (rect.bottom - monitor_rc.bottom).abs() <= epsilon;

        let style = GetWindowLongW(hwnd, GWL_STYLE) as u32;
        let has_frame = (style & (WS_CAPTION.0 | WS_THICKFRAME.0)) != 0;
        states.fullscreen = covers_monitor && !has_frame;

        Some(ForegroundWindowSnapshot {
            monitor_rect: monitor_rc,
            states,
        })
    }
}

fn is_shell_foreground_window(hwnd: HWND) -> bool {
    let mut class_buf = [0u16; 256];
    let len = unsafe { GetClassNameW(hwnd, &mut class_buf) };
    if len <= 0 {
        return false;
    }

    let class_name = String::from_utf16_lossy(&class_buf[..len as usize]).to_ascii_lowercase();
    matches!(
        class_name.as_str(),
        "progman" | "workerw" | "shell_traywnd" | "shell_secondarytraywnd"
    )
}

fn window_flags(window: &Value) -> (bool, bool, bool) {
    let focused = window
        .get("focused")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let state = window
        .get("window_state")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_lowercase();

    let maximized = state.contains("maximized") || state.contains("maximised");
    let fullscreen = state.contains("fullscreen") || state.contains("full screen");

    (focused, maximized, fullscreen)
}

fn rect_matches_monitor(lhs: RECT, rhs: RECT) -> bool {
    let epsilon = 1;
    (lhs.left - rhs.left).abs() <= epsilon
        && (lhs.top - rhs.top).abs() <= epsilon
        && (lhs.right - rhs.right).abs() <= epsilon
        && (lhs.bottom - rhs.bottom).abs() <= epsilon
}

fn resolve_monitor_id_for_rect(sysdata: &Value, rect: RECT) -> Option<String> {
    let displays = sysdata.get("displays")?.as_array()?;

    let mut best_overlap: i64 = -1;
    let mut best_overlap_id: Option<String> = None;
    let mut best_distance: i64 = i64::MAX;
    let mut nearest_id: Option<String> = None;

    for display in displays {
        let metadata = display.get("metadata").unwrap_or(display);
        let x = metadata.get("x").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
        let y = metadata.get("y").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
        let width = metadata.get("width").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
        let height = metadata.get("height").and_then(|v| v.as_i64()).unwrap_or(0) as i32;

        if width <= 0 || height <= 0 {
            continue;
        }

        let id = display
            .get("id")
            .and_then(|v| v.as_str())
            .or_else(|| metadata.get("id").and_then(|v| v.as_str()))
            .map(|v| v.to_string());

        let Some(id) = id else {
            continue;
        };

        let display_rect = RECT {
            left: x,
            top: y,
            right: x + width,
            bottom: y + height,
        };

        let overlap_left = rect.left.max(display_rect.left);
        let overlap_top = rect.top.max(display_rect.top);
        let overlap_right = rect.right.min(display_rect.right);
        let overlap_bottom = rect.bottom.min(display_rect.bottom);
        let overlap_w = (overlap_right - overlap_left).max(0) as i64;
        let overlap_h = (overlap_bottom - overlap_top).max(0) as i64;
        let overlap_area = overlap_w * overlap_h;

        if overlap_area > best_overlap {
            best_overlap = overlap_area;
            best_overlap_id = Some(id.clone());
        }

        let rect_center_x = ((rect.left + rect.right) / 2) as i64;
        let rect_center_y = ((rect.top + rect.bottom) / 2) as i64;
        let display_center_x = ((display_rect.left + display_rect.right) / 2) as i64;
        let display_center_y = ((display_rect.top + display_rect.bottom) / 2) as i64;
        let distance = (rect_center_x - display_center_x).abs() + (rect_center_y - display_center_y).abs();
        if distance < best_distance {
            best_distance = distance;
            nearest_id = Some(id);
        }
    }

    if best_overlap > 0 {
        return best_overlap_id;
    }

    best_overlap_id.or(nearest_id)
}

fn power_on_battery(sysdata: &Value) -> bool {
    let power = match sysdata.get("power") {
        Some(value) => value,
        None => return false,
    };

    let ac_status = power
        .get("ac_status")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_lowercase();

    if matches!(ac_status.as_str(), "online" | "ac" | "plugged" | "plugged_in") {
        return false;
    }

    if matches!(ac_status.as_str(), "offline" | "battery" | "on_battery") {
        return true;
    }

    power
        .get("battery")
        .and_then(|battery| battery.get("present"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
        && !power
            .get("battery")
            .and_then(|battery| battery.get("charging"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
}

fn build_registry_snapshot_and_payload(sections: &HashSet<String>) -> Option<(Value, Value, String)> {
    // Single IPC round-trip using the combined `snapshot` command.
    // Uses request_quick (no retries) so the tick loop never blocks for seconds.
    let mut section_list: Vec<String> = sections.iter().cloned().collect();
    section_list.sort();
    let args = serde_json::json!({ "sections": section_list });
    let snapshot_raw = request_quick("registry", "snapshot", Some(args))?;
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
        } else if let Ok(grouped) = serde_json::from_str::<serde_json::Map<String, Value>>(&raw) {
            let mut flattened = Vec::<RegistryAsset>::new();
            for (category, arr) in grouped {
                if !category.eq_ignore_ascii_case("wallpaper") {
                    continue;
                }
                if let Some(items) = arr.as_array() {
                    for item in items {
                        if let Ok(mut asset) = serde_json::from_value::<RegistryAsset>(item.clone()) {
                            if asset.category.is_empty() {
                                asset.category = category.clone();
                            }
                            flattened.push(asset);
                        }
                    }
                }
            }
            if !flattened.is_empty() {
                return flattened;
            }
        } else {
            warn!("[WALLPAPER] Failed to parse registry list_assets payload");
        }
    } else {
        warn!("[WALLPAPER] IPC list_assets request failed");
    }

    Vec::new()
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
    let mut found_window_state = false;

    for monitor_entry in app_map.values() {
        let windows = monitor_entry
            .get("windows")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();

        for window in windows {
            let (focused, maximized, fullscreen) = window_flags(&window);
            if focused {
                states.focused = true;
                found_window_state = true;
            }
            if maximized {
                states.maximized = true;
                found_window_state = true;
            }
            if fullscreen {
                states.fullscreen = true;
                found_window_state = true;
            }
        }
    }

    if found_window_state {
        Some(states)
    } else {
        None
    }
}

fn is_shell_foreground_active() -> bool {
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.0.is_null() {
            return true;
        }
        is_shell_foreground_window(hwnd)
    }
}

fn add_reload_nonce(url: &str) -> String {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);

    if url.contains('?') {
        format!("{}&__sentinel_reload={}", url, nonce)
    } else {
        format!("{}?__sentinel_reload={}", url, nonce)
    }
}