---
type: reference
title: Downpour — Architecture & Permanent Design Decisions
created: 2026-06-07
last-updated: 2026-07-02
load-behavior: on-demand
---

# Downpour — Architecture & Permanent Design Decisions

> **Case B memory** (project-permanent). Updated when major architectural decisions are made.  
> For agent-agnostic project rules see `AGENTS.md`. For the full spec see `.kiro/specs/idm-download-manager/`.

---

## What Kiro Built (Sprint 1 — completed June 2026)

All 17 task groups in `.kiro/specs/idm-download-manager/tasks.md` are marked `[x]` complete.
Kiro implemented a **feature-complete IDM-style download manager** from the spec. Here is what
exists and is wired end-to-end:

### Rust Backend (`src-tauri/src/`)

| Module | Status | Notes |
|---|---|---|
| `models.rs` | ✅ Complete | `DownloadItem`, `SegmentState`, `PausedState`, `DownloadConfig`, `QueueConfig`, enums |
| `downloader.rs` | ✅ Complete | Segmented parallel (HTTP `Range`), single-stream fallback, pause/resume with PausedState, 6-attempt retry with exponential backoff (1/2/4/8/16/30s), 30s request timeout, speed (2s window), ETA, progress throttling (3/sec), filename sanitization |
| `queue.rs` | ✅ Complete | `QueueManager` with `Arc<Semaphore>` concurrency, FIFO, pause/resume/cancel/remove/reorder, `pause_all`/`resume_all`, dynamic `set_max_concurrent`, `restore_from_disk`, `handle_disk_full` |
| `speed_limiter.rs` | ✅ Complete | Token-bucket rate limiter, 100ms refill interval, atomic operations, `set_rate()` live update |
| `persistence.rs` | ✅ Complete | JSON storage in OS app-data dir, 500ms debounced writes, corruption detection (`.corrupt` rename), `load_all_downloads`, `save_settings`, `load_settings` |
| `settings.rs` | ✅ Complete | `AppSettings` with all config fields, `CategoryRule`, validation helpers, property-tested |
| `categorizer.rs` | ✅ Complete | `Categorizer` with default categories (Videos/Music/Images/Documents/Archives/Programs/Other), conflict resolution with incrementing suffix |
| `media_extractor.rs` | ✅ Complete | yt-dlp/ffmpeg wrapper, `extract_info` (--dump-json, 30s timeout), `download` with progress forwarding, SIGTERM/force-kill cleanup, forbidden flags enforced |
| `capture_server.rs` | ✅ Complete | axum server on `127.0.0.1:53472`, `/capture` POST endpoint, URL validation, port retry (5x exponential backoff), wires to `QueueManager.enqueue()` |
| `lib.rs` | ✅ Complete | 19 Tauri commands registered, full state wiring, auto-categorizer listener |

### Frontend (`src/`)

| File | Status | Notes |
|---|---|---|
| `styles.css` | ✅ Complete | Glassmorphism design, sidebar + main + status bar layout, dark mode |
| `main.js` | ✅ Complete | 4 views (Downloads, Queue, Media, Settings), real-time event listeners, drag-to-reorder queue, settings forms, toast notifications |

### Browser Extension (`extension/`)

| File | Status | Notes |
|---|---|---|
| `background.js` | ✅ Complete | MV3 service worker, `chrome.downloads.onCreated`, cookie extraction, referer capture, size/extension filtering, cancels native download after capture |
| `content.js` | ✅ Complete | Detects video/image media sources on pages |
| `filter.js` | ✅ Complete | Whitelist/blacklist filter logic (up to 200 entries) |
| `popup.html` / `popup.js` | ✅ Complete | Extension popup UI with filter configuration |

---

## System Architecture

```
Browser Extension (extension/)  --POST /capture (cookies+headers+referer)--> Capture Server (127.0.0.1:53472)
        |                                                                               |
        v                                                                               v
   Web Dashboard (src/)  <------Tauri commands + events (download-progress, queue-changed)-------> QueueManager (queue.rs)
                                                                                                          |
                                                    +---------------------+----------------------------+-+---+
                                                    |                     |                            |     |
                                               downloader.rs       media_extractor.rs           speed_limiter.rs
                                                    |                     |                            |
                                               (HTTP engine)         (yt-dlp/ffmpeg)          (token bucket)
                                                    |                     |
                                                    +----> persistence.rs <----+
                                                    |                          |
                                                    v                          v
                                             files saved to disk          categorizer.rs
                                                                        (moves to subfolder)
```

## Event Contract (stable — do not break)

```typescript
// "download-progress" event payload (DownloadItem)
{
  id: string,
  url: string,
  filename: string,
  totalSize: number,
  downloaded: number,
  status: "queued" | "downloading" | "paused" | "complete" | "error" | "merging",
  category: string | null,
  createdAt: number,          // Unix seconds
  completedAt: number | null, // Unix seconds
  speed: number,              // bytes/sec (2s sliding window)
  eta: number | null,         // seconds remaining
  segments: SegmentState[],
  errorMessage: string | null,
  headers: Record<string, string>,
  cookies: string | null,
  referer: string | null,
  isResumable: boolean,
  downloadType: "http" | "media" | "batch",
  segmentCount: number,
  mediaFormatId: string | null
}

// "queue-changed" event payload: QueueItemSummary[]
{ id: string, filename: string, status: DownloadStatus, position: number }[]

// "disk-full" event payload: DiskFullPayload
{ paused: string[], message: string }
```

## Configuration Constraints (validated, reject invalid values)

| Setting | Type | Valid range | Default |
|---|---|---|---|
| `max_concurrent` | usize | 1–10 | 3 |
| `default_segments` | u32 | 1–32 | 4 |
| `speed_limit` | u64 | 0 (unlimited) or positive | 0 |
| `capture_min_size` | u64 | any | 1 MB |
| `categories` | Vec<CategoryRule> | ≤ 20 categories, ≤ 50 extensions each | see defaults |

## Key Invariants

1. `DownloadItem.downloaded` is always ≤ `total_size` when total is known (`clamp_reported_downloaded`).
2. Progress events fire at most 3x/sec per download.
3. Concurrency: `count(status=Downloading)` ≤ `max_concurrent` at all times.
4. On app restart: any item that was `Downloading` is restored as `Paused`.
5. The scheduler `suspended` flag is set to `true` after `restore_from_disk()` — user must explicitly resume, **unless** `AppSettings.resume_on_startup` is enabled, in which case `lib.rs` setup auto-calls `resume_all()` after restore.
6. Media downloads (yt-dlp) go through the same `QueueManager` and consume `max_concurrent` slots.
7. Speed limiter is shared across ALL active segments of ALL downloads.

## Dependency Rationale

| Crate | Role |
|---|---|
| `tauri 2` | Desktop shell + IPC |
| `tokio` | Async runtime (full features) |
| `reqwest` (rustls-tls) | HTTP client for downloading |
| `axum` | Capture server (local HTTP) |
| `tokio-util` (CancellationToken) | Pause / cancel propagation to segment tasks |
| `anyhow` | Error handling in engine code |
| `serde` / `serde_json` | Serialization for events + persistence |
| `uuid` | Download IDs |
| `dirs` | OS download/data directory paths |
| `chrono` | Timestamps |
| `proptest` (dev) | Property-based tests for invariants |
| `tempfile` (dev) | Temporary files in tests |
| `libc` (unix only) | SIGTERM for yt-dlp process group |

## Open TODOs (from AGENTS.md roadmap, not yet done)

- [ ] System tray + native notifications (tauri-plugin-notification registered but tray not implemented).
- [x] **`queue.rs` `suspended` flag is set after restore — expose "Resume All on Startup" setting.** ✅ (June 2026) `AppSettings.resume_on_startup` (default `false`); when enabled, `lib.rs` setup calls `queue.resume_all()` after `restore_from_disk()` so interrupted downloads auto-resume. UI toggle in the Settings view.
- [x] **`downloader.rs`: `downloads_dir()` ignored `AppSettings.download_dir`.** ✅ (June 2026) Resolved: the configured dir now flows `AppSettings.download_dir` → `QueueConfig.download_dir` → `QueueManager` (live `RwLock<PathBuf>`, updated by `set_download_dir` on settings change) → threaded as `dest_dir` into `run`/`resume_download`/`download_core`/`resume_core`, and used by cancel/media cleanup + the completion categorizer's source path. `downloads_dir()` remains only as the default fallback. In-flight downloads keep their original destination if the dir changes mid-flight.
- [ ] Extension: The `content.js` media detection is basic; expand for more platforms.
- [ ] HOW_TO_RUN.md mentions `run.bat` which handles Windows setup well.

## Testing

- **Property tests** in each module (via `proptest`): segment coverage, progress invariant, retry backoff,
  filename sanitization, speed limiter throughput/fairness, persistence round-trips, queue concurrency,
  FIFO scheduling, URL scheme validation, captured metadata flow-through, yt-dlp progress parser,
  forbidden flags, extension filtering, progress throttling, settings validation.
- Run with: `cargo test --manifest-path src-tauri/Cargo.toml`
