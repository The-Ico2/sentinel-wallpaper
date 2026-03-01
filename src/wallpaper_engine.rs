// sentinel-wallpaper/src/wallpaper_engine.rs
//
// CanvasX GPU-rendered wallpaper engine.
//
// Renders live wallpapers via wgpu (Vulkan/DX12) using CanvasX's HTML/CSS→CXRD
// compiler and GPU scene renderer. Each monitor gets its own GPU context,
// renderer, and scene graph. System data flows from sentinel-core through the
// SentinelBridge IPC client and is bound into the scene automatically.
//
// Replaces the previous WebView2-based renderer with a fully native GPU
// pipeline: no browser engine, no JavaScript runtime, just direct GPU draws.

use std::{
    collections::{HashMap, HashSet},
    fs, mem,
    path::{Path, PathBuf},
    ptr,
    sync::{mpsc, OnceLock},
    thread,
    time::{Duration, Instant},
};

use serde::Deserialize;
use serde_json::Value;
use image::{Rgba, RgbaImage};

// CanvasX Runtime
use canvasx_runtime::gpu::context::GpuContext;
use canvasx_runtime::gpu::renderer::Renderer;
use canvasx_runtime::scene::graph::SceneGraph;
use canvasx_runtime::compiler::html::compile_html;
use canvasx_runtime::compiler::editable::EditableContext;
use canvasx_runtime::ipc::sentinel::{SentinelBridge, SentinelBridgeConfig};
use canvasx_runtime::cxrd::document::SceneType;

// Windows APIs
use windows::{
    core::{w, BOOL, PCWSTR},
    Win32::{
        Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM},
        Graphics::Gdi::{
            BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject,
            EnumDisplayMonitors, GetDC, GetDIBits, GetMonitorInfoW, HDC, HGDIOBJ, HMONITOR,
            MonitorFromWindow, MONITORINFOEXW, MONITOR_DEFAULTTONEAREST, ReleaseDC, SelectObject,
            BI_RGB, BITMAPINFO, BITMAPINFOHEADER, DIB_RGB_COLORS, SRCCOPY,
        },
        Storage::Xps::{PrintWindow, PRINT_WINDOW_FLAGS},
        System::LibraryLoader::GetModuleHandleW,
        UI::WindowsAndMessaging::{
            CreateWindowExW, DefWindowProcW, DestroyWindow, EnumWindows, FindWindowExW,
            FindWindowW, GetClassNameW, GetForegroundWindow, GetWindowLongW, GetWindowRect,
            IsZoomed, RegisterClassW, SendMessageTimeoutW, SetWindowLongW, SetWindowPos,
            ShowWindow, GWL_EXSTYLE, GWL_STYLE, HWND_BOTTOM, HWND_NOTOPMOST, HWND_TOP,
            HWND_TOPMOST, SMTO_NORMAL, SW_HIDE, SW_SHOW, SWP_FRAMECHANGED, SWP_NOACTIVATE,
            SWP_SHOWWINDOW, WINDOW_EX_STYLE, WINDOW_STYLE, WNDCLASSW, WS_CAPTION, WS_CHILD,
            WS_CLIPCHILDREN, WS_CLIPSIBLINGS, WS_EX_APPWINDOW, WS_EX_DLGMODALFRAME,
            WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_WINDOWEDGE, WS_MAXIMIZEBOX,
            WS_MINIMIZEBOX, WS_SYSMENU, WS_THICKFRAME, WS_VISIBLE, SystemParametersInfoW,
            SPI_SETDESKWALLPAPER, SPIF_SENDCHANGE, SPIF_UPDATEINIFILE,
        },
    },
};

use crate::{
    data_loaders::config::{AddonConfig, PauseMode, WallpaperConfig},
    error, warn,
    utility::{sentinel_assets_dir, to_wstring},
};

const HOST_CLASS_NAME: PCWSTR = w!("SentinelWallpaperHostWindow");

// ── Data types ─────────────────────────────────────────────────────────────

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

/// A GPU-rendered wallpaper instance hosted on a single monitor.
struct HostedWallpaper {
    hwnd: HWND,
    gpu_ctx: GpuContext,
    renderer: Renderer,
    scene: SceneGraph,
    source_path: PathBuf,
    monitor_rect: RECT,
    monitor_id: Option<String>,
    pause_focus_mode: PauseMode,
    pause_maximized_mode: PauseMode,
    pause_fullscreen_mode: PauseMode,
    pause_battery_mode: PauseMode,
    paused: bool,
    asset_dir: PathBuf,
    last_frame: Instant,
}

impl HostedWallpaper {
    /// Render a single frame.
    fn render_frame(&mut self) {
        let now = Instant::now();
        let dt = now.duration_since(self.last_frame).as_secs_f32();
        self.last_frame = now;

        let (vw, vh) = (self.gpu_ctx.size.0 as f32, self.gpu_ctx.size.1 as f32);

        // Tick: layout → animate → paint.
        let (instances, clear_color) =
            self.scene.tick(vw, vh, dt, &mut self.renderer.font_system);
        let instances = instances.to_vec();
        let text_areas = self.scene.text_areas();

        // Diagnostic: log render stats once (first frame only).
        static LOGGED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        if !LOGGED.swap(true, std::sync::atomic::Ordering::Relaxed) {
            warn!("[WALLPAPER][DIAG] first frame: {} paint instances, {} text areas, clear={:?}, vp={}x{}",
                instances.len(), text_areas.len(), clear_color, vw, vh);
            // Dump ALL instances for debugging layout rects
            for (i, inst) in instances.iter().enumerate() {
                warn!("[WALLPAPER][DIAG] inst[{}]: rect=[{:.0},{:.0},{:.0},{:.0}] bg=[{:.2},{:.2},{:.2},{:.2}] flags=0x{:x}",
                    i, inst.rect[0], inst.rect[1], inst.rect[2], inst.rect[3],
                    inst.bg_color[0], inst.bg_color[1], inst.bg_color[2], inst.bg_color[3],
                    inst.flags);
            }
            // Dump ALL text areas
            for (i, ta) in text_areas.iter().enumerate() {
                warn!("[WALLPAPER][DIAG] text[{}]: pos=[{:.0},{:.0}] bounds=[{},{},{},{}]",
                    i,
                    ta.left, ta.top,
                    ta.bounds.left, ta.bounds.top, ta.bounds.right, ta.bounds.bottom);
            }
            // Dump node tree layout info for debugging
            let doc = &self.scene.document;
            for (i, node) in doc.nodes.iter().enumerate() {
                let rect = &node.layout.rect;
                let cr = &node.layout.content_rect;
                if !node.classes.is_empty() || i < 10 || (rect.width > 0.0 && rect.height > 0.0 && i < 102) {
                    warn!("[WALLPAPER][DIAG] node[{}] tag={:?} cls={:?} disp={:?} rect=[{:.0},{:.0},{:.0},{:.0}] cr=[{:.0},{:.0},{:.0},{:.0}] grid_col=({},{}) grid_row=({},{})",
                        i, node.tag, node.classes, node.style.display,
                        rect.x, rect.y, rect.width, rect.height,
                        cr.x, cr.y, cr.width, cr.height,
                        node.style.grid_column_start, node.style.grid_column_end,
                        node.style.grid_row_start, node.style.grid_row_end);
                }
            }
        }

        // Submit to GPU.
        self.renderer.begin_frame(&self.gpu_ctx, dt, 1.0);
        match self.renderer.render(&self.gpu_ctx, &instances, text_areas, clear_color) {
            Ok(()) => {}
            Err(wgpu::SurfaceError::Lost | wgpu::SurfaceError::Outdated) => {
                let (w, h) = self.gpu_ctx.size;
                self.gpu_ctx.resize(w, h);
            }
            Err(wgpu::SurfaceError::OutOfMemory) => {
                error!("[WALLPAPER] GPU out of memory");
            }
            Err(e) => {
                warn!("[WALLPAPER] Surface error: {:?}", e);
            }
        }
    }
}

impl Drop for HostedWallpaper {
    fn drop(&mut self) {
        // GPU resources (gpu_ctx, renderer, scene) are dropped automatically
        // before we destroy the window handle.
        unsafe {
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

// ── WallpaperRuntime ───────────────────────────────────────────────────────

pub struct WallpaperRuntime {
    hosted: Vec<HostedWallpaper>,
    /// Sentinel IPC bridge — polls sysdata/appdata in the background.
    bridge: SentinelBridge,
    /// Whether the bridge has connected at least once.
    registry_connected: bool,
    // Pause state
    last_pause_tick: Instant,
    pause_check_interval: Duration,
    idle_pause_after: Option<Duration>,
    log_pause_state_changes: bool,
    // Editable CSS var tracking
    last_editable_tick: Instant,
    editable_cache: HashMap<PathBuf, String>,
    // Monitor change detection
    last_monitor_rects: Vec<RECT>,
    // Snapshot
    last_snapshot_tick: Instant,
    snapshot_tx: Option<mpsc::SyncSender<SnapshotJob>>,
}

impl WallpaperRuntime {
    pub fn new() -> Self {
        let _ = ensure_host_class();

        // Start the Sentinel IPC bridge with wallpaper tracking demands.
        let bridge = SentinelBridge::with_config(SentinelBridgeConfig {
            tracking_demands: vec![
                "time", "cpu", "gpu", "ram", "storage", "displays", "network",
                "wifi", "bluetooth", "audio", "keyboard", "mouse", "power",
                "idle", "system", "processes",
            ]
            .into_iter()
            .map(String::from)
            .collect(),
            poll_interval_ms: 50,
            send_heartbeats: true,
            ..Default::default()
        });

        Self {
            hosted: Vec::new(),
            bridge,
            registry_connected: false,
            last_pause_tick: Instant::now(),
            pause_check_interval: Duration::from_millis(500),
            idle_pause_after: None,
            log_pause_state_changes: true,
            last_editable_tick: Instant::now(),
            editable_cache: HashMap::new(),
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
        // Drop existing hosted wallpapers (drops GPU resources + destroys HWNDs).
        self.hosted.clear();
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
        self.last_editable_tick = Instant::now();
        self.editable_cache.clear();
        warn!("[WALLPAPER][APPLY] Cleared previous hosted wallpapers");

        if config.wallpapers.is_empty() {
            warn!("[WALLPAPER] No wallpaper sections found in config");
            return;
        }

        let assets = fetch_wallpaper_assets(&self.bridge);
        if assets.is_empty() {
            warn!("[WALLPAPER] No wallpaper assets found from IPC or local Assets/wallpaper");
        }

        let monitors = enumerate_monitors();
        if monitors.is_empty() {
            error!("[WALLPAPER] No monitors detected, aborting runtime apply");
            return;
        }
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
                profile.section, profile.wallpaper_id
            );
            return;
        };

        // Find the HTML source file.
        let source_html = asset.path.join("index.html");
        if !source_html.exists() {
            warn!(
                "[WALLPAPER] Asset '{}' has no index.html at {}",
                asset.id,
                source_html.display()
            );
            return;
        }

        warn!(
            "[WALLPAPER][PROFILE] asset='{}' source='{}'",
            asset.id,
            source_html.display()
        );

        let targets =
            resolve_target_monitors(monitors, &profile.monitor_index, assigned_monitors);
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
            match self.launch_into_monitor(profile, &span_target, &source_html, &asset.path) {
                Ok(()) => warn!(
                    "[WALLPAPER] Embedded '{}' as span across {} monitor(s)",
                    profile.wallpaper_id,
                    targets.len(),
                ),
                Err(e) => warn!(
                    "[WALLPAPER] Failed to embed span '{}': {}",
                    profile.wallpaper_id, e
                ),
            }
            return;
        }

        for monitor in targets {
            match self.launch_into_monitor(profile, monitor, &source_html, &asset.path) {
                Ok(()) => warn!(
                    "[WALLPAPER] Embedded '{}' on monitor {}",
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
        source_html: &Path,
        asset_dir: &Path,
    ) -> Result<(), String> {
        warn!(
            "[WALLPAPER][EMBED] monitor={} primary={} rect=[l={},t={},r={},b={}]",
            monitor.index + 1,
            monitor.primary,
            monitor.rect.left,
            monitor.rect.top,
            monitor.rect.right,
            monitor.rect.bottom
        );

        // ── Create Win32 host window in WorkerW ────────────────────────
        let desktop = ensure_desktop_host()
            .ok_or_else(|| "Failed to locate WorkerW desktop host window".to_string())?;

        let parent_rect = window_rect(desktop)
            .ok_or_else(|| "Failed to read desktop host window rect".to_string())?;

        let hwnd = create_desktop_child_window(desktop, parent_rect, monitor.rect)?;
        apply_host_style(hwnd, &profile.z_index)?;
        warn!("[WALLPAPER][EMBED] desktop child created: {:?}", hwnd);

        // ── Create GPU context from the raw HWND ───────────────────────
        let width = (monitor.rect.right - monitor.rect.left).max(1) as u32;
        let height = (monitor.rect.bottom - monitor.rect.top).max(1) as u32;

        let gpu_ctx = pollster::block_on(GpuContext::from_raw_hwnd(hwnd.0 as isize, width, height))
            .map_err(|e| format!("GPU init failed: {e}"))?;
        warn!(
            "[WALLPAPER][EMBED] GPU context ready: {:?} ({}x{})",
            gpu_ctx.backend, width, height
        );

        let renderer = Renderer::new(&gpu_ctx)
            .map_err(|e| format!("Renderer init failed: {e}"))?;

        // ── Compile HTML/CSS → CXRD scene ──────────────────────────────
        let html = fs::read_to_string(source_html)
            .map_err(|e| format!("Failed to read {}: {e}", source_html.display()))?;

        let css_path = asset_dir.join("style.css");
        let css = if css_path.exists() {
            fs::read_to_string(&css_path).unwrap_or_default()
        } else {
            let alt_css = source_html.with_extension("css");
            if alt_css.exists() {
                fs::read_to_string(&alt_css).unwrap_or_default()
            } else {
                String::new()
            }
        };

        let name = asset_dir
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("wallpaper");

        let doc = compile_html(&html, &css, name, SceneType::Wallpaper, Some(asset_dir))
            .map_err(|e| format!("HTML compile failed: {e}"))?;

        // Diagnostic logging: show what the compiled scene contains.
        {
            let total_nodes = doc.nodes.len();
            let bg_nodes = doc.nodes.iter().filter(|n| {
                !matches!(n.style.background, canvasx_runtime::cxrd::style::Background::None)
            }).count();
            let text_nodes = doc.nodes.iter().filter(|n| {
                matches!(n.kind, canvasx_runtime::cxrd::node::NodeKind::Text { .. })
            }).count();
            let data_nodes = doc.nodes.iter().filter(|n| {
                matches!(n.kind, canvasx_runtime::cxrd::node::NodeKind::DataBound { .. })
            }).count();
            warn!("[WALLPAPER][DIAG] doc: {} nodes, {} with bg, {} text, {} data-bound, bg={:?}",
                total_nodes, bg_nodes, text_nodes, data_nodes, doc.background);
        }

        let scene = SceneGraph::new(doc);
        warn!("[WALLPAPER][EMBED] scene compiled from {}", source_html.display());

        self.hosted.push(HostedWallpaper {
            hwnd,
            gpu_ctx,
            renderer,
            scene,
            source_path: source_html.to_path_buf(),
            monitor_rect: monitor.rect,
            monitor_id: None,
            pause_focus_mode: profile.pause_focus_mode,
            pause_maximized_mode: profile.pause_maximized_mode,
            pause_fullscreen_mode: profile.pause_fullscreen_mode,
            pause_battery_mode: profile.pause_battery_mode,
            paused: false,
            asset_dir: asset_dir.to_path_buf(),
            last_frame: Instant::now(),
        });

        Ok(())
    }

    // ── Tick loop ──────────────────────────────────────────────────────

    pub fn tick_interactions(&mut self) -> bool {
        if self.hosted.is_empty() {
            return false;
        }

        let mut unpaused_transition = false;
        let all_paused = self.hosted.iter().all(|h| h.paused);

        // Track bridge connectivity.
        let connected = self.bridge.is_connected();
        if connected != self.registry_connected {
            if connected {
                warn!("[WALLPAPER][REGISTRY] Connection established");
            } else {
                warn!("[WALLPAPER][REGISTRY] Connection lost — suppressing data delivery");
            }
            self.registry_connected = connected;
        }

        // Feed system data from bridge → scene graphs.
        if connected && !all_paused {
            let flat_data = self.bridge.get_data();
            for hosted in &mut self.hosted {
                if hosted.paused {
                    continue;
                }
                hosted.scene.update_data_batch(flat_data.clone());
            }
        }

        // Editable CSS variable updates.
        if self.last_editable_tick.elapsed() >= Duration::from_millis(250) {
            self.last_editable_tick = Instant::now();
            self.check_editable_updates();
        }

        // Pause evaluation.
        if self.last_pause_tick.elapsed() >= self.pause_check_interval {
            self.last_pause_tick = Instant::now();
            let sysdata = self.bridge.get_sysdata_json();
            let appdata = self.bridge.get_appdata_json();
            unpaused_transition = self.sync_pause_state_now_with(all_paused, &sysdata, &appdata);
        }

        // Render active wallpapers.
        for hosted in &mut self.hosted {
            if hosted.paused {
                continue;
            }
            hosted.render_frame();
        }

        // Periodic BMP snapshot (keeps file fresh for crash recovery).
        if !all_paused && self.last_snapshot_tick.elapsed() >= Duration::from_secs(5) {
            self.last_snapshot_tick = Instant::now();
            self.save_snapshot_to_disk();
        }

        unpaused_transition
    }

    pub fn hosted_all_paused(&self) -> bool {
        self.hosted.iter().all(|h| h.paused)
    }

    // ── Snapshot ───────────────────────────────────────────────────────

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
                Err(e) => warn!("[WALLPAPER][SNAP] PrintWindow capture failed: {}", e),
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

    pub fn shutdown_snapshot(&mut self) {
        match self.capture_paused_wallpaper_snapshot(true) {
            Ok(()) => warn!("[WALLPAPER][SHUTDOWN] Captured and applied shutdown snapshot"),
            Err(e) => {
                warn!("[WALLPAPER][SHUTDOWN] Live capture failed ({}), falling back to saved BMP", e);
                self.apply_snapshot_as_wallpaper();
            }
        }
    }

    pub fn apply_snapshot_as_wallpaper(&self) {
        let snapshot_dir = sentinel_assets_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("wallpaper")
            .join("snapshots");
        let snapshot_path = snapshot_dir.join("paused_wallpaper_snapshot.bmp");
        if snapshot_path.exists() {
            match apply_windows_wallpaper(&snapshot_path) {
                Ok(()) => warn!(
                    "[WALLPAPER][SHUTDOWN] Applied snapshot wallpaper: {}",
                    snapshot_path.display()
                ),
                Err(e) => warn!(
                    "[WALLPAPER][SHUTDOWN] Failed to apply snapshot wallpaper: {}",
                    e
                ),
            }
        }
    }

    // ── Monitor change detection ───────────────────────────────────────

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

    // ── Hot reload ─────────────────────────────────────────────────────

    pub fn reload_wallpapers_for_asset_dir(&mut self, asset_dir: &Path) -> usize {
        let mut reloaded = 0usize;
        for hosted in &mut self.hosted {
            if hosted.asset_dir != asset_dir {
                continue;
            }

            // Recompile the scene from HTML/CSS.
            let html = match fs::read_to_string(&hosted.source_path) {
                Ok(h) => h,
                Err(e) => {
                    warn!(
                        "[WALLPAPER][WATCHER] Failed to read '{}': {}",
                        hosted.source_path.display(), e
                    );
                    continue;
                }
            };

            let css_path = asset_dir.join("style.css");
            let css = if css_path.exists() {
                fs::read_to_string(&css_path).unwrap_or_default()
            } else {
                let alt_css = hosted.source_path.with_extension("css");
                if alt_css.exists() {
                    fs::read_to_string(&alt_css).unwrap_or_default()
                } else {
                    String::new()
                }
            };

            let name = asset_dir
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("wallpaper");

            match compile_html(&html, &css, name, SceneType::Wallpaper, Some(asset_dir)) {
                Ok(doc) => {
                    hosted.scene.load_document(doc);
                    reloaded += 1;
                    warn!(
                        "[WALLPAPER][WATCHER] Recompiled scene for '{}'",
                        asset_dir.display()
                    );
                }
                Err(e) => {
                    warn!(
                        "[WALLPAPER][WATCHER] Failed to recompile '{}': {}",
                        asset_dir.display(), e
                    );
                }
            }
        }
        reloaded
    }

    pub fn has_registry_snapshot(&self) -> bool {
        self.bridge.is_connected()
    }

    // ── Pause system ───────────────────────────────────────────────────

    pub fn sync_pause_state_now(&mut self, all_paused_before: bool) -> bool {
        let sysdata = self.bridge.get_sysdata_json();
        let appdata = self.bridge.get_appdata_json();
        self.sync_pause_state_now_with(all_paused_before, &sysdata, &appdata)
    }

    fn sync_pause_state_now_with(
        &mut self,
        all_paused_before: bool,
        sysdata: &Value,
        appdata: &Value,
    ) -> bool {
        let paused_before: Vec<bool> = self.hosted.iter().map(|h| h.paused).collect();
        let states_changed = self.evaluate_and_apply_pause(sysdata, appdata);
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

        // Pause/resume the bridge polling when all wallpapers are paused.
        if all_paused_now {
            self.bridge.pause();
        } else {
            self.bridge.resume();
        }

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
                || mode_triggered(hosted.pause_focus_mode, local_states.focused, global_states.focused)
                || mode_triggered(hosted.pause_maximized_mode, local_states.maximized, global_states.maximized)
                || mode_triggered(hosted.pause_fullscreen_mode, local_states.fullscreen, global_states.fullscreen)
                || mode_triggered(hosted.pause_battery_mode, on_battery, on_battery);

            if should_pause != hosted.paused {
                hosted.paused = should_pause;
                states_changed = true;
                if self.log_pause_state_changes {
                    warn!(
                        "[WALLPAPER][PAUSE] monitor={:?} paused={} idle={} battery={} (local: f={} m={} fs={}; global: f={} m={} fs={})",
                        hosted.monitor_id, should_pause, idle_triggered, on_battery,
                        local_states.focused, local_states.maximized, local_states.fullscreen,
                        global_states.focused, global_states.maximized, global_states.fullscreen
                    );
                }
            }
        }

        states_changed
    }

    fn apply_host_visibility(&mut self) {
        for hosted in &self.hosted {
            unsafe {
                let _ = ShowWindow(
                    hosted.hwnd,
                    if hosted.paused { SW_HIDE } else { SW_SHOW },
                );
            }
        }
    }

    // ── Editable CSS variables ─────────────────────────────────────────

    fn check_editable_updates(&mut self) {
        let mut seen = HashSet::new();
        let mut dirs: Vec<PathBuf> = Vec::new();
        for h in &self.hosted {
            if seen.insert(h.asset_dir.clone()) {
                dirs.push(h.asset_dir.clone());
            }
        }

        for dir in &dirs {
            // Look for manifest.json or meta.json for the editable schema,
            // and editable.yaml for user overrides.
            let manifest_path = dir.join("manifest.json");
            let meta_path = dir.join("meta.json");
            let editable_yaml = dir.join("editable.yaml");

            let editable_source = if manifest_path.exists() {
                &manifest_path
            } else if meta_path.exists() {
                // meta.json may not have editables; check editable.yaml instead
                if editable_yaml.exists() {
                    &manifest_path // EditableContext handles fallback
                } else {
                    continue;
                }
            } else {
                continue;
            };

            // Try to load the editable context.
            let overrides_path = if editable_yaml.exists() {
                Some(editable_yaml.as_path())
            } else {
                None
            };

            let ctx = match EditableContext::load(editable_source, overrides_path) {
                Ok(c) => c,
                Err(_) => continue,
            };

            // Inject CSS variables into scene graph document variables.
            let css_vars_json = serde_json::to_string(&ctx.css_vars).unwrap_or_default();
            let unchanged = self
                .editable_cache
                .get(dir)
                .map(|prev| *prev == css_vars_json)
                .unwrap_or(false);
            if unchanged {
                continue;
            }
            self.editable_cache.insert(dir.clone(), css_vars_json);

            // Apply updated variables to all hosted wallpapers using this asset.
            for hosted in &mut self.hosted {
                if hosted.asset_dir == *dir {
                    for (var_name, var_value) in &ctx.css_vars {
                        if let Some(entry) = hosted.scene.document.variables.iter_mut()
                            .find(|(k, _)| k == var_name) {
                            entry.1 = var_value.clone();
                        } else {
                            hosted.scene.document.variables.push((var_name.clone(), var_value.clone()));
                        }
                    }
                    hosted.scene.invalidate_layout();
                }
            }
        }
    }

    fn capture_paused_wallpaper_snapshot(
        &mut self,
        apply_to_desktop: bool,
    ) -> Result<(), String> {
        if self.hosted.is_empty() {
            return Ok(());
        }

        let min_left = self.hosted.iter().map(|h| h.monitor_rect.left).min()
            .ok_or("No hosted monitor bounds")?;
        let min_top = self.hosted.iter().map(|h| h.monitor_rect.top).min()
            .ok_or("No hosted monitor bounds")?;
        let max_right = self.hosted.iter().map(|h| h.monitor_rect.right).max()
            .ok_or("No hosted monitor bounds")?;
        let max_bottom = self.hosted.iter().map(|h| h.monitor_rect.bottom).max()
            .ok_or("No hosted monitor bounds")?;

        let virtual_width = (max_right - min_left).max(1);
        let virtual_height = (max_bottom - min_top).max(1);
        let mut stitched = RgbaImage::from_pixel(
            virtual_width as u32, virtual_height as u32, Rgba([0, 0, 0, 255]),
        );
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
            return Err("Captured frame is fully black; refusing snapshot".to_string());
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
            if self.log_pause_state_changes {
                warn!("[WALLPAPER][PAUSE] Applied snapshot: {}", snapshot_path.display());
            }
        } else if self.log_pause_state_changes {
            warn!("[WALLPAPER][PAUSE] Snapshot saved (no apply): {}", snapshot_path.display());
        }
        Ok(())
    }
}

// ── Free functions ─────────────────────────────────────────────────────────

fn capture_window_bgra(hwnd: HWND, width: i32, height: i32) -> Result<Vec<u8>, String> {
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
            mem_dc, bitmap, 0, height as u32,
            Some(pixels.as_mut_ptr() as *mut core::ffi::c_void),
            &mut bmi, DIB_RGB_COLORS,
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

fn snapshot_worker(rx: mpsc::Receiver<SnapshotJob>) {
    while let Ok(job) = rx.recv() {
        let mut stitched = RgbaImage::from_pixel(
            job.virtual_width as u32, job.virtual_height as u32, Rgba([0, 0, 0, 255]),
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

        if !has_non_black_pixel { continue; }

        let snapshot_dir = sentinel_assets_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("wallpaper").join("snapshots");
        let _ = fs::create_dir_all(&snapshot_dir);
        let snapshot_path = snapshot_dir.join("paused_wallpaper_snapshot.bmp");
        if let Err(e) = stitched.save(&snapshot_path) {
            warn!("[WALLPAPER][SNAP] Failed to save snapshot: {}", e);
        }
    }
}

fn apply_windows_wallpaper(path: &Path) -> Result<(), String> {
    let wide = to_wstring(path.to_string_lossy().as_ref());
    unsafe {
        SystemParametersInfoW(
            SPI_SETDESKWALLPAPER, 0,
            Some(wide.as_ptr() as *mut core::ffi::c_void),
            SPIF_UPDATEINIFILE | SPIF_SENDCHANGE,
        )
        .map_err(|e| format!("SPI_SETDESKWALLPAPER failed: {e:?}"))
    }
}

// ── Pause helpers ──────────────────────────────────────────────────────────

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
    let Some(windows) = windows else { return states; };

    for window in windows {
        let (focused, maximized, fullscreen) = window_flags(window);
        if focused { states.focused = true; }
        if maximized { states.maximized = true; }
        if fullscreen { states.fullscreen = true; }
    }
    states
}

fn foreground_window_snapshot() -> Option<ForegroundWindowSnapshot> {
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.0.is_null() { return None; }
        if is_shell_foreground_window(hwnd) { return None; }

        let mut states = MonitorWindowStates {
            focused: true,
            maximized: IsZoomed(hwnd).0 != 0,
            fullscreen: false,
        };

        let mut rect = RECT::default();
        if GetWindowRect(hwnd, &mut rect).is_err() {
            return Some(ForegroundWindowSnapshot { monitor_rect: RECT::default(), states });
        }

        let monitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
        if monitor.0.is_null() {
            return Some(ForegroundWindowSnapshot { monitor_rect: RECT::default(), states });
        }

        let mut mi_ex: MONITORINFOEXW = std::mem::zeroed();
        mi_ex.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
        if GetMonitorInfoW(monitor, &mut mi_ex.monitorInfo).0 == 0 {
            return Some(ForegroundWindowSnapshot { monitor_rect: RECT::default(), states });
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

        Some(ForegroundWindowSnapshot { monitor_rect: monitor_rc, states })
    }
}

fn is_shell_foreground_window(hwnd: HWND) -> bool {
    let mut class_buf = [0u16; 256];
    let len = unsafe { GetClassNameW(hwnd, &mut class_buf) };
    if len <= 0 { return false; }

    let class_name = String::from_utf16_lossy(&class_buf[..len as usize]).to_ascii_lowercase();
    matches!(
        class_name.as_str(),
        "progman" | "workerw" | "shell_traywnd" | "shell_secondarytraywnd"
    )
}

fn window_flags(window: &Value) -> (bool, bool, bool) {
    let focused = window.get("focused").and_then(|v| v.as_bool()).unwrap_or(false);
    let state = window.get("window_state").and_then(|v| v.as_str()).unwrap_or("").to_lowercase();
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
        if width <= 0 || height <= 0 { continue; }

        let id = display.get("id").and_then(|v| v.as_str())
            .or_else(|| metadata.get("id").and_then(|v| v.as_str()))
            .map(String::from);
        let Some(id) = id else { continue; };

        let display_rect = RECT { left: x, top: y, right: x + width, bottom: y + height };

        let overlap_left = rect.left.max(display_rect.left);
        let overlap_top = rect.top.max(display_rect.top);
        let overlap_right = rect.right.min(display_rect.right);
        let overlap_bottom = rect.bottom.min(display_rect.bottom);
        let overlap_area = (overlap_right - overlap_left).max(0) as i64
            * (overlap_bottom - overlap_top).max(0) as i64;

        if overlap_area > best_overlap {
            best_overlap = overlap_area;
            best_overlap_id = Some(id.clone());
        }

        let cx = ((rect.left + rect.right) / 2) as i64;
        let cy = ((rect.top + rect.bottom) / 2) as i64;
        let dx = ((display_rect.left + display_rect.right) / 2) as i64;
        let dy = ((display_rect.top + display_rect.bottom) / 2) as i64;
        let distance = (cx - dx).abs() + (cy - dy).abs();
        if distance < best_distance {
            best_distance = distance;
            nearest_id = Some(id);
        }
    }

    if best_overlap > 0 { return best_overlap_id; }
    best_overlap_id.or(nearest_id)
}

fn power_on_battery(sysdata: &Value) -> bool {
    let power = match sysdata.get("power") {
        Some(value) => value,
        None => return false,
    };

    let ac_status = power.get("ac_status").and_then(|v| v.as_str()).unwrap_or("").to_lowercase();

    if matches!(ac_status.as_str(), "online" | "ac" | "plugged" | "plugged_in") { return false; }
    if matches!(ac_status.as_str(), "offline" | "battery" | "on_battery") { return true; }

    power.get("battery").and_then(|b| b.get("present")).and_then(|v| v.as_bool()).unwrap_or(false)
        && !power.get("battery").and_then(|b| b.get("charging")).and_then(|v| v.as_bool()).unwrap_or(false)
}

fn global_window_states(appdata: &Value) -> Option<MonitorWindowStates> {
    let app_map = appdata.as_object()?;
    let mut states = MonitorWindowStates::default();
    let mut found = false;

    for entry in app_map.values() {
        for window in entry.get("windows").and_then(|v| v.as_array()).cloned().unwrap_or_default() {
            let (focused, maximized, fullscreen) = window_flags(&window);
            if focused { states.focused = true; found = true; }
            if maximized { states.maximized = true; found = true; }
            if fullscreen { states.fullscreen = true; found = true; }
        }
    }

    if found { Some(states) } else { None }
}

fn is_shell_foreground_active() -> bool {
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.0.is_null() { return true; }
        is_shell_foreground_window(hwnd)
    }
}

// ── Asset resolution ───────────────────────────────────────────────────────

fn fetch_wallpaper_assets(bridge: &SentinelBridge) -> Vec<RegistryAsset> {
    // Try IPC first.
    if let Ok(raw) = bridge.list_assets() {
        if let Ok(entries) = serde_json::from_value::<Vec<RegistryAsset>>(raw.clone()) {
            let filtered: Vec<RegistryAsset> = entries
                .into_iter()
                .filter(|e| e.category.eq_ignore_ascii_case("wallpaper"))
                .collect();
            if !filtered.is_empty() {
                return filtered;
            }
        } else if let Some(grouped) = raw.as_object() {
            let mut flattened = Vec::<RegistryAsset>::new();
            for (category, arr) in grouped {
                if !category.eq_ignore_ascii_case("wallpaper") { continue; }
                if let Some(items) = arr.as_array() {
                    for item in items {
                        if let Ok(mut asset) = serde_json::from_value::<RegistryAsset>(item.clone()) {
                            if asset.category.is_empty() { asset.category = category.clone(); }
                            flattened.push(asset);
                        }
                    }
                }
            }
            if !flattened.is_empty() {
                return flattened;
            }
        }
    }

    warn!("[WALLPAPER] IPC list_assets unavailable");
    Vec::new()
}

fn resolve_asset<'a>(assets: &'a [RegistryAsset], wallpaper_id: &str) -> Option<&'a RegistryAsset> {
    assets.iter().find(|a| a.id == wallpaper_id)
}

fn resolve_target_monitors<'a>(
    monitors: &'a [MonitorArea],
    keys: &[String],
    assigned_monitors: &HashSet<usize>,
) -> Vec<&'a MonitorArea> {
    let mut result = Vec::<&MonitorArea>::new();

    if keys.iter().any(|key| key.eq_ignore_ascii_case("p")) {
        if let Some(primary) = monitors.iter().find(|m| m.primary) {
            result.push(primary);
        }
    }

    for key in keys {
        if key == "*" || key.eq_ignore_ascii_case("p") { continue; }
        if let Ok(index) = key.parse::<usize>() {
            if let Some(monitor) = monitors.get(index) {
                if assigned_monitors.contains(&monitor.index) { continue; }
                if !result.iter().any(|m| m.index == monitor.index) {
                    result.push(monitor);
                }
            }
        }
    }

    if keys.iter().any(|key| key == "*") {
        for monitor in monitors {
            if assigned_monitors.contains(&monitor.index) { continue; }
            if !result.iter().any(|m| m.index == monitor.index) {
                result.push(monitor);
            }
        }
    }

    result
}

fn profile_priority(profile: &WallpaperConfig) -> u8 {
    if profile.monitor_index.iter().any(|key| key.eq_ignore_ascii_case("p")) { return 0; }
    if profile.monitor_index.iter().any(|key| key == "*") { return 2; }
    1
}

fn make_span_monitor_area(monitors: &[&MonitorArea]) -> MonitorArea {
    let left = monitors.iter().map(|m| m.rect.left).min().unwrap_or(0);
    let top = monitors.iter().map(|m| m.rect.top).min().unwrap_or(0);
    let right = monitors.iter().map(|m| m.rect.right).max().unwrap_or(0);
    let bottom = monitors.iter().map(|m| m.rect.bottom).max().unwrap_or(0);
    let primary = monitors.iter().any(|m| m.primary);
    let index = monitors.iter().map(|m| m.index).min().unwrap_or(0);
    MonitorArea { index, primary, rect: RECT { left, top, right, bottom } }
}

// ── Win32 window helpers ───────────────────────────────────────────────────

fn ensure_host_class() -> Result<(), String> {
    static CLASS_ONCE: OnceLock<bool> = OnceLock::new();
    if CLASS_ONCE.get().is_some() { return Ok(()); }

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

    unsafe { let _ = RegisterClassW(&wc); }
    let _ = CLASS_ONCE.set(true);
    Ok(())
}

unsafe extern "system" fn host_window_proc(
    hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM,
) -> LRESULT {
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

fn create_desktop_child_window(
    worker: HWND, parent_rect: RECT, rect: RECT,
) -> Result<HWND, String> {
    let x = rect.left - parent_rect.left;
    let y = rect.top - parent_rect.top;
    let width = rect.right - rect.left;
    let height = rect.bottom - rect.top;

    let style = WINDOW_STYLE((WS_CHILD | WS_VISIBLE | WS_CLIPSIBLINGS | WS_CLIPCHILDREN).0);
    let ex_style = WINDOW_EX_STYLE((WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE).0);

    let hinstance = unsafe {
        GetModuleHandleW(None)
            .map(|h| HINSTANCE(h.0))
            .map_err(|e| format!("GetModuleHandleW: {e:?}"))?
    };

    let hwnd = unsafe {
        CreateWindowExW(
            ex_style, HOST_CLASS_NAME, PCWSTR::null(), style,
            x, y, width, height,
            Some(worker), None, Some(hinstance), Some(ptr::null()),
        )
    }
    .map_err(|e| format!("CreateWindowExW failed: {e:?}"))?;

    Ok(hwnd)
}

fn window_rect(hwnd: HWND) -> Option<RECT> {
    unsafe {
        let mut rect = RECT::default();
        if GetWindowRect(hwnd, &mut rect).is_ok() { Some(rect) } else { None }
    }
}

fn apply_host_style(hwnd: HWND, z_index: &str) -> Result<(), String> {
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

        if SetWindowPos(
            hwnd, Some(insert_after), 0, 0, 0, 0,
            SWP_NOACTIVATE | SWP_SHOWWINDOW | SWP_FRAMECHANGED,
        ).is_err() {
            return Err("SetWindowPos failed".to_string());
        }
    }
    Ok(())
}

fn ensure_desktop_host() -> Option<HWND> {
    unsafe {
        let progman = FindWindowW(w!("Progman"), None).ok()?;

        let mut spawn_result = 0usize;
        let _ = SendMessageTimeoutW(
            progman, 0x052C, WPARAM(0), LPARAM(0),
            SMTO_NORMAL, 1000, Some(&mut spawn_result),
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
            if let Some(workerw) = FindWindowExW(None, Some(host), w!("WorkerW"), None).ok() {
                return Some(workerw);
            }
            if let Some(workerw) = FindWindowExW(Some(progman), None, w!("WorkerW"), None).ok() {
                return Some(workerw);
            }
            return Some(host);
        }

        if let Some(workerw) = FindWindowExW(Some(progman), None, w!("WorkerW"), None).ok() {
            return Some(workerw);
        }

        Some(progman)
    }
}

// ── Monitor enumeration ────────────────────────────────────────────────────

fn enumerate_monitors() -> Vec<MonitorArea> {
    unsafe extern "system" fn enum_monitor_proc(
        monitor: HMONITOR, _hdc: HDC, _rect: *mut RECT, lparam: LPARAM,
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
            None, None, Some(enum_monitor_proc),
            LPARAM((&mut monitors as *mut Vec<MonitorArea>) as isize),
        );
    }

    // Sort: top-to-bottom rows, then left-to-right within each row.
    if monitors.len() > 1 {
        let min_height = monitors.iter()
            .map(|m| (m.rect.bottom - m.rect.top).max(1))
            .min().unwrap_or(1);
        let row_tolerance = (min_height / 4).max(80);

        monitors.sort_by(|a, b| b.rect.top.cmp(&a.rect.top));

        let mut rows: Vec<(i32, Vec<MonitorArea>)> = Vec::new();
        for monitor in monitors.into_iter() {
            if let Some((_, row)) = rows.iter_mut()
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
