#![windows_subsystem = "windows"]

mod data_loaders;
mod ipc_connector;
mod logging;
mod utility;
mod wallpaper_engine;

use std::{thread, time::Duration};

use crate::{
	data_loaders::config::AddonConfig,
	utility::{addon_root_dir, sentinel_addons_dir},
	wallpaper_engine::WallpaperRuntime,
};

pub const ADDON_NAME: &str = "wallpaper";
pub const DEBUG_NAME: &str = "WALLPAPER";

fn ensure_config_exists() {
	let config_path = addon_config_path();
	if config_path.exists() {
		return;
	}

	let default_config = r#"update_check: true
debug: false
log_level: warn

wallpaper:
  enabled: true
  monitor_index:
	- "*"
  wallpaper_id: "sentinel.default.dark"
  mode: "fill"
  z_index: "desktop"
"#;

	if let Some(parent) = config_path.parent() {
		let _ = std::fs::create_dir_all(parent);
	}

	match std::fs::write(&config_path, default_config) {
		Ok(_) => info!("[{}] Created default config at {}", DEBUG_NAME, config_path.display()),
		Err(e) => error!("[{}] Failed to create config.yaml: {}", DEBUG_NAME, e),
	}
}

fn addon_config_path() -> std::path::PathBuf {
	if let Some(root) = addon_root_dir() {
		return root.join("config.yaml");
	}

	if let Some(addons_dir) = sentinel_addons_dir() {
		return addons_dir.join(ADDON_NAME).join("config.yaml");
	}

	std::path::PathBuf::from("config.yaml")
}

fn main() -> windows::core::Result<()> {
	ensure_config_exists();

	let config_path = addon_config_path();
	let config = AddonConfig::load(&config_path).unwrap_or_else(|| AddonConfig {
		update_check: true,
		debug: false,
		log_level: "warn".to_string(),
		wallpapers: Vec::new(),
	});

	logging::init(config.debug, &config.log_level);
	std::panic::set_hook(Box::new(|panic_info| {
		error!("[{}] Panic: {}", DEBUG_NAME, panic_info);
	}));

	info!("!---------- [{}] Starting Wallpaper Addon ----------!", DEBUG_NAME);
	info!("[{}] Config loaded from {}", DEBUG_NAME, config_path.display());

	let mut runtime = WallpaperRuntime::new();
	runtime.apply(&config);

	loop {
		thread::sleep(Duration::from_secs(30));
	}
}

