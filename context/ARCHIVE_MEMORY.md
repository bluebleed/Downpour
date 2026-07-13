# Downpour — Active Sprint Memory
> **Case C memory** (active sprint scratchpad). Managed by `memory_manager.py`.
> Auto-archived to `context/ARCHIVE_MEMORY.md` when over 2,500 chars.
---
## Current Status (as of 2026-06-07)
**Sprint 1 COMPLETE.** Kiro built the full feature set from `.kiro/specs/idm-download-manager/tasks.md`.
All 17 task groups are marked `[x]`. The project is a functioning download manager.
## Key Context
- This is a **Tauri 2** desktop app — the Rust core and web UI are compiled together.
- Running `npm run tauri dev` (or `run.bat`) compiles Rust on first launch (slow), then hot-reloads.
- The `run.bat` script handles Windows setup (npm install, dependency checks).
- The Kiro spec lives in `.kiro/specs/idm-download-manager/` — requirements.md + tasks.md + design.md.
- The frontend is vanilla JS (no framework) — Tauri IPC via `@tauri-apps/api`.
## Known TODOs / Next Work
1. ~~**Wire `AppSettings.download_dir` to `downloader.rs`**~~ ✅ DONE (June 2026). Configured dir flows settings → QueueConfig → QueueManager live `RwLock<PathBuf>` → `dest_dir` through the engine. `downloads_dir()` is now fallback-only. See ARCHITECTURE.md.
2. ~~**"Resume All on Startup" UX**~~ ✅ DONE (June 2026). New `AppSettings.resume_on_startup` (default off); `lib.rs` calls `resume_all()` after restore when enabled. UI toggle added to Settings view.
3. **System tray**: `tauri-plugin-notification` is registered but tray minimize not implemented. ⚠️ Needs a full app build to verify — best done on the user's machine (no display/webview in agent env).
4. **Test the UI end-to-end**: Run the app and verify all 4 views work (Downloads, Queue, Media, Settings).
5. **Extension needs loading**: Load `extension/` as unpacked extension in Chrome to test capture flow.
6. **Expand `extension/content.js`** media detection for more platforms.
## Critical gotchas
- Don't run `npm run tauri build` without a display/WebView — only run in dev mode here.
- `DownloadItem` serde uses `camelCase` — frontend JS expects camelCase field names.
- The `queue.rs` scheduler is **suspended after restore** — `suspended` AtomicBool = true.
