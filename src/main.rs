#![windows_subsystem = "windows"]

mod bootstrap;
mod data_loaders;
mod ipc_connector;
mod logging;
mod utility;
mod wallpaper_engine;
mod paths;

use std::{
	fs,
	thread,
	time::{Duration, Instant, SystemTime},
};
use windows::Win32::UI::HiDpi::{
	SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::WindowsAndMessaging::{
	DispatchMessageW, PeekMessageW, TranslateMessage, MSG, PM_REMOVE, WM_QUIT,
};

use crate::{
	data_loaders::config::{AddonConfig, AddonSettings},
	utility::{addon_root_dir, sentinel_addons_dir},
	wallpaper_engine::WallpaperRuntime,
};

pub const ADDON_NAME: &str = "wallpaper";
pub const DEBUG_NAME: &str = "WALLPAPER";

fn addon_config_path() -> std::path::PathBuf {
	if let Some(root) = addon_root_dir() {
		return root.join("config.yaml");
	}

	if let Some(addons_dir) = sentinel_addons_dir() {
		return addons_dir.join(ADDON_NAME).join("config.yaml");
	}

	std::path::PathBuf::from("config.yaml")
}

fn enable_per_monitor_dpi_awareness() {
	unsafe {
		if SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2).is_err() {
			warn!(
				"[{}] Failed to set process DPI awareness to PerMonitorV2; monitor sizes may be scaled",
				DEBUG_NAME
			);
		}
	}
}

fn main() -> windows::core::Result<()> {
	logging::init(true, "info");
	bootstrap::bootstrap_addon();
	enable_per_monitor_dpi_awareness();

	let config_path = addon_config_path();
	let mut config = AddonConfig::load(&config_path).unwrap_or_else(|| AddonConfig {
		debug: false,
		log_level: "warn".to_string(),
		settings: AddonSettings::default(),
		wallpapers: Vec::new(),
	});

	logging::set_debug(config.debug);
	std::panic::set_hook(Box::new(|panic_info| {
		error!("[{}] Panic: {}", DEBUG_NAME, panic_info);
	}));

	info!("!---------- [{}] Starting Wallpaper Addon ----------!", DEBUG_NAME);
	info!("[{}] Config loaded from {}", DEBUG_NAME, config_path.display());

	let mut runtime = WallpaperRuntime::new();
	runtime.apply(&config);
	let mut loop_sleep = Duration::from_millis(config.settings.runtime.tick_sleep_ms.max(1));
	let mut watcher_enabled = config.settings.performance.watcher.enabled;
	let mut watcher_interval =
		Duration::from_millis(config.settings.performance.watcher.interval_ms.max(100));
	let mut last_watch_tick = Instant::now();
	let mut last_config_modified: Option<SystemTime> = fs::metadata(&config_path)
		.and_then(|m| m.modified())
		.ok();

	loop {
		unsafe {
			let mut msg = MSG::default();
			while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
				if msg.message == WM_QUIT {
					return Ok(());
				}
				let _ = TranslateMessage(&msg);
				DispatchMessageW(&msg);
			}
		}

		let unpaused_transition = runtime.tick_interactions();
		if unpaused_transition && config.settings.runtime.reapply_on_pause_change {
			runtime.apply(&config);
			warn!("[{}][PAUSE] Reapplied runtime after unpause transition", DEBUG_NAME);
		}

		if watcher_enabled && last_watch_tick.elapsed() >= watcher_interval {
			last_watch_tick = Instant::now();

			let current_modified = fs::metadata(&config_path)
				.and_then(|m| m.modified())
				.ok();

			let changed = match (last_config_modified, current_modified) {
				(Some(prev), Some(curr)) => curr > prev,
				(None, Some(_)) => true,
				_ => false,
			};

			if changed {
				match AddonConfig::load(&config_path) {
					Some(new_config) => {
						config = new_config;
						runtime.apply(&config);
						loop_sleep = Duration::from_millis(config.settings.runtime.tick_sleep_ms.max(1));
						watcher_enabled = config.settings.performance.watcher.enabled;
						watcher_interval = Duration::from_millis(
							config.settings.performance.watcher.interval_ms.max(100),
						);
						if config.settings.diagnostics.log_watcher_reloads {
							warn!(
								"[{}][WATCHER] Reloaded config from {}",
								DEBUG_NAME,
								config_path.display()
							);
						}
					}
					None => {
						warn!(
							"[{}][WATCHER] Detected config change but failed to parse {}; keeping previous config",
							DEBUG_NAME,
							config_path.display()
						);
					}
				}

				last_config_modified = current_modified;
			}
		}

		thread::sleep(loop_sleep);
	}
}

