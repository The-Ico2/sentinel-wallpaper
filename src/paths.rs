// ~/sentinel/sentinel-backend/src/paths.rs

use std::path::PathBuf;
use crate::{info, warn};

pub fn user_home_dir() -> Option<PathBuf> {
    // Primary (most reliable on Windows)
    if let Ok(profile) = std::env::var("USERPROFILE") {
        info!("USERPROFILE environment variable found: {}", profile);
        return Some(PathBuf::from(profile));
    }

    // Fallback (older / edge cases)
    let drive = std::env::var("HOMEDRIVE").ok();
    let path = std::env::var("HOMEPATH").ok();

    match (drive, path) {
        (Some(d), Some(p)) => {
            let full = PathBuf::from(format!("{}{}", d, p));
            info!("Resolved home directory from HOMEDRIVE/HOMEPATH: {}", full.display());
            Some(full)
        }
        _ => {
            warn!("Could not resolve home directory using USERPROFILE or HOMEDRIVE/HOMEPATH");
            None
        }
    }
}

/// The canonical Sentinel root is always `~/.Sentinel/`.
/// All config, addons, and assets live here.
pub fn sentinel_root_dir() -> PathBuf {
    if let Some(home) = user_home_dir() {
        home.join(".Sentinel")
    } else {
        warn!("Could not resolve home directory, falling back to exe parent");
        match std::env::current_exe() {
            Ok(path) => path.parent().map(|p| p.to_path_buf())
                .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))),
            Err(e) => {
                warn!("Failed to get current executable path: {e}");
                std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
            }
        }
    }
}