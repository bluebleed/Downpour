---
type: reference
title: GEMINI.md
created: 2026-06-07
last-updated: 2026-07-02
load-behavior: on-demand
---

# GEMINI.md

Guidance for Antigravity (Google DeepMind) working in this repository.

**Read [AGENTS.md](./AGENTS.md) for the full project overview, architecture, commands,
conventions, and roadmap.** It is the single source of truth; this file adds
Antigravity-specific notes and session-start instructions.

## Quick facts

- **Downpour** — a universal (macOS / Windows / Linux) IDM-style download manager.
- **Stack**: Tauri 2 (Rust core in `src-tauri/`, web UI in `src/`, browser extension in `extension/`).
- **Engine**: `src-tauri/src/downloader.rs` — parallel segmented downloads via HTTP `Range`.
- **Event contract**: `download-progress` (full `DownloadItem` payload) + `queue-changed` — keep these stable.

## Session Start Checklist

At the start of every session with this project:

1. Read `AGENTS.md` in this folder (project rules).
2. Read `../../_workspace-config/AGENTS.md` (global workspace rules).
3. Read `context/WORKING_MEMORY.md` (active sprint notes, if it exists).
4. Read `ARCHITECTURE.md` (permanent architecture decisions, if it exists).

## Working agreements (Antigravity-specific)

- **Memory routing**: Follow the Tri-Level system from `_workspace-config/AGENTS.md`.
  - Case A (global) → `_workspace-config/antigravity-knowledge/cheatsheet.md`
  - Case B (project) → `ARCHITECTURE.md` in this folder
  - Case C (sprint) → `python "D:\Workspace\_workspace-config\scripts\memory_manager.py" add . "<fact>"`
- **Rust changes**: run `cargo fmt` and `cargo clippy` (manifest: `src-tauri/Cargo.toml`) after any Rust edits.
- **No full tauri build**: Don't run `npm run tauri build` — no display/webview in this environment.
- **DownloadItem stability**: Never remove or rename fields from `DownloadItem` — the event contract is stable.
- **Responsible use**: No DRM bypass, no mass-download violating site ToS. See AGENTS.md.

## Module Map (fast reference)

| File | Purpose |
|---|---|
| `src-tauri/src/lib.rs` | Tauri commands + app setup + state wiring |
| `src-tauri/src/models.rs` | All shared types: `DownloadItem`, `DownloadStatus`, `SegmentState`, etc. |
| `src-tauri/src/downloader.rs` | Core HTTP download engine (segmented + single-stream, pause/resume) |
| `src-tauri/src/queue.rs` | `QueueManager` — concurrency, FIFO scheduling, pause-all/resume-all |
| `src-tauri/src/speed_limiter.rs` | Token-bucket rate limiter (global across all segments) |
| `src-tauri/src/persistence.rs` | JSON persistence (debounced writes, corruption recovery) |
| `src-tauri/src/settings.rs` | `AppSettings` — all config fields + validation |
| `src-tauri/src/categorizer.rs` | Auto-categorizer: moves files into category subfolders |
| `src-tauri/src/media_extractor.rs` | yt-dlp/ffmpeg wrapper for video/audio downloads |
| `src-tauri/src/capture_server.rs` | localhost HTTP server (127.0.0.1:53472) for browser extension |
| `src/main.js` | Frontend dashboard (vanilla JS, Vite, @tauri-apps/api) |
| `src/styles.css` | Glassmorphism UI styles |
| `extension/background.js` | MV3 service worker: intercepts downloads, sends to capture server |
| `extension/content.js` | Content script: detects media links on pages |
| `extension/popup.js` / `popup.html` | Extension popup UI + filter config |

## Tauri Commands (complete list)

```
start_download         pause_download     resume_download   cancel_download
remove_download        reorder_download   pause_all         resume_all
get_queue_state        get_settings       update_settings   set_speed_limit
set_max_concurrent     extract_media_info start_media_download  cancel_media_download
list_downloads
```

## Key design invariants

- `DownloadItem.downloaded` ≤ `DownloadItem.total_size` (when total is known) — enforced by `clamp_reported_downloaded()`.
- Progress events throttled to ≤ 3/sec per download.
- Speed calculated over a 2-second sliding window.
- Concurrency permit acquired before starting, released on complete/pause/error/cancel.
- Downloads interrupted while active are restored as `Paused` on next launch.
- `queue.rs` `suspended` flag prevents scheduler from auto-starting restored downloads.
