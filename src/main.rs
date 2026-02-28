#![windows_subsystem = "windows"]

mod bootstrap;
mod data_loaders;
mod ipc_connector;
mod logging;
mod utility;
mod wallpaper_engine;
mod paths;

use std::{
	collections::HashMap,
	fs,
	path::Path,
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

fn should_ignore_asset_reload_path(path: &Path) -> bool {
	let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
		return false;
	};

	let lower_name = file_name.to_ascii_lowercase();
	if lower_name == "manifest.json"
		|| lower_name.ends_with(".tmp")
		|| lower_name.ends_with(".temp")
		|| lower_name.ends_with(".swp")
		|| lower_name.ends_with(".bak")
		|| lower_name.starts_with(".~")
	{
		return true;
	}

	let in_preview_dir = path
		.components()
		.filter_map(|c| c.as_os_str().to_str())
		.any(|seg| seg.eq_ignore_ascii_case("preview"));

	if in_preview_dir {
		return true;
	}

	false
}

fn newest_file_modified_recursive(dir: &Path) -> Option<SystemTime> {
	let mut newest: Option<SystemTime> = None;
	let entries = fs::read_dir(dir).ok()?;

	for entry in entries.flatten() {
		let path = entry.path();
		if path.is_dir() {
			if let Some(child_newest) = newest_file_modified_recursive(&path) {
				newest = match newest {
					Some(current) if current >= child_newest => Some(current),
					_ => Some(child_newest),
				};
			}
		} else {
			if should_ignore_asset_reload_path(&path) {
				continue;
			}

			let Ok(modified) = fs::metadata(&path).and_then(|m| m.modified()) else {
				continue;
			};

			newest = match newest {
				Some(current) if current >= modified => Some(current),
				_ => Some(modified),
			};
		}
	}

	newest
}

fn main() -> windows::core::Result<()> {
	logging::init(true, "info");
	bootstrap::bootstrap_addon();
	enable_per_monitor_dpi_awareness();

	let config_path = addon_config_path();
	let mut config = AddonConfig::load(&config_path).unwrap_or_else(|| AddonConfig {
		debug: false,
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

	// Refresh Windows' wallpaper cache with the saved snapshot BMP BEFORE
	// creating WorkerW children.  This ensures that if the process is later
	// killed (Task Manager, crash) Windows shows a recent frame instead of
	// whatever was cached from a previous session.
	runtime.apply_snapshot_as_wallpaper();

	runtime.apply(&config);
	if runtime.has_registry_snapshot() {
		let _ = runtime.sync_pause_state_now(false);
	}
	let mut loop_sleep = Duration::from_millis(config.settings.runtime.tick_sleep_ms.max(1));
	let mut watcher_enabled = config.settings.performance.watcher.enabled;
	let mut watcher_interval =
		Duration::from_millis(config.settings.performance.watcher.interval_ms.max(100));
	let mut last_watch_tick = Instant::now();
	let mut last_config_modified: Option<SystemTime> = fs::metadata(&config_path)
		.and_then(|m| m.modified())
		.ok();
	let mut watched_asset_mtime: HashMap<std::path::PathBuf, SystemTime> = runtime
		.active_asset_dirs()
		.into_iter()
		.filter_map(|dir| newest_file_modified_recursive(&dir).map(|mtime| (dir, mtime)))
		.collect();
	let mut pending_asset_reload_since: HashMap<std::path::PathBuf, Instant> = HashMap::new();
	let watcher_debounce = Duration::from_millis(400);

	let mut last_monitor_check = Instant::now();
	let monitor_check_interval = Duration::from_secs(2);

	loop {
		unsafe {
			let mut msg = MSG::default();
			while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
				if msg.message == WM_QUIT {
					warn!("[{}] WM_QUIT received — applying shutdown snapshot", DEBUG_NAME);
					runtime.shutdown_snapshot();
					return Ok(());
				}
				let _ = TranslateMessage(&msg);
				DispatchMessageW(&msg);
			}
		}

		let unpaused_transition = runtime.tick_interactions();
		if unpaused_transition && config.settings.runtime.reapply_on_pause_change {
			let all_paused_before = runtime.hosted_all_paused();
			runtime.apply(&config);
			if runtime.has_registry_snapshot() {
				let _ = runtime.sync_pause_state_now(all_paused_before);
			}
			warn!("[{}][PAUSE] Reapplied runtime after unpause transition", DEBUG_NAME);
		}

		// Detect monitor layout changes (rearranged, added, removed, resolution)
		// and fully reapply so wallpaper windows land on the correct rects.
		if last_monitor_check.elapsed() >= monitor_check_interval {
			last_monitor_check = Instant::now();
			if runtime.monitors_changed() {
				let all_paused_before = runtime.hosted_all_paused();
				runtime.apply(&config);
				if runtime.has_registry_snapshot() {
					let _ = runtime.sync_pause_state_now(all_paused_before);
				}
				warn!("[{}][MONITORS] Layout change detected — reapplied wallpapers", DEBUG_NAME);

				// Refresh asset watcher baselines after full reapply
				watched_asset_mtime = runtime
					.active_asset_dirs()
					.into_iter()
					.filter_map(|dir| newest_file_modified_recursive(&dir).map(|mtime| (dir, mtime)))
					.collect();
			}
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
						let all_paused_before = runtime.hosted_all_paused();
						config = new_config;
						runtime.apply(&config);
						if runtime.has_registry_snapshot() {
							let _ = runtime.sync_pause_state_now(all_paused_before);
						}
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
						watched_asset_mtime = runtime
							.active_asset_dirs()
							.into_iter()
							.filter_map(|dir| newest_file_modified_recursive(&dir).map(|mtime| (dir, mtime)))
							.collect();
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

			let active_dirs = runtime.active_asset_dirs();
			let active_set: std::collections::HashSet<_> = active_dirs.iter().cloned().collect();
			watched_asset_mtime.retain(|dir, _| active_set.contains(dir));
			pending_asset_reload_since.retain(|dir, _| active_set.contains(dir));

			for dir in active_dirs {
				let Some(current_modified) = newest_file_modified_recursive(&dir) else {
					continue;
				};

				let changed = match watched_asset_mtime.get(&dir) {
					Some(prev) => current_modified > *prev,
					None => false,
				};

				if changed {
					pending_asset_reload_since.insert(dir.clone(), Instant::now());
				}

				if let Some(since) = pending_asset_reload_since.get(&dir).copied() {
					if since.elapsed() >= watcher_debounce {
						let reloaded = runtime.reload_wallpapers_for_asset_dir(&dir);
						if reloaded > 0 && config.settings.diagnostics.log_watcher_reloads {
							warn!(
								"[{}][WATCHER] Debounced reload: {} hosted wallpaper instance(s) for asset dir {}",
								DEBUG_NAME,
								reloaded,
								dir.display()
							);
						}
						pending_asset_reload_since.remove(&dir);
					}
				}

				watched_asset_mtime.insert(dir, current_modified);
			}
		}

		thread::sleep(loop_sleep);
	}
}

