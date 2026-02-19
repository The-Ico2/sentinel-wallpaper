use std::path::Path;

use serde_yaml::{Mapping, Value};

use super::yaml::load_yaml;

#[derive(Debug, Clone)]
pub struct AddonConfig {
    pub update_check: bool,
    pub debug: bool,
    pub log_level: String,
    pub wallpapers: Vec<WallpaperConfig>,
}

#[derive(Debug, Clone)]
pub struct WallpaperConfig {
    pub section: String,
    pub enabled: bool,
    pub monitor_index: Vec<String>,
    pub mode: String,
    pub z_index: String,
    pub wallpaper_id: String,
}

impl AddonConfig {
    pub fn load(path: &Path) -> Option<Self> {
        let value = load_yaml(path)?;
        Self::from_yaml(&value)
    }

    pub fn from_yaml(root: &Value) -> Option<Self> {
        let map = root.as_mapping()?;

        let update_check = bool_at(map, "update_check").unwrap_or(true);
        let debug = bool_at(map, "debug").unwrap_or(false);
        let log_level = str_at(map, "log_level").unwrap_or("warn").to_lowercase();

        let mut wallpapers = parse_wallpaper_sections(map);
        wallpapers.sort_by(|a, b| section_order_key(&a.section).cmp(&section_order_key(&b.section)));

        Some(Self {
            update_check,
            debug,
            log_level,
            wallpapers,
        })
    }

    pub fn enabled_wallpapers(&self) -> Vec<&WallpaperConfig> {
        self.wallpapers.iter().filter(|w| w.enabled).collect()
    }
}

fn parse_wallpaper_sections(map: &Mapping) -> Vec<WallpaperConfig> {
    map.iter()
        .filter_map(|(k, v)| {
            let section = k.as_str()?;
            if !section.starts_with("wallpaper") {
                return None;
            }

            let section_map = v.as_mapping()?;
            let wallpaper_id = str_at(section_map, "wallpaper_id")?.trim().to_string();
            if wallpaper_id.is_empty() {
                return None;
            }

            let enabled = bool_at(section_map, "enabled").unwrap_or(true);
            let monitor_index = string_list_at(section_map, "monitor_index").unwrap_or_else(|| vec!["*".to_string()]);
            let mode = str_at(section_map, "mode").unwrap_or("fill").to_string();
            let z_index = str_at(section_map, "z_index").unwrap_or("desktop").to_lowercase();

            Some(WallpaperConfig {
                section: section.to_string(),
                enabled,
                monitor_index,
                mode,
                z_index,
                wallpaper_id,
            })
        })
        .collect()
}

fn bool_at<'a>(map: &'a Mapping, key: &str) -> Option<bool> {
    map.get(Value::String(key.to_string()))?.as_bool()
}

fn str_at<'a>(map: &'a Mapping, key: &str) -> Option<&'a str> {
    map.get(Value::String(key.to_string()))?.as_str()
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
