# Working Memory

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
- `speed_limiter.rs` is shared across ALL segments; setting rate to 0 = unlimited.
- Port 53472 is hardcoded for the capture server; browser extension POSTs to `http://127.0.0.1:53472/capture`.
- [2026-06-07] 2026-06-07 sprint: Completed 2 roadmap TODOs. (1) AppSettings.download_dir now wired end-to-end: settings -> QueueConfig.download_dir -> QueueManager live RwLock<PathBuf> (set_download_dir on settings change) -> threaded as dest_dir into downloader run/resume_download/download_core/resume_core; cancel/media cleanup + completion-categorizer source path use it too. downloads_dir() is now fallback-only. In-flight downloads keep original dest if dir changes mid-flight. (2) AppSettings.resume_on_startup added (default false, serde-default for forward-compat); lib.rs setup auto-calls queue.resume_all() after restore_from_disk when enabled; UI checkbox in Settings view (index.html set-resume-on-startup + main.js fill/collect). Both are Case B changes already in ARCHITECTURE.md. Verified: cargo fmt clean, clippy zero warnings, 194 tests pass (184 unit incl 3 new resume_on_startup tests + 10 integration). Not committed yet. Next TODO: system tray + minimize-to-tray (needs full app build/display to verify, best done on user machine).
