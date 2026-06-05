# Downpour

A fast, universal (macOS · Windows · Linux) download manager — an IDM-style app for personal use.

Built with **Tauri 2** (Rust core + web UI), so it ships as a tiny native binary and runs
natively on Apple Silicon + Intel Macs, Windows, and Linux from a single codebase.

> Responsible use: Downpour is a general-purpose downloader. Only download content you are
> allowed to access. Do not use it to bypass DRM/paywalls or to download content in violation
> of a site's Terms of Service (this includes most YouTube content unless it's your own,
> Creative Commons, or otherwise permitted).

---

## Features (planned)

- [x] Project scaffold (Tauri 2, cross-platform targets)
- [ ] Segmented / parallel downloading via HTTP `Range` requests (faster downloads)
- [ ] Pause / resume (survives app restarts)
- [ ] Download queue (run N at a time)
- [ ] Speed limiting
- [ ] Auto-capture from the browser (companion extension)
- [ ] Video extraction via `yt-dlp` + `ffmpeg` (for permitted content)
- [ ] System tray + native notifications
- [ ] Auto-categorize by file type

## Architecture

```
┌──────────────────┐  captures URL + cookies + headers
│ Browser Extension│ ──────────────────────────────────┐
│ (extension/)     │                                    │
└──────────────────┘                                    ▼
                                       ┌───────────────────────────────┐
┌──────────────────┐  Tauri commands  │  Downpour core (src-tauri/)   │
│ Web Dashboard    │ ◄──────────────► │  - segmented download engine  │
│ (src/)           │  + live events   │  - queue manager              │
└──────────────────┘                  │  - local capture HTTP server  │
                                       └───────────────────────────────┘
                                                       │
                                                       ▼
                                              files saved to disk
```

- **`src-tauri/`** — Rust: the download engine, queue, and a tiny localhost HTTP server the
  browser extension posts captured URLs to.
- **`src/`** — the dashboard UI (Vite + vanilla JS, swap for React/Svelte if you like).
- **`extension/`** — Manifest V3 browser extension that intercepts downloads and forwards them.

## Prerequisites (on your Mac)

1. **Rust** — https://rustup.rs
2. **Node.js** ≥ 18 — https://nodejs.org
3. **Tauri CLI** — `cargo install tauri-cli --version "^2.0.0"` (or use `npm`/`pnpm` scripts)
4. (Optional, for video) **yt-dlp** and **ffmpeg** on your `PATH`.

## Develop

```bash
npm install
npm run tauri dev      # launches the desktop app with hot-reload
```

## Build (universal)

```bash
# macOS universal (Apple Silicon + Intel)
rustup target add aarch64-apple-darwin x86_64-apple-darwin
npm run tauri build -- --target universal-apple-darwin

# Windows
npm run tauri build -- --target x86_64-pc-windows-msvc

# Linux
npm run tauri build
```

## Browser extension

Load `extension/` as an unpacked extension:
- Chrome/Edge: `chrome://extensions` → enable Developer mode → "Load unpacked" → select `extension/`.
- Firefox: `about:debugging` → "This Firefox" → "Load Temporary Add-on" → select `extension/manifest.json`.

The extension posts captured downloads to `http://127.0.0.1:53472/capture` (the Downpour app
must be running).

## Status

This is a **starter scaffold** — the core download engine has a working baseline; queue,
speed-limiting, resume-across-restart, and yt-dlp integration are marked with `TODO`s.
