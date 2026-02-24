# Sentinel Wallpaper Addon

A WebView2-based wallpaper engine for Windows that embeds interactive HTML/CSS/JS wallpapers directly into the desktop layer. Part of the [Sentinel](https://github.com/The-Ico2/Sentinel) desktop customization platform.

> **Note:** Installing third-party wallpapers is not currently supported. The addon ships with a bundled default wallpaper (`sentinel.default`), but there is no wallpaper marketplace or install mechanism yet — I'm still figuring out the best approach for this. For now, wallpapers must be manually placed in `~/.Sentinel/Assets/wallpaper/`.

---

## What It Does

The wallpaper addon runs as a background process (`sentinel-wallpaper.exe`) that:

- Embeds WebView2 instances into the Windows desktop host (WorkerW), rendering wallpapers behind desktop icons
- Supports multiple wallpaper profiles across multiple monitors
- Streams live system data, audio levels, mouse/keyboard input, and CSS variable updates to wallpapers via `postMessage`
- Intelligently pauses wallpapers when windows are focused, maximized, or fullscreen — capturing a static snapshot as the Windows wallpaper to save resources
- Hot-reloads configuration changes without restarting
- Self-installs on first run, scaffolding config files and the bundled default wallpaper asset

---

## Installation

Run `sentinel-wallpaper.exe` from anywhere. On first launch it will:

1. Start the Sentinel backend (`sentinelc.exe`) if not already running
2. Create `~/.Sentinel/Addons/wallpaper/` with default config files (`addon.json`, `config.yaml`, `schema.yaml`)
3. Install `sentinel.default` wallpaper to `~/.Sentinel/Assets/wallpaper/sentinel.default/`
4. Copy itself to `~/.Sentinel/Addons/wallpaper/bin/` and relaunch

Subsequent launches from the installed location skip the install step.

---

## File Layout

```ps1
~/.Sentinel/
├── Addons/
│   └── wallpaper/
│       ├── addon.json          # Addon manifest
│       ├── config.yaml         # Runtime configuration
│       ├── schema.yaml         # Config UI schema
│       ├── bin/
│       │   └── sentinel-wallpaper.exe
│       └── options/
│           ├── settings.html
│           ├── discover.html
│           ├── editor.html
│           └── library.html
└── Assets/
    └── wallpaper/
        └── sentinel.default/
            ├── manifest.json
            ├── index.html
            └── preview/
                └── 1.png
```

---

## Configuration

The addon reads `config.yaml` from its install directory. A default config is scaffolded on first run.

### Wallpaper Profiles

Wallpaper sections are discovered by YAML key prefix `wallpaper` (`wallpaper`, `wallpaper2`, `wallpaper3`, ...):

```yaml
wallpaper:
  enabled: true
  monitor_index:
    - "*"
  wallpaper_id: "sentinel.default"
  mode: "fill"
  z_index: "desktop"
```

Each profile supports:

| Field | Type | Description |
| ------- | ------ | ------------- |
| `enabled` | bool | Enable/disable this profile |
| `monitor_index` | string list | `["*"]` (all), `["p"]` (primary), `["0"]`, `["1"]`, ... |
| `wallpaper_id` | string | Asset ID to display (e.g. `sentinel.default`) |
| `mode` | string | Layout mode: `fill`, `fit`, `stretch`, `center`, `tile`, `span` |
| `z_index` | string | Window layer (see below) |

### z_index Layers

| Value | Behavior |
| ------- | ---------- |
| `desktop` | Embedded behind desktop icons (WorkerW parent). **Default.** |
| `bottom` | Bottom of parent z-order |
| `normal` | Regular non-topmost |
| `top` | Top of non-topmost stack |
| `topmost` | Topmost layer |
| `overlay` | Topmost tool-window style (widget-like) |

### Performance Settings

```yaml
settings:
  performance:
    pausing:
      focus: "per-monitor"        # off | per-monitor | all-monitors
      maximized: "per-monitor"
      fullscreen: "all-monitors"
      check_interval_ms: 500
    watcher:
      enabled: true
      interval_ms: 600
    interactions:
      send_move: true
      send_click: true
      poll_interval_ms: 8
      move_threshold_px: 0.5
    audio:
      enabled: true
      sample_interval_ms: 100
      endpoint_refresh_ms: 1200
      retry_interval_ms: 2000
      change_threshold: 0.015
      quantize_decimals: 2
  runtime:
    tick_sleep_ms: 8
    reapply_on_pause_change: true
  diagnostics:
    log_pause_state_changes: true
    log_watcher_reloads: true
  development:
    update_check: true
    debug: false
    log_level: warn
```

---

## Pause Behavior

When a window is focused, maximized, or fullscreen (depending on config), the addon:

1. Captures the current wallpaper frame from each hosted WebView2 window
2. Stitches per-monitor captures into a single bitmap
3. Sets it as the Windows desktop wallpaper via `SystemParametersInfoW`
4. Hides the WebView2 controllers to save GPU/CPU resources
5. On unpause, restores the live wallpapers and reapplies the runtime

Pause modes can be set per-profile or globally in `settings.performance.pausing`:

- **`off`** — Never pause for this condition
- **`per-monitor`** — Pause only the wallpaper on the monitor where the condition is true
- **`all-monitors`** — Pause all wallpapers when the condition is true on any monitor

---

## WebView2 Message Protocol

The addon communicates with wallpapers via `window.chrome.webview.addEventListener("message", ...)`. All messages are JSON objects with a `type` field.

### Messages Sent to Wallpapers

| Type | Fields | Description |
| ------ | -------- | ------------- |
| `native_move` | `x`, `y`, `nx`, `ny` | Cursor position (local px + normalized 0–1) |
| `native_click` | `x`, `y`, `nx`, `ny` | Left mouse button press |
| `native_key` | `key`, `vk`, `state` | Keyboard key down/up (A–Z, 0–9, F1–F12, modifiers, etc.) |
| `native_audio` | `level` | System audio peak level (0.0–1.0) |
| `native_registry` | `sysdata`, `appdata` | Full system data + per-monitor app data snapshot |
| `native_pause` | `paused` | Pause state change notification |
| `native_css_vars` | `vars` | CSS variable updates from manifest `editable` section |

---

## Asset Resolution

Wallpaper assets are resolved in this order:

1. **IPC registry** — Query `registry.list_assets` from the backend, filter by `category == "wallpaper"`
2. **Local fallback** — Scan `~/.Sentinel/Assets/wallpaper/*/manifest.json` directly

Each asset must provide one of:

- An `index.html` file in the asset directory (loaded as `file:///` URL)
- A `url` field in `manifest.json` metadata

### Editable Properties

Assets can declare editable CSS variables in `manifest.json`:

```json
{
  "editable": {
    "hudOpacity": {
      "variable": "--hud-opacity",
      "value": 1,
      "selector": "slider",
      "min": 0,
      "max": 1,
      "step": 0.05
    }
  }
}
```

Changes to these values are pushed live to the wallpaper as `native_css_vars` messages. The addon polls the manifest file every 250ms for changes.

---

## Bundled Asset: sentinel.default

An animated dark theme wallpaper included with the addon. Features:

- Ember particle canvas with accent-colored glow effects
- HUD panels displaying live system data (CPU, GPU, RAM, storage, network, audio, time, processes, etc.)
- Customizable accent/ember colors, background, text color, font, and HUD opacity via manifest editable fields
- Audio-reactive visual intensity

---

## IPC

The addon communicates with the Sentinel backend over the named pipe `\\.\pipe\sentinel`:

| Namespace | Command | Purpose |
| ----------- | --------- | --------- |
| `registry` | `list_assets` | Discover wallpaper assets |
| `registry` | `list_sysdata` | Fetch system data for wallpapers |
| `registry` | `list_appdata` | Fetch per-monitor app data for pause evaluation |

---

## Requirements

- Windows 10/11
- [WebView2 Runtime](https://developer.microsoft.com/en-us/microsoft-edge/webview2/) (typically pre-installed)
- Sentinel Backend (`sentinelc.exe`) — auto-started if not running

---

## Tech Stack

- **Language:** Rust
- **Rendering:** WebView2 via `webview2-com`
- **Desktop integration:** Win32 API (`windows` crate) — WorkerW embedding, DPI awareness, window management
- **Audio:** WASAPI peak metering via `IMMDeviceEnumerator` / `IAudioMeterInformation`
- **Image:** `image` crate for pause snapshot capture & stitching
- **IPC:** Named pipes to Sentinel backend (JSON request/response)

---

## Project Status

Alpha release (`v0.1.0-alpha`). APIs, config format, and behavior may change. Windows only.
