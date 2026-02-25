// ~/Sentinel/sentinel-addons/wallpaper/src/bootstrap.rs

use std::fs;
use std::path::PathBuf;
use crate::ADDON_NAME;
use crate::utility::{sentinel_addons_dir, sentinel_assets_dir};
use crate::{info, warn};

const EXE_NAME: &str = "sentinel-wallpaper.exe";

/// Returns the canonical addon install directory: `~/.Sentinel/Addons/wallpaper/`
fn addon_install_dir() -> Option<PathBuf> {
    sentinel_addons_dir().map(|d| d.join(ADDON_NAME))
}

/// Returns true if the currently running exe is inside the addon's `bin/` folder.
fn is_running_from_install_dir() -> bool {
    let install_bin = match addon_install_dir() {
        Some(d) => d.join("bin"),
        None => return false,
    };
    match std::env::current_exe() {
        Ok(exe) => exe.starts_with(&install_bin),
        Err(_) => false,
    }
}

/// Check if sentinelc.exe (the backend) is running; if not, start it.
fn ensure_backend_running() {
    info!("[{}] Checking if sentinelc.exe is running...", ADDON_NAME);
    let backend_running = std::process::Command::new("tasklist")
        .args(["/FI", "IMAGENAME eq sentinelc.exe", "/NH"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).contains("sentinelc.exe"))
        .unwrap_or(false);

    if backend_running {
        info!("[{}] sentinelc.exe is already running", ADDON_NAME);
        return;
    }

    warn!("[{}] sentinelc.exe is NOT running, attempting to start it", ADDON_NAME);
    let Some(home) = std::env::var("USERPROFILE").ok() else {
        warn!("[{}] Cannot resolve USERPROFILE to find sentinelc.exe", ADDON_NAME);
        return;
    };
    let backend_exe = PathBuf::from(&home).join(".Sentinel").join("sentinelc.exe");
    if !backend_exe.exists() {
        warn!("[{}] Backend not found at {}", ADDON_NAME, backend_exe.display());
        return;
    }
    match std::process::Command::new(&backend_exe).spawn() {
        Ok(_) => {
            info!("[{}] Started sentinelc.exe from {}", ADDON_NAME, backend_exe.display());
            // Poll until the IPC pipe is available (up to ~10 seconds)
            wait_for_ipc_pipe();
        }
        Err(e) => warn!("[{}] Failed to start sentinelc.exe: {e}", ADDON_NAME),
    }
}

/// Poll until the Sentinel IPC named pipe exists, or give up after ~10 seconds.
fn wait_for_ipc_pipe() {
    use crate::utility::to_wstring;
    use windows::core::PCWSTR;
    use windows::Win32::System::Pipes::WaitNamedPipeW;

    let pipe_wide = to_wstring(r"\\.\pipe\sentinel");
    let pipe_name = PCWSTR(pipe_wide.as_ptr());
    let max_attempts = 40; // 40 * 250ms = 10 seconds

    for attempt in 1..=max_attempts {
        let available = unsafe { WaitNamedPipeW(pipe_name, 250).as_bool() };
        if available {
            info!(
                "[{}] IPC pipe available after {} attempt(s)",
                ADDON_NAME, attempt
            );
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    warn!(
        "[{}] IPC pipe not available after {}ms — continuing anyway",
        ADDON_NAME,
        max_attempts * 250
    );
}

/// Bootstrap the addon: create directory structure, scaffold default files,
/// copy the exe into `bin/`, and relaunch from the installed location.
pub fn bootstrap_addon() {
    info!("[{}] === Bootstrap starting ===", ADDON_NAME);
    info!("[{}] Current exe: {:?}", ADDON_NAME, std::env::current_exe());

    // Ensure the backend is running first
    ensure_backend_running();

    let addon_dir = match addon_install_dir() {
        Some(d) => d,
        None => {
            warn!("[{}] Cannot resolve addon install directory", ADDON_NAME);
            return;
        }
    };
    info!("[{}] Addon directory: {}", ADDON_NAME, addon_dir.display());

    // Create directory structure
    let bin_dir = addon_dir.join("bin");
    let options_dir = addon_dir.join("options");
    let _ = fs::create_dir_all(&bin_dir);
    let _ = fs::create_dir_all(&options_dir);
    info!("[{}] Created directory structure at {}", ADDON_NAME, addon_dir.display());

    // Scaffold default files (only if they don't already exist)
    scaffold_addon_json(&addon_dir);
    scaffold_config_yaml(&addon_dir);
    scaffold_schema_yaml(&addon_dir);
    scaffold_options_html(&options_dir);
    scaffold_options_assets(&options_dir);
    scaffold_default_asset();
    info!("[{}] Scaffolding complete", ADDON_NAME);

    // If already running from the install dir, nothing more to do
    if is_running_from_install_dir() {
        info!("[{}] Already running from install directory — continuing startup", ADDON_NAME);
        return;
    }

    // --- Self-install: copy exe into bin/ and relaunch ---
    let current_exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => { warn!("[{}] Cannot determine current exe path: {e}", ADDON_NAME); return; }
    };

    let dst = bin_dir.join(EXE_NAME);
    info!("[{}] Source: {}", ADDON_NAME, current_exe.display());
    info!("[{}] Target: {}", ADDON_NAME, dst.display());

    let should_copy = match (fs::metadata(&current_exe), fs::metadata(&dst)) {
        (Ok(src_meta), Ok(dst_meta)) => {
            let src_size = src_meta.len();
            let dst_size = dst_meta.len();
            let src_newer = src_meta.modified().ok().zip(dst_meta.modified().ok())
                .map(|(s, d)| s > d).unwrap_or(false);
            info!("[{}] Source size={src_size}, Target size={dst_size}, source_newer={src_newer}", ADDON_NAME);
            src_newer || src_size != dst_size
        }
        (Ok(src_meta), Err(_)) => {
            info!("[{}] Target does not exist, source size={}", ADDON_NAME, src_meta.len());
            true
        }
        _ => {
            warn!("[{}] Cannot read source exe metadata", ADDON_NAME);
            false
        }
    };

    if should_copy {
        info!("[{}] Copying exe to install directory...", ADDON_NAME);
        match fs::copy(&current_exe, &dst) {
            Ok(bytes) => info!("[{}] Copied {bytes} bytes -> {}", ADDON_NAME, dst.display()),
            Err(e) => {
                warn!("[{}] Failed to copy exe: {e}", ADDON_NAME);
                return;
            }
        }
    } else {
        info!("[{}] Exe already up to date, skipping copy", ADDON_NAME);
    }

    // Relaunch from installed location
    let args: Vec<String> = std::env::args().skip(1).collect();
    info!("[{}] Relaunching from {} with args: {:?}", ADDON_NAME, dst.display(), args);
    match std::process::Command::new(&dst).args(&args).spawn() {
        Ok(_) => {
            info!("[{}] Relaunch successful, exiting current process", ADDON_NAME);
            std::process::exit(0);
        }
        Err(e) => warn!("[{}] Failed to relaunch: {e}", ADDON_NAME),
    }
}

fn scaffold_addon_json(addon_dir: &PathBuf) {
    let path = addon_dir.join("addon.json");
    if path.exists() { return; }

    let content = r#"{
    "id": "sentinel.addon.wallpaper",
    "name": "Wallpaper",
    "package": "wallpaper",
    "exe_path": "bin/sentinel-wallpaper.exe",
    "accepts_assets": true,
    "asset_categories": [
        "wallpaper"
    ],
    "version": "1.0.0",
    "repo": "https://github.com/The-Ico2/sentinel-wallpaper",
    "author": {
        "Ico2": "https://github.com/The-Ico2"
    }
}
"#;
    match fs::write(&path, content) {
        Ok(_) => info!("[{}] Created addon.json", ADDON_NAME),
        Err(e) => warn!("[{}] Failed to create addon.json: {e}", ADDON_NAME),
    }
}

fn scaffold_config_yaml(addon_dir: &PathBuf) {
    let path = addon_dir.join("config.yaml");
    if path.exists() { return; }

    let content = r#"settings:
  performance:
    pausing:
      focus: "per-monitor"
      maximized: "per-monitor"
      fullscreen: "all-monitors"
      check_interval_ms: 500
    watcher:
      enabled: true
      interval_ms: 600
    interactions:
      send_move: true
      send_click: true
      poll_interval_ms: 8
      move_threshold_px: 0.5
    audio:
      enabled: true
      sample_interval_ms: 100
      endpoint_refresh_ms: 1200
      retry_interval_ms: 2000
      change_threshold: 0.015
      quantize_decimals: 2
  runtime:
    tick_sleep_ms: 8
    reapply_on_pause_change: true
  diagnostics:
    log_pause_state_changes: true
    log_watcher_reloads: true
  development:
    update_check: true
    debug: false
    log_level: warn

wallpaper:
  enabled: true
  monitor_index:
    - "*"
  wallpaper_id: "sentinel.default"
  mode: "fill"
  z_index: "desktop"
"#;
    match fs::write(&path, content) {
        Ok(_) => info!("[{}] Created config.yaml", ADDON_NAME),
        Err(e) => warn!("[{}] Failed to create config.yaml: {e}", ADDON_NAME),
    }
}

fn scaffold_schema_yaml(addon_dir: &PathBuf) {
    let path = addon_dir.join("schema.yaml");
    if path.exists() { return; }

    let content = r#"version: "1.0"
ui:
  sections:
    - title: "Performance"
      path: "settings.performance"
      sections:
        - title: "Pausing"
          path: "pausing"
          fields:
            - path: "focus"
              label: "Pause On Focus"
              control: "dropdown"
              options: ["off", "on"]
            - path: "maximized"
              label: "Pause On Maximized"
              control: "dropdown"
              options: ["off", "per-monitor", "all-monitors"]
            - path: "fullscreen"
              label: "Pause On Fullscreen"
              control: "dropdown"
              options: ["off", "per-monitor", "all-monitors"]
            - path: "check_interval_ms"
              label: "Pause Check Interval (ms)"
              control: "number_range"
              min: 50
              max: 5000
              step: 50

        - title: "Watcher"
          path: "watcher"
          fields:
            - path: "enabled"
              label: "Watcher Enabled"
              control: "toggle"
            - path: "interval_ms"
              label: "Watcher Interval (ms)"
              control: "number_range"
              min: 100
              max: 10000
              step: 50

        - title: "Interactions"
          path: "interactions"
          fields:
            - path: "send_move"
              label: "Send Mouse Move"
              control: "toggle"
            - path: "send_click"
              label: "Send Mouse Click"
              control: "toggle"
            - path: "poll_interval_ms"
              label: "Interaction Poll Interval (ms)"
              control: "number_range"
              min: 1
              max: 200
              step: 1
            - path: "move_threshold_px"
              label: "Move Threshold (px)"
              control: "number_range"
              min: 0
              max: 20
              step: 0.1

        - title: "Audio"
          path: "audio"
          fields:
            - path: "enabled"
              label: "Audio Enabled"
              control: "toggle"
            - path: "sample_interval_ms"
              label: "Audio Sample Interval (ms)"
              control: "number_range"
              min: 20
              max: 5000
              step: 10
            - path: "endpoint_refresh_ms"
              label: "Audio Endpoint Refresh (ms)"
              control: "number_range"
              min: 100
              max: 10000
              step: 50
            - path: "retry_interval_ms"
              label: "Audio Retry Interval (ms)"
              control: "number_range"
              min: 100
              max: 20000
              step: 100
            - path: "change_threshold"
              label: "Audio Change Threshold"
              control: "number_range"
              min: 0
              max: 1
              step: 0.001
            - path: "quantize_decimals"
              label: "Audio Quantize Decimals"
              control: "number_range"
              min: 0
              max: 6
              step: 1

    - title: "Runtime"
      path: "settings.runtime"
      fields:
        - path: "tick_sleep_ms"
          label: "Tick Sleep (ms)"
          control: "number_range"
          min: 1
          max: 100
          step: 1
        - path: "reapply_on_pause_change"
          label: "Reapply On Pause Change"
          control: "toggle"

    - title: "Diagnostics"
      path: "settings.diagnostics"
      fields:
        - path: "log_pause_state_changes"
          label: "Log Pause State Changes"
          control: "toggle"
        - path: "log_watcher_reloads"
          label: "Log Watcher Reloads"
          control: "toggle"

    - title: "Development"
      path: "settings.development"
      fields:
        - path: "update_check"
          label: "Check For Updates"
          control: "toggle"
        - path: "debug"
          label: "Debug Mode"
          control: "toggle"
        - path: "log_level"
          label: "Log Level"
          control: "dropdown"
          options: ["error", "warn", "info", "debug", "trace"]

    - title: "Wallpaper"
      path: "wallpaper"
      fields:
        - path: "enabled"
          label: "Enabled"
          control: "toggle"
        - path: "monitor_index"
          label: "Monitor Index List"
          description: "Comma-separated list. Use * for all monitors."
          control: "text_list"
        - path: "wallpaper_id"
          label: "Wallpaper Asset"
          control: "asset_selector"
          asset_category: "Wallpapers"
          show_preview: true
        - path: "mode"
          label: "Mode"
          control: "dropdown"
          options: ["fill", "fit", "stretch", "center", "tile"]
        - path: "z_index"
          label: "Layer"
          control: "dropdown"
          options: ["desktop", "bottom", "normal", "top"]
"#;
    match fs::write(&path, content) {
        Ok(_) => info!("[{}] Created schema.yaml", ADDON_NAME),
        Err(e) => warn!("[{}] Failed to create schema.yaml: {e}", ADDON_NAME),
    }
}

fn scaffold_options_html(options_dir: &PathBuf) {
    let pages = [
        ("settings.html", "Wallpaper - Settings", "settings-root"),
        ("discover.html", "Wallpaper - Discover", "discover-root"),
        ("editor.html",   "Wallpaper - Editor",   "editor-root"),
        ("library.html",  "Wallpaper - Library",  "library-root"),
    ];
    for (file, title, root_id) in &pages {
        let path = options_dir.join(file);
        if path.exists() { continue; }
        let content = format!(
r#"<!DOCTYPE html>
<html>
<head>
	<meta charset="UTF-8" />
	<meta name="viewport" content="width=device-width, initial-scale=1.0" />
	<title>{title}</title>
	<link rel="stylesheet" href="./options.css" />
	<script src="./options.js"></script>
</head>
<body>
	<main class="page">
		<div id="{root_id}"></div>
	</main>
</body>
</html>
"#);
        match fs::write(&path, content) {
            Ok(_) => info!("[{}] Created options/{}", ADDON_NAME, file),
            Err(e) => warn!("[{}] Failed to create options/{}: {e}", ADDON_NAME, file),
        }
    }
}

// ── Bundled options CSS / JS ──────────────────────────────────────────

const OPTIONS_CSS: &str = include_str!("../options/options.css");
const OPTIONS_JS: &str = include_str!("../options/options.js");

fn scaffold_options_assets(options_dir: &PathBuf) {
    let files: &[(&str, &str)] = &[
        ("options.css", OPTIONS_CSS),
        ("options.js", OPTIONS_JS),
    ];
    for (name, content) in files {
        let path = options_dir.join(name);
        if path.exists() { continue; }
        match fs::write(&path, content) {
            Ok(_) => info!("[{}] Created options/{}", ADDON_NAME, name),
            Err(e) => warn!("[{}] Failed to create options/{}: {e}", ADDON_NAME, name),
        }
    }
}

// ── Bundled default wallpaper asset ──────────────────────────────────

const DEFAULT_ASSET_MANIFEST: &str = include_str!("../assets/sentinel.default/manifest.json");
const DEFAULT_ASSET_INDEX: &str = include_str!("../assets/sentinel.default/index.html");
const DEFAULT_ASSET_PREVIEW: &[u8] = include_bytes!("../assets/sentinel.default/preview/1.png");
const SENTINEL_JS: &str = include_str!("../assets/sentinel.js");

fn scaffold_default_asset() {
    let Some(assets_dir) = sentinel_assets_dir() else {
        warn!("[{}] Cannot resolve Assets directory for default asset", ADDON_NAME);
        return;
    };

    let asset_dir = assets_dir.join("wallpaper").join("sentinel.default");
    let preview_dir = asset_dir.join("preview");
    let _ = fs::create_dir_all(&preview_dir);

    // Scaffold sentinel.js into Assets/wallpaper/ (shared SDK for all wallpapers)
    let sentinel_js_path = assets_dir.join("wallpaper").join("sentinel.js");
    if !sentinel_js_path.exists() {
        match fs::write(&sentinel_js_path, SENTINEL_JS) {
            Ok(_) => info!("[{}] Created Assets/wallpaper/sentinel.js", ADDON_NAME),
            Err(e) => warn!("[{}] Failed to create Assets/wallpaper/sentinel.js: {e}", ADDON_NAME),
        }
    }

    let manifest_path = asset_dir.join("manifest.json");
    if !manifest_path.exists() {
        match fs::write(&manifest_path, DEFAULT_ASSET_MANIFEST) {
            Ok(_) => info!("[{}] Created default asset manifest.json", ADDON_NAME),
            Err(e) => warn!("[{}] Failed to create default asset manifest.json: {e}", ADDON_NAME),
        }
    }

    let index_path = asset_dir.join("index.html");
    if !index_path.exists() {
        match fs::write(&index_path, DEFAULT_ASSET_INDEX) {
            Ok(_) => info!("[{}] Created default asset index.html", ADDON_NAME),
            Err(e) => warn!("[{}] Failed to create default asset index.html: {e}", ADDON_NAME),
        }
    }

    let preview_path = preview_dir.join("1.png");
    if !preview_path.exists() {
        match fs::write(&preview_path, DEFAULT_ASSET_PREVIEW) {
            Ok(_) => info!("[{}] Created default asset preview/1.png", ADDON_NAME),
            Err(e) => warn!("[{}] Failed to create default asset preview/1.png: {e}", ADDON_NAME),
        }
    }

    info!("[{}] Default wallpaper asset (sentinel.default) scaffolded", ADDON_NAME);
}