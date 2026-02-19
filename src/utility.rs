use std::{
    env,
    ffi::OsStr,
    os::windows::ffi::OsStrExt,
    path::{Path, PathBuf},
};

pub fn to_wstring(s: &str) -> Vec<u16> {
    OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

pub fn user_home_dir() -> Option<PathBuf> {
    env::var("USERPROFILE").map(PathBuf::from).ok()
}

pub fn addon_root_dir() -> Option<PathBuf> {
    let exe_path = env::current_exe().ok()?;
    let exe_dir = exe_path.parent()?;

    if exe_dir.file_name().and_then(|n| n.to_str()) == Some("bin") {
        return exe_dir.parent().map(Path::to_path_buf);
    }

    Some(exe_dir.to_path_buf())
}

pub fn sentinel_root_dir() -> Option<PathBuf> {
    let mut cursor = addon_root_dir()?;
    loop {
        if cursor.file_name().and_then(|n| n.to_str()) == Some(".Sentinel") {
            return Some(cursor);
        }

        if let Some(parent) = cursor.parent() {
            cursor = parent.to_path_buf();
        } else {
            break;
        }
    }

    user_home_dir().map(|p| p.join(".Sentinel"))
}

pub fn sentinel_assets_dir() -> Option<PathBuf> {
    sentinel_root_dir().map(|p| p.join("Assets"))
}

pub fn sentinel_addons_dir() -> Option<PathBuf> {
    sentinel_root_dir().map(|p| p.join("Addons"))
}
