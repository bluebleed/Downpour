---
inclusion: always
---

# Downpour — project steering

Downpour is a universal (macOS · Windows · Linux) IDM-style download manager for personal
use, built with **Tauri 2** (Rust core + web UI).

The full agent guide lives in `AGENTS.md` at the project root — read it for architecture,
commands, conventions, and the roadmap. Key points for Kiro:

- **Engine**: `src-tauri/src/downloader.rs` — parallel segmented downloads (HTTP `Range`),
  emits the `download-progress` event with a `DownloadItem` payload.
- **UI**: `src/main.js` + `src/styles.css` (Vite, vanilla JS, `@tauri-apps/api`).
- **Capture**: `src-tauri/src/capture_server.rs` ⇄ `extension/background.js` share the
  `http://127.0.0.1:53472` contract.
- **Conventions**: `rustfmt` (edition 2021) + `clippy`; `anyhow::Result` in engine code,
  `String` errors at the Tauri command boundary.
- **Responsible use**: only download content the user is allowed to access — no DRM/paywall
  bypass, and respect site Terms of Service (e.g., most YouTube content is off-limits).
- Don't run a full `tauri build` without a display/webview libraries; it builds on the user's machine.
