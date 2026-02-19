use std::{
    env,
    fs,
    mem,
    path::{Path, PathBuf},
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
            EnumWindows, FindWindowExW, FindWindowW, GetWindow, GetWindowLongW, GetWindowTextW,
            GetWindowThreadProcessId, IsWindowVisible, SendMessageTimeoutW, SetParent, SetWindowLongW,
            SetWindowPos, GW_OWNER, GWL_EXSTYLE, GWL_STYLE, HWND_BOTTOM, HWND_NOTOPMOST, HWND_TOP,
            HWND_TOPMOST, SMTO_NORMAL, SPI_SETDESKWALLPAPER, SPIF_SENDCHANGE, SPIF_UPDATEINIFILE,
            SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_SHOWWINDOW, SYSTEM_PARAMETERS_INFO_ACTION,
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, WS_CAPTION, WS_EX_APPWINDOW, WS_EX_DLGMODALFRAME,
            WS_EX_TOOLWINDOW, WS_EX_WINDOWEDGE, WS_MAXIMIZEBOX, WS_MINIMIZEBOX, WS_SYSMENU,
            WS_THICKFRAME, WS_VISIBLE, SystemParametersInfoW,
        },
    },
};

use crate::{
    data_loaders::config::{AddonConfig, WallpaperConfig},
    error,
    ipc_connector::request,
    utility::{sentinel_assets_dir, to_wstring},
    warn,
};

#[derive(Debug, Deserialize, Clone)]
struct RegistryAsset {
    id: String,
    #[serde(default)]
    category: String,
    #[serde(default)]
    metadata: Value,
    #[serde(default)]
    exe_path: String,
    #[serde(default)]
    path: PathBuf,
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
            warn!("[WALLPAPER] No wallpaper assets found from IPC or local Assets/wallpaper");
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
        let title_hint = launch_spec.window_title_hint();

        let hwnd = wait_for_window(pid, title_hint, Duration::from_secs(8))
            .or_else(|| title_hint.and_then(|t| wait_for_window_by_title(t, Duration::from_secs(8))))
            .ok_or_else(|| {
                if let Some(t) = title_hint {
                    format!("No top-level window found for PID {} or title hint '{}'", pid, t)
                } else {
                    format!("No top-level window found for PID {}", pid)
                }
            })?;

        apply_wallpaper_layer(hwnd, monitor.rect, &profile.z_index)?;

        warn!(
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

        let local_html = asset.path.join("index.html");
        if local_html.exists() {
            return Some(Self::Url {
                url: path_to_file_url(&local_html),
                title_hint,
            });
        }

        None
    }

    fn spawn(&self) -> Result<Child, String> {
        match self {
            LaunchSpec::Url { url, title_hint } => spawn_web_wallpaper(url, title_hint.as_deref()),
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

fn spawn_web_wallpaper(url: &str, title_hint: Option<&str>) -> Result<Child, String> {
    if let Some(browser) = find_chromium_browser() {
        let app_arg = format!("--app={url}");
        match Command::new(&browser)
            .arg("--new-window")
            .arg(app_arg)
            .spawn()
        {
            Ok(child) => {
                warn!("[WALLPAPER] Launched web wallpaper host via {}", browser.display());
                return Ok(child);
            }
            Err(e) => {
                warn!(
                    "[WALLPAPER] Chromium host launch failed '{}': {} (falling back to mshta)",
                    browser.display(),
                    e
                );
            }
        }
    }

    let hta_path = build_web_wallpaper_hta(url, title_hint)?;
    if let Ok(child) = Command::new("mshta.exe").arg(&hta_path).spawn() {
        warn!("[WALLPAPER] Launched web wallpaper host via mshta.exe fallback");
        return Ok(child);
    }

    let windir = env::var("WINDIR").unwrap_or_else(|_| "C:\\Windows".to_string());
    let system_mshta = PathBuf::from(windir).join("System32").join("mshta.exe");

    Command::new(&system_mshta)
        .arg(&hta_path)
        .spawn()
        .map_err(|e| format!("failed to launch web wallpaper host via mshta.exe: {e}"))
}

fn find_chromium_browser() -> Option<PathBuf> {
    if let Ok(local) = env::var("LOCALAPPDATA") {
        let local = PathBuf::from(local);
        let candidates = [
            local.join("Microsoft").join("Edge").join("Application").join("msedge.exe"),
            local.join("Google").join("Chrome").join("Application").join("chrome.exe"),
            local.join("BraveSoftware").join("Brave-Browser").join("Application").join("brave.exe"),
        ];

        for candidate in candidates {
            if candidate.exists() {
                return Some(candidate);
            }
        }
    }

    for base in [env::var("ProgramFiles").ok(), env::var("ProgramFiles(x86)").ok()]
        .into_iter()
        .flatten()
    {
        let base = PathBuf::from(base);
        let candidates = [
            base.join("Microsoft").join("Edge").join("Application").join("msedge.exe"),
            base.join("Google").join("Chrome").join("Application").join("chrome.exe"),
            base.join("BraveSoftware").join("Brave-Browser").join("Application").join("brave.exe"),
        ];

        for candidate in candidates {
            if candidate.exists() {
                return Some(candidate);
            }
        }
    }

    None
}

fn build_web_wallpaper_hta(url: &str, title_hint: Option<&str>) -> Result<PathBuf, String> {
    let mut dir = env::temp_dir();
    dir.push("sentinel-wallpaper");
    fs::create_dir_all(&dir)
        .map_err(|e| format!("failed to create temp wallpaper host dir: {e}"))?;

    let file_name = format!("{}.hta", sanitize_file_name(title_hint.unwrap_or("sentinel-wallpaper")));
    let path = dir.join(file_name);

    let title = escape_html(title_hint.unwrap_or("Sentinel Wallpaper"));
    let page_url = escape_html(url);

    let contents = format!(
        "<html><head><meta http-equiv=\"X-UA-Compatible\" content=\"IE=edge\" />\
<title>{}</title>\
<hta:application id=\"sentinel\" border=\"none\" caption=\"no\" showInTaskBar=\"no\" sysMenu=\"no\"\
 windowState=\"maximize\" maximizeButton=\"no\" minimizeButton=\"no\" scroll=\"no\" />\
<style>html,body{{margin:0;width:100%;height:100%;overflow:hidden;background:#000}}\
iframe{{border:0;width:100%;height:100%}}</style></head>\
<body><iframe src=\"{}\"></iframe></body></html>",
        title, page_url
    );

    fs::write(&path, contents)
        .map_err(|e| format!("failed to write temp wallpaper host file: {e}"))?;

    Ok(path)
}

fn sanitize_file_name(value: &str) -> String {
    let mut out = String::new();
    for c in value.chars() {
        if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
            out.push(c);
        } else {
            out.push('_');
        }
    }

    if out.is_empty() {
        "sentinel_wallpaper".to_string()
    } else {
        out
    }
}

fn escape_html(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
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

        let exe_path = metadata
            .get("exe_path")
            .and_then(|v| v.as_str())
            .unwrap_or("NULL")
            .to_string();

        results.push(RegistryAsset {
            id,
            category: "wallpaper".to_string(),
            metadata,
            exe_path,
            path: dir,
        });
    }

    results
}

fn resolve_asset<'a>(assets: &'a [RegistryAsset], wallpaper_id: &str) -> Option<&'a RegistryAsset> {
    assets.iter().find(|a| a.id == wallpaper_id)
}

fn resolve_target_monitors<'a>(monitors: &'a [MonitorArea], keys: &[String]) -> Vec<&'a MonitorArea> {
    if keys.iter().any(|v| v == "*") {
        return monitors.iter().collect();
    }

    let mut result = Vec::<&MonitorArea>::new();
    for key in keys {
        if key.eq_ignore_ascii_case("p") {
            for monitor in monitors {
                if monitor.primary && !result.iter().any(|m| m.index == monitor.index) {
                    result.push(monitor);
                }
            }
            continue;
        }

        if let Ok(index) = key.parse::<usize>() {
            if index > 0 {
                if let Some(monitor) = monitors.get(index - 1) {
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
        .map(|items| {
            items
                .iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default()
}

fn path_to_file_url(path: &Path) -> String {
    let normalized = path.to_string_lossy().replace('\\', "/");
    format!("file:///{normalized}")
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

fn wait_for_window_by_title(title_hint: &str, timeout: Duration) -> Option<HWND> {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if let Some(hwnd) = find_top_level_window_by_title(title_hint) {
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

fn find_top_level_window_by_title(title_hint: &str) -> Option<HWND> {
    #[derive(Default)]
    struct SearchState {
        title_hint: String,
        found: Option<HWND>,
    }

    unsafe extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let state = &mut *(lparam.0 as *mut SearchState);

        if !IsWindowVisible(hwnd).as_bool() {
            return BOOL(1);
        }

        if GetWindow(hwnd, GW_OWNER).map(|v| v.0).unwrap_or_default() != std::ptr::null_mut() {
            return BOOL(1);
        }

        let mut title_buf = [0u16; 512];
        let len = GetWindowTextW(hwnd, &mut title_buf);
        if len <= 0 {
            return BOOL(1);
        }

        let title = String::from_utf16_lossy(&title_buf[..len as usize]);
        if title.to_lowercase().contains(&state.title_hint.to_lowercase()) {
            state.found = Some(hwnd);
            return BOOL(0);
        }

        BOOL(1)
    }

    let mut state = SearchState {
        title_hint: title_hint.to_string(),
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
        let worker = ensure_workerw()
            .ok_or_else(|| "Failed to locate WorkerW desktop host window".to_string())?;
        unsafe {
            let _ = SetParent(hwnd, Some(worker));
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

        let mut result = 0usize;
        let _ = SendMessageTimeoutW(
            progman,
            0x052C,
            WPARAM(0),
            LPARAM(0),
            SMTO_NORMAL,
            1000,
            Some(&mut result),
        );

        #[derive(Default)]
        struct WorkerSearch {
            worker: Option<HWND>,
        }

        unsafe extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
            let state = &mut *(lparam.0 as *mut WorkerSearch);

            let shell = FindWindowExW(
                Some(hwnd),
                None,
                PCWSTR(to_wstring("SHELLDLL_DefView").as_ptr()),
                None,
            )
            .ok();

            if shell.is_some() {
                let worker = FindWindowExW(
                    None,
                    Some(hwnd),
                    PCWSTR(to_wstring("WorkerW").as_ptr()),
                    None,
                )
                .ok();

                if let Some(worker) = worker {
                    state.worker = Some(worker);
                    return BOOL(0);
                }
            }

            BOOL(1)
        }

        let mut state = WorkerSearch::default();
        let _ = EnumWindows(
            Some(enum_proc),
            LPARAM((&mut state as *mut WorkerSearch) as isize),
        );

        if state.worker.is_some() {
            return state.worker;
        }

        FindWindowExW(
            Some(progman),
            None,
            PCWSTR(to_wstring("WorkerW").as_ptr()),
            None,
        )
        .ok()
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
