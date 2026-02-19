use std::{
    fs::OpenOptions,
    io::Write,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Sender},
        OnceLock,
    },
    thread,
};

use chrono;

use crate::utility::sentinel_root_dir;

/* =========================
   GLOBAL STATE
   ========================= */

static ENABLED: AtomicBool = AtomicBool::new(false);
static LOG_PATH: OnceLock<PathBuf> = OnceLock::new();
static LOG_TX: OnceLock<Sender<String>> = OnceLock::new();
static LOG_LEVEL: OnceLock<String> = OnceLock::new();

/* =========================
   PUBLIC API
   ========================= */

pub fn init(debug: bool, level: &str) {
    if LOG_TX.get().is_some() {
        panic!("logging::init() called more than once");
    }

    ENABLED.store(debug, Ordering::Relaxed);    let _ = LOG_LEVEL.set(level.to_lowercase());
    let path = log_path().clone();
    let (tx, rx) = mpsc::channel::<String>();
    LOG_TX.set(tx).expect("LOG_TX already set");

    thread::spawn(move || {
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .expect("Failed to open log file");

        while let Ok(line) = rx.recv() {
            let _ = writeln!(file, "{line}");
            let _ = file.flush();
        }
    });
}

#[inline]
pub fn should_log(level: &str) -> bool {
    if !ENABLED.load(Ordering::Relaxed) {
        return level == "WARN" || level == "ERROR";
    }
    true
}

/* =========================
   INTERNAL
   ========================= */

#[inline]
pub fn enqueue(level: &str, msg: String) {
    if let Some(tx) = LOG_TX.get() {
        let ts = timestamp();
        let _ = tx.send(format!("{ts} [{level}] {msg}"));
    }
}

fn timestamp() -> String {
    let now = chrono::Local::now();
    now.format("%Y-%m-%d %H:%M:%S%.3f").to_string()
}

/* =========================
   MACROS
   ========================= */

#[macro_export]
macro_rules! info {
    ($($arg:tt)*) => {{
        if $crate::logging::should_log("INFO") {
            $crate::logging::enqueue(
                "INFO",
                format!($($arg)*)
            );
        }
    }};
}

#[macro_export]
macro_rules! warn {
    ($($arg:tt)*) => {{
        $crate::logging::enqueue(
            "WARN",
            format!($($arg)*)
        );
    }};
}

#[macro_export]
macro_rules! error {
    ($($arg:tt)*) => {{
        $crate::logging::enqueue(
            "ERROR",
            format!($($arg)*)
        );
    }};
}

/* =========================
   PATH
   ========================= */

fn log_path() -> &'static PathBuf {
    LOG_PATH.get_or_init(|| {
        sentinel_root_dir()
            .map(|p| p.join("sentinel.wallpaper.log"))
            .unwrap_or_else(|| PathBuf::from("sentinel.wallpaper.log"))
    })
}
