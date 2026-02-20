use std::path::Path;

use serde_yaml::{Mapping, Value};

use super::yaml::load_yaml;

#[derive(Debug, Clone)]
pub struct AddonConfig {
    pub debug: bool,
    pub log_level: String,
    pub settings: AddonSettings,
    pub wallpapers: Vec<WallpaperConfig>,
}

#[derive(Debug, Clone)]
pub struct AddonSettings {
    pub performance: PerformanceSettings,
    pub runtime: RuntimeSettings,
    pub diagnostics: DiagnosticsSettings,
    pub development: DevelopmentSettings,
}

#[derive(Debug, Clone)]
pub struct PerformanceSettings {
    pub pausing: PausingSettings,
    pub watcher: WatcherSettings,
    pub interactions: InteractionSettings,
    pub audio: AudioSettings,
}

#[derive(Debug, Clone)]
pub struct PausingSettings {
    pub focus: PauseMode,
    pub maximized: PauseMode,
    pub fullscreen: PauseMode,
    pub check_interval_ms: u64,
}

#[derive(Debug, Clone)]
pub struct WatcherSettings {
    pub enabled: bool,
    pub interval_ms: u64,
}

#[derive(Debug, Clone)]
pub struct InteractionSettings {
    pub send_move: bool,
    pub send_click: bool,
    pub poll_interval_ms: u64,
    pub move_threshold_px: f32,
}

#[derive(Debug, Clone)]
pub struct AudioSettings {
    pub enabled: bool,
    pub sample_interval_ms: u64,
    pub endpoint_refresh_ms: u64,
    pub retry_interval_ms: u64,
    pub change_threshold: f32,
    pub quantize_decimals: u8,
}

#[derive(Debug, Clone)]
pub struct RuntimeSettings {
    pub tick_sleep_ms: u64,
    pub reapply_on_pause_change: bool,
}

#[derive(Debug, Clone)]
pub struct DiagnosticsSettings {
    pub log_pause_state_changes: bool,
    pub log_watcher_reloads: bool,
}

#[derive(Debug, Clone)]
pub struct DevelopmentSettings {
    pub update_check: bool,
    pub debug: bool,
    pub log_level: String,
}

#[derive(Debug, Clone)]
pub struct WallpaperConfig {
    pub section: String,
    pub enabled: bool,
    pub monitor_index: Vec<String>,
    pub mode: String,
    pub z_index: String,
    pub wallpaper_id: String,
    pub pause_focus_mode: PauseMode,
    pub pause_maximized_mode: PauseMode,
    pub pause_fullscreen_mode: PauseMode,
}

impl Default for AddonSettings {
    fn default() -> Self {
        Self {
            performance: PerformanceSettings::default(),
            runtime: RuntimeSettings::default(),
            diagnostics: DiagnosticsSettings::default(),
            development: DevelopmentSettings::default(),
        }
    }
}

impl Default for PerformanceSettings {
    fn default() -> Self {
        Self {
            pausing: PausingSettings::default(),
            watcher: WatcherSettings::default(),
            interactions: InteractionSettings::default(),
            audio: AudioSettings::default(),
        }
    }
}

impl Default for PausingSettings {
    fn default() -> Self {
        Self {
            focus: PauseMode::Off,
            maximized: PauseMode::Off,
            fullscreen: PauseMode::Off,
            check_interval_ms: 500,
        }
    }
}

impl Default for WatcherSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            interval_ms: 600,
        }
    }
}

impl Default for InteractionSettings {
    fn default() -> Self {
        Self {
            send_move: true,
            send_click: true,
            poll_interval_ms: 8,
            move_threshold_px: 0.5,
        }
    }
}

impl Default for AudioSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            sample_interval_ms: 100,
            endpoint_refresh_ms: 1200,
            retry_interval_ms: 2000,
            change_threshold: 0.015,
            quantize_decimals: 2,
        }
    }
}

impl Default for RuntimeSettings {
    fn default() -> Self {
        Self {
            tick_sleep_ms: 8,
            reapply_on_pause_change: true,
        }
    }
}

impl Default for DiagnosticsSettings {
    fn default() -> Self {
        Self {
            log_pause_state_changes: true,
            log_watcher_reloads: true,
        }
    }
}

impl Default for DevelopmentSettings {
    fn default() -> Self {
        Self {
            update_check: true,
            debug: false,
            log_level: "warn".to_string(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PauseMode {
    Off,
    PerMonitor,
    AllMonitors,
}

impl PauseMode {
    fn parse(value: &str) -> Option<Self> {
        match value.trim().to_lowercase().as_str() {
            "off" | "none" | "disabled" | "false" => Some(Self::Off),
            "per-monitor" | "per_monitor" | "permonitor" | "monitor" | "true" => {
                Some(Self::PerMonitor)
            }
            "all-monitors" | "all_monitors" | "allmonitors" | "global" | "all" => {
                Some(Self::AllMonitors)
            }
            _ => None,
        }
    }

    fn from_legacy_bool(value: bool) -> Self {
        if value {
            Self::PerMonitor
        } else {
            Self::Off
        }
    }
}

impl AddonConfig {
    pub fn load(path: &Path) -> Option<Self> {
        let value = load_yaml(path)?;
        Self::from_yaml(&value)
    }

    pub fn from_yaml(root: &Value) -> Option<Self> {
        let map = root.as_mapping()?;

        let settings = parse_settings(map);
        let debug = settings.development.debug;
        let log_level = settings.development.log_level.clone();

        let mut wallpapers = parse_wallpaper_sections(map, &settings);
        wallpapers.sort_by(|a, b| section_order_key(&a.section).cmp(&section_order_key(&b.section)));

        Some(Self {
            debug,
            log_level,
            settings,
            wallpapers,
        })
    }

    pub fn enabled_wallpapers(&self) -> Vec<&WallpaperConfig> {
        self.wallpapers.iter().filter(|w| w.enabled).collect()
    }
}

fn parse_wallpaper_sections(map: &Mapping, settings: &AddonSettings) -> Vec<WallpaperConfig> {
    let mut wallpapers = Vec::<WallpaperConfig>::new();

    for (k, v) in map.iter() {
        let Some(section) = k.as_str() else {
            continue;
        };

        if !section.starts_with("wallpaper") {
            continue;
        }

        if let Some(section_map) = v.as_mapping() {
            if let Some(parsed) = parse_wallpaper_section(section, section_map, settings) {
                wallpapers.push(parsed);
            }
        }
    }

    if let Some(wallpapers_map) = mapping_at(map, "wallpapers") {
        for (k, v) in wallpapers_map.iter() {
            let Some(section) = k.as_str() else {
                continue;
            };

            if !section.starts_with("wallpaper") {
                continue;
            }

            if let Some(section_map) = v.as_mapping() {
                if let Some(parsed) = parse_wallpaper_section(section, section_map, settings) {
                    wallpapers.push(parsed);
                }
            }
        }
    }

    wallpapers
}

fn parse_wallpaper_section(
    section: &str,
    section_map: &Mapping,
    settings: &AddonSettings,
) -> Option<WallpaperConfig> {
    let wallpaper_id = str_at(section_map, "wallpaper_id")?.trim().to_string();
    if wallpaper_id.is_empty() {
        return None;
    }

    let enabled = bool_at(section_map, "enabled").unwrap_or(true);
    let monitor_index =
        string_list_at(section_map, "monitor_index").unwrap_or_else(|| vec!["*".to_string()]);
    let mode = str_at(section_map, "mode").unwrap_or("fill").to_lowercase();
    let z_index = str_at(section_map, "z_index").unwrap_or("desktop").to_lowercase();

    let legacy_focus = bool_at(section_map, "pause_on_focus").map(PauseMode::from_legacy_bool);
    let legacy_maximized = bool_at(section_map, "pause_on_maximized").map(PauseMode::from_legacy_bool);
    let legacy_fullscreen = bool_at(section_map, "pause_on_fullscreen").map(PauseMode::from_legacy_bool);

    let pause_focus_mode = pause_mode_at(section_map, "pause_focus")
        .or_else(|| pause_mode_in_pausing(section_map, "focus"))
        .or(legacy_focus)
        .unwrap_or(settings.performance.pausing.focus);

    let pause_maximized_mode = pause_mode_at(section_map, "pause_maximized")
        .or_else(|| pause_mode_in_pausing(section_map, "maximized"))
        .or(legacy_maximized)
        .unwrap_or(settings.performance.pausing.maximized);

    let mut pause_fullscreen_mode = pause_mode_at(section_map, "pause_fullscreen")
        .or_else(|| pause_mode_in_pausing(section_map, "fullscreen"))
        .or(legacy_fullscreen)
        .unwrap_or(settings.performance.pausing.fullscreen);

    if bool_at(section_map, "pause_fullscreen_all_monitors").unwrap_or(false) {
        pause_fullscreen_mode = PauseMode::AllMonitors;
    }

    Some(WallpaperConfig {
        section: section.to_string(),
        enabled,
        monitor_index,
        mode,
        z_index,
        wallpaper_id,
        pause_focus_mode,
        pause_maximized_mode,
        pause_fullscreen_mode,
    })
}

fn parse_settings(root: &Mapping) -> AddonSettings {
    let mut settings = AddonSettings::default();

    settings.development.update_check = bool_at(root, "update_check").unwrap_or(settings.development.update_check);
    settings.development.debug = bool_at(root, "debug").unwrap_or(settings.development.debug);
    settings.development.log_level = str_at(root, "log_level")
        .unwrap_or(&settings.development.log_level)
        .to_lowercase();

    let settings_map = mapping_at(root, "settings");
    let performance_map = settings_map.and_then(|v| mapping_at(v, "performance"));
    let runtime_map = settings_map.and_then(|v| mapping_at(v, "runtime"));
    let diagnostics_map = settings_map.and_then(|v| mapping_at(v, "diagnostics"));
    let development_map = settings_map.and_then(|v| mapping_at(v, "development"));

    if let Some(perf) = performance_map {
        if let Some(pausing) = mapping_at(perf, "pausing") {
            settings.performance.pausing.focus =
                pause_mode_at(pausing, "focus").unwrap_or(settings.performance.pausing.focus);
            settings.performance.pausing.maximized = pause_mode_at(pausing, "maximized")
                .unwrap_or(settings.performance.pausing.maximized);
            settings.performance.pausing.fullscreen = pause_mode_at(pausing, "fullscreen")
                .unwrap_or(settings.performance.pausing.fullscreen);
            settings.performance.pausing.check_interval_ms = u64_at(pausing, "check_interval_ms")
                .unwrap_or(settings.performance.pausing.check_interval_ms)
                .max(100);
        }

        if let Some(watcher) = mapping_at(perf, "watcher") {
            settings.performance.watcher.enabled = bool_any(
                watcher,
                &["enabled", "auto_reload", "live_reload", "watch_files"],
            )
            .unwrap_or(settings.performance.watcher.enabled);
            settings.performance.watcher.interval_ms = u64_any(
                watcher,
                &["interval_ms", "scan_interval_ms", "check_interval_ms"],
            )
                .unwrap_or(settings.performance.watcher.interval_ms)
                .max(100);
        }

        if let Some(interactions) = mapping_at(perf, "interactions") {
            settings.performance.interactions.send_move = bool_any(
                interactions,
                &["send_move", "pointer_move", "cursor_move", "track_pointer"],
            )
                .unwrap_or(settings.performance.interactions.send_move);
            settings.performance.interactions.send_click = bool_any(
                interactions,
                &["send_click", "pointer_click", "cursor_click"],
            )
                .unwrap_or(settings.performance.interactions.send_click);
            settings.performance.interactions.poll_interval_ms =
                u64_any(interactions, &["poll_interval_ms", "sample_interval_ms", "tick_ms"])
                    .unwrap_or(settings.performance.interactions.poll_interval_ms)
                    .max(1);
            settings.performance.interactions.move_threshold_px =
                f32_any(interactions, &["move_threshold_px", "movement_threshold_px", "threshold_px"])
                    .unwrap_or(settings.performance.interactions.move_threshold_px)
                    .max(0.0);
        }

        if let Some(audio) = mapping_at(perf, "audio") {
            settings.performance.audio.enabled = bool_any(audio, &["enabled", "reactive", "reactivity"])
                .unwrap_or(settings.performance.audio.enabled);
            settings.performance.audio.sample_interval_ms =
                u64_any(audio, &["sample_interval_ms", "update_interval_ms", "tick_ms"])
                .unwrap_or(settings.performance.audio.sample_interval_ms)
                .max(33);
            settings.performance.audio.endpoint_refresh_ms =
                u64_any(audio, &["endpoint_refresh_ms", "device_refresh_ms"])
                .unwrap_or(settings.performance.audio.endpoint_refresh_ms)
                .max(200);
            settings.performance.audio.retry_interval_ms =
                u64_any(audio, &["retry_interval_ms", "device_retry_ms"])
                .unwrap_or(settings.performance.audio.retry_interval_ms)
                .max(200);
            settings.performance.audio.change_threshold =
                f32_any(audio, &["change_threshold", "sensitivity_threshold", "delta_threshold"])
                .unwrap_or(settings.performance.audio.change_threshold)
                .clamp(0.0, 1.0);
            settings.performance.audio.quantize_decimals =
                u64_any(audio, &["quantize_decimals", "precision_decimals"])
                .map(|v| v as u8)
                .unwrap_or(settings.performance.audio.quantize_decimals)
                .min(4);
        }
    }

    if let Some(runtime) = runtime_map {
        settings.runtime.tick_sleep_ms = u64_at(runtime, "tick_sleep_ms")
            .unwrap_or(settings.runtime.tick_sleep_ms)
            .max(1);
        settings.runtime.reapply_on_pause_change = bool_at(runtime, "reapply_on_pause_change")
            .unwrap_or(settings.runtime.reapply_on_pause_change);
    }

    if let Some(diag) = diagnostics_map {
        settings.diagnostics.log_pause_state_changes = bool_any(
            diag,
            &["log_pause_state_changes", "log_pause_changes"],
        )
            .unwrap_or(settings.diagnostics.log_pause_state_changes);
        settings.diagnostics.log_watcher_reloads = bool_any(
            diag,
            &["log_watcher_reloads", "log_live_reload"],
        )
            .unwrap_or(settings.diagnostics.log_watcher_reloads);
    }

    if let Some(dev) = development_map {
        settings.development.update_check =
            bool_any(dev, &["update_check", "check_for_updates"]).unwrap_or(settings.development.update_check);
        settings.development.debug = bool_any(dev, &["debug", "debug_mode"]).unwrap_or(settings.development.debug);
        settings.development.log_level = str_any(dev, &["log_level", "logging"]).unwrap_or("warn").to_lowercase();
    }

    settings
}

fn bool_at<'a>(map: &'a Mapping, key: &str) -> Option<bool> {
    map.get(Value::String(key.to_string()))?.as_bool()
}

fn bool_any(map: &Mapping, keys: &[&str]) -> Option<bool> {
    keys.iter().find_map(|k| bool_at(map, k))
}

fn str_at<'a>(map: &'a Mapping, key: &str) -> Option<&'a str> {
    map.get(Value::String(key.to_string()))?.as_str()
}

fn str_any<'a>(map: &'a Mapping, keys: &[&str]) -> Option<&'a str> {
    keys.iter().find_map(|k| str_at(map, k))
}

fn mapping_at<'a>(map: &'a Mapping, key: &str) -> Option<&'a Mapping> {
    map.get(Value::String(key.to_string()))?.as_mapping()
}

fn u64_at(map: &Mapping, key: &str) -> Option<u64> {
    map.get(Value::String(key.to_string()))?
        .as_i64()
        .and_then(|v| if v >= 0 { Some(v as u64) } else { None })
}

fn u64_any(map: &Mapping, keys: &[&str]) -> Option<u64> {
    keys.iter().find_map(|k| u64_at(map, k))
}

fn f32_at(map: &Mapping, key: &str) -> Option<f32> {
    map.get(Value::String(key.to_string()))?
        .as_f64()
        .map(|v| v as f32)
}

fn f32_any(map: &Mapping, keys: &[&str]) -> Option<f32> {
    keys.iter().find_map(|k| f32_at(map, k))
}

fn pause_mode_at(map: &Mapping, key: &str) -> Option<PauseMode> {
    PauseMode::parse(str_at(map, key)?)
}

fn pause_mode_in_pausing(section_map: &Mapping, key: &str) -> Option<PauseMode> {
    let pausing = mapping_at(section_map, "pausing")?;
    pause_mode_at(pausing, key)
}

fn string_list_at(map: &Mapping, key: &str) -> Option<Vec<String>> {
    let list = map.get(Value::String(key.to_string()))?.as_sequence()?;
    let parsed: Vec<String> = list
        .iter()
        .filter_map(|v| v.as_str().map(|s| s.to_string()))
        .collect();

    if parsed.is_empty() {
        None
    } else {
        Some(parsed)
    }
}

fn section_order_key(section: &str) -> (u8, u32, String) {
    if section == "wallpaper" {
        return (0, 0, section.to_string());
    }

    if let Some(suffix) = section.strip_prefix("wallpaper") {
        if let Ok(number) = suffix.parse::<u32>() {
            return (1, number, section.to_string());
        }
    }

    (2, u32::MAX, section.to_string())
}
