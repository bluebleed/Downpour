---
type: rule-pointer
title: Agent Rules Pointer
created: 2026-06-07
last-updated: 2026-07-02
load-behavior: eager
---

# AGENTS.md

Canonical guidance for AI coding agents working in this repository. (Tool-specific
files like `CLAUDE.md`, `AGENT.md`, and `GEMINI.md` point here.)

## Project: Downpour

A fast, universal (macOS · Windows · Linux) download manager — an IDM-style desktop app
for personal use. Built with **Tauri 2**: a **Rust** core for the download engine and a
web-based UI, shipped as a small native binary from a single codebase.

## Responsible-use boundary (important)

Downpour is a general-purpose downloader. Only help implement features that download
content the user is allowed to access. Do **not** add features whose purpose is to bypass
DRM/paywalls or to mass-download content in violation of a site's Terms of Service (this
includes most YouTube content unless it is the user's own, Creative Commons, or otherwise
permitted). The download engine itself is neutral; keep usage within those lines.

## Architecture

```
Browser Extension (extension/)  --POST /capture (cookies+headers+referer)-->  Capture server (src-tauri)
        |                                                                               |
        v                                                                               v
   Web Dashboard (src/)  <----Tauri commands + download-progress / queue-changed---->  QueueManager (src-tauri)
                                                                                               |
                                     +------------------+------------------+-----------+-----+
                                     |                  |                  |           |
                                downloader.rs   media_extractor.rs  speed_limiter.rs  persistence.rs
                                     |                  |                              |
                                     +---> categorizer.rs (on completion)              |
                                     +---> settings.rs ----------------------------------+
```

> **Sprint 1 complete (Kiro, June 2026):** All planned modules are implemented and wired.
> Read `ARCHITECTURE.md` for the full module map, event contract, and open TODOs.

- `src-tauri/src/downloader.rs` — segmented parallel downloads (HTTP `Range`), pause/resume with PausedState, 6-attempt exponential retry, 30s timeout, speed/ETA calculation, progress throttling.
- `src-tauri/src/queue.rs` — `QueueManager` with `Arc<Semaphore>` concurrency, FIFO scheduling, pause/resume/cancel/reorder, `pause_all`/`resume_all`, disk-full handling, restore-from-disk.
- `src-tauri/src/speed_limiter.rs` — token-bucket rate limiter shared across all segments.
- `src-tauri/src/persistence.rs` — JSON state persistence with 500ms debounce, corruption recovery.
- `src-tauri/src/settings.rs` — `AppSettings` with all config fields and validation.
- `src-tauri/src/categorizer.rs` — auto-sorts completed downloads into category subfolders.
- `src-tauri/src/media_extractor.rs` — yt-dlp/ffmpeg wrapper for permitted video downloads.
- `src-tauri/src/capture_server.rs` — axum server on `127.0.0.1:53472`, wired to `QueueManager`.
- `src-tauri/src/lib.rs` — 19 Tauri commands + full state wiring + auto-categorizer listener.
- `src/` — Vite + vanilla-JS dashboard: 4 views (Downloads, Queue, Media, Settings), glassmorphism UI.
- `extension/` — Manifest V3 extension: cookie/header capture, size/extension filtering, content script.

## Commands

```bash
npm install                          # install frontend deps
npm run tauri icon path/to/logo.png  # one-time: generate the icon set
npm run tauri dev                    # run the desktop app (hot reload)
npm run tauri build                  # production build (current OS)
npm run tauri build -- --target universal-apple-darwin   # macOS universal binary

cargo fmt   --manifest-path src-tauri/Cargo.toml         # format Rust
cargo clippy --manifest-path src-tauri/Cargo.toml        # lint Rust
```

> Note: a full Tauri build needs system webview libraries and is done on the user's
> machine, not in a headless CI/sandbox without a display.

## Conventions

- **Rust**: format with `rustfmt` (edition 2021); keep the engine `async` (Tokio). Prefer
  returning `anyhow::Result` in engine code; convert to `String` errors at the Tauri command
  boundary. Share state via the `Downloads` type alias (`Arc<Mutex<HashMap<..>>>`).
- **Frontend**: plain ES modules, no framework yet (swap to React/Svelte if it grows).
  Talk to the core only through `@tauri-apps/api` (`invoke` for commands, `listen` for events).
- **Events**: the engine pushes UI updates via the `download-progress` event with a full
  `DownloadItem` payload. Keep that contract stable if you change the struct.
- Do not commit `node_modules/`, `src-tauri/target/`, or the bundled `binaries/` (see `.gitignore`).

## Roadmap

> Kiro completed Sprint 1 (all core features). Remaining work:

- [x] Pause / resume that survives app restarts (persist progress + segment offsets). ✅
- [x] Wire `queue.rs` so `start_download` acquires a permit (max N concurrent). ✅
- [x] Speed limiting (token-bucket bandwidth throttle). ✅
- [x] `yt-dlp` + `ffmpeg` integration for permitted video downloads. ✅
- [x] Auto-categorize downloads by file type. ✅
- [ ] Wire `AppSettings.download_dir` into `downloader.rs` (`downloads_dir()` ignores the setting).
- [ ] System tray + minimize-to-tray (plugin registered, tray UI not built).
- [ ] "Resume All on Startup" UX (scheduler suspends after restore; user must manually resume).
- [ ] Extension: expand `content.js` media detection for more platforms.

## Where to start

For engine changes, read `src-tauri/src/downloader.rs` first. For UI changes, start in
`src/main.js`. For capture/extension changes, read `src-tauri/src/capture_server.rs` and
`extension/background.js` together (they share the `:53472` contract).
For architecture overview and permanent decisions, read `ARCHITECTURE.md`.
For active sprint context, read `context/WORKING_MEMORY.md`.
Agent-specific configs: `GEMINI.md` (Antigravity), `CLAUDE.md` (Claude Code).

**CRITICAL:** Before executing any task, read the global agent rules.
> Read: `../../_workspace-config/AGENTS.md`
> **At session start:** run the context primer → `../../_workspace-config/context-primer.md`
