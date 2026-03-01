# Sentinel Wallpaper Addon

A GPU-native wallpaper engine for Windows that renders interactive wallpapers directly into the desktop layer using [CanvasX Runtime](https://github.com/The-Ico2/CanvasX). Part of the [Sentinel](https://github.com/The-Ico2/Sentinel) desktop customization platform.

> **Note:** Installing third-party wallpapers is not currently supported. The addon ships with a bundled default wallpaper (`sentinel.default`), but there is no wallpaper marketplace or install mechanism yet — I'm still figuring out the best approach for this. For now, wallpapers must be manually placed in `~/.Sentinel/Assets/wallpaper/`.

---

## What It Does

The wallpaper addon runs as a background process (`sentinel-wallpaper.exe`) that:

- Compiles wallpaper HTML/CSS into CanvasX Runtime Documents (CXRD) and renders them on the GPU via wgpu (Vulkan / DX12) — no browser engine, no WebView, no JavaScript runtime
- Embeds render surfaces into the Windows desktop host (WorkerW), displaying wallpapers behind desktop icons
- Streams live system data (CPU, GPU, RAM, storage, network, audio, etc.) from the Sentinel backend into data-bound elements via named-pipe IPC
- Supports multiple wallpaper profiles across multiple monitors with virtual viewport scaling (consistent appearance across all resolutions)
- Intelligently pauses wallpapers when windows are focused, maximized, or fullscreen — capturing a static snapshot as the Windows wallpaper to save resources
- Hot-reloads configuration and editable CSS variable changes without restarting
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

## Rendering Pipeline

Each wallpaper goes through the CanvasX pipeline:

```
index.html + style.css → Compiler → CXRD → Layout → Animate → Paint → GPU
                                              ↑                          |
                                     SentinelBridge (IPC)         wgpu (Vulkan/DX12)
```

1. **Compile** — `compile_html()` parses the wallpaper's HTML/CSS into a CanvasX Runtime Document (binary scene graph with resolved styles, assets, and data bindings)
2. **GPU context** — A wgpu surface is created on the Win32 HWND embedded in the desktop WorkerW
3. **Per-frame tick** — `SceneGraph::tick()` runs layout (if dirty) → animate → update data bars → prepare text → paint → return GPU instances
4. **Render** — Instanced SDF quads are submitted to the GPU in a single draw call, plus a text pass via glyphon

### Virtual Viewport Scaling

Layouts are normalized to a 1080p-equivalent virtual viewport (`scale = physical_height / 1080.0`). A wallpaper designed at 1080p will look identical on 1440p, 4K, or any other resolution — font sizes, padding, column widths all scale proportionally.

---

## File Layout

```
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
        ├── sentinel.js         # Shared wallpaper SDK
        └── sentinel.default/
            ├── manifest.json
            ├── index.html
            ├── style.css
            ├── editable.yaml
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
      idle_timeout_ms: 0          # pause all wallpapers after idle timeout (0 disables)
      check_interval_ms: 500
    watcher:
      enabled: true
      interval_ms: 600
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

## Live Data Binding

Wallpapers use CanvasX `<data-bind>` elements to display live system data. Values are streamed from the Sentinel backend via the `SentinelBridge` (named-pipe IPC, 250ms poll interval).

```html
<data-bind binding="cpu.usage" format="{value}%"></data-bind>
<data-bar binding="ram.used_bytes" max-binding="ram.total_bytes"></data-bar>
<data-bar-stack max-binding="storage.total_bytes">
    <data-bar-segment binding="storage.disks.0.used_bytes" style="background: var(--disk0-color)"></data-bar-segment>
    <data-bar-segment binding="storage.disks.1.used_bytes" style="background: var(--disk1-color)"></data-bar-segment>
</data-bar-stack>
```

The bridge tracks 16 data sections: time, cpu, gpu, ram, storage, displays, network, wifi, bluetooth, audio, keyboard, mouse, power, idle, system, and processes. Data is flattened into dot-notation keys (e.g. `cpu.model`, `ram.used_gb`, `storage.disks.0.name`) and pushed into the scene graph each frame.

---

## Pause Behavior

When a window is focused, maximized, fullscreen, or the system exceeds `idle_timeout_ms` (depending on config), the addon:

1. Captures the current wallpaper frame from each hosted window via `PrintWindow`
2. Stitches per-monitor captures into a single bitmap spanning the virtual desktop
3. Sets it as the Windows desktop wallpaper via `SystemParametersInfoW`
4. Hides the render windows and pauses the SentinelBridge to save GPU/CPU resources
5. On unpause, restores the live wallpapers and resumes data polling

A periodic snapshot thread captures frames every 5 seconds for crash recovery. On startup, the last saved snapshot is applied as the Windows wallpaper before render surfaces are created.

Pause modes can be set per-profile or globally in `settings.performance.pausing`:

- **`off`** — Never pause for this condition
- **`per-monitor`** — Pause only the wallpaper on the monitor where the condition is true
- **`all-monitors`** — Pause all wallpapers when the condition is true on any monitor

---

## Asset Resolution

Wallpaper assets are resolved in this order:

1. **IPC registry** — Query `registry.list_assets` from the backend, filter by `category == "wallpaper"`
2. **Local fallback** — Scan `~/.Sentinel/Assets/wallpaper/*/manifest.json` directly

Each asset must provide an `index.html` file in the asset directory.

### Editable Properties

Assets declare editable CSS variables via `manifest.json` schema + `editable.yaml` user overrides:

```yaml
# editable.yaml
hudOpacity: 0.85
accentColor: "#ff6b35"
fontSize: 13
```

The `EditableContext` resolves manifest defaults against YAML overrides, producing a flat CSS variable map injected into the CXRD document. Changes are polled every 250ms and trigger layout invalidation — no restart required.

---

## Bundled Asset: sentinel.default

A dark-themed HUD wallpaper included with the addon, displaying live system data across organized panels. Features:

- **Left column** — CPU, GPU, Memory, Storage (with stacked disk usage bar), Network, Processes
- **Right column** — Audio, Bluetooth, Media, Power, Display, System, Input
- **Top bar** — Date, time, uptime
- Fully customizable via 50+ editable CSS variables (colors, fonts, layout, panel styles, blur, opacity)

---

## IPC

The addon communicates with the Sentinel backend over the named pipe `\\.\pipe\sentinel`:

| Namespace | Command | Purpose |
| ----------- | --------- | --------- |
| `registry` | `list_assets` | Discover wallpaper assets |
| `registry` | `list_sysdata` | Fetch system data for wallpapers |
| `registry` | `list_appdata` | Fetch per-monitor app data for pause evaluation |

The `SentinelBridge` (from `canvasx-runtime`) handles connection management, heartbeats, reconnection, and data flattening automatically.

---

## Requirements

- Windows 10/11
- GPU with Vulkan or DirectX 12 support
- Sentinel Backend (`sentinelc.exe`) — auto-started if not running

---

## Tech Stack

- **Language:** Rust
- **Rendering:** [CanvasX Runtime](https://github.com/The-Ico2/CanvasX) — GPU-native scene graph renderer (wgpu, Vulkan/DX12, instanced SDF quads)
- **Text:** glyphon (cosmic-text) — GPU text rendering with font shaping
- **Desktop integration:** Win32 API (`windows` crate) — WorkerW embedding, per-monitor DPI awareness, window management
- **Image:** `image` crate for pause snapshot capture & stitching
- **IPC:** Named pipes to Sentinel backend (JSON request/response)
- **Config:** `serde_yaml` for YAML configuration and editables

---

## Project Status

Active development (`v0.3.0`). APIs, config format, and behavior may change. Windows only.

---

## Contact

- **Discord:** the_ico2
- **X (Twitter):** The_Ico2
