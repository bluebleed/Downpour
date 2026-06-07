# Right-Click "Download with Downpour" — Plan & Tasks

**Date:** 2026-06-08
**Status:** Planned (not yet implemented)
**Owner:** Downpour

---

## 1. Goal

Add an IDM-style browser **right-click → "Download with Downpour"** action so users
can send videos/links from YouTube and social sites (X/Twitter, Instagram, Facebook,
TikTok, Reddit, Vimeo, etc.) to the Downpour app for download via yt-dlp — and direct
file links via the HTTP engine.

Builds on the existing MV3 browser extension (`extension/`) and the localhost capture
server (`127.0.0.1:53472`).

---

## 2. Confirmed decisions

- **Default quality = highest available.** The quick-download action uses the
  "Best available" selector (`bestvideo+bestaudio/best`, merged to mp4). No prompt.
- **Two menu items:**
  - **"Download with Downpour"** (primary) → opens/focuses the app on the **Media tab,
    pre-filled and auto-extracted**, so the user can pick quality/format and see
    playlists. Reuses all existing Media-tab UI.
  - **"Quick download (Best)"** (secondary) → enqueues immediately at highest quality.
- **Media routing via yt-dlp**; DRM-protected sites (Netflix etc.) remain blocked by
  the responsible-use guard — by design.
- **App must be running** for capture to work (same constraint as today's capture).

---

## 3. Architecture / design

```
Browser (right-click) ──► Extension contextMenus.onClicked
                              │  resolve URL by context:
                              │   - page/video → pageUrl (watch/post URL)
                              │   - link       → linkUrl
                              ▼
                POST 127.0.0.1:53472/capture-media
                   { url, mode: "options" | "quick", quality }
                              │
                              ▼
                     capture_server.rs route
                   ├─ mode=quick   → enqueue Media download (selector = Best)
                   └─ mode=options → emit "open-media" event to UI
                              │
                              ▼
            Frontend: switch to Media tab, fill URL, auto-extract
            (existing extract → format picker → playlist checklist)
```

**Key wrinkle — which URL to send:** right-clicking a `<video>` yields a `blob:` source
that is **not** downloadable. So:
- **video / page context** → send `pageUrl` (what yt-dlp needs).
- **link context** → send `linkUrl`; server sniffs it (real file extension → HTTP
  engine; otherwise → yt-dlp).

---

## 4. Tasks

### Phase 1 — Extension context menu
- [ ] Add `"contextMenus"` to `permissions` in `extension/manifest.json`.
- [ ] On install/startup, register two menu items via `chrome.contextMenus.create`
      (contexts: `page`, `video`, `link`), gated by an enable flag in extension storage.
- [ ] `chrome.contextMenus.onClicked` handler: resolve URL by context
      (`pageUrl` for page/video, `linkUrl` for link).
- [ ] POST to `127.0.0.1:53472/capture-media` with `{ url, mode, quality }`,
      forwarding cookies/referer like the existing capture path.
- [ ] On POST failure (app closed), show a notification: "Open Downpour first".

### Phase 2 — Capture server media endpoint
- [ ] Add a `/capture-media` route (or a `kind` field on `/capture`) in
      `capture_server.rs`.
- [ ] Validate URL (reuse existing URL validation + size/scheme guards).
- [ ] `mode=quick` → build a `Media` `DownloadItem` with `media_format_id` =
      Best selector, `output_template` = `%(title)s.%(ext)s`, enqueue via `QueueManager`.
- [ ] `mode=options` → emit a Tauri event (e.g. `open-media`) carrying the URL.

### Phase 3 — App UI "open in Media tab"
- [ ] Frontend listener for `open-media`: switch to the Media view, fill the URL input,
      and trigger Extract automatically (reuses `classifyMediaUrl` → single/playlist).
- [ ] Focus/raise the app window when the event arrives (Tauri window `set_focus` +
      `unminimize`/`show`).

### Phase 4 — Settings
- [ ] Add a "Enable right-click download" toggle (synced to extension storage via popup).
- [ ] Add a "Right-click default quality" setting (default: **Best available**),
      used by the quick-download path.
- [ ] Wire the toggle in the extension popup (`popup.js`/`popup.html`).

### Phase 5 — Verify
- [ ] Manual: right-click a YouTube video → "Download with Downpour" opens the Media tab
      pre-filled; "Quick download (Best)" enqueues at highest quality, merges to mp4,
      lands in `Videos/`.
- [ ] Manual: right-click a video on X/Instagram/Reddit → page URL sent → yt-dlp extracts.
- [ ] Manual: right-click a direct `.zip`/`.pdf` link → routed to the HTTP engine.
- [ ] Manual: app closed → friendly "Open Downpour first" notification.
- [ ] `cargo fmt` + `cargo clippy` + `cargo test` for the server changes.

---

## 5. Constraints & risks

- **App must be running** — capture server only listens while open. A future
  native-messaging "launch on capture" is out of scope here.
- **DRM sites won't work** (Netflix/Spotify/etc.) — intentional policy boundary
  (`FORBIDDEN_FLAGS` guard).
- **Site coverage = yt-dlp's** — public content works; private/login content needs the
  deferred cookies feature.
- **Cross-browser** — targets Chrome/Edge (MV3 `chrome.contextMenus`). Firefox
  (`browser.menus`) is a later, separate pass.
- **blob: video sources** — handled by preferring `pageUrl` over `srcUrl`.

---

## 6. Out of scope (this iteration)

- Native messaging / auto-launching the app when closed.
- Firefox support.
- Cookies / login-gated content (tracked separately).
- Per-site custom handling beyond what yt-dlp provides.

---

## 7. Dependencies on existing work (already done)

- Media filename capture (`--print-to-file`), mp4 merge, real size, auto-categorize to
  `Videos/` — quick-download results inherit all of this automatically.
- `start_media_batch` / `start_media_download` and the Media-tab extract/format/playlist
  UI — reused by the "options" mode.
