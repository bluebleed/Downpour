---
type: reference
title: Implementation Plan: IDM-Style Download Manager
created: 2026-06-06
last-updated: 2026-07-02
load-behavior: on-demand
---

# Implementation Plan: IDM-Style Download Manager

## Overview

This plan extends Downpour's existing Tauri 2 downloader into a full-featured IDM-style download manager. Implementation proceeds bottom-up: core data models and infrastructure first, then the download engine enhancements, queue/scheduler, speed limiter, persistence, media extraction, browser extension, UI, and finally integration wiring. Each step builds incrementally on the previous, ensuring no orphaned code.

## Tasks

- [x] 1. Core data models, configuration, and project dependencies
  - [x] 1.1 Update Cargo.toml with new dependencies and create module files
    - Add `tokio-util` (CancellationToken), `chrono`, and `proptest` (dev) to Cargo.toml
    - Create new module files: `speed_limiter.rs`, `persistence.rs`, `media_extractor.rs`, `categorizer.rs`, `settings.rs`
    - Add `mod` declarations in `lib.rs` for all new modules
    - _Requirements: 1.1, 5.1, 4.1, 8.1_

  - [x] 1.2 Define enhanced data models and shared types
    - Expand `DownloadItem` struct with: `category`, `created_at`, `completed_at`, `speed`, `segments` (Vec<SegmentState>), `error_message`, `headers` (HashMap), `cookies`, `referer`, `is_resumable`, `download_type`, `segment_count`
    - Define `SegmentState` struct (index, start, end, downloaded, status)
    - Define `DownloadStatus` enum (Queued, Downloading, Paused, Complete, Error, Merging)
    - Define `PausedState` struct for holding segment offsets
    - Define `DownloadConfig` struct (segments, speed_limit, retry_count, retry_delay_ms)
    - Define `AppSettings` struct with all configuration fields and validation
    - Define `QueueConfig` struct (max_concurrent, max_retries, auto_start, speed_limit_global)
    - Ensure all types derive Clone, Serialize, Deserialize as needed
    - _Requirements: 1.1, 2.1, 3.1, 5.1, 11.1, 11.2, 11.7_

  - [x] 1.3 Write property tests for settings validation
    - **Property 19: Settings validation rejects out-of-bounds values**
    - **Validates: Requirements 11.1, 11.2, 11.3**

- [x] 2. Speed Limiter (Token Bucket)
  - [x] 2.1 Implement the token bucket speed limiter
    - Create `speed_limiter.rs` with `SpeedLimiter` struct using AtomicU64 for rate and tokens
    - Implement `new(bytes_per_sec)`, `set_rate(bytes_per_sec)`, `acquire(bytes)`, `current_rate()`
    - Token bucket refill at 100ms intervals for smooth throughput
    - Capacity = 1 second's worth of tokens (burst allowance)
    - Rate of 0 means unlimited (acquire returns immediately)
    - Dynamic rate adjustment without restarting downloads
    - _Requirements: 4.1, 4.2, 4.3, 4.5_

  - [x] 2.2 Write property test for speed limiter throughput bound
    - **Property 7: Speed limiter throughput bound**
    - **Validates: Requirement 4.1**

  - [x] 2.3 Write property test for speed limiter fairness
    - **Property 8: Speed limiter fairness**
    - **Validates: Requirement 4.4**

- [x] 3. Persistence Layer
  - [x] 3.1 Implement the persistence layer with JSON storage
    - Create `persistence.rs` with `PersistenceLayer` struct
    - Implement `save_download`, `load_all_downloads`, `save_segment_state`, `load_segments`
    - Implement `save_settings`, `load_settings`, `delete_download`
    - Use JSON file storage in the app data directory
    - Implement 500ms debounced writes to avoid excessive disk I/O
    - Handle corrupted file detection: rename with `.corrupt` suffix and start fresh
    - Provide default settings on first launch (max_concurrent: 3, segments: 4, speed_limit: 0)
    - _Requirements: 5.1, 5.2, 5.4, 5.5, 5.6, 5.7, 11.7_

  - [x] 3.2 Write property test for download persistence round-trip
    - **Property 9: Persistence round-trip for downloads**
    - **Validates: Requirements 5.1, 5.2**

  - [x] 3.3 Write property test for settings persistence round-trip
    - **Property 10: Persistence round-trip for settings**
    - **Validates: Requirement 5.4**

- [x] 4. Checkpoint - Ensure all tests pass
  - Ensure all tests pass, ask the user if questions arise.

- [x] 5. Enhanced Download Engine
  - [x] 5.1 Refactor downloader for segmented parallel downloads with pause/resume
    - Rewrite `downloader.rs` to support configurable segments (1-32)
    - Implement HEAD probe with 10s timeout for size and range support detection
    - Implement `compute_segments(total_size, num_segments)` ensuring full coverage with no gaps/overlaps
    - Pre-allocate destination file to total expected size
    - Download segments in parallel using `tokio::spawn` per segment with CancellationToken
    - Integrate `SpeedLimiter` — call `acquire(chunk_len)` before each write
    - Forward custom headers and cookies from DownloadItem in all HTTP requests
    - Fall back to single-stream when server doesn't support Range or file < 1MB
    - Verify file size on completion; mark "error" if mismatch
    - _Requirements: 1.1, 1.2, 1.3, 1.4, 1.5, 1.6, 1.7, 6.4_

  - [x] 5.2 Implement pause and resume functionality
    - Implement `pause(id)`: cancel all segment tasks via CancellationToken, record per-segment byte offsets into PausedState, set status to "paused"
    - Implement `resume(id, PausedState)`: restore segment offsets, continue from saved positions with Range requests
    - Handle edge cases: server no longer supports Range (restart from scratch), Content-Length changed (discard and restart), partial file missing (restart)
    - Persist PausedState via the persistence layer on pause
    - Emit download-progress events on state changes
    - _Requirements: 2.1, 2.2, 2.3, 2.4, 2.5, 2.6, 2.7_

  - [x] 5.3 Implement retry logic with exponential backoff
    - Retry on retryable errors (connection reset, timeout, DNS failure, HTTP 5xx) with delays: 1s, 2s, 4s, 8s, 16s, 30s (max 6 attempts per segment)
    - Immediately fail on non-retryable errors (HTTP 401, 403, 404, 410)
    - Resume from last successful byte offset on retry
    - Enforce 30s timeout per HTTP request (no data received)
    - Mark download as "error" after exhausting retries with descriptive message
    - _Requirements: 10.1, 10.2, 10.3, 10.6_

  - [x] 5.4 Implement filename sanitization and progress reporting
    - Sanitize filenames: strip `../`, `..\`, null bytes, control chars (0x00-0x1F, 0x7F), truncate to 200 chars (excluding extension)
    - Implement throttled progress events (max 3/sec per download)
    - Calculate current speed over 2-second sliding window
    - Calculate ETA (remaining_bytes / current_speed) when total_size known
    - Never report downloaded > total_size
    - _Requirements: 10.7, 12.1, 12.2, 12.4, 12.5_

  - [x] 5.5 Write property test for segment coverage
    - **Property 1: Segment coverage is total and non-overlapping**
    - **Validates: Requirement 1.1**

  - [x] 5.6 Write property test for download progress invariant
    - **Property 2: Download progress invariant**
    - **Validates: Requirements 1.5, 12.4**

  - [x] 5.7 Write property test for retry backoff calculation
    - **Property 17: Retry backoff is exponential and capped**
    - **Validates: Requirement 10.1**

  - [x] 5.8 Write property test for filename sanitization
    - **Property 18: Filename sanitization removes dangerous characters**
    - **Validates: Requirement 10.7**

- [x] 6. Checkpoint - Ensure all tests pass
  - Ensure all tests pass, ask the user if questions arise.

- [x] 7. Queue Manager
  - [x] 7.1 Implement the queue manager with concurrency control
    - Rewrite `queue.rs` with full `QueueManager` struct using `Arc<Semaphore>` for concurrency control
    - Implement `enqueue`, `pause`, `resume`, `cancel`, `remove`, `reorder`
    - Implement `pause_all` and `resume_all`
    - Implement `set_max_concurrent` with dynamic semaphore adjustment
    - Implement `set_speed_limit` for global speed coordination
    - FIFO scheduling: start next queued item when permit becomes available
    - Release permit on pause, complete, error, or cancel
    - Scheduler loop: react to queue-change notifications or 1s tick
    - Emit `queue-changed` events on state transitions
    - _Requirements: 3.1, 3.2, 3.3, 3.4, 3.5, 3.6, 10.4, 10.5_

  - [x] 7.2 Integrate queue with persistence for state recovery
    - Persist queue state on every enqueue, pause, complete, error
    - On app start: restore downloads from disk, set "downloading" items to "paused"
    - Restore queue ordering from persisted order
    - Do not auto-start restored downloads until user explicitly resumes
    - Handle disk-full scenario: pause all active downloads, emit error event
    - _Requirements: 5.1, 5.2, 5.3_

  - [x] 7.3 Write property test for queue concurrency invariant
    - **Property 4: Queue concurrency invariant**
    - **Validates: Requirements 3.1, 10.4**

  - [x] 7.4 Write property test for FIFO scheduling
    - **Property 5: Queue scheduling respects FIFO order**
    - **Validates: Requirement 3.2**

  - [x] 7.5 Write property test for queue reorder
    - **Property 6: Queue reorder is respected**
    - **Validates: Requirement 3.3**

- [x] 8. Auto-Categorizer
  - [x] 8.1 Implement the auto-categorizer
    - Create `categorizer.rs` with `Categorizer` struct and `CategoryRule` type
    - Implement `categorize(filename, mime)`: match extension first, then MIME, then "Other"
    - Default categories: Videos (.mp4, .mkv, .avi, .webm), Music (.mp3, .flac, .wav, .ogg), Images (.jpg, .png, .gif, .webp, .svg), Documents (.pdf, .doc, .docx, .txt, .xlsx), Archives (.zip, .rar, .7z, .tar, .gz), Programs (.exe, .msi, .dmg, .appimage, .deb), Other
    - Implement `move_to_category`: move file to subfolder, create subfolder if needed
    - Handle filename conflicts with incrementing suffix: "file (1).ext", "file (2).ext"
    - Skip categorization if disabled in settings
    - Emit error event if move fails, leave file in default directory
    - Enforce max 20 user categories, 50 extensions each
    - _Requirements: 7.1, 7.2, 7.3, 7.4, 7.5, 7.6, 7.7, 11.6_

  - [x] 8.2 Write property test for auto-categorizer totality
    - **Property 13: Auto-categorizer totality**
    - **Validates: Requirements 7.1, 7.3, 7.4**

- [x] 9. Capture Server Enhancements
  - [x] 9.1 Enhance the capture server with full metadata support
    - Expand `CaptureReq` to include: cookies, headers (HashMap), referer, page_url, mime_type, filesize, is_media
    - Validate URL: must be http/https scheme, 1-2048 chars, non-empty
    - Reject invalid schemes (file:, javascript:, data:) with descriptive error
    - Reject malformed JSON or missing "url" field with error response
    - Respond within 500ms with JSON {id, status: "queued"}
    - Handle optional fields gracefully: proceed with whatever is present
    - Pass cookies and headers through to the created DownloadItem
    - Wire capture to QueueManager.enqueue() instead of directly spawning downloads
    - _Requirements: 9.1, 9.2, 9.3, 9.4, 9.6, 6.3, 6.7_

  - [x] 9.2 Implement capture server port retry logic
    - Retry binding up to 5 times with exponential backoff (1s, 2s, 4s, 8s, 16s)
    - Notify user via system notification after all retries exhausted
    - _Requirements: 9.5_

  - [x] 9.3 Write property test for URL scheme validation
    - **Property 16: URL scheme validation**
    - **Validates: Requirement 9.4**

  - [x] 9.4 Write property test for captured metadata flow-through
    - **Property 11: Captured metadata flows through to HTTP requests**
    - **Validates: Requirements 6.3, 6.4**

- [x] 10. Checkpoint - Ensure all tests pass
  - Ensure all tests pass, ask the user if questions arise.

- [x] 11. Media Extractor (yt-dlp Integration)
  - [x] 11.1 Implement the media extractor module
    - Create `media_extractor.rs` with `MediaExtractor` struct
    - Implement `check_availability()`: verify yt-dlp and ffmpeg binaries exist at configured paths
    - Implement `extract_info(url, cookies)`: spawn `yt-dlp --dump-json` with 30s timeout, parse JSON output into `MediaInfo`
    - Implement `download(url, format_id, output_path, progress_tx)`: spawn yt-dlp child process, parse stdout progress lines, forward via channel
    - Parse yt-dlp progress format: extract percentage, speed, ETA
    - Throttle progress events to max 3/sec per media download
    - Never pass forbidden flags: `--allow-unplayable-formats`, `--cookies-from-browser`, geo-bypass flags
    - On cancel: send SIGTERM, wait 5s, then force-kill if still running
    - On error: include last 5 lines of stderr in error message, clean up partial files
    - On completion/failure: ensure no orphan child processes within 5s
    - _Requirements: 8.1, 8.2, 8.3, 8.4, 8.5, 8.6, 8.7, 8.8_

  - [x] 11.2 Write property test for yt-dlp progress parser
    - **Property 14: yt-dlp progress parser correctness**
    - **Validates: Requirement 8.3**

  - [x] 11.3 Write property test for forbidden flags
    - **Property 15: Media extractor never passes forbidden flags**
    - **Validates: Requirement 8.6**

- [x] 12. Browser Extension Enhancements
  - [x] 12.1 Enhance the browser extension with full metadata capture
    - Update `background.js` to intercept downloads via `chrome.downloads.onCreated`
    - Extract cookies for download domain via `chrome.cookies.getAll`
    - Capture referer from active tab URL and page URL
    - Build enhanced `CapturePayload` with: url, filename, filesize, mimeType, cookies, headers, referer, pageUrl, isMedia
    - Send POST to capture server with full payload
    - Cancel browser's native download after successful capture
    - _Requirements: 6.1, 6.2, 6.3_

  - [x] 12.2 Implement extension filtering and configuration
    - Size filtering: skip capture if file size < configurable minimum (default 1MB)
    - Extension whitelist/blacklist support (up to 200 entries per list)
    - Store filter configuration in extension local storage
    - Update `popup.html`/`popup.js` with filter configuration UI
    - Detect media links on page via content script (video/image sources)
    - _Requirements: 6.5, 6.6_

  - [x] 12.3 Write property test for extension capture filtering
    - **Property 12: Extension capture filtering correctness**
    - **Validates: Requirement 6.5**

- [x] 13. Tauri Commands and App Wiring
  - [x] 13.1 Register all Tauri commands and wire state management
    - Add new Tauri commands: `pause_download`, `resume_download`, `cancel_download`, `remove_download`, `reorder_download`
    - Add queue commands: `pause_all`, `resume_all`, `get_queue_state`
    - Add settings commands: `get_settings`, `update_settings`, `set_speed_limit`, `set_max_concurrent`
    - Add media commands: `extract_media_info`, `start_media_download`, `cancel_media_download`
    - Manage QueueManager, SpeedLimiter, PersistenceLayer, MediaExtractor, Categorizer as Tauri managed state
    - Wire app setup: initialize persistence, restore queue, start capture server, start scheduler loop
    - Connect download completion to auto-categorizer
    - Settings changes apply to components without restart
    - _Requirements: 11.5, 12.3, 3.1, 2.1_

  - [x] 13.2 Implement progress reporting and event system
    - Emit `download-progress` events with full DownloadItem state (throttled to 3/sec)
    - Emit `queue-changed` events on queue state changes (add, start, pause, resume, complete, error, remove, reorder)
    - Include speed (bytes/sec over 2s window) and ETA in progress events
    - _Requirements: 12.1, 12.2, 12.3, 12.5_

  - [x] 13.3 Write property test for progress event throttling
    - **Property 20: Progress event throttling**
    - **Validates: Requirement 12.2**

- [x] 14. Frontend UI Implementation
  - [x] 14.1 Build the glassmorphism layout and navigation
    - Implement sidebar + main content + status bar layout in `styles.css`
    - Implement glass panel base component with backdrop-filter
    - Sidebar navigation: Downloads, Queue, Media, Settings views
    - Status bar: global speed, active/queued counts, pause all/resume all buttons
    - Floating action button for "Add Download" modal
    - Responsive behavior: sidebar auto-collapse at narrow widths
    - View transitions with slide animations
    - _Requirements: 12.1, 12.3_

  - [x] 14.2 Implement the download list and card components
    - Download card with: file icon, filename, progress bar, speed, ETA, status badge, pause/cancel buttons
    - Progress bar with status-colored fills and shimmer animation
    - Compact vs comfortable view toggle
    - Status filter pills (All, Downloading, Queued, Paused, Complete, Error)
    - Search/filter functionality
    - Listen to `download-progress` and `queue-changed` events for real-time updates
    - _Requirements: 12.1, 12.4, 12.5_

  - [x] 14.3 Implement queue view, settings view, and media view
    - Queue view: ordered list with drag-to-reorder, start/pause/remove actions
    - Settings view: forms for max_concurrent, default_segments, speed_limit, download_dir, categories, yt-dlp/ffmpeg paths
    - Validate settings inputs client-side before invoking Tauri commands
    - Media view: URL input, format selection, download progress for media items
    - Add Download modal: URL input, filename override, segment count selector
    - Toast notifications for errors and status changes
    - _Requirements: 11.1, 11.2, 11.3, 11.4, 11.5, 11.6, 8.1, 8.2_

- [x] 15. Checkpoint - Ensure all tests pass
  - Ensure all tests pass, ask the user if questions arise.

- [x] 16. Integration and final wiring
  - [x] 16.1 End-to-end integration: extension → capture → queue → download → categorize
    - Verify the full flow: extension captures URL with cookies/headers → POST to capture server → enqueued in queue → download starts when permit available → file categorized on completion
    - Ensure pause/resume survives app restart via persistence layer
    - Verify concurrent download limit is enforced end-to-end
    - Verify speed limiting applies across all active segments
    - Verify media downloads go through queue manager
    - Clean up any dead code from the old simple downloader path
    - _Requirements: 1.1, 2.1, 3.1, 4.1, 5.2, 6.3, 7.1, 9.2_

  - [x] 16.2 Write integration tests for end-to-end flows
    - Test capture → queue → download → categorize flow
    - Test pause/resume cycle with persistence
    - Test concurrent download limit enforcement
    - Test settings change propagation
    - _Requirements: 1.1, 2.1, 3.1, 5.2, 11.5_

- [x] 17. Final checkpoint - Ensure all tests pass
  - Ensure all tests pass, ask the user if questions arise.

## Notes

- Tasks marked with `*` are optional and can be skipped for faster MVP
- Each task references specific requirements for traceability
- Checkpoints ensure incremental validation
- Property tests validate universal correctness properties from the design document
- Unit tests validate specific examples and edge cases
- The project uses Rust with `proptest` for property-based testing
- Existing code in `downloader.rs`, `capture_server.rs`, and `queue.rs` will be refactored in place
- Browser extension code (`extension/`) uses vanilla JavaScript (Manifest V3)
- Frontend (`src/`) uses vanilla JS with Vite and `@tauri-apps/api`

## Task Dependency Graph

```json
{
  "waves": [
    { "id": 0, "tasks": ["1.1"] },
    { "id": 1, "tasks": ["1.2"] },
    { "id": 2, "tasks": ["1.3", "2.1", "3.1"] },
    { "id": 3, "tasks": ["2.2", "2.3", "3.2", "3.3"] },
    { "id": 4, "tasks": ["5.1"] },
    { "id": 5, "tasks": ["5.2", "5.3", "5.4"] },
    { "id": 6, "tasks": ["5.5", "5.6", "5.7", "5.8", "8.1"] },
    { "id": 7, "tasks": ["7.1", "8.2"] },
    { "id": 8, "tasks": ["7.2", "7.3", "7.4", "7.5"] },
    { "id": 9, "tasks": ["9.1", "11.1"] },
    { "id": 10, "tasks": ["9.2", "9.3", "9.4", "11.2", "11.3"] },
    { "id": 11, "tasks": ["12.1", "13.1"] },
    { "id": 12, "tasks": ["12.2", "12.3", "13.2", "13.3"] },
    { "id": 13, "tasks": ["14.1"] },
    { "id": 14, "tasks": ["14.2", "14.3"] },
    { "id": 15, "tasks": ["16.1"] },
    { "id": 16, "tasks": ["16.2"] }
  ]
}
```
