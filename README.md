# Sentinel Wallpaper Addon

This addon launches wallpaper targets by ID and embeds their windows into desktop layers on Windows.

## Required files

- `addon.json`
- `config.yaml`

## Config model (current)

Top-level flags:

- `update_check`
- `debug`
- `log_level`

Wallpaper sections are discovered by key prefix `wallpaper` (`wallpaper`, `wallpaper2`, `wallpaper3`, ...).

Each section supports:

- `enabled` (bool)
- `monitor_index` (`["*"]`, `["p"]`, `["1"]`, `["2"]`, ...)
- `wallpaper_id` (asset ID, example: `sentinel.default.dark`)
- `mode` (reserved for layout mode, default: `fill`)
- `z_index` (default: `desktop`)

## z_index values

- `desktop` (default): embeds behind desktop icons using WorkerW parent.
- `bottom`: bottom of parent z-order.
- `normal`: regular non-topmost.
- `top`: top of non-topmost stack.
- `topmost`: topmost layer.
- `overlay`: topmost tool-window style (useful for widget-like layers).

## Asset metadata contract (MVP)

The addon resolves wallpaper IDs through IPC (`registryd -> list_assets`) and expects assets in category `wallpaper`.

Supported metadata keys:

- `url`: launches a web wallpaper with `msedge.exe --new-window --app=<url>`
- `command`: executable/command path
- `args`: argument array for `command` or `exe_path`
- `window_title`: optional title hint used to locate the created window handle

If `command`/`url` is not provided, addon falls back to registry `exe_path` when available.

