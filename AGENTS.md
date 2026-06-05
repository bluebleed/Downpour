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
Browser Extension (extension/)  --captures URL+cookies-->  Capture server (src-tauri)
        |                                                          |
        v                                                          v
   Web Dashboard (src/)  <--Tauri commands + events-->  Download engine (src-tauri)
                                                                   |
                                                                   v
                                                          files saved to disk
```

- `src-tauri/src/downloader.rs` — the engine. Probes a URL, then downloads in parallel
  segments via HTTP `Range` requests (4 segments) when supported; single-stream fallback
  otherwise. Emits `download-progress` events to the UI.
- `src-tauri/src/capture_server.rs` — localhost HTTP server (`127.0.0.1:53472`) the browser
  extension POSTs captured downloads to.
- `src-tauri/src/queue.rs` — concurrency limiter (skeleton, not yet wired).
- `src-tauri/src/lib.rs` — Tauri commands (`start_download`, `list_downloads`) + setup.
- `src/` — Vite + vanilla-JS dashboard (`main.js`, `styles.css`).
- `extension/` — Manifest V3 browser extension (auto-capture).

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

## Roadmap (open TODOs)

- [ ] Pause / resume that survives app restarts (persist progress + part offsets).
- [ ] Wire `queue.rs` so `start_download` acquires a permit (max N concurrent).
- [ ] Speed limiting (bandwidth throttle).
- [ ] `yt-dlp` + `ffmpeg` integration for permitted video downloads.
- [ ] System tray + native notifications.
- [ ] Auto-categorize downloads by file type.

## Where to start

For engine changes, read `src-tauri/src/downloader.rs` first. For UI changes, start in
`src/main.js`. For capture/extension changes, read `src-tauri/src/capture_server.rs` and
`extension/background.js` together (they share the `:53472` contract).

**CRITICAL:** Before executing any task, read the global agent rules.
> Read: `../../_workspace-config/AGENTS.md`
> **At session start:** run the context primer � `../../_workspace-config/context-primer.md`
