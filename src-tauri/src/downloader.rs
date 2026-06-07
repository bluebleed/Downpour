//! The download engine: probes a URL, then downloads it in parallel segments
//! (HTTP `Range` requests) when the server supports it, falling back to a single
//! stream otherwise. Progress is emitted to the UI via the `download-progress` event.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use futures_util::StreamExt;
use reqwest::StatusCode;
use tauri::{AppHandle, Emitter};
use tokio_util::sync::CancellationToken;

use crate::models::{
    CancelTokens, DownloadItem, DownloadStatus, Downloads, PausedState, SegmentState, SegmentStatus,
};
use crate::persistence::PersistenceLayer;
use crate::speed_limiter::SpeedLimiter;

/// Minimum file size (1 MB) required to use segmented parallel downloads.
const MIN_SEGMENTED_SIZE: u64 = 1_048_576;

/// Maximum number of retry attempts per segment.
const MAX_RETRIES: u32 = 6;

/// Maximum backoff delay in milliseconds (30 seconds).
const MAX_BACKOFF_MS: u64 = 30_000;

/// Timeout for HTTP requests: if no data is received within this duration,
/// the request is considered timed out (retryable).
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Compute exponential backoff delay for a given attempt index.
///
/// Formula: min(2^attempt * 1000, 30000) ms
/// Produces: 1s, 2s, 4s, 8s, 16s, 30s for attempts 0..5
pub fn compute_backoff_delay(attempt: u32) -> Duration {
    let delay_ms = (1000u64)
        .saturating_mul(2u64.saturating_pow(attempt))
        .min(MAX_BACKOFF_MS);
    Duration::from_millis(delay_ms)
}

/// Returns `true` if the reqwest error is retryable (transient network issue).
///
/// Retryable: timeouts, connection errors, and other network errors.
/// Non-retryable: decode errors, builder errors, redirect errors, etc.
pub fn is_retryable_error(error: &reqwest::Error) -> bool {
    error.is_timeout() || error.is_connect() || error.is_request()
}

/// Returns `true` if the HTTP status code indicates a retryable server error (5xx).
pub fn is_retryable_status(status: StatusCode) -> bool {
    status.is_server_error()
}

/// Returns `true` if the HTTP status code is non-retryable (immediate failure).
///
/// Non-retryable: 401 Unauthorized, 403 Forbidden, 404 Not Found, 410 Gone.
pub fn is_non_retryable_status(status: StatusCode) -> bool {
    matches!(
        status,
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN | StatusCode::NOT_FOUND | StatusCode::GONE
    )
}

/// Maximum number of progress events per second per download.
const MAX_PROGRESS_EVENTS_PER_SEC: u64 = 3;

/// Minimum interval between progress events (333ms for 3/sec).
const PROGRESS_INTERVAL_MS: u64 = 1000 / MAX_PROGRESS_EVENTS_PER_SEC;

/// Sliding window duration for speed calculation (2 seconds).
const SPEED_WINDOW_SECS: u64 = 2;

/// Decide whether a progress event is due, given the timestamp of the last
/// emitted event (`None` if none has been emitted yet), the current time, and
/// the minimum interval between events. Returns `true` when at least
/// `interval_ms` has elapsed since the last event (or no event has fired yet).
///
/// This is the throttling primitive that bounds progress events to a maximum
/// rate (Req 12.2). It is a pure function so it can be property-tested without a
/// runtime (task 13.3).
pub fn progress_event_due(last_emit_ms: Option<u64>, now_ms: u64, interval_ms: u64) -> bool {
    match last_emit_ms {
        None => true,
        Some(last) => now_ms.saturating_sub(last) >= interval_ms,
    }
}

/// Compute the current speed in bytes/sec from a sliding window of
/// `(timestamp_ms, cumulative_bytes)` samples (Req 12.1, oldest-to-newest).
///
/// Returns 0 when fewer than two samples are available or no time has elapsed.
/// Pure helper so it is unit-testable.
pub fn speed_from_samples(samples: &[(u64, u64)]) -> u64 {
    if samples.len() < 2 {
        return 0;
    }
    let oldest = samples[0];
    let newest = samples[samples.len() - 1];
    let elapsed_ms = newest.0.saturating_sub(oldest.0);
    let bytes_delta = newest.1.saturating_sub(oldest.1);
    bytes_delta
        .saturating_mul(1000)
        .checked_div(elapsed_ms)
        .unwrap_or(0)
}

/// Compute the ETA in seconds (remaining bytes ÷ speed) when the total size is
/// known and the speed is non-zero, else `None` (Req 12.5). Pure helper.
pub fn compute_eta(total: u64, downloaded: u64, speed: u64) -> Option<u64> {
    if total > 0 && speed > 0 {
        Some(total.saturating_sub(downloaded) / speed)
    } else {
        None
    }
}

/// Maximum filename stem length (excluding extension).
const MAX_FILENAME_STEM_LEN: usize = 200;

/// Outcome of a download run or resume.
///
/// Cancellation (pause) is an expected, non-error outcome, so it is modelled
/// here rather than as an `Err`. This lets callers distinguish a paused
/// download (status already set to `Paused`) from a genuine failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DownloadOutcome {
    /// All bytes were downloaded and the file passed size verification.
    Completed,
    /// The download was cancelled (paused) before completing.
    Paused,
}

/// The action the resume logic should take, decided from the server's current
/// response and the on-disk partial file. Pure, so it is unit-testable without
/// any network access.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ResumeAction {
    /// Server state matches the saved state — resume from saved offsets.
    Resume,
    /// Server reports a different Content-Length — discard and restart (Req 2.6).
    RestartContentLengthChanged,
    /// Server no longer supports Range requests — restart single-stream (Req 2.5).
    RestartRangeUnsupported,
    /// Partial file is missing or shorter than the recorded offset (Req 2.7).
    RestartPartialFileMissing,
}

/// Sanitize a filename by removing dangerous characters and sequences.
///
/// - Strips path traversal sequences (`../` and `..\`)
/// - Removes null bytes (`\0`)
/// - Removes control characters (ASCII 0x00-0x1F and 0x7F)
/// - Truncates the stem (name without extension) to 200 characters
/// - Preserves the file extension
/// - Returns "download.bin" if the result is empty after sanitization
pub fn sanitize_filename(raw: &str) -> String {
    // Remove null bytes and control characters FIRST (0x00-0x1F, 0x7F).
    // Doing this before stripping traversal sequences prevents control chars
    // from masking a traversal sequence that re-forms once they are removed
    // (e.g. "..\0./" -> "../").
    let mut name: String = raw
        .chars()
        .filter(|c| {
            let code = *c as u32;
            code > 0x1F && code != 0x7F
        })
        .collect();

    // Repeatedly strip path-traversal sequences until the string is stable.
    // A single non-overlapping pass can leave a fresh "../" behind once the
    // surrounding characters recombine (e.g. "..../" -> "../"), so loop until
    // no traversal sequence remains.
    loop {
        let stripped = name.replace("../", "").replace("..\\", "");
        if stripped == name {
            break;
        }
        name = stripped;
    }

    // Trim whitespace
    let name = name.trim();

    if name.is_empty() {
        return "download.bin".to_string();
    }

    // Split into stem and extension
    let (stem, ext) = match name.rfind('.') {
        Some(pos) if pos > 0 => (&name[..pos], &name[pos..]),
        _ => (name, ""),
    };

    // Truncate stem to MAX_FILENAME_STEM_LEN characters
    let truncated_stem: String = stem.chars().take(MAX_FILENAME_STEM_LEN).collect();

    if truncated_stem.is_empty() {
        return "download.bin".to_string();
    }

    format!("{}{}", truncated_stem, ext)
}

/// Best-effort filename from a URL (strips query string), then sanitizes it.
pub fn filename_from_url(url: &str) -> String {
    let raw = url
        .rsplit('/')
        .next()
        .map(|s| s.split('?').next().unwrap_or(s))
        .filter(|s| !s.is_empty())
        .unwrap_or("download.bin");

    sanitize_filename(raw)
}

/// Where files are saved. TODO: make this user-configurable via AppSettings.
pub fn downloads_dir() -> PathBuf {
    dirs::download_dir().unwrap_or_else(std::env::temp_dir)
}

/// Compute non-overlapping segment ranges covering [0, total_size).
///
/// Each segment covers a contiguous byte range. The last segment absorbs
/// any remainder so the union is exactly [0, total_size) with no gaps or overlaps.
///
/// # Panics
/// Panics if `total_size == 0` or `num_segments == 0` or `num_segments > 32`.
pub fn compute_segments(total_size: u64, num_segments: u32) -> Vec<SegmentState> {
    assert!(total_size > 0, "total_size must be > 0");
    assert!(
        (1..=32).contains(&num_segments),
        "num_segments must be 1-32"
    );

    let n = num_segments as u64;
    let seg_size = total_size / n;
    let mut segments = Vec::with_capacity(num_segments as usize);

    for i in 0..n {
        let start = i * seg_size;
        let end = if i == n - 1 {
            total_size - 1
        } else {
            (i + 1) * seg_size - 1
        };
        segments.push(SegmentState {
            index: i as u32,
            start,
            end,
            downloaded: 0,
            status: SegmentStatus::Pending,
        });
    }

    segments
}

/// The smallest recorded segment offset (`start + downloaded`) across all
/// segments, or 0 if there are none. Used to detect a truncated partial file
/// on resume (Requirement 2.7).
pub fn min_segment_offset(segments: &[SegmentState]) -> u64 {
    segments
        .iter()
        .map(|s| s.start.saturating_add(s.downloaded))
        .min()
        .unwrap_or(0)
}

/// Decide what the resume logic should do, given the server's current response
/// and the on-disk partial file. Pure function (no I/O) so it is unit-testable.
///
/// Precedence:
/// 1. Content-Length changed (Req 2.6)
/// 2. Range no longer supported (Req 2.5)
/// 3. Partial file missing or too short (Req 2.7)
/// 4. Otherwise resume from saved offsets (Req 2.2)
///
/// `file_len` is `None` when the partial file is missing.
pub fn decide_resume_action(
    original_total: u64,
    current_total: u64,
    supports_range: bool,
    file_len: Option<u64>,
    segments: &[SegmentState],
) -> ResumeAction {
    // Req 2.6: a different Content-Length means the remote file changed.
    if original_total > 0 && current_total > 0 && current_total != original_total {
        return ResumeAction::RestartContentLengthChanged;
    }

    // Req 2.5: the server must still support Range to resume.
    if !supports_range {
        return ResumeAction::RestartRangeUnsupported;
    }

    // Req 2.7: the partial file must exist and be at least as long as the
    // smallest recorded segment offset.
    match file_len {
        None => ResumeAction::RestartPartialFileMissing,
        Some(len) if len < min_segment_offset(segments) => ResumeAction::RestartPartialFileMissing,
        Some(_) => ResumeAction::Resume,
    }
}

/// Build the `PausedState` segment list, synthesizing a single segment for a
/// single-stream download (which has no recorded segments) when the total size
/// is known. Pure helper so it is unit-testable.
pub fn segments_for_pause(
    segments: &[SegmentState],
    total_size: u64,
    downloaded: u64,
) -> Vec<SegmentState> {
    if !segments.is_empty() {
        segments.to_vec()
    } else if total_size > 0 {
        vec![SegmentState {
            index: 0,
            start: 0,
            end: total_size - 1,
            downloaded: downloaded.min(total_size),
            status: SegmentStatus::Paused,
        }]
    } else {
        Vec::new()
    }
}

/// Clamp the reported `downloaded` byte count so it never exceeds a known
/// `total_size`.
///
/// Implements the progress invariant (Requirements 1.5, 12.4): when the total
/// size is known (`total > 0`), the reported `downloaded` is capped at `total`.
/// A `total` of 0 means the size is unknown, in which case the raw count is
/// reported as-is. Pure helper so it is unit-testable.
pub fn clamp_reported_downloaded(downloaded: u64, total: u64) -> u64 {
    if total > 0 {
        downloaded.min(total)
    } else {
        downloaded
    }
}

/// Current wall-clock time in milliseconds since the Unix epoch.
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Emit the current DownloadItem state to the frontend.
async fn emit(app: &AppHandle, downloads: &Downloads, id: &str) {
    if let Some(item) = downloads.lock().await.get(id).cloned() {
        let _ = app.emit("download-progress", item);
    }
}

/// Build a reqwest client that forwards custom headers and cookies from the DownloadItem.
fn build_request(
    client: &reqwest::Client,
    method: reqwest::Method,
    url: &str,
    item: &DownloadItem,
) -> reqwest::RequestBuilder {
    let mut req = client.request(method, url);

    // Forward custom headers
    for (key, value) in &item.headers {
        req = req.header(key.as_str(), value.as_str());
    }

    // Forward cookies
    if let Some(ref cookies) = item.cookies {
        if !cookies.is_empty() {
            req = req.header(reqwest::header::COOKIE, cookies.as_str());
        }
    }

    // Forward referer if present
    if let Some(ref referer) = item.referer {
        if !referer.is_empty() {
            req = req.header(reqwest::header::REFERER, referer.as_str());
        }
    }

    req
}

/// Run a download to completion (or until cancelled via the token).
///
/// Registers the cancellation token in `cancel_tokens` (so [`pause_download`]
/// can find it) and removes it when finished. The actual work is delegated to
/// [`download_core`].
pub async fn run(
    app: AppHandle,
    downloads: Downloads,
    id: String,
    limiter: SpeedLimiter,
    cancel_token: CancellationToken,
    cancel_tokens: CancelTokens,
) -> Result<DownloadOutcome> {
    cancel_tokens
        .lock()
        .await
        .insert(id.clone(), cancel_token.clone());

    let result = download_core(&app, &downloads, &id, &limiter, &cancel_token).await;

    cancel_tokens.lock().await.remove(&id);
    result
}

/// The core download logic: probe the URL, then download segmented (parallel)
/// or single-stream. Does not manage the cancel-token registry — callers do.
async fn download_core(
    app: &AppHandle,
    downloads: &Downloads,
    id: &str,
    limiter: &SpeedLimiter,
    cancel_token: &CancellationToken,
) -> Result<DownloadOutcome> {
    let (url, item_snapshot) = {
        let map = downloads.lock().await;
        let it = map.get(id).ok_or_else(|| anyhow!("unknown download id"))?;
        (it.url.clone(), it.clone())
    };

    let client = reqwest::Client::builder().build()?;

    // --- Probe: total size + range support (10s timeout) ---
    let head_req = build_request(&client, reqwest::Method::HEAD, &url, &item_snapshot)
        .timeout(Duration::from_secs(10));

    let head = head_req.send().await?;
    let total = head.content_length().unwrap_or(0);
    let supports_range = head
        .headers()
        .get(reqwest::header::ACCEPT_RANGES)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.eq_ignore_ascii_case("bytes"))
        .unwrap_or(false);

    let num_segments = item_snapshot.segment_count.clamp(1, 32);

    // Determine if we can use segmented mode
    let use_segments = supports_range && total > MIN_SEGMENTED_SIZE;

    {
        let mut map = downloads.lock().await;
        if let Some(it) = map.get_mut(id) {
            it.total_size = total;
            it.status = DownloadStatus::Downloading;
            it.is_resumable = supports_range;
        }
    }
    emit(app, downloads, id).await;

    let dest = downloads_dir().join(&item_snapshot.filename);

    if use_segments {
        // Compute segments and store them on the item.
        let segments = compute_segments(total, num_segments);
        {
            let mut map = downloads.lock().await;
            if let Some(it) = map.get_mut(id) {
                it.segments = segments.clone();
            }
        }

        // Pre-allocate the destination file to the total expected size.
        let file = tokio::fs::File::create(&dest).await?;
        file.set_len(total).await?;
        drop(file);

        run_segments(
            app,
            downloads,
            id,
            &url,
            &dest,
            &item_snapshot,
            &segments,
            total,
            limiter,
            cancel_token,
        )
        .await
    } else {
        // Fallback: single stream (no parallelism / size may be unknown).
        single_stream(
            app,
            downloads,
            id,
            &url,
            &dest,
            &item_snapshot,
            total,
            limiter,
            cancel_token,
        )
        .await
    }
}

/// Download `[0, total)` as a single sequential stream (fallback path).
#[allow(clippy::too_many_arguments)]
async fn single_stream(
    app: &AppHandle,
    downloads: &Downloads,
    id: &str,
    url: &str,
    dest: &Path,
    item_snapshot: &DownloadItem,
    total: u64,
    limiter: &SpeedLimiter,
    cancel_token: &CancellationToken,
) -> Result<DownloadOutcome> {
    let client = reqwest::Client::builder().build()?;
    let get_req = build_request(&client, reqwest::Method::GET, url, item_snapshot);
    // Req 10.6: bound the wait for response headers (30s per request).
    let resp = tokio::time::timeout(REQUEST_TIMEOUT, get_req.send())
        .await
        .map_err(|_| anyhow!("no response within {}s", REQUEST_TIMEOUT.as_secs()))??;
    let mut file = tokio::fs::File::create(dest).await?;
    let mut stream = resp.bytes_stream();
    let mut downloaded: u64 = 0;

    // Throttle progress events to MAX_PROGRESS_EVENTS_PER_SEC (Req 12.2) and
    // compute speed over a 2s sliding window (Req 12.1) plus ETA (Req 12.5),
    // mirroring the segmented path's reporter.
    let mut last_emit_ms: Option<u64> = None;
    let mut speed_samples: Vec<(u64, u64)> = Vec::new();

    loop {
        tokio::select! {
            _ = cancel_token.cancelled() => {
                tokio::io::AsyncWriteExt::flush(&mut file).await?;
                let mut map = downloads.lock().await;
                if let Some(it) = map.get_mut(id) {
                    it.downloaded = downloaded;
                    it.status = DownloadStatus::Paused;
                    it.speed = 0;
                    it.eta = None;
                }
                drop(map);
                emit(app, downloads, id).await;
                return Ok(DownloadOutcome::Paused);
            }
            // Req 10.6: bound the wait for each chunk (idle timeout).
            chunk = tokio::time::timeout(REQUEST_TIMEOUT, stream.next()) => {
                match chunk {
                    Err(_) => {
                        return Err(anyhow!("no data within {}s", REQUEST_TIMEOUT.as_secs()));
                    }
                    Ok(Some(Ok(chunk))) => {
                        limiter.acquire(chunk.len() as u64).await;
                        tokio::io::AsyncWriteExt::write_all(&mut file, &chunk).await?;
                        downloaded += chunk.len() as u64;

                        let now_ms = now_ms();
                        // Throttle: only emit when the next event is due (Req 12.2).
                        if progress_event_due(last_emit_ms, now_ms, PROGRESS_INTERVAL_MS) {
                            last_emit_ms = Some(now_ms);
                            let clamped = clamp_reported_downloaded(downloaded, total);

                            speed_samples.push((now_ms, clamped));
                            let window_start = now_ms.saturating_sub(SPEED_WINDOW_SECS * 1000);
                            speed_samples.retain(|&(ts, _)| ts >= window_start);
                            let speed = speed_from_samples(&speed_samples);
                            let eta = compute_eta(total, clamped, speed);

                            {
                                let mut map = downloads.lock().await;
                                if let Some(it) = map.get_mut(id) {
                                    it.downloaded = clamped;
                                    it.speed = speed;
                                    it.eta = eta;
                                }
                            }
                            emit(app, downloads, id).await;
                        }
                    }
                    Ok(Some(Err(e))) => return Err(e.into()),
                    Ok(None) => break,
                }
            }
        }
    }

    tokio::io::AsyncWriteExt::flush(&mut file).await?;

    // Verify size when known.
    if total > 0 {
        let meta = tokio::fs::metadata(dest).await?;
        if meta.len() != total {
            mark_size_mismatch(app, downloads, id, total, meta.len()).await;
            return Err(anyhow!("file size mismatch"));
        }
    }

    mark_complete(app, downloads, id, total).await;
    Ok(DownloadOutcome::Completed)
}

/// Spawn one task per incomplete segment plus a progress reporter, wait for
/// them, then finalize (verify size + mark complete, or record paused state).
///
/// Used by both the fresh segmented download and resume, so the resume path
/// produces a byte-for-byte identical file (Requirement 2.4): each segment
/// seeks to `start + downloaded` and writes only its own byte range.
#[allow(clippy::too_many_arguments)]
async fn run_segments(
    app: &AppHandle,
    downloads: &Downloads,
    id: &str,
    url: &str,
    dest: &Path,
    item_snapshot: &DownloadItem,
    segments: &[SegmentState],
    total: u64,
    limiter: &SpeedLimiter,
    cancel_token: &CancellationToken,
) -> Result<DownloadOutcome> {
    let client = reqwest::Client::builder().build()?;

    // One counter per segment, seeded with the bytes already downloaded so the
    // global total stays correct across pauses/resumes.
    let seg_counters: Vec<Arc<AtomicU64>> = segments
        .iter()
        .map(|s| Arc::new(AtomicU64::new(s.downloaded)))
        .collect();

    // Spawn a task for every segment that still has bytes remaining.
    let mut tasks = Vec::new();
    for (pos, seg) in segments.iter().enumerate() {
        let seg_len = seg.end.saturating_sub(seg.start) + 1;
        if seg.downloaded >= seg_len {
            continue; // already complete (resume)
        }
        tasks.push(tokio::spawn(download_segment(
            client.clone(),
            url.to_string(),
            dest.to_path_buf(),
            downloads.clone(),
            id.to_string(),
            seg.index,
            seg.start,
            seg.end,
            seg.downloaded,
            seg_counters[pos].clone(),
            limiter.clone(),
            cancel_token.clone(),
            item_snapshot.clone(),
        )));
    }

    // Progress reporter (max 3/sec): syncs per-segment offsets into the shared
    // map, computes speed over a 2s window, and emits download-progress events.
    let reporter = spawn_progress_reporter(
        app.clone(),
        downloads.clone(),
        id.to_string(),
        segments
            .iter()
            .enumerate()
            .map(|(pos, s)| {
                let seg_len = s.end.saturating_sub(s.start) + 1;
                (s.index, seg_counters[pos].clone(), seg_len)
            })
            .collect(),
        total,
    );

    // Wait for all segment tasks to wind down gracefully. On cancellation each
    // task observes the token, flushes its file, records its final offset into
    // the shared map, and returns — so we never hard-abort mid-write.
    let mut first_err: Option<anyhow::Error> = None;
    for t in tasks {
        match t.await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                if first_err.is_none() {
                    first_err = Some(e);
                }
            }
            Err(e) => {
                if first_err.is_none() {
                    first_err = Some(anyhow!("segment task panicked: {e}"));
                }
            }
        }
    }

    reporter.abort();

    // Cancellation (pause) takes precedence over the synthetic per-segment
    // "cancelled" errors. Segment tasks already wrote their offsets to the map.
    if cancel_token.is_cancelled() {
        let global: u64 = seg_counters.iter().map(|c| c.load(Ordering::Relaxed)).sum();
        let mut map = downloads.lock().await;
        if let Some(it) = map.get_mut(id) {
            it.downloaded = clamp_reported_downloaded(global, total);
            it.status = DownloadStatus::Paused;
            it.speed = 0;
            it.eta = None;
        }
        drop(map);
        emit(app, downloads, id).await;
        return Ok(DownloadOutcome::Paused);
    }

    if let Some(e) = first_err {
        return Err(e);
    }

    // Verify the written file matches the expected size.
    let meta = tokio::fs::metadata(dest).await?;
    if meta.len() != total {
        mark_size_mismatch(app, downloads, id, total, meta.len()).await;
        return Err(anyhow!("file size mismatch"));
    }

    mark_complete(app, downloads, id, total).await;
    Ok(DownloadOutcome::Completed)
}

/// Spawn the periodic progress reporter task. `seg_info` is a list of
/// `(segment_index, counter, segment_len)` tuples used to sync per-segment
/// offsets into the shared download map.
fn spawn_progress_reporter(
    app: AppHandle,
    downloads: Downloads,
    id: String,
    seg_info: Vec<(u32, Arc<AtomicU64>, u64)>,
    total: u64,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        // Sliding window of (timestamp_ms, total_bytes) samples for speed calc.
        let mut speed_samples: Vec<(u64, u64)> = Vec::new();

        loop {
            tokio::time::sleep(Duration::from_millis(PROGRESS_INTERVAL_MS)).await;

            let global: u64 = seg_info
                .iter()
                .map(|(_, c, len)| c.load(Ordering::Relaxed).min(*len))
                .sum();
            let clamped = clamp_reported_downloaded(global, total);

            let now_ms = now_ms();

            speed_samples.push((now_ms, clamped));
            let window_start = now_ms.saturating_sub(SPEED_WINDOW_SECS * 1000);
            speed_samples.retain(|&(ts, _)| ts >= window_start);

            let speed = speed_from_samples(&speed_samples);
            let eta = compute_eta(total, clamped, speed);

            {
                let mut map = downloads.lock().await;
                if let Some(it) = map.get_mut(&id) {
                    it.downloaded = clamped;
                    it.speed = speed;
                    it.eta = eta;
                    for (idx, counter, len) in &seg_info {
                        if let Some(seg) = it.segments.iter_mut().find(|s| s.index == *idx) {
                            seg.downloaded = counter.load(Ordering::Relaxed).min(*len);
                        }
                    }
                }
            }
            emit(&app, &downloads, &id).await;
        }
    })
}

/// Mark a download as errored due to a final size mismatch and emit.
async fn mark_size_mismatch(
    app: &AppHandle,
    downloads: &Downloads,
    id: &str,
    expected: u64,
    actual: u64,
) {
    {
        let mut map = downloads.lock().await;
        if let Some(it) = map.get_mut(id) {
            it.status = DownloadStatus::Error;
            it.error_message = Some(format!(
                "File size mismatch: expected {expected} bytes, got {actual} bytes"
            ));
        }
    }
    emit(app, downloads, id).await;
}

/// Mark a download as complete and emit.
async fn mark_complete(app: &AppHandle, downloads: &Downloads, id: &str, total: u64) {
    {
        let mut map = downloads.lock().await;
        if let Some(it) = map.get_mut(id) {
            it.downloaded = total.max(it.downloaded);
            it.status = DownloadStatus::Complete;
            it.speed = 0;
            it.eta = Some(0);
            for seg in it.segments.iter_mut() {
                seg.status = SegmentStatus::Complete;
            }
            it.completed_at = Some(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs(),
            );
        }
    }
    emit(app, downloads, id).await;
}

/// Pause an active download: cancel its segment tasks, record per-segment byte
/// offsets into a [`PausedState`], set status to `Paused`, and persist.
///
/// Implements Requirement 2.1 (cancel within 2s, record offsets, set "paused")
/// and Requirement 2.3's persistence side (the queue layer releases the permit).
pub async fn pause_download(
    app: &AppHandle,
    downloads: &Downloads,
    id: &str,
    cancel_tokens: &CancelTokens,
    persistence: &PersistenceLayer,
) -> Result<PausedState> {
    // 1. Look up and cancel the token; segment tasks observe this and exit.
    let token = {
        let tokens = cancel_tokens.lock().await;
        tokens
            .get(id)
            .cloned()
            .ok_or_else(|| anyhow!("no active cancellation token for download {id}"))?
    };
    token.cancel();

    // 2. Give the segment tasks a moment to flush their files and write their
    //    final offsets into the shared map (well within the 2s bound of Req 2.1).
    //    Poll until the item is observed as Paused, or the deadline elapses.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        let paused = {
            let map = downloads.lock().await;
            map.get(id)
                .map(|it| it.status == DownloadStatus::Paused)
                .unwrap_or(true)
        };
        if paused || tokio::time::Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // 3. Snapshot the recorded offsets and mark the item paused.
    let paused_state = {
        let mut map = downloads.lock().await;
        let item = map
            .get_mut(id)
            .ok_or_else(|| anyhow!("unknown download id: {id}"))?;

        item.status = DownloadStatus::Paused;
        item.speed = 0;
        item.eta = None;

        let segment_offsets = segments_for_pause(&item.segments, item.total_size, item.downloaded);
        item.segments = segment_offsets.clone();

        PausedState {
            id: id.to_string(),
            downloaded: item.downloaded,
            segment_offsets,
        }
    };

    // 4. Persist the segment offsets and the updated item (Req 2.1 / 5.1).
    persistence
        .save_segment_state(id, &paused_state.segment_offsets)
        .await?;
    {
        let map = downloads.lock().await;
        if let Some(item) = map.get(id) {
            persistence.save_download(item).await?;
        }
    }

    // 5. Drop the now-cancelled token so a future resume installs a fresh one.
    cancel_tokens.lock().await.remove(id);

    // 6. Emit the paused state to the UI.
    emit(app, downloads, id).await;

    Ok(paused_state)
}

/// Resume a paused download from saved segment offsets.
///
/// Validates server state via a HEAD request, then either resumes from the
/// saved offsets or restarts according to [`decide_resume_action`]:
/// - Content-Length changed → discard partial file and restart (Req 2.6)
/// - Range no longer supported → restart single-stream (Req 2.5)
/// - Partial file missing/too short → restart from scratch (Req 2.7)
/// - Otherwise → resume from offsets via Range requests (Req 2.2, 2.4)
#[allow(clippy::too_many_arguments)]
pub async fn resume_download(
    app: AppHandle,
    downloads: Downloads,
    id: String,
    paused_state: PausedState,
    limiter: SpeedLimiter,
    cancel_token: CancellationToken,
    cancel_tokens: CancelTokens,
    persistence: PersistenceLayer,
) -> Result<DownloadOutcome> {
    cancel_tokens
        .lock()
        .await
        .insert(id.clone(), cancel_token.clone());

    let result = resume_core(&app, &downloads, &id, paused_state, &limiter, &cancel_token).await;

    // Persist whatever final state we reached (complete, paused, or unchanged).
    {
        let map = downloads.lock().await;
        if let Some(item) = map.get(&id) {
            let _ = persistence.save_download(item).await;
            let _ = persistence.save_segment_state(&id, &item.segments).await;
        }
    }

    cancel_tokens.lock().await.remove(&id);
    result
}

/// The core resume logic (no token-registry management).
async fn resume_core(
    app: &AppHandle,
    downloads: &Downloads,
    id: &str,
    paused_state: PausedState,
    limiter: &SpeedLimiter,
    cancel_token: &CancellationToken,
) -> Result<DownloadOutcome> {
    let (url, item_snapshot, original_total_size) = {
        let map = downloads.lock().await;
        let it = map
            .get(id)
            .ok_or_else(|| anyhow!("unknown download id: {id}"))?;
        (it.url.clone(), it.clone(), it.total_size)
    };

    let dest = downloads_dir().join(&item_snapshot.filename);

    // If we never learned the size, we cannot verify or seek reliably — just
    // restart from scratch.
    if original_total_size == 0 {
        let _ = tokio::fs::remove_file(&dest).await;
        reset_for_restart(
            app,
            downloads,
            id,
            None,
            true,
            "Restarting download (size unknown)",
        )
        .await;
        return download_core(app, downloads, id, limiter, cancel_token).await;
    }

    let client = reqwest::Client::builder().build()?;

    // --- HEAD probe to validate the server still matches the saved state ---
    let head_req = build_request(&client, reqwest::Method::HEAD, &url, &item_snapshot)
        .timeout(Duration::from_secs(10));
    let head = head_req.send().await?;
    let current_total = head.content_length().unwrap_or(0);
    let supports_range = head
        .headers()
        .get(reqwest::header::ACCEPT_RANGES)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.eq_ignore_ascii_case("bytes"))
        .unwrap_or(false);

    let file_len = tokio::fs::metadata(&dest).await.ok().map(|m| m.len());

    let action = decide_resume_action(
        original_total_size,
        current_total,
        supports_range,
        file_len,
        &paused_state.segment_offsets,
    );

    match action {
        ResumeAction::RestartContentLengthChanged => {
            let _ = tokio::fs::remove_file(&dest).await;
            reset_for_restart(
                app,
                downloads,
                id,
                Some(current_total),
                true,
                "File changed on server, restarting download",
            )
            .await;
            return download_core(app, downloads, id, limiter, cancel_token).await;
        }
        ResumeAction::RestartRangeUnsupported => {
            let _ = tokio::fs::remove_file(&dest).await;
            reset_for_restart(
                app,
                downloads,
                id,
                None,
                false,
                "Server no longer supports Range, restarting download",
            )
            .await;
            return download_core(app, downloads, id, limiter, cancel_token).await;
        }
        ResumeAction::RestartPartialFileMissing => {
            let _ = tokio::fs::remove_file(&dest).await;
            reset_for_restart(
                app,
                downloads,
                id,
                None,
                true,
                "Partial data lost, restarting download",
            )
            .await;
            return download_core(app, downloads, id, limiter, cancel_token).await;
        }
        ResumeAction::Resume => {}
    }

    // --- Resume from saved offsets ---
    let total = original_total_size;
    {
        let mut map = downloads.lock().await;
        if let Some(it) = map.get_mut(id) {
            it.status = DownloadStatus::Downloading;
            it.error_message = None;
            it.is_resumable = true;
            it.segments = paused_state.segment_offsets.clone();
        }
    }
    emit(app, downloads, id).await;

    run_segments(
        app,
        downloads,
        id,
        &url,
        &dest,
        &item_snapshot,
        &paused_state.segment_offsets,
        total,
        limiter,
        cancel_token,
    )
    .await
}

/// Reset a download item's state ahead of a from-scratch restart and emit an
/// informational `download-progress` event (Req 2.5/2.6/2.7).
async fn reset_for_restart(
    app: &AppHandle,
    downloads: &Downloads,
    id: &str,
    new_total: Option<u64>,
    resumable: bool,
    notice: &str,
) {
    {
        let mut map = downloads.lock().await;
        if let Some(it) = map.get_mut(id) {
            if let Some(t) = new_total {
                it.total_size = t;
            }
            it.downloaded = 0;
            it.segments.clear();
            it.speed = 0;
            it.eta = None;
            it.is_resumable = resumable;
            it.status = DownloadStatus::Downloading;
            it.error_message = Some(notice.to_string());
        }
    }
    emit(app, downloads, id).await;
}

/// Outcome of a single segment download attempt (one HTTP request).
enum SegmentAttempt {
    /// The segment's byte range was fully written.
    Completed,
    /// The cancellation token fired mid-transfer (pause).
    Cancelled,
}

/// Classified failure of a single segment download attempt.
enum SegmentError {
    /// Transient failure (connection reset, timeout, DNS, HTTP 5xx) — retry
    /// with backoff is appropriate (Requirement 10.1).
    Retryable(String),
    /// Permanent failure (HTTP 401/403/404/410) — fail immediately (Req 10.2).
    NonRetryable(String),
}

/// Download a single byte range into the correct offset of `dest`, resuming
/// from `already_downloaded` bytes within the segment, with automatic retry on
/// transient failures (Requirement 10.1).
///
/// Retry policy (Requirements 10.1, 10.2, 10.3):
/// - Retryable errors (connection reset, timeout, DNS failure, HTTP 5xx) are
///   retried up to [`MAX_RETRIES`] times with exponential backoff delays of
///   1s, 2s, 4s, 8s, 16s, 30s, each retry resuming from the last successful
///   byte offset.
/// - Non-retryable errors (HTTP 401, 403, 404, 410) fail immediately.
/// - After exhausting retries, returns a descriptive error including the
///   number of attempts made.
///
/// Records the segment's final offset into the shared map on both cancellation
/// (so [`pause_download`] sees an accurate offset) and normal completion.
#[allow(clippy::too_many_arguments)]
async fn download_segment(
    client: reqwest::Client,
    url: String,
    dest: PathBuf,
    downloads: Downloads,
    id: String,
    seg_index: u32,
    start: u64,
    end: u64,
    already_downloaded: u64,
    seg_counter: Arc<AtomicU64>,
    limiter: SpeedLimiter,
    cancel_token: CancellationToken,
    item: DownloadItem,
) -> Result<()> {
    let seg_len = end.saturating_sub(start) + 1;
    if already_downloaded >= seg_len {
        return Ok(()); // nothing left to do
    }

    // `retries` counts retries already performed (0 on the first attempt).
    let mut retries: u32 = 0;

    loop {
        // Honor a pause requested before (or between) attempts.
        if cancel_token.is_cancelled() {
            record_segment_offset(
                &downloads,
                &id,
                seg_index,
                &seg_counter,
                seg_len,
                SegmentStatus::Paused,
            )
            .await;
            return Err(anyhow!("segment cancelled"));
        }

        // Resume from the last successful byte offset (Requirement 10.1).
        let downloaded_so_far = seg_counter.load(Ordering::Relaxed).min(seg_len);
        if downloaded_so_far >= seg_len {
            break; // already complete
        }
        let resume_from = start + downloaded_so_far;

        match download_segment_attempt(
            &client,
            &url,
            &dest,
            &seg_counter,
            seg_index,
            end,
            seg_len,
            resume_from,
            &limiter,
            &cancel_token,
            &item,
        )
        .await
        {
            Ok(SegmentAttempt::Completed) => break,
            Ok(SegmentAttempt::Cancelled) => {
                record_segment_offset(
                    &downloads,
                    &id,
                    seg_index,
                    &seg_counter,
                    seg_len,
                    SegmentStatus::Paused,
                )
                .await;
                return Err(anyhow!("segment cancelled"));
            }
            // Req 10.2: non-retryable errors fail immediately, no retry.
            Err(SegmentError::NonRetryable(msg)) => {
                return Err(anyhow!("segment {seg_index} failed (non-retryable): {msg}"));
            }
            Err(SegmentError::Retryable(msg)) => {
                // Req 10.3: stop after exhausting the retry budget.
                if retries >= MAX_RETRIES {
                    return Err(anyhow!(
                        "segment {seg_index} failed after {MAX_RETRIES} retries: {msg}"
                    ));
                }

                // Req 10.1: exponential backoff (1s, 2s, 4s, 8s, 16s, 30s),
                // interruptible by a pause request.
                let delay = compute_backoff_delay(retries);
                tokio::select! {
                    _ = cancel_token.cancelled() => {
                        record_segment_offset(
                            &downloads,
                            &id,
                            seg_index,
                            &seg_counter,
                            seg_len,
                            SegmentStatus::Paused,
                        )
                        .await;
                        return Err(anyhow!("segment cancelled"));
                    }
                    _ = tokio::time::sleep(delay) => {}
                }
                retries += 1;
            }
        }
    }

    record_segment_offset(
        &downloads,
        &id,
        seg_index,
        &seg_counter,
        seg_len,
        SegmentStatus::Complete,
    )
    .await;
    Ok(())
}

/// Perform a single HTTP request for a segment, streaming bytes into `dest`
/// starting at `resume_from`. Classifies any failure as retryable or
/// non-retryable so the caller can decide whether to back off and retry.
///
/// Enforces a [`REQUEST_TIMEOUT`] (30s) on both the initial response and each
/// subsequent chunk read: if no data arrives within that window, the request
/// is treated as a retryable timeout (Requirement 10.6).
#[allow(clippy::too_many_arguments)]
async fn download_segment_attempt(
    client: &reqwest::Client,
    url: &str,
    dest: &Path,
    seg_counter: &Arc<AtomicU64>,
    seg_index: u32,
    end: u64,
    seg_len: u64,
    resume_from: u64,
    limiter: &SpeedLimiter,
    cancel_token: &CancellationToken,
    item: &DownloadItem,
) -> std::result::Result<SegmentAttempt, SegmentError> {
    let mut req = build_request(client, reqwest::Method::GET, url, item);
    req = req.header(reqwest::header::RANGE, format!("bytes={resume_from}-{end}"));

    // Req 10.6: bound the wait for response headers.
    let resp = match tokio::time::timeout(REQUEST_TIMEOUT, req.send()).await {
        Err(_) => {
            return Err(SegmentError::Retryable(format!(
                "no response within {}s for segment {seg_index}",
                REQUEST_TIMEOUT.as_secs()
            )));
        }
        Ok(Err(e)) => {
            return Err(classify_request_error(e));
        }
        Ok(Ok(resp)) => resp,
    };

    let status = resp.status();
    if is_non_retryable_status(status) {
        return Err(SegmentError::NonRetryable(format!(
            "HTTP {status} for segment {seg_index}"
        )));
    }
    if is_retryable_status(status) {
        return Err(SegmentError::Retryable(format!(
            "HTTP {status} for segment {seg_index}"
        )));
    }

    let mut file = match tokio::fs::OpenOptions::new().write(true).open(dest).await {
        Ok(f) => f,
        Err(e) => return Err(SegmentError::NonRetryable(format!("open dest failed: {e}"))),
    };
    if let Err(e) =
        tokio::io::AsyncSeekExt::seek(&mut file, std::io::SeekFrom::Start(resume_from)).await
    {
        return Err(SegmentError::NonRetryable(format!("seek failed: {e}")));
    }

    let mut stream = resp.bytes_stream();
    loop {
        tokio::select! {
            _ = cancel_token.cancelled() => {
                let _ = tokio::io::AsyncWriteExt::flush(&mut file).await;
                return Ok(SegmentAttempt::Cancelled);
            }
            // Req 10.6: bound the wait for each chunk (idle timeout).
            chunk = tokio::time::timeout(REQUEST_TIMEOUT, stream.next()) => {
                match chunk {
                    Err(_) => {
                        let _ = tokio::io::AsyncWriteExt::flush(&mut file).await;
                        return Err(SegmentError::Retryable(format!(
                            "no data within {}s for segment {seg_index}",
                            REQUEST_TIMEOUT.as_secs()
                        )));
                    }
                    Ok(Some(Ok(chunk))) => {
                        // Don't write past the end of this segment, even if the
                        // server returns extra bytes.
                        let remaining = seg_len.saturating_sub(seg_counter.load(Ordering::Relaxed));
                        if remaining == 0 {
                            break;
                        }
                        let take = (chunk.len() as u64).min(remaining) as usize;
                        limiter.acquire(take as u64).await;
                        if let Err(e) =
                            tokio::io::AsyncWriteExt::write_all(&mut file, &chunk[..take]).await
                        {
                            return Err(SegmentError::NonRetryable(format!("write failed: {e}")));
                        }
                        seg_counter.fetch_add(take as u64, Ordering::Relaxed);
                        if take < chunk.len() {
                            break;
                        }
                    }
                    Ok(Some(Err(e))) => {
                        let _ = tokio::io::AsyncWriteExt::flush(&mut file).await;
                        return Err(classify_request_error(e));
                    }
                    Ok(None) => break,
                }
            }
        }
    }

    if let Err(e) = tokio::io::AsyncWriteExt::flush(&mut file).await {
        return Err(SegmentError::NonRetryable(format!("flush failed: {e}")));
    }

    // A clean stream end before the segment is fully written means the
    // connection dropped early — retry from where we left off.
    if seg_counter.load(Ordering::Relaxed).min(seg_len) < seg_len {
        return Err(SegmentError::Retryable(format!(
            "connection closed before segment {seg_index} completed"
        )));
    }

    Ok(SegmentAttempt::Completed)
}

/// Classify a reqwest error as retryable (transient network failure) or
/// non-retryable, attaching the error's display text for diagnostics.
fn classify_request_error(error: reqwest::Error) -> SegmentError {
    if is_retryable_error(&error) {
        SegmentError::Retryable(error.to_string())
    } else {
        SegmentError::NonRetryable(error.to_string())
    }
}

/// Write a segment's current offset and status into the shared download map.
async fn record_segment_offset(
    downloads: &Downloads,
    id: &str,
    seg_index: u32,
    seg_counter: &Arc<AtomicU64>,
    seg_len: u64,
    status: SegmentStatus,
) {
    let mut map = downloads.lock().await;
    if let Some(it) = map.get_mut(id) {
        if let Some(seg) = it.segments.iter_mut().find(|s| s.index == seg_index) {
            seg.downloaded = seg_counter.load(Ordering::Relaxed).min(seg_len);
            seg.status = status;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn test_filename_from_url_basic() {
        assert_eq!(
            filename_from_url("https://example.com/file.zip"),
            "file.zip"
        );
    }

    #[test]
    fn test_filename_from_url_with_query() {
        assert_eq!(
            filename_from_url("https://example.com/file.zip?token=abc"),
            "file.zip"
        );
    }

    #[test]
    fn test_filename_from_url_empty_path() {
        assert_eq!(filename_from_url("https://example.com/"), "download.bin");
    }

    #[test]
    fn test_compute_segments_single() {
        let segs = compute_segments(100, 1);
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].start, 0);
        assert_eq!(segs[0].end, 99);
    }

    #[test]
    fn test_compute_segments_even_split() {
        let segs = compute_segments(100, 4);
        assert_eq!(segs.len(), 4);
        assert_eq!(segs[0].start, 0);
        assert_eq!(segs[0].end, 24);
        assert_eq!(segs[1].start, 25);
        assert_eq!(segs[1].end, 49);
        assert_eq!(segs[2].start, 50);
        assert_eq!(segs[2].end, 74);
        assert_eq!(segs[3].start, 75);
        assert_eq!(segs[3].end, 99);
    }

    #[test]
    fn test_compute_segments_uneven_split() {
        // 10 bytes / 3 segments = 3, 3, 4 bytes
        let segs = compute_segments(10, 3);
        assert_eq!(segs.len(), 3);
        assert_eq!(segs[0].start, 0);
        assert_eq!(segs[0].end, 2);
        assert_eq!(segs[1].start, 3);
        assert_eq!(segs[1].end, 5);
        assert_eq!(segs[2].start, 6);
        assert_eq!(segs[2].end, 9);
    }

    #[test]
    fn test_compute_segments_full_coverage() {
        // Verify no gaps or overlaps for various sizes
        for size in [1, 2, 7, 100, 1023, 1048576, 10_000_000] {
            for n in 1..=32u32 {
                if n as u64 > size {
                    continue; // skip if more segments than bytes
                }
                let segs = compute_segments(size, n);
                assert_eq!(segs.len(), n as usize);

                // First segment starts at 0
                assert_eq!(segs[0].start, 0);
                // Last segment ends at size-1
                assert_eq!(segs.last().unwrap().end, size - 1);

                // No gaps or overlaps between consecutive segments
                for w in segs.windows(2) {
                    assert_eq!(
                        w[0].end + 1,
                        w[1].start,
                        "gap or overlap at segment boundary"
                    );
                }
            }
        }
    }

    #[test]
    #[should_panic]
    fn test_compute_segments_zero_size_panics() {
        compute_segments(0, 4);
    }

    #[test]
    #[should_panic]
    fn test_compute_segments_zero_segments_panics() {
        compute_segments(100, 0);
    }

    #[test]
    #[should_panic]
    fn test_compute_segments_too_many_segments_panics() {
        compute_segments(100, 33);
    }

    // ── Property 1: Segment coverage is total and non-overlapping ────────────────
    // **Validates: Requirement 1.1**

    proptest! {
        /// Property 1: For any total_size > 0 and num_segments in [1, 32], the
        /// computed segments cover exactly [0, total_size) with no gaps and no
        /// overlaps: the first segment starts at 0, each segment connects to the
        /// next with no gap/overlap, and the last segment ends at total_size - 1.
        #[test]
        fn prop_segment_coverage_total_and_non_overlapping(
            total_size in 1u64..=10_000_000_000u64,
            num_segments in 1u32..=32u32,
        ) {
            let segs = compute_segments(total_size, num_segments);

            // Exactly num_segments produced.
            prop_assert_eq!(segs.len(), num_segments as usize);

            // First segment starts at 0 (no gap at the front).
            prop_assert_eq!(segs[0].start, 0);

            // Last segment ends at total_size - 1 (full coverage to the end).
            prop_assert_eq!(segs.last().unwrap().end, total_size - 1);

            for (i, s) in segs.iter().enumerate() {
                // Each segment is non-empty and well-formed (start <= end).
                prop_assert!(s.start <= s.end, "segment {} has start > end", i);

                // Consecutive segments are contiguous: no gap and no overlap.
                if i > 0 {
                    prop_assert_eq!(
                        segs[i - 1].end + 1,
                        s.start,
                        "gap or overlap at boundary between segment {} and {}",
                        i - 1,
                        i
                    );
                }
            }
        }
    }

    // ── Resume edge-case decision logic (Requirements 2.5, 2.6, 2.7) ─────────────

    fn seg(index: u32, start: u64, end: u64, downloaded: u64) -> SegmentState {
        SegmentState {
            index,
            start,
            end,
            downloaded,
            status: SegmentStatus::Paused,
        }
    }

    #[test]
    fn test_min_segment_offset() {
        let segs = vec![seg(0, 0, 49, 10), seg(1, 50, 99, 30)];
        // offsets are 0+10=10 and 50+30=80 → min 10
        assert_eq!(min_segment_offset(&segs), 10);
        assert_eq!(min_segment_offset(&[]), 0);
    }

    #[test]
    fn test_resume_action_resume_when_state_matches() {
        let segs = vec![seg(0, 0, 49, 10), seg(1, 50, 99, 30)];
        let action = decide_resume_action(100, 100, true, Some(100), &segs);
        assert_eq!(action, ResumeAction::Resume);
    }

    #[test]
    fn test_resume_action_content_length_changed() {
        // Requirement 2.6: different Content-Length → discard + restart.
        let segs = vec![seg(0, 0, 49, 10)];
        let action = decide_resume_action(100, 200, true, Some(100), &segs);
        assert_eq!(action, ResumeAction::RestartContentLengthChanged);
    }

    #[test]
    fn test_resume_action_range_unsupported() {
        // Requirement 2.5: range no longer supported → restart single-stream.
        let segs = vec![seg(0, 0, 99, 50)];
        let action = decide_resume_action(100, 100, false, Some(100), &segs);
        assert_eq!(action, ResumeAction::RestartRangeUnsupported);
    }

    #[test]
    fn test_resume_action_partial_file_missing() {
        // Requirement 2.7: missing partial file → restart.
        let segs = vec![seg(0, 0, 99, 50)];
        let action = decide_resume_action(100, 100, true, None, &segs);
        assert_eq!(action, ResumeAction::RestartPartialFileMissing);
    }

    #[test]
    fn test_resume_action_partial_file_too_short() {
        // Requirement 2.7: file shorter than the smallest recorded offset.
        // Smallest offset = 0+40 = 40; file is only 30 bytes → restart.
        let segs = vec![seg(0, 0, 99, 40)];
        let action = decide_resume_action(100, 100, true, Some(30), &segs);
        assert_eq!(action, ResumeAction::RestartPartialFileMissing);
    }

    #[test]
    fn test_resume_action_content_length_takes_precedence() {
        // Even with no range support and a missing file, a changed
        // Content-Length is reported first.
        let segs = vec![seg(0, 0, 99, 40)];
        let action = decide_resume_action(100, 250, false, None, &segs);
        assert_eq!(action, ResumeAction::RestartContentLengthChanged);
    }

    #[test]
    fn test_resume_action_unknown_current_size_still_resumes() {
        // A current Content-Length of 0 (unknown) must not be treated as a
        // change; if range is supported and the file is intact, resume.
        let segs = vec![seg(0, 0, 99, 40)];
        let action = decide_resume_action(100, 0, true, Some(100), &segs);
        assert_eq!(action, ResumeAction::Resume);
    }

    // ── Single-stream pause segment synthesis ────────────────────────────────────

    #[test]
    fn test_segments_for_pause_preserves_existing() {
        let segs = vec![seg(0, 0, 49, 10), seg(1, 50, 99, 20)];
        let out = segments_for_pause(&segs, 100, 30);
        assert_eq!(out.len(), 2);
        assert_eq!(out[1].downloaded, 20);
    }

    #[test]
    fn test_segments_for_pause_synthesizes_single_stream() {
        // No segments but a known size → synthesize one covering the whole file.
        let out = segments_for_pause(&[], 100, 42);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].start, 0);
        assert_eq!(out[0].end, 99);
        assert_eq!(out[0].downloaded, 42);
    }

    #[test]
    fn test_segments_for_pause_unknown_size_is_empty() {
        // No segments and unknown size → cannot synthesize, so not resumable.
        let out = segments_for_pause(&[], 0, 42);
        assert!(out.is_empty());
    }

    #[test]
    fn test_segments_for_pause_clamps_downloaded() {
        let out = segments_for_pause(&[], 100, 500);
        assert_eq!(out[0].downloaded, 100);
    }

    // ── Retry backoff + error classification (Requirements 10.1, 10.2, 10.6) ─────

    #[test]
    fn test_compute_backoff_delay_sequence() {
        // Req 10.1: delays of 1s, 2s, 4s, 8s, 16s, then capped at 30s.
        assert_eq!(compute_backoff_delay(0), Duration::from_secs(1));
        assert_eq!(compute_backoff_delay(1), Duration::from_secs(2));
        assert_eq!(compute_backoff_delay(2), Duration::from_secs(4));
        assert_eq!(compute_backoff_delay(3), Duration::from_secs(8));
        assert_eq!(compute_backoff_delay(4), Duration::from_secs(16));
        // 2^5 = 32s would exceed the cap, so it clamps to 30s.
        assert_eq!(compute_backoff_delay(5), Duration::from_secs(30));
    }

    #[test]
    fn test_compute_backoff_delay_never_exceeds_cap() {
        // Even for absurdly large attempt counts, the delay stays capped and
        // never panics (saturating arithmetic).
        for attempt in 0..100u32 {
            assert!(compute_backoff_delay(attempt) <= Duration::from_millis(MAX_BACKOFF_MS));
        }
    }

    #[test]
    fn test_is_retryable_status_5xx() {
        // Req 10.1: HTTP 5xx is retryable.
        assert!(is_retryable_status(StatusCode::INTERNAL_SERVER_ERROR));
        assert!(is_retryable_status(StatusCode::BAD_GATEWAY));
        assert!(is_retryable_status(StatusCode::SERVICE_UNAVAILABLE));
        assert!(is_retryable_status(StatusCode::GATEWAY_TIMEOUT));
        // Success and 4xx (other than the non-retryable set) are not 5xx.
        assert!(!is_retryable_status(StatusCode::OK));
        assert!(!is_retryable_status(StatusCode::TOO_MANY_REQUESTS));
    }

    #[test]
    fn test_is_non_retryable_status_set() {
        // Req 10.2: 401, 403, 404, 410 fail immediately.
        assert!(is_non_retryable_status(StatusCode::UNAUTHORIZED));
        assert!(is_non_retryable_status(StatusCode::FORBIDDEN));
        assert!(is_non_retryable_status(StatusCode::NOT_FOUND));
        assert!(is_non_retryable_status(StatusCode::GONE));
        // Others are not in the immediate-fail set.
        assert!(!is_non_retryable_status(StatusCode::INTERNAL_SERVER_ERROR));
        assert!(!is_non_retryable_status(StatusCode::OK));
        assert!(!is_non_retryable_status(StatusCode::BAD_REQUEST));
    }

    #[test]
    fn test_retryable_and_non_retryable_status_are_disjoint() {
        // No status should be classified as both retryable and immediately fatal.
        for code in 100u16..600 {
            if let Ok(status) = StatusCode::from_u16(code) {
                assert!(
                    !(is_retryable_status(status) && is_non_retryable_status(status)),
                    "status {status} classified as both retryable and non-retryable"
                );
            }
        }
    }

    // ── Filename sanitization (Requirement 10.7) ─────────────────────────────────

    #[test]
    fn test_sanitize_filename_strips_path_traversal() {
        // Req 10.7: strip `../` and `..\` traversal sequences. Note that only
        // the traversal sequence is removed; a lone separator is left intact.
        assert_eq!(sanitize_filename("../etc/passwd"), "etc/passwd");
        assert_eq!(
            sanitize_filename("..\\windows\\system32"),
            "windows\\system32"
        );
        assert_eq!(sanitize_filename("../../secret.txt"), "secret.txt");
    }

    #[test]
    fn test_sanitize_filename_removes_null_and_control_chars() {
        // Req 10.7: remove null bytes and control chars (0x00-0x1F, 0x7F).
        assert_eq!(sanitize_filename("file\0name.txt"), "filename.txt");
        assert_eq!(sanitize_filename("a\u{0007}b\u{001F}c.bin"), "abc.bin");
        assert_eq!(sanitize_filename("tab\there.log"), "tabhere.log");
        assert_eq!(sanitize_filename("del\u{007F}ete.dat"), "delete.dat");
    }

    #[test]
    fn test_sanitize_filename_preserves_normal_name() {
        assert_eq!(sanitize_filename("report-2024.pdf"), "report-2024.pdf");
        assert_eq!(sanitize_filename("My File (1).zip"), "My File (1).zip");
    }

    #[test]
    fn test_sanitize_filename_truncates_stem_to_200_chars() {
        // Req 10.7: truncate the stem to 200 chars, excluding the extension.
        let long_stem = "a".repeat(300);
        let raw = format!("{long_stem}.txt");
        let out = sanitize_filename(&raw);
        let (stem, ext) = out.rsplit_once('.').unwrap();
        assert_eq!(stem.chars().count(), MAX_FILENAME_STEM_LEN);
        assert_eq!(ext, "txt");
    }

    #[test]
    fn test_sanitize_filename_no_extension_truncates() {
        let long = "b".repeat(250);
        let out = sanitize_filename(&long);
        assert_eq!(out.chars().count(), MAX_FILENAME_STEM_LEN);
    }

    #[test]
    fn test_sanitize_filename_empty_or_whitespace_falls_back() {
        assert_eq!(sanitize_filename(""), "download.bin");
        assert_eq!(sanitize_filename("   "), "download.bin");
        assert_eq!(sanitize_filename("\0\0"), "download.bin");
    }

    #[test]
    fn test_sanitize_filename_result_has_no_dangerous_chars() {
        // The sanitized output must never contain control chars or traversal.
        let inputs = [
            "../../../etc/shadow\0",
            "weird\u{0001}\u{0002}name..\\x.exe",
            "\u{007F}\u{001F}data.bin",
        ];
        for raw in inputs {
            let out = sanitize_filename(raw);
            assert!(!out.contains("../"), "output retained `../`: {out}");
            assert!(!out.contains("..\\"), "output retained `..\\`: {out}");
            assert!(
                out.chars().all(|c| (c as u32) > 0x1F && (c as u32) != 0x7F),
                "output retained a control char: {out:?}"
            );
        }
    }

    // ── Property 18: Filename sanitization removes dangerous characters ────────────
    // **Validates: Requirement 10.7**

    proptest! {
        /// Property 18: For any input string, the sanitized filename SHALL not
        /// contain path-traversal sequences ("../", "..\\"), null bytes, or
        /// control characters (ASCII 0x00-0x1F, 0x7F); the stem SHALL be at most
        /// 200 chars; and the output SHALL never be empty.
        ///
        /// The generator builds strings from fragments biased toward the
        /// dangerous input space (traversal sequences, dots, separators, null
        /// bytes, and control chars) interleaved with ordinary filename
        /// characters, so reconstructions like "....//" -> "../" are exercised.
        #[test]
        fn prop_sanitize_filename_removes_dangerous_chars(
            raw in prop::collection::vec(
                prop_oneof![
                    Just("../".to_string()),
                    Just("..\\".to_string()),
                    Just("..".to_string()),
                    Just(".".to_string()),
                    Just("/".to_string()),
                    Just("\\".to_string()),
                    Just("\0".to_string()),
                    "[a-zA-Z0-9 ._-]{0,6}",
                    (0u32..0x20u32).prop_map(|c| char::from_u32(c).unwrap().to_string()),
                    Just("\u{007F}".to_string()),
                ],
                0..24,
            )
            .prop_map(|parts| parts.concat()),
        ) {
            let out = sanitize_filename(&raw);

            // Output is never empty (falls back to a default when nothing valid).
            prop_assert!(!out.is_empty(), "empty output for input {:?}", raw);

            // No path-traversal sequences survive.
            prop_assert!(
                !out.contains("../"),
                "output retained `../`: {:?} (input {:?})",
                out,
                raw
            );
            prop_assert!(
                !out.contains("..\\"),
                "output retained `..\\`: {:?} (input {:?})",
                out,
                raw
            );

            // No null bytes or control characters (0x00-0x1F, 0x7F).
            prop_assert!(
                out.chars().all(|c| {
                    let code = c as u32;
                    code > 0x1F && code != 0x7F
                }),
                "output retained a control char: {:?} (input {:?})",
                out,
                raw
            );

            // Stem (portion before the last extension dot) is at most 200 chars.
            let stem = match out.rfind('.') {
                Some(pos) if pos > 0 => &out[..pos],
                _ => &out[..],
            };
            prop_assert!(
                stem.chars().count() <= MAX_FILENAME_STEM_LEN,
                "stem exceeds {} chars: {:?} (input {:?})",
                MAX_FILENAME_STEM_LEN,
                stem,
                raw
            );
        }
    }

    // ── Property 17: Retry backoff is exponential and capped ──────────────────────
    // For any retry attempt number n, the computed delay SHALL equal
    // min(2^n * base_delay, 30_000ms), producing exponential backoff capped at 30s.
    // **Validates: Requirement 10.1**

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(256))]

        /// Property 17: the backoff delay is always capped at MAX_BACKOFF_MS (30s)
        /// for arbitrary attempt counts, never exceeding the cap and never panicking.
        #[test]
        fn prop_backoff_never_exceeds_cap(attempt in any::<u32>()) {
            let delay = compute_backoff_delay(attempt);
            prop_assert!(
                delay <= Duration::from_millis(MAX_BACKOFF_MS),
                "attempt {attempt} produced {delay:?} which exceeds the {MAX_BACKOFF_MS}ms cap"
            );
        }

        /// Property 17: the backoff delay is monotonically non-decreasing as the
        /// attempt number increases (exponential growth until it saturates at the cap).
        #[test]
        fn prop_backoff_is_monotonic_non_decreasing(attempt in 0u32..64) {
            let current = compute_backoff_delay(attempt);
            let next = compute_backoff_delay(attempt + 1);
            prop_assert!(
                next >= current,
                "backoff decreased: attempt {attempt} -> {current:?}, \
                 attempt {} -> {next:?}",
                attempt + 1
            );
        }

        /// Property 17: while below the cap, the delay equals exactly
        /// 2^attempt * base_delay (1000ms); once that value would exceed the cap,
        /// the delay equals the cap.
        #[test]
        fn prop_backoff_matches_formula(attempt in 0u32..32) {
            const BASE_DELAY_MS: u64 = 1000;
            let uncapped = BASE_DELAY_MS.saturating_mul(2u64.saturating_pow(attempt));
            let expected = uncapped.min(MAX_BACKOFF_MS);
            let actual = compute_backoff_delay(attempt);
            prop_assert_eq!(
                actual,
                Duration::from_millis(expected),
                "attempt {} expected {}ms, got {:?}",
                attempt,
                expected,
                actual
            );
            // When the uncapped value fits under the cap, it must match exactly.
            if uncapped <= MAX_BACKOFF_MS {
                prop_assert_eq!(actual, Duration::from_millis(uncapped));
            }
        }
    }

    // ── Download progress invariant (Requirements 1.5, 12.4) ─────────────────────

    #[test]
    fn test_clamp_reported_downloaded_caps_at_total() {
        // When more bytes are reported than the known total, cap at total.
        assert_eq!(clamp_reported_downloaded(150, 100), 100);
    }

    #[test]
    fn test_clamp_reported_downloaded_passes_through_below_total() {
        // At or below the total, the raw count is reported unchanged.
        assert_eq!(clamp_reported_downloaded(40, 100), 40);
        assert_eq!(clamp_reported_downloaded(100, 100), 100);
        assert_eq!(clamp_reported_downloaded(0, 100), 0);
    }

    #[test]
    fn test_clamp_reported_downloaded_unknown_total_passes_through() {
        // total == 0 means the size is unknown, so report the raw count as-is.
        assert_eq!(clamp_reported_downloaded(12_345, 0), 12_345);
        assert_eq!(clamp_reported_downloaded(0, 0), 0);
    }

    // ── Progress reporting helpers (Requirements 12.1, 12.2, 12.5) ───────────────

    #[test]
    fn test_progress_event_due_first_event_always_due() {
        // With no prior event, the first one is always due.
        assert!(progress_event_due(None, 0, PROGRESS_INTERVAL_MS));
        assert!(progress_event_due(None, 12_345, PROGRESS_INTERVAL_MS));
    }

    #[test]
    fn test_progress_event_due_respects_interval() {
        // Req 12.2: an event is due only once the interval has elapsed.
        let interval = PROGRESS_INTERVAL_MS; // 333ms for 3/sec
        assert!(!progress_event_due(
            Some(1_000),
            1_000 + interval - 1,
            interval
        ));
        assert!(progress_event_due(Some(1_000), 1_000 + interval, interval));
        assert!(progress_event_due(
            Some(1_000),
            1_000 + interval + 50,
            interval
        ));
    }

    #[test]
    fn test_speed_from_samples_basic() {
        // Req 12.1: speed = (bytes_delta * 1000) / elapsed_ms.
        // 2000 bytes over 1000ms = 2000 bytes/sec.
        let samples = vec![(0u64, 0u64), (1_000u64, 2_000u64)];
        assert_eq!(speed_from_samples(&samples), 2_000);
    }

    #[test]
    fn test_speed_from_samples_uses_window_endpoints() {
        // Only the oldest and newest samples matter (sliding window).
        let samples = vec![(0u64, 0u64), (500u64, 100u64), (2_000u64, 4_000u64)];
        // 4000 bytes over 2000ms = 2000 bytes/sec.
        assert_eq!(speed_from_samples(&samples), 2_000);
    }

    #[test]
    fn test_speed_from_samples_insufficient_or_zero_elapsed() {
        // Fewer than two samples → 0; zero elapsed time → 0 (no division by zero).
        assert_eq!(speed_from_samples(&[]), 0);
        assert_eq!(speed_from_samples(&[(0, 100)]), 0);
        assert_eq!(speed_from_samples(&[(1_000, 0), (1_000, 500)]), 0);
    }

    #[test]
    fn test_compute_eta_known_total_and_speed() {
        // Req 12.5: ETA = remaining / speed. (1000 - 200) / 100 = 8 seconds.
        assert_eq!(compute_eta(1_000, 200, 100), Some(8));
    }

    #[test]
    fn test_compute_eta_unknown_total_or_zero_speed_is_none() {
        // Unknown size or zero speed → no ETA.
        assert_eq!(compute_eta(0, 200, 100), None);
        assert_eq!(compute_eta(1_000, 200, 0), None);
    }

    proptest! {
        /// Req 12.2: with any positive interval and ordered timestamps, an event
        /// is due iff at least `interval` has elapsed since the last emit. After
        /// firing, an immediate follow-up at the same instant is never due.
        #[test]
        fn prop_progress_event_due_monotone(
            last in 0u64..1_000_000u64,
            delta in 0u64..1_000_000u64,
            interval in 1u64..10_000u64,
        ) {
            let now = last.saturating_add(delta);
            let due = progress_event_due(Some(last), now, interval);
            prop_assert_eq!(due, delta >= interval);
            // Once an event fires, emitting again at the same instant is not due.
            if due {
                prop_assert!(!progress_event_due(Some(now), now, interval));
            }
        }

        /// Req 12.1/12.4: the computed speed, multiplied back out over the elapsed
        /// window, never implies more bytes than were actually transferred, and is
        /// 0 whenever no time elapsed or fewer than two samples exist.
        #[test]
        fn prop_speed_from_samples_non_negative_and_bounded(
            start_bytes in 0u64..1_000_000u64,
            delta_bytes in 0u64..1_000_000u64,
            elapsed in 0u64..10_000u64,
        ) {
            let samples = vec![(1_000u64, start_bytes), (1_000 + elapsed, start_bytes + delta_bytes)];
            let speed = speed_from_samples(&samples);
            if elapsed == 0 {
                prop_assert_eq!(speed, 0);
            } else {
                // speed * elapsed_ms / 1000 should not exceed the bytes delta.
                prop_assert!(speed.saturating_mul(elapsed) / 1000 <= delta_bytes);
            }
        }

        /// Req 12.5: ETA is Some iff total > 0 and speed > 0, and equals
        /// remaining / speed (saturating at 0 remaining).
        #[test]
        fn prop_compute_eta_matches_formula(
            total in 0u64..1_000_000u64,
            downloaded in 0u64..1_000_000u64,
            speed in 0u64..1_000u64,
        ) {
            let eta = compute_eta(total, downloaded, speed);
            if total > 0 && speed > 0 {
                prop_assert_eq!(eta, Some(total.saturating_sub(downloaded) / speed));
            } else {
                prop_assert_eq!(eta, None);
            }
        }

        /// Property 20: Progress event throttling.
        ///
        /// **Validates: Requirement 12.2**
        ///
        /// Feeds an arbitrary stream of monotonically non-decreasing event
        /// timestamps through the `progress_event_due` throttle gate (the same
        /// gating logic the engine uses in its progress loop) and asserts the
        /// emitted stream never exceeds the allowed rate:
        ///   * any two consecutive emitted events are at least `interval` apart,
        ///   * no 1-second (1000ms) window ever contains more than the configured
        ///     maximum (3) emitted events, and
        ///   * the total emitted count is bounded by ceil(span / interval) + 1.
        #[test]
        fn prop_progress_event_throttling_rate_bound(
            // Inter-arrival gaps between candidate events, in ms (0 = same instant).
            gaps in proptest::collection::vec(0u64..1_000u64, 0..200),
            base in 0u64..1_000_000u64,
        ) {
            let interval = PROGRESS_INTERVAL_MS; // 333ms => max 3 events/sec.

            // Build monotonically non-decreasing candidate timestamps.
            let mut now = base;
            let mut candidates = Vec::with_capacity(gaps.len() + 1);
            candidates.push(now);
            for g in &gaps {
                now = now.saturating_add(*g);
                candidates.push(now);
            }

            // Run candidates through the throttle gate exactly as the engine does.
            let mut last_emit: Option<u64> = None;
            let mut emitted: Vec<u64> = Vec::new();
            for &ts in &candidates {
                if progress_event_due(last_emit, ts, interval) {
                    emitted.push(ts);
                    last_emit = Some(ts);
                }
            }

            // (a) Consecutive emitted events are at least `interval` apart.
            for pair in emitted.windows(2) {
                prop_assert!(pair[1] - pair[0] >= interval);
            }

            // (b) No 1-second window contains more than MAX_PROGRESS_EVENTS_PER_SEC
            //     emitted events. For each emitted event, count how many fall within
            //     the following 1000ms (the event itself plus later ones).
            for (i, &start) in emitted.iter().enumerate() {
                let count = emitted[i..]
                    .iter()
                    .take_while(|&&t| t < start + 1000)
                    .count() as u64;
                prop_assert!(
                    count <= MAX_PROGRESS_EVENTS_PER_SEC,
                    "window [{}, {}) had {} emitted events (> {})",
                    start,
                    start + 1000,
                    count,
                    MAX_PROGRESS_EVENTS_PER_SEC
                );
            }

            // (c) Total emitted is bounded by ceil(span / interval) + 1.
            if let (Some(&first), Some(&last)) = (candidates.first(), candidates.last()) {
                let span = last - first;
                let bound = span / interval + 1 + 1; // ceil(span/interval) + 1
                prop_assert!(
                    emitted.len() as u64 <= bound,
                    "emitted {} exceeds bound {} for span {}",
                    emitted.len(),
                    bound,
                    span
                );
            }
        }
    }

    // Property 2: Download progress invariant
    // For any DownloadItem at any point during its lifecycle, the reported
    // `downloaded` bytes SHALL never exceed `total_size`.
    // **Validates: Requirements 1.5, 12.4**
    proptest! {
        #![proptest_config(ProptestConfig::with_cases(256))]

        /// Property 2: with a known total (> 0), the reported `downloaded` is
        /// always ≤ total_size, no matter how large the raw counted value is.
        #[test]
        fn prop_reported_downloaded_never_exceeds_total(
            raw in any::<u64>(),
            total in 1u64..=u64::MAX,
        ) {
            let reported = clamp_reported_downloaded(raw, total);
            prop_assert!(
                reported <= total,
                "reported {reported} exceeds total {total} (raw {raw})"
            );
            // The clamp never fabricates progress beyond what was counted.
            prop_assert!(reported <= raw);
        }

        /// Property 2: when the raw count is within [0, total], it is reported
        /// verbatim (the invariant only clamps the overshoot).
        #[test]
        fn prop_reported_downloaded_preserves_value_within_bounds(
            total in 1u64..=u64::MAX,
            raw in 0u64..=u64::MAX,
        ) {
            prop_assume!(raw <= total);
            prop_assert_eq!(clamp_reported_downloaded(raw, total), raw);
        }

        /// Property 2: an unknown total (== 0) reports the raw count unchanged,
        /// since there is no bound to clamp against.
        #[test]
        fn prop_reported_downloaded_unknown_total_is_identity(raw in any::<u64>()) {
            prop_assert_eq!(clamp_reported_downloaded(raw, 0), raw);
        }
    }

    // ─── Property 11: Captured metadata flows through to HTTP requests ──────────
    //
    // For any capture payload containing cookies and headers, those values SHALL
    // be preserved on the resulting DownloadItem and SHALL be included in every
    // HTTP request made by the Download_Engine for that download.
    // **Validates: Requirements 6.3, 6.4**
    //
    // Two legs, mirroring the requirement's two clauses:
    //   - Req 6.3: capture context maps verbatim onto the DownloadItem
    //     (`build_captured_item`).
    //   - Req 6.4: every request the engine builds for that item carries those
    //     cookies, headers, and the Referer (`build_request`).

    use crate::capture_server::build_captured_item;

    /// A custom header *name* that is a valid HTTP token and never collides with
    /// the dedicated cookie/referer handling (always `x-`-prefixed, lowercase).
    fn header_name_strategy() -> impl Strategy<Value = String> {
        "[a-z]{1,15}".prop_map(|s| format!("x-{s}"))
    }

    /// A non-empty header/cookie *value* drawn from visible ASCII (no control
    /// characters), which `reqwest`'s `HeaderValue` accepts without error.
    fn header_value_strategy() -> impl Strategy<Value = String> {
        proptest::collection::vec(0x21u8..=0x7e, 1..30)
            .prop_map(|bytes| String::from_utf8(bytes).unwrap())
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(256))]

        /// Property 11 (leg 1 — Req 6.3): every captured field is preserved
        /// verbatim on the resulting DownloadItem.
        #[test]
        fn prop_captured_metadata_preserved_on_item(
            cookies in header_value_strategy(),
            referer in header_value_strategy(),
            headers in proptest::collection::hash_map(
                header_name_strategy(),
                header_value_strategy(),
                0..6,
            ),
            filesize in proptest::option::of(any::<u64>()),
            is_media in any::<bool>(),
        ) {
            let item = build_captured_item(
                "id-1".to_string(),
                "https://example.com/file.zip".to_string(),
                "file.zip".to_string(),
                Some(cookies.clone()),
                headers.clone(),
                Some(referer.clone()),
                filesize,
                is_media,
            );

            prop_assert_eq!(item.cookies.as_deref(), Some(cookies.as_str()));
            prop_assert_eq!(item.referer.as_deref(), Some(referer.as_str()));
            prop_assert_eq!(&item.headers, &headers);
            // filesize (when present) becomes total_size; None leaves the default 0.
            prop_assert_eq!(item.total_size, filesize.unwrap_or(0));
            // The media hint is the only thing that switches the download type.
            let expected_type = if is_media {
                crate::models::DownloadType::Media
            } else {
                crate::models::DownloadType::Http
            };
            prop_assert_eq!(item.download_type, expected_type);
        }

        /// Property 11 (leg 2 — Req 6.4): every HTTP request the engine builds
        /// for the captured item carries the cookies (Cookie header), the
        /// custom headers, and the Referer header.
        #[test]
        fn prop_captured_metadata_included_in_http_request(
            cookies in header_value_strategy(),
            referer in header_value_strategy(),
            headers in proptest::collection::hash_map(
                header_name_strategy(),
                header_value_strategy(),
                0..6,
            ),
            // Exercise the request methods the engine actually builds.
            method_idx in 0usize..2,
        ) {
            let item = build_captured_item(
                "id-2".to_string(),
                "https://example.com/file.zip".to_string(),
                "file.zip".to_string(),
                Some(cookies.clone()),
                headers.clone(),
                Some(referer.clone()),
                None,
                false,
            );

            let client = reqwest::Client::new();
            let method = if method_idx == 0 {
                reqwest::Method::GET
            } else {
                reqwest::Method::HEAD
            };
            let req = build_request(&client, method, &item.url, &item)
                .build()
                .expect("request should build with valid captured headers");
            let sent = req.headers();

            // Cookie header carries the captured cookie string.
            prop_assert_eq!(
                sent.get(reqwest::header::COOKIE).and_then(|v| v.to_str().ok()),
                Some(cookies.as_str()),
                "Cookie header missing or altered"
            );
            // Referer header carries the captured referer.
            prop_assert_eq!(
                sent.get(reqwest::header::REFERER).and_then(|v| v.to_str().ok()),
                Some(referer.as_str()),
                "Referer header missing or altered"
            );
            // Every captured custom header is present with its value.
            for (name, value) in &headers {
                prop_assert_eq!(
                    sent.get(name).and_then(|v| v.to_str().ok()),
                    Some(value.as_str()),
                    "custom header {} missing or altered", name
                );
            }
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Task 16.2 — End-to-end integration tests
//
// These exercise the real download engine, auto-categorizer, and persistence
// layer composed together, below the Tauri command boundary. The QueueManager
// scheduler and the completion→categorize event listener both require a
// concrete `tauri::AppHandle` (a real Wry runtime) to emit events, which cannot
// be constructed in a headless test, so those event-emitting seams are verified
// manually (see task 16.1). Everything testable without a Tauri runtime is
// covered here against a local HTTP server that speaks HTTP Range, so the
// genuine segmented-download code path runs end to end.
// ─────────────────────────────────────────────────────────────────────────────
#[cfg(test)]
mod e2e_integration_tests {
    use super::*;

    use std::collections::HashMap;
    use std::sync::Arc;

    use tempfile::TempDir;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::sync::Mutex;

    use crate::capture_server::{build_captured_item, validate_capture_url};
    use crate::categorizer::Categorizer;
    use crate::settings::AppSettings;

    /// A deterministic test payload: `len` bytes where byte `i` is `i % 251`.
    fn make_body(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 251) as u8).collect()
    }

    /// Build an empty shared downloads registry holding a single item.
    async fn registry_with(item: DownloadItem) -> Downloads {
        let downloads: Downloads = Arc::new(Mutex::new(HashMap::new()));
        downloads.lock().await.insert(item.id.clone(), item);
        downloads
    }

    /// Spawn a minimal local HTTP/1.1 server that serves `body` with HTTP Range
    /// support, bound to 127.0.0.1 on an OS-assigned port. Returns the URL the
    /// engine should download from. The server answers `GET`/`HEAD`, honours a
    /// `Range: bytes=START-END` header with a `206 Partial Content` reply, and
    /// advertises `Accept-Ranges: bytes`, so the real downloader's segment
    /// requests work against it without any live network access.
    ///
    /// The response body is written in `chunk_size` pieces with `delay_ms`
    /// between pieces (each flushed), so the download takes a predictable amount
    /// of wall-clock time. This lets a test pause a download deterministically
    /// mid-flight without relying on the speed limiter to slow the transfer.
    async fn spawn_range_server(
        body: Vec<u8>,
        chunk_size: usize,
        delay_ms: u64,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("http://{addr}/file.bin");
        let body = Arc::new(body);

        let handle = tokio::spawn(async move {
            loop {
                let (mut socket, _) = match listener.accept().await {
                    Ok(pair) => pair,
                    Err(_) => break,
                };
                let body = body.clone();
                tokio::spawn(async move {
                    // Read the request head (until the blank line).
                    let mut buf: Vec<u8> = Vec::new();
                    let mut tmp = [0u8; 1024];
                    loop {
                        match socket.read(&mut tmp).await {
                            Ok(0) => return,
                            Ok(n) => {
                                buf.extend_from_slice(&tmp[..n]);
                                if buf.windows(4).any(|w| w == b"\r\n\r\n") || buf.len() > 16_384 {
                                    break;
                                }
                            }
                            Err(_) => return,
                        }
                    }

                    let req = String::from_utf8_lossy(&buf);
                    let is_head = req.starts_with("HEAD");
                    let total = body.len() as u64;

                    // Parse an optional `Range: bytes=START-END` header.
                    let range_line = req
                        .lines()
                        .find(|l| l.to_ascii_lowercase().starts_with("range:"));
                    let (status_line, start, end) = match range_line {
                        Some(line) => {
                            let spec = line.split('=').nth(1).unwrap_or("").trim();
                            let mut parts = spec.split('-');
                            let s: u64 = parts.next().unwrap_or("0").trim().parse().unwrap_or(0);
                            let e: u64 = parts
                                .next()
                                .and_then(|x| x.trim().parse().ok())
                                .unwrap_or(total.saturating_sub(1));
                            (
                                "HTTP/1.1 206 Partial Content",
                                s.min(total.saturating_sub(1)),
                                e.min(total.saturating_sub(1)),
                            )
                        }
                        None => ("HTTP/1.1 200 OK", 0, total.saturating_sub(1)),
                    };

                    let slice = &body[start as usize..=end as usize];
                    let mut head = String::new();
                    head.push_str(status_line);
                    head.push_str("\r\n");
                    head.push_str("Accept-Ranges: bytes\r\n");
                    head.push_str(&format!("Content-Length: {}\r\n", slice.len()));
                    if status_line.contains("206") {
                        head.push_str(&format!("Content-Range: bytes {start}-{end}/{total}\r\n"));
                    }
                    head.push_str("Connection: close\r\n\r\n");

                    if socket.write_all(head.as_bytes()).await.is_err() {
                        return;
                    }
                    if !is_head {
                        let step = chunk_size.max(1);
                        for piece in slice.chunks(step) {
                            if socket.write_all(piece).await.is_err() {
                                return;
                            }
                            let _ = socket.flush().await;
                            if delay_ms > 0 {
                                tokio::time::sleep(std::time::Duration::from_millis(delay_ms))
                                    .await;
                            }
                        }
                    }
                    let _ = socket.flush().await;
                });
            }
        });

        (url, handle)
    }

    /// Download every segment of `item` from `url` into `dest` by driving the
    /// real per-segment engine worker (`download_segment`) in parallel, exactly
    /// as the production segmented path does. `dest` must already be
    /// pre-allocated to `total`. Returns once all segments finish (or errors).
    async fn run_all_segments(
        url: &str,
        dest: &std::path::Path,
        downloads: &Downloads,
        id: &str,
        segments: &[SegmentState],
        item: &DownloadItem,
        limiter: &SpeedLimiter,
    ) -> Result<()> {
        let client = reqwest::Client::new();
        let cancel = CancellationToken::new();
        let mut tasks = Vec::new();
        for seg in segments {
            let counter = Arc::new(AtomicU64::new(seg.downloaded));
            tasks.push(tokio::spawn(download_segment(
                client.clone(),
                url.to_string(),
                dest.to_path_buf(),
                downloads.clone(),
                id.to_string(),
                seg.index,
                seg.start,
                seg.end,
                seg.downloaded,
                counter,
                limiter.clone(),
                cancel.clone(),
                item.clone(),
            )));
        }
        for t in tasks {
            t.await.expect("segment task panicked")?;
        }
        Ok(())
    }

    // ─── Flow 1: capture → queue → download → categorize ─────────────────────────
    //
    // Validates the full pipeline below the Tauri boundary:
    //   1. validate + accept a captured URL (capture_server::validate_capture_url)
    //   2. build the DownloadItem from the capture payload (build_captured_item)
    //   3. download it with the real segmented engine against a local Range
    //      server (downloader::download_segment ×N + compute_segments)
    //   4. auto-categorize the completed file into the right category folder
    //      (Categorizer::categorize + move_to_category)
    //
    // Requirements: 1.1 (segmented parallel download), 3.1 (queued item shape),
    // 7.1 (categorize by extension).
    #[tokio::test]
    async fn e2e_capture_download_categorize_flow() {
        let total: usize = 256 * 1024; // 256 KiB, split into 4 segments
        let body = make_body(total);
        // Serve quickly: large chunks, no inter-chunk delay.
        let (url, _server) = spawn_range_server(body.clone(), 64 * 1024, 0).await;

        // 1. Capture step: the URL must be accepted by the capture validator.
        assert!(
            validate_capture_url(&url).is_ok(),
            "captured URL should be accepted"
        );

        // 2. Build the DownloadItem from the capture payload (cookies/headers/referer
        //    flow through verbatim — Req 6.3). A ".mp4" name → "Videos" category.
        let id = "e2e-capture-1".to_string();
        let mut headers = HashMap::new();
        headers.insert("X-Test".to_string(), "1".to_string());
        let item = build_captured_item(
            id.clone(),
            url.clone(),
            "holiday.mp4".to_string(),
            Some("session=abc123".to_string()),
            headers,
            Some("https://example.com/page".to_string()),
            Some(total as u64),
            false,
        );
        // The capture path produces a queued HTTP download carrying the metadata.
        assert_eq!(item.status, DownloadStatus::Queued);
        assert_eq!(item.cookies.as_deref(), Some("session=abc123"));
        assert_eq!(item.total_size, total as u64);

        // 3. Download step: pre-allocate the file, compute segments, run the engine.
        let tmp = TempDir::new().unwrap();
        let dest = tmp.path().join(&item.filename);
        let file = tokio::fs::File::create(&dest).await.unwrap();
        file.set_len(total as u64).await.unwrap();
        drop(file);

        let segments = compute_segments(total as u64, 4);
        let mut active = item.clone();
        active.segments = segments.clone();
        active.status = DownloadStatus::Downloading;
        let downloads = registry_with(active.clone()).await;

        let limiter = SpeedLimiter::new(0); // unlimited
        run_all_segments(&url, &dest, &downloads, &id, &segments, &active, &limiter)
            .await
            .expect("segmented download should complete");

        // The downloaded file matches the served bytes exactly.
        let downloaded = tokio::fs::read(&dest).await.unwrap();
        assert_eq!(downloaded.len(), total, "downloaded size mismatch");
        assert_eq!(downloaded, body, "downloaded bytes differ from source");

        // 4. Categorize step: the completed .mp4 lands in the Videos folder.
        let organized = tmp.path().join("organized");
        let categorizer =
            Categorizer::new(organized.clone(), AppSettings::default_categories(), true);
        let category = categorizer
            .categorize(&item.filename, None)
            .expect("a category should always be resolved")
            .to_string();
        assert_eq!(category, "Videos");

        let final_path = categorizer
            .move_to_category(&dest, &category)
            .await
            .expect("file should move into its category folder");

        assert_eq!(final_path, organized.join("Videos").join("holiday.mp4"));
        assert!(final_path.exists(), "categorized file should exist");
        assert!(!dest.exists(), "source file should have been moved");
        let moved = tokio::fs::read(&final_path).await.unwrap();
        assert_eq!(moved, body, "categorized file contents changed");
    }

    // ─── Flow 2: pause / resume with persistence ─────────────────────────────────
    //
    // Starts a real download, pauses it mid-flight (the engine records the
    // segment offset — the PausedState), persists that offset via the real
    // PersistenceLayer, reloads it, then resumes from the saved offset to
    // completion against the same server. The resulting file must be
    // byte-for-byte identical to a clean download (Req 2.4).
    //
    // Requirements: 2.1 (pause records offset), 2.2/2.4 (resume from offset,
    // identical output), 5.1/5.2 (persist & reload state).
    #[tokio::test]
    async fn e2e_pause_resume_with_persistence_flow() {
        let total: usize = 100_000;
        let body = make_body(total);
        // Stream slowly (4 KiB pieces, 40ms apart → ~1s total) so we can pause
        // the download deterministically mid-flight.
        let (url, _server) = spawn_range_server(body.clone(), 4096, 40).await;

        let tmp = TempDir::new().unwrap();
        let dest = tmp.path().join("archive.bin");
        let file = tokio::fs::File::create(&dest).await.unwrap();
        file.set_len(total as u64).await.unwrap();
        drop(file);

        // Single segment covering the whole file.
        let id = "e2e-pause-1".to_string();
        let seg = SegmentState {
            index: 0,
            start: 0,
            end: (total - 1) as u64,
            downloaded: 0,
            status: SegmentStatus::Pending,
        };
        let mut item = DownloadItem::new(id.clone(), url.clone(), "archive.bin".to_string());
        item.total_size = total as u64;
        item.is_resumable = true;
        item.segments = vec![seg.clone()];
        item.status = DownloadStatus::Downloading;
        let downloads = registry_with(item.clone()).await;

        // Start the download (the server paces the transfer) so we can pause it
        // mid-flight deterministically.
        let limiter = SpeedLimiter::new(0); // unlimited; the server controls pacing
        let cancel = CancellationToken::new();
        let counter = Arc::new(AtomicU64::new(0));
        let client = reqwest::Client::new();
        let worker = tokio::spawn(download_segment(
            client.clone(),
            url.clone(),
            dest.clone(),
            downloads.clone(),
            id.clone(),
            0,
            0,
            (total - 1) as u64,
            0,
            counter.clone(),
            limiter.clone(),
            cancel.clone(),
            item.clone(),
        ));

        // Let some bytes transfer, then pause (cancel) the segment.
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        cancel.cancel();
        let _ = worker.await; // returns Err("segment cancelled") by design

        // The engine recorded the paused offset into the item's segment state.
        let paused_offset = {
            let map = downloads.lock().await;
            map.get(&id).unwrap().segments[0].downloaded
        };
        assert!(
            paused_offset > 0 && (paused_offset as usize) < total,
            "expected a partial pause offset, got {paused_offset} of {total}"
        );

        // Persist the paused state via the real persistence layer, then reload it.
        let persistence = PersistenceLayer::with_path(tmp.path().join("data")).unwrap();
        let snapshot = {
            let map = downloads.lock().await;
            map.get(&id).unwrap().clone()
        };
        persistence.save_download(&snapshot).await.unwrap();
        persistence
            .save_segment_state(&id, &snapshot.segments)
            .await
            .unwrap();

        let reloaded = persistence.load_segments(&id).await.unwrap();
        assert_eq!(reloaded.len(), 1);
        assert_eq!(
            reloaded[0].downloaded, paused_offset,
            "persisted offset should round-trip"
        );

        // Resume from the reloaded offset (no throttle now) and run to completion.
        let resume_offset = reloaded[0].downloaded;
        let resume_counter = Arc::new(AtomicU64::new(resume_offset));
        let resume_cancel = CancellationToken::new();
        let resume_limiter = SpeedLimiter::new(0);
        // Reflect the reloaded segment offset on the item we resume with.
        {
            let mut map = downloads.lock().await;
            if let Some(it) = map.get_mut(&id) {
                it.segments[0].downloaded = resume_offset;
                it.status = DownloadStatus::Downloading;
            }
        }
        let resume_item = {
            let map = downloads.lock().await;
            map.get(&id).unwrap().clone()
        };
        download_segment(
            client,
            url,
            dest.clone(),
            downloads.clone(),
            id.clone(),
            0,
            0,
            (total - 1) as u64,
            resume_offset,
            resume_counter,
            resume_limiter,
            resume_cancel,
            resume_item,
        )
        .await
        .expect("resume should complete");

        // The resumed file is byte-for-byte identical to a clean download.
        let final_bytes = tokio::fs::read(&dest).await.unwrap();
        assert_eq!(final_bytes.len(), total, "resumed file size mismatch");
        assert_eq!(
            final_bytes, body,
            "resumed file is not byte-for-byte identical to source"
        );
    }
}
