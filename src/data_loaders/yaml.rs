// ~/src/data_loaders/yaml.rs

use std::{
    fs,
    sync::{RwLock, LazyLock},
    time::{Duration, Instant},
    path::Path,
    collections::HashMap,
};

use serde_yaml::Value;

/* =========================
   CONFIG CACHE
========================= */


// Per-file cache for YAML data
static YAML_CACHE: LazyLock<RwLock<HashMap<String, (Value, Instant)>>> = LazyLock::new(|| RwLock::new(HashMap::new()));
const CACHE_TTL: Duration = Duration::from_secs(1);

/// Universal YAML loader with per-file cache (max 100 entries)
pub fn load_yaml(path: &Path) -> Option<Value> {
    let now = Instant::now();
    let key = path.to_string_lossy().to_string();
    {
        let cache = YAML_CACHE.read().unwrap();
        if let Some((v, t)) = cache.get(&key) {
            if now.duration_since(*t) < CACHE_TTL {
                return Some(v.clone());
            }
        }
    }

    let txt = fs::read_to_string(path).ok()?;
    let v: Value = serde_yaml::from_str(&txt).ok()?;
    let mut cache = YAML_CACHE.write().unwrap();
    
    // Evict oldest entries if cache exceeds 100 items
    if cache.len() >= 100 {
        if let Some(oldest_key) = cache
            .iter()
            .min_by_key(|(_, (_, t))| t)
            .map(|(k, _)| k.clone())
        {
            cache.remove(&oldest_key);
        }
    }
    
    cache.insert(key, (v.clone(), now));
    Some(v)
}