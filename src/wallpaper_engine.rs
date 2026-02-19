use std::{
    mem,
    path::Path,
    process::{Child, Command},
    thread,
    time::{Duration, Instant},
};

use serde::Deserialize;
use serde_json::Value;
use windows::{
    core::{BOOL, PCWSTR},
    Win32::{
        Foundation::{HWND, LPARAM, RECT, WPARAM},
        Graphics::Gdi::{EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFOEXW},
        UI::WindowsAndMessaging::{
            EnumWindows, FindWindowW, FindWindowExW, GetWindow, GetWindowLongW, GetWindowTextW,
            GetWindowThreadProcessId, IsWindowVisible, SendMessageTimeoutW, SetParent, SetWindowLongW,
            SetWindowPos, GW_OWNER, GWL_EXSTYLE, GWL_STYLE, HWND_BOTTOM, HWND_NOTOPMOST, HWND_TOP,
            HWND_TOPMOST, SMTO_NORMAL, SPI_SETDESKWALLPAPER, SPIF_SENDCHANGE, SPIF_UPDATEINIFILE,
            SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_SHOWWINDOW, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS,
            SYSTEM_PARAMETERS_INFO_ACTION, WS_CAPTION, WS_EX_APPWINDOW, WS_EX_DLGMODALFRAME,
            WS_EX_TOOLWINDOW, WS_EX_WINDOWEDGE, WS_MAXIMIZEBOX, WS_MINIMIZEBOX, WS_SYSMENU,
            WS_THICKFRAME, WS_VISIBLE, SystemParametersInfoW,
        },
    },
};

use crate::{
    data_loaders::config::{AddonConfig, WallpaperConfig},
    error, info,
    ipc_connector::request,
    utility::to_wstring,
    warn,
};

#[derive(Debug, Deserialize, Clone)]
struct RegistryAsset {
    id: String,
    category: String,
    metadata: Value,
    exe_path: String,
}

#[derive(Debug, Clone)]
struct MonitorArea {
    index: usize,
    primary: bool,
    rect: RECT,
}

pub struct WallpaperRuntime {
    launched: Vec<Child>,
}

impl WallpaperRuntime {
    pub fn new() -> Self {
        Self { launched: Vec::new() }
    }

    pub fn apply(&mut self, config: &AddonConfig) {
        if config.wallpapers.is_empty() {
            warn!("[WALLPAPER] No wallpaper sections found in config");
            return;
        }

        let assets = fetch_wallpaper_assets();
        if assets.is_empty() {
            warn!("[WALLPAPER] No wallpaper assets found from registryd::list_assets");
        }

        let monitors = enumerate_monitors();
        if monitors.is_empty() {
            error!("[WALLPAPER] No monitors detected, aborting runtime apply");
            return;
        }

        for profile in config.enabled_wallpapers() {
            self.launch_profile(profile, &assets, &monitors);
        }
    }

    fn launch_profile(&mut self, profile: &WallpaperConfig, assets: &[RegistryAsset], monitors: &[MonitorArea]) {
        let Some(asset) = resolve_asset(assets, &profile.wallpaper_id) else {
            warn!(
                "[WALLPAPER] Section '{}' references missing wallpaper_id '{}'",
                profile.section,
                profile.wallpaper_id
            );
            return;
        };

        let launch_spec = match LaunchSpec::from_asset(asset) {
            Some(v) => v,
            None => {
                warn!(
                    "[WALLPAPER] Unable to build launch target from asset '{}'",
                    asset.id
                );
                return;
            }
        };

        let targets = resolve_target_monitors(monitors, &profile.monitor_index);
        if targets.is_empty() {
            warn!(
                "[WALLPAPER] Section '{}' has no resolved monitor targets",
                profile.section
            );
            return;
        }

        for monitor in targets {
            match self.launch_into_monitor(&launch_spec, profile, monitor) {
                Ok(()) => {}
                Err(e) => warn!(
                    "[WALLPAPER] Failed to launch '{}' for monitor {}: {}",
                    profile.wallpaper_id,
                    monitor.index + 1,
                    e
                ),
            }
        }
    }

    fn launch_into_monitor(
        &mut self,
        launch_spec: &LaunchSpec,
        profile: &WallpaperConfig,
        monitor: &MonitorArea,
    ) -> Result<(), String> {
        let child = launch_spec.spawn()?;
        let pid = child.id();

        let hwnd = wait_for_window(pid, launch_spec.window_title_hint(), Duration::from_secs(15))
            .ok_or_else(|| format!("No top-level window detected for PID {}", pid))?;

        apply_wallpaper_layer(hwnd, monitor.rect, &profile.z_index)?;

        info!(
            "[WALLPAPER] Embedded '{}' (section '{}') on monitor {} with z_index='{}'",
            profile.wallpaper_id,
            profile.section,
            monitor.index + 1,
            profile.z_index
        );

        self.launched.push(child);
        Ok(())
    }
}

enum LaunchSpec {
    Url {
        url: String,
        title_hint: Option<String>,
    },
    Command {
        program: String,
        args: Vec<String>,
        title_hint: Option<String>,
    },
}

impl LaunchSpec {
    fn from_asset(asset: &RegistryAsset) -> Option<Self> {
        let title_hint = metadata_string(&asset.metadata, "window_title")
            .or_else(|| metadata_string(&asset.metadata, "title"));

        if let Some(url) = metadata_string(&asset.metadata, "url") {
            return Some(Self::Url { url, title_hint });
        }

        if let Some(command) = metadata_string(&asset.metadata, "command") {
            let args = metadata_string_array(&asset.metadata, "args");
            return Some(Self::Command {
                program: command,
                args,
                title_hint,
            });
        }

        if !asset.exe_path.is_empty() && !asset.exe_path.eq_ignore_ascii_case("NULL") {
            let args = metadata_string_array(&asset.metadata, "args");
            return Some(Self::Command {
                program: asset.exe_path.clone(),
                args,
                title_hint,
            });
        }

        None
    }

    fn spawn(&self) -> Result<Child, String> {
        match self {
            LaunchSpec::Url { url, .. } => {
                let browser_arg = format!("--app={url}");
                Command::new("msedge.exe")
                    .arg("--new-window")
                    .arg(browser_arg)
                    .spawn()
                    .map_err(|e| format!("failed to launch web wallpaper via msedge.exe: {e}"))
            }
            LaunchSpec::Command { program, args, .. } => Command::new(program)
                .args(args)
                .spawn()
                .map_err(|e| format!("failed to launch command '{program}': {e}")),
        }
    }

    fn window_title_hint(&self) -> Option<&str> {
        match self {
            LaunchSpec::Url { title_hint, .. } => title_hint.as_deref(),
            LaunchSpec::Command { title_hint, .. } => title_hint.as_deref(),
        }
    }
}

fn fetch_wallpaper_assets() -> Vec<RegistryAsset> {
    let Some(raw) = request("registryd", "list_assets", None) else {
        warn!("[WALLPAPER] IPC list_assets request failed");
        return Vec::new();
    };

    let parsed = serde_json::from_str::<Vec<RegistryAsset>>(&raw);
    let Ok(entries) = parsed else {
        warn!("[WALLPAPER] Failed to parse registryd list_assets payload");
        return Vec::new();
    };

    entries
        .into_iter()
        .filter(|e| e.category.eq_ignore_ascii_case("wallpaper"))
        .collect()
}

fn resolve_asset<'a>(assets: &'a [RegistryAsset], wallpaper_id: &str) -> Option<&'a RegistryAsset> {
    assets.iter().find(|a| a.id == wallpaper_id)
}

fn resolve_target_monitors<'a>(monitors: &'a [MonitorArea], keys: &[String]) -> Vec<&'a MonitorArea> {
    if keys.iter().any(|v| v == "*") {
        return monitors.iter().collect();
    }

    let mut result: Vec<&MonitorArea> = Vec::new();

    for key in keys {
        if key.eq_ignore_ascii_case("p") {
            for monitor in monitors {
                if monitor.primary && !result.iter().any(|m| m.index == monitor.index) {
                    result.push(monitor);
                }
            }
            continue;
        }

        if let Ok(n) = key.parse::<usize>() {
            if n > 0 {
                if let Some(monitor) = monitors.get(n - 1) {
                    if !result.iter().any(|m| m.index == monitor.index) {
                        result.push(monitor);
                    }
                }
            }
        }
    }

    result
}

fn metadata_string(metadata: &Value, key: &str) -> Option<String> {
    metadata.get(key)?.as_str().map(|s| s.to_string())
}

fn metadata_string_array(metadata: &Value, key: &str) -> Vec<String> {
    metadata
        .get(key)
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default()
}

fn wait_for_window(pid: u32, title_hint: Option<&str>, timeout: Duration) -> Option<HWND> {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if let Some(hwnd) = find_top_level_window_for_pid(pid, title_hint) {
            return Some(hwnd);
        }
        thread::sleep(Duration::from_millis(150));
    }

    None
}

fn find_top_level_window_for_pid(pid: u32, title_hint: Option<&str>) -> Option<HWND> {
    #[derive(Default)]
    struct SearchState {
        pid: u32,
        title_hint: Option<String>,
        found: Option<HWND>,
    }

    unsafe extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let state = &mut *(lparam.0 as *mut SearchState);

        let mut window_pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut window_pid));
        if window_pid != state.pid {
            return BOOL(1);
        }

        if !IsWindowVisible(hwnd).as_bool() {
            return BOOL(1);
        }

        if GetWindow(hwnd, GW_OWNER).map(|v| v.0).unwrap_or_default() != std::ptr::null_mut() {
            return BOOL(1);
        }

        if let Some(ref hint) = state.title_hint {
            let mut title_buf = [0u16; 512];
            let len = GetWindowTextW(hwnd, &mut title_buf);
            if len <= 0 {
                return BOOL(1);
            }

            let title = String::from_utf16_lossy(&title_buf[..len as usize]);
            if !title.to_lowercase().contains(&hint.to_lowercase()) {
                return BOOL(1);
            }
        }

        state.found = Some(hwnd);
        BOOL(0)
    }

    let mut state = SearchState {
        pid,
        title_hint: title_hint.map(|s| s.to_string()),
        found: None,
    };

    unsafe {
        let _ = EnumWindows(Some(enum_proc), LPARAM((&mut state as *mut SearchState) as isize));
    }

    state.found
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

    monitors
}

fn apply_wallpaper_layer(hwnd: HWND, rect: RECT, z_index: &str) -> Result<(), String> {
    let layer = LayerMode::from_config(z_index);

    if layer == LayerMode::Desktop {
        if let Some(worker) = ensure_workerw() {
            unsafe {
                let _ = SetParent(hwnd, Some(worker));
            }
        }
    }

    unsafe {
        let style = GetWindowLongW(hwnd, GWL_STYLE) as u32;
        let mut new_style = style
            & !(WS_CAPTION.0
                | WS_THICKFRAME.0
                | WS_MINIMIZEBOX.0
                | WS_MAXIMIZEBOX.0
                | WS_SYSMENU.0);
        new_style |= WS_VISIBLE.0;
        let _ = SetWindowLongW(hwnd, GWL_STYLE, new_style as i32);

        let ex_style = GetWindowLongW(hwnd, GWL_EXSTYLE) as u32;
        let mut new_ex = ex_style & !(WS_EX_APPWINDOW.0 | WS_EX_WINDOWEDGE.0 | WS_EX_DLGMODALFRAME.0);
        if layer == LayerMode::Overlay {
            new_ex |= WS_EX_TOOLWINDOW.0;
        }
        let _ = SetWindowLongW(hwnd, GWL_EXSTYLE, new_ex as i32);

        let insert_after = layer.insert_after();
        let width = rect.right - rect.left;
        let height = rect.bottom - rect.top;

        if SetWindowPos(
            hwnd,
            Some(insert_after),
            rect.left,
            rect.top,
            width,
            height,
            SWP_NOACTIVATE | SWP_SHOWWINDOW | SWP_FRAMECHANGED,
        )
        .is_err()
        {
            return Err("SetWindowPos failed while embedding wallpaper window".to_string());
        }
    }

    Ok(())
}

#[derive(Copy, Clone, Eq, PartialEq)]
enum LayerMode {
    Desktop,
    Bottom,
    Normal,
    Top,
    TopMost,
    Overlay,
}

impl LayerMode {
    fn from_config(value: &str) -> Self {
        match value.to_lowercase().as_str() {
            "bottom" => Self::Bottom,
            "normal" => Self::Normal,
            "top" => Self::Top,
            "topmost" => Self::TopMost,
            "overlay" => Self::Overlay,
            _ => Self::Desktop,
        }
    }

    fn insert_after(self) -> HWND {
        match self {
            LayerMode::Desktop => HWND_BOTTOM,
            LayerMode::Bottom => HWND_BOTTOM,
            LayerMode::Normal => HWND_NOTOPMOST,
            LayerMode::Top => HWND_TOP,
            LayerMode::TopMost => HWND_TOPMOST,
            LayerMode::Overlay => HWND_TOPMOST,
        }
    }
}

fn ensure_workerw() -> Option<HWND> {
    unsafe {
        let progman = FindWindowW(PCWSTR(to_wstring("Progman").as_ptr()), None).ok()?;

        let mut _result = 0usize;
        let _ = SendMessageTimeoutW(
            progman,
            0x052C,
            WPARAM(0),
            LPARAM(0),
            SMTO_NORMAL,
            1000,
            Some(&mut _result),
        );

        let shell = FindWindowExW(
            Some(progman),
            None,
            PCWSTR(to_wstring("SHELLDLL_DefView").as_ptr()),
            None,
        )
        .ok();
        if shell.is_none() {
            return None;
        }

        let worker = FindWindowExW(
            None,
            Some(progman),
            PCWSTR(to_wstring("WorkerW").as_ptr()),
            None,
        )
        .ok();
        if worker.is_none() {
            return None;
        }

        worker
    }
}

pub fn set_static_wallpaper(path: &Path) {
    let mut wide = to_wstring(path.to_string_lossy().as_ref());
    unsafe {
        let _ = SystemParametersInfoW(
            SYSTEM_PARAMETERS_INFO_ACTION(SPI_SETDESKWALLPAPER.0),
            0,
            Some(wide.as_mut_ptr() as _),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(SPIF_UPDATEINIFILE.0 | SPIF_SENDCHANGE.0),
        );
    }
}
