---
type: reference
title: Running Downpour
created: 2026-06-07
last-updated: 2026-07-02
load-behavior: on-demand
---

# Running Downpour

Downpour is a desktop download manager built with Tauri (a Rust core + a web UI).
This guide covers launching it with the `run.bat` helper, what to expect, and how
to use the main features.

## 1. Prerequisites (one-time)

Install these and make sure they're on your `PATH`:

- **Node.js** (LTS) + npm — https://nodejs.org/
- **Rust toolchain** (rustup) — https://rustup.rs/
- **WebView2 runtime** — preinstalled on Windows 10/11. If the window fails to
  open, install it from Microsoft's "Evergreen" WebView2 page.

Optional, only needed for the **Media** (video/audio) feature:

- **yt-dlp** and **ffmpeg** on your `PATH`, or set their paths in Settings.

`run.bat` checks for Node, npm, and cargo and tells you if any are missing.

## 2. Launch with run.bat

From the project folder (`d:\workspace\desktop-apps\Downpour`), double-click
**`run.bat`** or run it from a terminal:

```bat
run.bat
```

What happens:

1. On the **first run only**, it runs `npm install` to fetch frontend packages.
2. It starts the app in dev mode (`npm run tauri dev`).
3. The **first launch compiles the Rust core**, which can take a few minutes.
   Later launches are much faster (incremental build).
4. The Downpour window opens once the build finishes.

Keep the console window open while using the app. Press **Ctrl+C** in that window
(or close the app window) to quit.

### Other commands

| Command          | What it does                                                        |
|------------------|---------------------------------------------------------------------|
| `run.bat`        | Launch the app in dev mode (hot reload for UI changes).             |
| `run.bat build`  | Produce a production binary/installer in `src-tauri\target\release`.|
| `run.bat help`   | Print the available commands.                                       |

## 3. Using the app

The window has a sidebar with four views.

### Downloads
- Click the **+** floating button (bottom-right) to open **Add Download**.
- Paste a URL, optionally override the filename, pick a **segment count**
  (more segments can download faster), and click **Download**.
- Each download shows a progress bar, live speed, ETA, and a status badge.
- Per-card buttons: **pause / resume / cancel** (cancel discards the partial file).
- Use the **search box**, **status filter pills** (All / Downloading / Queued /
  Paused / Complete / Error), and the **compact/comfortable** toggle to manage a
  long list.

### Queue
- Shows downloads in scheduling order. **Drag to reorder** queued items.
- Per-row **start / pause / remove** actions.
- Only `max_concurrent` downloads run at once; the rest wait their turn. Media
  downloads share the same limit.

### Media (optional, needs yt-dlp + ffmpeg)
- Paste a media page URL and click **Extract** to list available formats.
- Pick a format, optionally set a filename, and click **Download**.
- Only download media you are permitted to access (see the responsible-use note
  in `AGENTS.md`).

### Settings
- **Max concurrent downloads** (1–10), **default segments** (1–32),
  **speed limit** (KB/s, 0 = unlimited), and **download directory**.
- **Categories**: completed files are auto-sorted into category subfolders
  (Videos, Music, Images, …) when auto-categorize is on.
- **yt-dlp / ffmpeg paths**: leave blank to use whatever is on your `PATH`.
- Changes apply immediately — no restart needed — and persist across restarts.

### Status bar (bottom)
- Global download speed, active/queued counts, and **Pause All / Resume All**.

## 4. Browser capture extension (optional)

The `extension/` folder is a Manifest V3 browser extension that hands your
browser's downloads to Downpour automatically.

1. Open your browser's extensions page and enable **Developer mode**.
2. Choose **Load unpacked** and select the `extension/` folder.
3. With Downpour running, the extension's popup shows "Connected to Downpour".
4. Configure size/extension filters in the popup as desired.

The extension talks to Downpour over `http://127.0.0.1:53472` (local only).

## 5. Where files go

- Downloads are saved to the **download directory** set in Settings (defaults to
  your OS Downloads folder), then moved into a category subfolder if
  auto-categorize is enabled.
- App state (queue, settings, segment offsets) is persisted under your OS app-data
  directory, so paused downloads and settings survive a restart. Downloads that
  were active when the app closed come back **paused** — resume them manually.

## 6. Troubleshooting

- **"Node.js / npm / cargo was not found"** — install the missing tool (section 1)
  and re-run `run.bat`.
- **First launch is slow** — that's the one-time Rust compile. Subsequent launches
  are fast.
- **Window doesn't open / blank window** — install the WebView2 runtime.
- **Port 53472 in use** — another app (or a second Downpour instance) holds the
  capture port; Downpour retries a few times then disables browser capture and
  notifies you. Close the conflicting app and restart.
- **Media extraction fails** — make sure `yt-dlp` and `ffmpeg` are installed and
  either on `PATH` or set in Settings.
- **To stop the app** — press Ctrl+C in the `run.bat` console or close the window.
