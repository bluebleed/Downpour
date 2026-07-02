---
type: reference
title: Requirements Document
created: 2026-06-05
last-updated: 2026-07-02
load-behavior: on-demand
---

# Requirements Document

## Introduction

Downpour is a full-featured IDM-style download manager built with Tauri 2 (Rust core + vanilla JS UI). This document specifies the requirements for extending the existing segmented downloader into a production-ready download management system with: enhanced browser extension capture, pause/resume with persistence, download queue with concurrency control, bandwidth throttling, auto-categorization, and media extraction via yt-dlp/ffmpeg integration.

All features operate within the responsible-use boundary: only content the user is permitted to access is downloaded. No DRM/paywall bypass or mass-download features that violate site Terms of Service are included.

## Glossary

- **Download_Engine**: The Rust async component responsible for segmented parallel HTTP downloads, pause/resume, and progress reporting
- **Queue_Manager**: The scheduling layer that controls download concurrency, ordering, and lifecycle
- **Speed_Limiter**: The token-bucket rate limiter that throttles bandwidth across all active downloads
- **Persistence_Layer**: The storage component that saves download state, queue, and settings to disk (JSON/SQLite)
- **Media_Extractor**: The component that orchestrates yt-dlp and ffmpeg for video/image downloads from supported platforms
- **Browser_Extension**: The Manifest V3 Chrome extension that intercepts downloads and captures request metadata
- **Capture_Server**: The localhost HTTP server (127.0.0.1:53472) that receives capture payloads from the Browser_Extension
- **Auto_Categorizer**: The component that sorts completed downloads into category folders based on file extension or MIME type
- **Segment**: A contiguous byte range of a file downloaded independently in parallel with other segments
- **DownloadItem**: The data structure representing a single download's metadata, progress, and state
- **Token_Bucket**: An algorithm for rate limiting where tokens (representing bytes) are consumed on writes and refilled at a fixed rate
- **PausedState**: A snapshot of per-segment byte offsets used to resume a download from where it stopped

## Requirements

### Requirement 1: Segmented Parallel Downloads

**User Story:** As a user, I want my downloads to use multiple parallel connections, so that I can maximize download speed by utilizing available bandwidth.

#### Acceptance Criteria

1. WHEN a download is started and the server responds with `Accept-Ranges: bytes` and the file size is greater than 1 MB, THE Download_Engine SHALL split the file into segments (configurable from 1 to 32, default 4) and download them in parallel
2. WHEN the server does not support range requests, THE Download_Engine SHALL fall back to single-stream download mode and proceed with a single sequential connection
3. WHEN a download starts, THE Download_Engine SHALL send a HEAD request with a timeout of 10 seconds to determine file size and range support before beginning transfer
4. THE Download_Engine SHALL pre-allocate the destination file to the total expected size before writing segments
5. WHEN all segments complete, THE Download_Engine SHALL verify that the written file size equals the expected total size and IF the size does not match, THEN THE Download_Engine SHALL mark the download status as "error" and retain the partial file on disk
6. IF a segment download fails after 3 retry attempts, THEN THE Download_Engine SHALL abort the entire download, mark the download status as "error", and retain the partial file on disk
7. IF pre-allocation fails due to insufficient disk space, THEN THE Download_Engine SHALL mark the download status as "error" and report the failure before writing any data

### Requirement 2: Pause and Resume

**User Story:** As a user, I want to pause and resume downloads without losing progress, so that I can manage bandwidth and handle interruptions gracefully.

#### Acceptance Criteria

1. WHEN a user pauses an active download, THE Download_Engine SHALL cancel all active segment tasks within 2 seconds, record each segment's current byte offset into a PausedState, and set the DownloadItem status to "paused"
2. WHEN a user resumes a paused download, THE Download_Engine SHALL restore segment offsets from PausedState and continue downloading from those positions using HTTP Range requests starting at each segment's recorded offset
3. WHEN a download is paused, THE Queue_Manager SHALL release the concurrency permit so another queued download can start
4. WHEN a download is resumed, THE Download_Engine SHALL produce a file byte-for-byte identical to one downloaded without interruption
5. IF the server no longer supports range requests on resume, THEN THE Download_Engine SHALL restart the download from scratch in single-stream mode and emit a download-progress event indicating the download was restarted
6. IF the server reports a different Content-Length than the original total_size on resume, THEN THE Download_Engine SHALL discard the existing partial file, restart the download from the beginning, and emit a download-progress event indicating the file changed
7. IF the partial file on disk is missing or shorter than the smallest segment offset recorded in PausedState, THEN THE Download_Engine SHALL restart the download from scratch and emit a download-progress event indicating the partial data was lost

### Requirement 3: Download Queue with Concurrency Control

**User Story:** As a user, I want my downloads managed in a queue with a configurable maximum concurrent limit, so that I can control resource usage without manually starting each download.

#### Acceptance Criteria

1. THE Queue_Manager SHALL enforce that the number of active downloads (status "downloading") never exceeds the configured max_concurrent limit (1-10)
2. WHEN a download completes, is paused, or errors, THE Queue_Manager SHALL automatically start the next queued download in FIFO order if a queued download exists and the active count is below max_concurrent
3. WHEN a user reorders downloads in the queue, THE Queue_Manager SHALL use the new ordering for all subsequent scheduling decisions, applying only to downloads with status "queued"
4. WHEN max_concurrent setting is increased, THE Queue_Manager SHALL immediately start queued downloads until active count reaches the new limit; WHEN decreased below the current active count, THE Queue_Manager SHALL allow active downloads to complete naturally without cancellation and SHALL NOT start new downloads until active count drops below the new limit
5. WHEN a pause_all operation is invoked, THE Queue_Manager SHALL cancel all active downloads (recording their PausedState) and set all queued downloads to "paused" status, preventing any new downloads from starting
6. WHEN a resume_all operation is invoked, THE Queue_Manager SHALL transition all paused downloads to "queued" status and start up to max_concurrent downloads in queue order

### Requirement 4: Speed Limiting

**User Story:** As a user, I want to set a global bandwidth limit for downloads, so that I can use the internet for other activities while downloading.

#### Acceptance Criteria

1. WHEN a speed limit is configured (bytes/sec > 0), THE Speed_Limiter SHALL ensure total write throughput across all active downloads does not exceed the configured rate plus one burst allowance equal to (configured_rate × 0.1) bytes
2. WHEN the speed limit is set to 0, THE Speed_Limiter SHALL allow unlimited throughput
3. WHEN the speed limit is changed at runtime, THE Speed_Limiter SHALL apply the new rate within 100ms without pausing active downloads
4. WHILE multiple download segments are active, THE Speed_Limiter SHALL distribute bandwidth equally across all active segments such that no single segment receives more than 110% of (configured_rate ÷ active_segment_count) sustained over any 1-second window
5. THE Speed_Limiter SHALL use a token-bucket algorithm with refill interval of 100ms for smooth throughput
6. IF a speed limit value is configured, THEN THE Speed_Limiter SHALL accept only 0 (unlimited) or a positive integer between 1024 and 1073741824 bytes per second

### Requirement 5: Persistence and Recovery

**User Story:** As a user, I want my downloads, queue state, and settings to survive application restarts, so that I never lose progress due to closing the app or a crash.

#### Acceptance Criteria

1. WHEN a download is created, paused, or changes state, THE Persistence_Layer SHALL save the DownloadItem (including status, segment offsets, and queue position) to disk within 1 second of the state change
2. WHEN the application starts, THE Persistence_Layer SHALL restore all previously queued and paused downloads from disk, and any download that was in "downloading" state at the time of exit SHALL be restored with status "paused"
3. WHEN the application starts with restored downloads, THE Queue_Manager SHALL place restored downloads into the queue in their previously persisted order without automatically starting them until the user explicitly resumes
4. THE Persistence_Layer SHALL persist user settings (speed limits, concurrent downloads, categories, download directory) across restarts
5. IF the persistence file fails a structural validity check (malformed JSON or unreadable schema), THEN THE Persistence_Layer SHALL rename the corrupted file with a ".corrupt" suffix, start with a fresh empty state, and emit an error event to the UI indicating recovery occurred
6. THE Persistence_Layer SHALL debounce writes with a 500ms delay to avoid excessive disk I/O on rapid progress updates
7. WHEN the application starts and the persisted download directory no longer exists, THE Persistence_Layer SHALL fall back to the OS default downloads directory and notify the user via a UI event

### Requirement 6: Enhanced Browser Extension Capture

**User Story:** As a user, I want my browser extension to capture full request context (cookies, headers, referer) when intercepting downloads, so that authenticated and protected downloads succeed.

#### Acceptance Criteria

1. WHEN the Browser_Extension intercepts a download, THE Browser_Extension SHALL extract all cookies whose domain attribute matches the download URL's domain (including subdomain-scoped cookies) via the browser cookies API
2. WHEN the Browser_Extension intercepts a download, THE Browser_Extension SHALL capture the referer URL from the initiating tab's current URL and the page URL from the tab that triggered the download
3. WHEN the Browser_Extension sends a capture payload to the Capture_Server, THE Capture_Server SHALL store the received cookies, referer, and headers as part of the resulting DownloadItem and make them available to the Download_Engine before the download begins
4. WHEN the Download_Engine starts a captured download, THE Download_Engine SHALL include all captured cookies in the Cookie header and all captured headers (including the Referer header) in every HTTP request for that download, including segment requests and requests following redirects
5. IF the intercepted download's declared or detected file size is below the configured minimum size threshold (default: 1 MB, configurable from 0 to 100 MB), THEN THE Browser_Extension SHALL skip capture and allow the browser to handle the download natively
6. WHEN the Browser_Extension evaluates a download for capture, THE Browser_Extension SHALL compare the file extension against a user-configurable whitelist (capture only these extensions) or blacklist (skip these extensions), supporting up to 200 entries per list, and skip capture for any download that does not pass the active filter
7. IF the Capture_Server receives a payload missing the cookies or headers fields, THEN THE Capture_Server SHALL proceed with the download using only the URL and any fields that are present, without rejecting the request

### Requirement 7: Auto-Categorization

**User Story:** As a user, I want completed downloads automatically sorted into category folders, so that my files are organized without manual effort.

#### Acceptance Criteria

1. WHEN a download completes, THE Auto_Categorizer SHALL determine the category by matching the file extension first; if the extension is not mapped to any category, THE Auto_Categorizer SHALL fall back to the MIME type reported by the server
2. THE Auto_Categorizer SHALL provide default categories: Videos, Music, Images, Documents, Archives, Programs, and Other, each mapped to a subfolder within the configured download directory
3. WHEN a file extension matches a category rule, THE Auto_Categorizer SHALL move the completed file to the corresponding category subfolder, creating the subfolder if it does not exist
4. WHEN a file extension does not match any specific category and no MIME type rule matches, THE Auto_Categorizer SHALL place it in the "Other" category subfolder
5. WHERE auto-categorization is disabled in settings, THE Auto_Categorizer SHALL leave completed files in the default download directory
6. IF a file with the same name already exists in the target category subfolder, THEN THE Auto_Categorizer SHALL append an incrementing numeric suffix (e.g., "file (1).ext", "file (2).ext") to avoid overwriting the existing file
7. IF the file move operation fails due to a filesystem error, THEN THE Auto_Categorizer SHALL leave the file in the default download directory and emit an error event to the UI indicating the categorization failure

### Requirement 8: Media Extraction (yt-dlp Integration)

**User Story:** As a user, I want to download permitted video and image content from supported platforms using yt-dlp, so that I can save media I am authorized to access.

#### Acceptance Criteria

1. WHEN a user requests media info extraction, THE Media_Extractor SHALL spawn yt-dlp with --dump-json and return available formats without downloading content, applying a timeout of 30 seconds to the info extraction process
2. WHEN a user selects a format and initiates download, THE Media_Extractor SHALL spawn yt-dlp as a child process to download and merge the selected format, saving the output to the configured download directory
3. WHILE yt-dlp is running, THE Media_Extractor SHALL parse stdout progress output and emit progress events to the UI at a maximum rate of 3 events per second per download
4. WHEN a media extraction or download completes or fails, THE Media_Extractor SHALL terminate the yt-dlp child process tree and ensure no orphan child processes remain within 5 seconds
5. IF yt-dlp or ffmpeg binaries are not found at the configured path, THEN THE Media_Extractor SHALL return an error indicating which binary is missing and providing setup instructions
6. THE Media_Extractor SHALL respect the responsible-use boundary by never passing DRM-bypass flags (--allow-unplayable-formats), credential-extraction flags (--cookies-from-browser), or geo-bypass flags to yt-dlp
7. IF yt-dlp exits with a non-zero status code during info extraction or download, THEN THE Media_Extractor SHALL mark the download as "error", include the last 5 lines of yt-dlp stderr in the error message, and clean up any partial output files
8. WHEN a user cancels an in-progress media download, THE Media_Extractor SHALL send SIGTERM (or platform equivalent) to the yt-dlp child process, wait up to 5 seconds for graceful exit, then force-kill if still running, and remove incomplete output files

### Requirement 9: Capture Server

**User Story:** As a user, I want the desktop app to receive downloads from my browser extension reliably, so that intercepted downloads are seamlessly handed off to the download manager.

#### Acceptance Criteria

1. THE Capture_Server SHALL bind only to 127.0.0.1 on port 53472
2. WHEN a POST request is received on the /capture endpoint with a JSON body containing a required "url" field (string, 1–2048 characters, http: or https: scheme) and an optional "filename" field (string, 1–255 characters), THE Capture_Server SHALL create a DownloadItem and enqueue it in the Queue_Manager
3. WHEN a valid capture request is received, THE Capture_Server SHALL respond within 500 milliseconds with a JSON body containing the created item's ID and a status field set to "queued"
4. IF a capture request contains a URL with a scheme other than http: or https: (including file:, javascript:, and data:), or a URL that exceeds 2048 characters, or a missing/empty URL field, THEN THE Capture_Server SHALL respond with an error response indicating the reason for rejection and SHALL NOT create a DownloadItem
5. IF port 53472 is already in use at startup, THEN THE Capture_Server SHALL retry binding up to 5 times with exponential backoff starting at 1 second (1s, 2s, 4s, 8s, 16s) and notify the user via system notification after all retries are exhausted
6. IF the request body is not valid JSON or is missing the required "url" field, THEN THE Capture_Server SHALL respond with an error response indicating malformed input

### Requirement 10: Error Handling and Resilience

**User Story:** As a user, I want downloads to recover from transient failures automatically, so that temporary network issues do not require manual intervention.

#### Acceptance Criteria

1. WHEN a segment download fails due to a retryable error (connection reset, timeout, DNS resolution failure, or HTTP 5xx response), THE Download_Engine SHALL retry the segment from the last successful byte offset using exponential backoff delays of 1s, 2s, 4s, 8s, 16s, and 30s, up to a maximum of 6 retry attempts per segment
2. IF a segment download fails due to a non-retryable error (HTTP 401, 403, 404, or 410 response), THEN THE Download_Engine SHALL immediately mark the download as "error" with an error message indicating the HTTP status and reason, without retrying
3. WHEN a segment exceeds 6 retry attempts, THE Download_Engine SHALL mark the download as "error" with an error message indicating the failure reason and the number of attempts made
4. IF the disk becomes full during a download (write operation returns an OS out-of-space error), THEN THE Queue_Manager SHALL pause all active downloads and emit an error event to the UI indicating insufficient disk space
5. WHEN a download is marked as error, THE Queue_Manager SHALL release the concurrency permit and attempt to start the next queued item in FIFO order
6. THE Download_Engine SHALL enforce a 30-second timeout per individual HTTP request; if no data is received within that period, the request SHALL be treated as a retryable error
7. THE Download_Engine SHALL sanitize filenames by stripping path traversal sequences (../, ..\), null bytes, and characters in the ASCII range 0x00-0x1F and 0x7F, and truncating the resulting name to a maximum of 200 characters (excluding extension) before writing to disk

### Requirement 11: Settings and Configuration

**User Story:** As a user, I want to configure download behavior (concurrent limit, segment count, speed limit, download directory, categories), so that the manager adapts to my preferences and system.

#### Acceptance Criteria

1. IF a max_concurrent value outside the range 1-10 is submitted, THEN THE Queue_Manager SHALL reject the value, retain the previous setting, and return an error message indicating the valid range
2. IF a default_segments value outside the range 1-32 is submitted, THEN THE Download_Engine SHALL reject the value, retain the previous setting, and return an error message indicating the valid range
3. WHEN a speed_limit value is configured, THE Speed_Limiter SHALL validate it is either 0 (unlimited) or a positive integer representing bytes per second, and IF the value is negative or non-integer, THEN THE Speed_Limiter SHALL reject it and retain the previous setting
4. IF the configured download_dir path does not exist and cannot be created due to permissions or invalid path, THEN THE Persistence_Layer SHALL reject the change, retain the previous download_dir, and return an error message indicating the reason
5. WHEN settings are changed via the UI, THE Persistence_Layer SHALL persist changes to disk within 1 second and affected components (Queue_Manager, Download_Engine, Speed_Limiter, Auto_Categorizer) SHALL apply the new values to subsequent operations without app restart
6. WHEN a user configures category rules, THE Auto_Categorizer SHALL accept a category name and a list of associated file extensions, and SHALL enforce a maximum of 20 user-defined categories each with a maximum of 50 file extensions
7. THE Persistence_Layer SHALL provide default settings on first launch: max_concurrent of 3, default_segments of 4, speed_limit of 0 (unlimited), download_dir set to the OS-standard downloads folder, and default category rules as defined in Requirement 7

### Requirement 12: Progress Reporting and UI Events

**User Story:** As a user, I want to see real-time progress for all my downloads, so that I can monitor speed, ETA, and status at a glance.

#### Acceptance Criteria

1. WHILE a download is active, THE Download_Engine SHALL emit download-progress events containing the full DownloadItem state including bytes downloaded, current speed in bytes per second (calculated over the most recent 2-second window), and status
2. THE Download_Engine SHALL throttle progress events to a maximum of 3 per second per download to avoid flooding the UI
3. WHEN the queue state changes (item added, started, paused, resumed, completed, errored, removed, or reordered), THE Queue_Manager SHALL emit a queue-changed event containing the ordered list of DownloadItem summaries (id, filename, status, and position)
4. IF total_size is known (greater than 0), THEN THE Download_Engine SHALL never report downloaded bytes exceeding total_size in the DownloadItem payload
5. WHILE a download is active and total_size is known, THE Download_Engine SHALL include an estimated time remaining (ETA) in seconds, calculated as remaining bytes divided by current speed, in each download-progress event
