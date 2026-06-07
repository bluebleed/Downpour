//! End-to-end integration tests exercised through the public `downpour_lib`
//! crate API (task 16.2).
//!
//! These cargo-level integration tests live outside the crate, so they compile
//! against the public surface of `downpour_lib`. The engine modules
//! (`downloader`, `persistence`, `queue`, `categorizer`, `speed_limiter`,
//! `settings`, `models`) are exported as `pub mod` for this purpose.
//!
//! ## What is covered directly vs. via underlying logic
//!
//! Several end-to-end entry points (`downloader::run`, `queue::QueueManager`,
//! `capture_server::serve`) require a live `tauri::AppHandle` to emit
//! `download-progress` / `queue-changed` events. A real `AppHandle` cannot be
//! constructed headlessly (it needs the Tauri runtime + a webview/event loop),
//! so those flows are tested through the pure/IO layers they are built on:
//!
//!   * **Capture → queue → download → categorize**
//!       - capture shape: the public `DownloadItem` model (queued, metadata).
//!       - download: a REAL segmented HTTP download against a local axum server
//!         using the engine's own `compute_segments`, with the bytes written to
//!         disk and verified byte-for-byte (Req 1.1).
//!       - categorize: `Categorizer::move_to_category` moves the finished file
//!         into its category subfolder (Req 7.1, 7.3) — no AppHandle needed.
//!   * **Pause/resume with persistence** — a partial segmented download is
//!     persisted via `PersistenceLayer::save_segment_state`, reloaded, the
//!     engine's `decide_resume_action` confirms a clean resume, the remaining
//!     ranges are fetched from the persisted offsets, and the final file is
//!     byte-for-byte identical to an uninterrupted download (Req 2.2, 2.4, 5.1).
//!   * **Concurrent download limit** — the queue's pure scheduling decisions
//!     (`count_active`, `next_queued_id`) enforce that the active count never
//!     exceeds `max_concurrent` and that selection is FIFO (Req 3.1, 3.2), plus
//!     `reorder_vec` for Req 3.3.
//!   * **Settings change propagation** — `AppSettings` validation gates the
//!     values pushed to live components, and `PersistenceLayer` round-trips a
//!     settings change so it survives a restart (Req 11.5, 5.4).
//!
//! The AppHandle-dependent orchestration (semaphore acquisition, event
//! emission) that glues these layers together is covered by the in-crate
//! `#[cfg(test)]` modules in `queue.rs` and `downloader.rs`.

use std::collections::HashMap;
use std::io::{Seek, SeekFrom, Write};
use std::sync::Arc;

use axum::body::Body;
use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::Response;
use axum::routing::get;
use axum::Router;

use downpour_lib::categorizer::Categorizer;
use downpour_lib::downloader::{
    compute_segments, decide_resume_action, min_segment_offset, segments_for_pause, ResumeAction,
};
use downpour_lib::models::{
    DownloadItem, DownloadStatus, DownloadType, SegmentState, SegmentStatus,
};
use downpour_lib::persistence::PersistenceLayer;
use downpour_lib::queue::{build_restore, count_active, next_queued_id, reorder_vec};
use downpour_lib::settings::AppSettings;

// ═══════════════════════════════════════════════════════════════════════════════
// Local HTTP test server (serves byte ranges so the engine logic can run for real)
// ═══════════════════════════════════════════════════════════════════════════════

/// A deterministic byte pattern of length `n` so range fetches are verifiable.
fn make_payload(n: usize) -> Vec<u8> {
    (0..n).map(|i| (i % 251) as u8).collect()
}

/// Parse a single `bytes=start-end` range header against a known total length.
/// Returns inclusive `(start, end)`.
fn parse_range(value: &str, total: u64) -> Option<(u64, u64)> {
    let spec = value.strip_prefix("bytes=")?;
    let (start_s, end_s) = spec.split_once('-')?;
    let start: u64 = start_s.parse().ok()?;
    let end = if end_s.is_empty() {
        total - 1
    } else {
        end_s.parse().ok()?
    };
    if start > end || end >= total {
        return None;
    }
    Some((start, end))
}

/// Handler that serves the payload, honouring `Range` requests with a 206
/// partial-content response and advertising `Accept-Ranges: bytes`.
async fn serve_file(State(data): State<Arc<Vec<u8>>>, headers: HeaderMap) -> Response {
    let total = data.len() as u64;

    if let Some(range) = headers.get(header::RANGE).and_then(|v| v.to_str().ok()) {
        if let Some((start, end)) = parse_range(range, total) {
            let slice = data[start as usize..=end as usize].to_vec();
            let len = slice.len();
            return Response::builder()
                .status(StatusCode::PARTIAL_CONTENT)
                .header(header::ACCEPT_RANGES, "bytes")
                .header(
                    header::CONTENT_RANGE,
                    format!("bytes {start}-{end}/{total}"),
                )
                .header(header::CONTENT_LENGTH, len)
                .body(Body::from(slice))
                .unwrap();
        }
    }

    Response::builder()
        .status(StatusCode::OK)
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::CONTENT_LENGTH, total)
        .body(Body::from(data.as_ref().clone()))
        .unwrap()
}

/// Handler for HEAD requests: returns size + range-support headers with no body.
/// (Axum's automatic HEAD-from-GET handling drops the `Content-Length` header,
/// so the engine-style probe needs an explicit HEAD route.)
async fn head_file(State(data): State<Arc<Vec<u8>>>) -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::CONTENT_LENGTH, data.len() as u64)
        .body(Body::empty())
        .unwrap()
}

/// Start the test server on an ephemeral port and return its `/file.bin` URL.
async fn start_server(data: Vec<u8>) -> String {
    let state = Arc::new(data);
    let app = Router::new()
        .route("/file.bin", get(serve_file).head(head_file))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}/file.bin")
}

/// Probe `url` with a HEAD request, returning `(content_length, supports_range)`.
async fn probe(client: &reqwest::Client, url: &str) -> (u64, bool) {
    let head = client.head(url).send().await.unwrap();
    // Read the Content-Length header directly: reqwest's `content_length()` can
    // be `None` for a HEAD response since there is no body to size.
    let total = head
        .headers()
        .get(reqwest::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        .or_else(|| head.content_length())
        .unwrap_or(0);
    let supports_range = head
        .headers()
        .get(reqwest::header::ACCEPT_RANGES)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.eq_ignore_ascii_case("bytes"))
        .unwrap_or(false);
    (total, supports_range)
}

/// Fetch an inclusive byte range `[start, end]` from `url`.
async fn fetch_range(client: &reqwest::Client, url: &str, start: u64, end: u64) -> Vec<u8> {
    let resp = client
        .get(url)
        .header(reqwest::header::RANGE, format!("bytes={start}-{end}"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        reqwest::StatusCode::PARTIAL_CONTENT,
        "server should answer a Range request with 206"
    );
    resp.bytes().await.unwrap().to_vec()
}

// ═══════════════════════════════════════════════════════════════════════════════
// Flow 1: capture → queue → download → categorize
// ═══════════════════════════════════════════════════════════════════════════════

/// Capture slice: a freshly captured download is a well-formed queued HTTP item
/// that carries the forwarded cookies/headers/referer verbatim.
///
/// Requirements: 1.1, 3.1.
#[test]
fn captured_download_item_is_queued_and_carries_metadata() {
    let mut item = DownloadItem::new(
        "cap-1".into(),
        "https://example.com/holiday.mp4".into(),
        "holiday.mp4".into(),
    );
    item.cookies = Some("session=abc123".into());
    item.referer = Some("https://example.com/page".into());
    let mut headers = HashMap::new();
    headers.insert("X-Test".to_string(), "1".to_string());
    item.headers = headers;
    item.total_size = 256 * 1024;

    assert_eq!(item.status, DownloadStatus::Queued);
    assert_eq!(item.download_type, DownloadType::Http);
    assert_eq!(item.cookies.as_deref(), Some("session=abc123"));
    assert_eq!(item.referer.as_deref(), Some("https://example.com/page"));
    assert_eq!(item.headers.get("X-Test").map(String::as_str), Some("1"));
    assert_eq!(item.downloaded, 0);
}

/// Download slice (REAL): a segmented parallel download against the local server
/// reconstructs the file byte-for-byte, then the categorizer moves it into the
/// matching category subfolder.
///
/// Drives the engine's real `compute_segments` (Req 1.1: total, non-overlapping
/// coverage) over actual HTTP Range requests, writes the segments to disk, and
/// verifies the bytes, then `Categorizer::move_to_category` (Req 7.1, 7.3).
#[tokio::test]
async fn capture_to_download_to_categorize_end_to_end() {
    // 1 MB + change so segmentation produces uneven last-segment remainder.
    let payload = make_payload(1_500_000);
    let url = start_server(payload.clone()).await;
    let client = reqwest::Client::new();

    // Probe like the engine does.
    let (total, supports_range) = probe(&client, &url).await;
    assert_eq!(total, payload.len() as u64);
    assert!(supports_range, "test server must advertise Range support");

    // Engine's own segment planner (8 segments).
    let segments = compute_segments(total, 8);

    // Pre-allocate the destination and fetch each segment into its byte range.
    let tmp = tempfile::tempdir().unwrap();
    let dest = tmp.path().join("holiday.mp4");
    {
        let mut f = std::fs::File::create(&dest).unwrap();
        f.set_len(total).unwrap();
        for seg in &segments {
            let bytes = fetch_range(&client, &url, seg.start, seg.end).await;
            assert_eq!(bytes.len() as u64, seg.end - seg.start + 1);
            f.seek(SeekFrom::Start(seg.start)).unwrap();
            f.write_all(&bytes).unwrap();
        }
        f.flush().unwrap();
    }

    // Verify the reassembled file matches the source byte-for-byte (Req 1.1).
    let written = std::fs::read(&dest).unwrap();
    assert_eq!(written.len(), payload.len());
    assert_eq!(
        written, payload,
        "segmented download must reassemble exactly"
    );

    // Categorize on completion: ".mp4" → Videos, moved into the subfolder.
    let categorizer = Categorizer::new(
        tmp.path().to_path_buf(),
        AppSettings::default_categories(),
        true,
    );
    let category = categorizer.categorize("holiday.mp4", None).unwrap();
    assert_eq!(category, "Videos");

    let moved = categorizer.move_to_category(&dest, category).await.unwrap();
    assert_eq!(moved, tmp.path().join("Videos").join("holiday.mp4"));
    assert!(moved.exists());
    assert!(!dest.exists(), "source file should have been moved");
    // Bytes survive the move intact.
    assert_eq!(std::fs::read(&moved).unwrap(), payload);
}

// ═══════════════════════════════════════════════════════════════════════════════
// Flow 2: pause/resume cycle with persistence
// ═══════════════════════════════════════════════════════════════════════════════

/// A segmented download is interrupted partway, its per-segment offsets are
/// persisted and reloaded, the resume decision confirms a clean resume, and the
/// remaining ranges are fetched from the saved offsets to produce a file
/// byte-for-byte identical to an uninterrupted download.
///
/// Requirements: 2.2, 2.4, 5.1.
#[tokio::test]
async fn pause_resume_with_persistence_reassembles_identically() {
    let payload = make_payload(1_200_000);
    let url = start_server(payload.clone()).await;
    let client = reqwest::Client::new();

    let (total, supports_range) = probe(&client, &url).await;
    assert!(supports_range);

    let segments = compute_segments(total, 4);

    let tmp = tempfile::tempdir().unwrap();
    let dest = tmp.path().join("big.bin");
    {
        let f = std::fs::File::create(&dest).unwrap();
        f.set_len(total).unwrap();
    }

    // ── First pass: download only the first half of each segment, then "pause". ──
    let mut paused_segments: Vec<SegmentState> = Vec::new();
    {
        let mut f = std::fs::OpenOptions::new().write(true).open(&dest).unwrap();
        for seg in &segments {
            let seg_len = seg.end - seg.start + 1;
            let half = seg_len / 2;
            if half > 0 {
                let bytes = fetch_range(&client, &url, seg.start, seg.start + half - 1).await;
                f.seek(SeekFrom::Start(seg.start)).unwrap();
                f.write_all(&bytes).unwrap();
            }
            paused_segments.push(SegmentState {
                index: seg.index,
                start: seg.start,
                end: seg.end,
                downloaded: half,
                status: SegmentStatus::Paused,
            });
        }
        f.flush().unwrap();
    }

    // Sanity: the synthesized pause state matches what the engine would record.
    let total_downloaded: u64 = paused_segments.iter().map(|s| s.downloaded).sum();
    let engine_paused = segments_for_pause(&paused_segments, total, total_downloaded);
    assert_eq!(engine_paused.len(), paused_segments.len());

    // ── Persist the paused segment state, then reload it (survives a restart). ──
    let persistence = PersistenceLayer::with_path(tmp.path().join("store")).unwrap();
    persistence
        .save_segment_state("big-1", &paused_segments)
        .await
        .unwrap();
    let reloaded = persistence.load_segments("big-1").await.unwrap();
    assert_eq!(reloaded.len(), paused_segments.len());
    for (a, b) in reloaded.iter().zip(paused_segments.iter()) {
        assert_eq!(a.start, b.start);
        assert_eq!(a.end, b.end);
        assert_eq!(a.downloaded, b.downloaded);
    }

    // ── Resume decision: server unchanged + valid partial file ⇒ Resume. ──
    let file_len = std::fs::metadata(&dest).unwrap().len();
    assert!(file_len >= min_segment_offset(&reloaded));
    let action = decide_resume_action(total, total, true, Some(file_len), &reloaded);
    assert_eq!(action, ResumeAction::Resume);

    // ── Second pass: fetch the remaining bytes of each segment from saved offsets. ──
    {
        let mut f = std::fs::OpenOptions::new().write(true).open(&dest).unwrap();
        for seg in &reloaded {
            let resume_from = seg.start + seg.downloaded;
            if resume_from <= seg.end {
                let bytes = fetch_range(&client, &url, resume_from, seg.end).await;
                f.seek(SeekFrom::Start(resume_from)).unwrap();
                f.write_all(&bytes).unwrap();
            }
        }
        f.flush().unwrap();
    }

    // ── The resumed file is byte-for-byte identical to the source (Req 2.4). ──
    let written = std::fs::read(&dest).unwrap();
    assert_eq!(written.len(), payload.len());
    assert_eq!(
        written, payload,
        "resumed download must be byte-for-byte identical"
    );
}

/// Persistence restore (Req 5.2): a download that was `Downloading` at exit is
/// restored as `Paused` and is NOT auto-started, while queue order is preserved.
#[test]
fn restore_maps_downloading_to_paused_and_keeps_order() {
    let mk = |id: &str, status: DownloadStatus| {
        let mut it = DownloadItem::new(id.into(), format!("https://e/{id}"), format!("{id}.bin"));
        it.status = status;
        it
    };

    let loaded = vec![
        mk("a", DownloadStatus::Downloading),
        mk("b", DownloadStatus::Queued),
        mk("c", DownloadStatus::Complete),
    ];
    let (restored, order) = build_restore(loaded);

    // The previously active download comes back paused (Req 5.2).
    let a = restored.iter().find(|i| i.id == "a").unwrap();
    assert_eq!(a.status, DownloadStatus::Paused);
    // Other statuses are untouched.
    assert_eq!(
        restored.iter().find(|i| i.id == "b").unwrap().status,
        DownloadStatus::Queued
    );
    // Order is preserved from the persisted sequence (Req 5.3).
    assert_eq!(order, vec!["a", "b", "c"]);
}

// ═══════════════════════════════════════════════════════════════════════════════
// Flow 3: concurrent download limit enforcement (FIFO scheduling decisions)
// ═══════════════════════════════════════════════════════════════════════════════

fn item(id: &str, status: DownloadStatus) -> DownloadItem {
    let mut it = DownloadItem::new(id.into(), format!("https://e/{id}"), format!("{id}.bin"));
    it.status = status;
    it
}

fn map_of(items: Vec<DownloadItem>) -> HashMap<String, DownloadItem> {
    items.into_iter().map(|i| (i.id.clone(), i)).collect()
}

/// Simulate the scheduler's start decisions and assert the active count never
/// exceeds `max_concurrent` and that downloads start in FIFO order (Req 3.1, 3.2).
#[test]
fn concurrency_limit_enforced_with_fifo_scheduling() {
    let max_concurrent = 3usize;
    let order: Vec<String> = (0..7).map(|i| format!("d{i}")).collect();
    let mut map = map_of(
        order
            .iter()
            .map(|id| item(id, DownloadStatus::Queued))
            .collect(),
    );

    // Drive the same decision the scheduler makes: while there is a free slot and
    // a queued item, start it (mark Downloading).
    let mut started_order = Vec::new();
    loop {
        if count_active(&map) >= max_concurrent {
            break;
        }
        match next_queued_id(&order, &map) {
            Some(id) => {
                map.get_mut(&id).unwrap().status = DownloadStatus::Downloading;
                started_order.push(id);
            }
            None => break,
        }
        // Invariant after every start: never exceed the limit (Req 3.1).
        assert!(count_active(&map) <= max_concurrent);
    }

    // Exactly max_concurrent started, and they are the first in FIFO order.
    assert_eq!(count_active(&map), max_concurrent);
    assert_eq!(started_order, vec!["d0", "d1", "d2"]);

    // No further item starts while the limit is saturated.
    assert!(next_queued_id(&order, &map).is_some()); // queued items remain
    assert!(count_active(&map) >= max_concurrent);

    // When one active download completes, the next FIFO item becomes startable.
    map.get_mut("d0").unwrap().status = DownloadStatus::Complete;
    assert!(count_active(&map) < max_concurrent);
    let next = next_queued_id(&order, &map).unwrap();
    assert_eq!(next, "d3", "next started download follows FIFO order");
}

/// Reordering a queued download changes subsequent scheduling selection (Req 3.3).
#[test]
fn reorder_changes_next_scheduled_item() {
    let mut order: Vec<String> = vec!["a".into(), "b".into(), "c".into()];
    let map = map_of(vec![
        item("a", DownloadStatus::Queued),
        item("b", DownloadStatus::Queued),
        item("c", DownloadStatus::Queued),
    ]);

    // Initially "a" is first.
    assert_eq!(next_queued_id(&order, &map).as_deref(), Some("a"));

    // Move "c" to the front; it now schedules first.
    reorder_vec(&mut order, "c", 0);
    assert_eq!(order, vec!["c", "a", "b"]);
    assert_eq!(next_queued_id(&order, &map).as_deref(), Some("c"));
}

// ═══════════════════════════════════════════════════════════════════════════════
// Flow 4: settings change propagation
// ═══════════════════════════════════════════════════════════════════════════════

/// The values that the app pushes to its live components must pass validation
/// first; out-of-range values are rejected and the previous setting is retained.
///
/// Requirements: 11.1, 11.3, 11.5.
#[test]
fn settings_validation_accepts_valid_and_rejects_out_of_range() {
    assert_eq!(AppSettings::validate_max_concurrent(5), Ok(5));
    assert_eq!(AppSettings::validate_segments(8), Ok(8));
    assert_eq!(
        AppSettings::validate_speed_limit_signed(1_048_576),
        Ok(1_048_576)
    );

    assert!(AppSettings::validate_max_concurrent(0).is_err());
    assert!(AppSettings::validate_max_concurrent(11).is_err());
    assert!(AppSettings::validate_segments(0).is_err());
    assert!(AppSettings::validate_segments(33).is_err());
    assert!(AppSettings::validate_speed_limit_signed(-1).is_err());
}

#[test]
fn rejected_settings_update_retains_previous_values() {
    let mut settings = AppSettings {
        max_concurrent: 4,
        speed_limit: 5000,
        ..AppSettings::default()
    };

    assert!(settings.set_max_concurrent(99).is_err());
    assert_eq!(settings.max_concurrent, 4);

    assert!(settings.set_speed_limit_signed(-42).is_err());
    assert_eq!(settings.speed_limit, 5000);

    assert!(settings.set_max_concurrent(7).is_ok());
    assert_eq!(settings.max_concurrent, 7);
}

/// A validated settings change propagates AND survives a restart: it is written
/// by `save_settings` and read back identically by `load_settings` (Req 11.5,
/// 5.4). This is the persistence half of "settings changes apply without restart".
#[tokio::test]
async fn settings_change_persists_and_reloads() {
    let tmp = tempfile::tempdir().unwrap();
    // A real, existing download_dir so it survives the load-time existence check.
    let download_dir = tmp.path().join("downloads");
    std::fs::create_dir_all(&download_dir).unwrap();

    let persistence = PersistenceLayer::with_path(tmp.path().join("store")).unwrap();

    // Start from defaults, then apply a validated change (as update_settings does).
    let mut settings = AppSettings {
        download_dir: download_dir.clone(),
        ..AppSettings::default()
    };
    settings.set_max_concurrent(7).unwrap();
    settings.set_default_segments(16).unwrap();
    settings.set_speed_limit_signed(2_097_152).unwrap();

    persistence.save_settings(&settings).await.unwrap();

    // Simulate an app restart: a fresh layer at the same path reloads the values.
    let reopened = PersistenceLayer::with_path(tmp.path().join("store")).unwrap();
    let loaded = reopened.load_settings().await.unwrap();

    assert_eq!(loaded.max_concurrent, 7);
    assert_eq!(loaded.default_segments, 16);
    assert_eq!(loaded.speed_limit, 2_097_152);
    assert_eq!(loaded.download_dir, download_dir);
}

/// Default settings match the spec (Req 11.7) — the baseline that propagates on
/// first launch.
#[test]
fn default_settings_match_spec() {
    let s = AppSettings::default();
    assert_eq!(s.max_concurrent, 3);
    assert_eq!(s.default_segments, 4);
    assert_eq!(s.speed_limit, 0);
    let names: Vec<&str> = s.categories.iter().map(|r| r.category.as_str()).collect();
    for expected in [
        "Videos",
        "Music",
        "Images",
        "Documents",
        "Archives",
        "Programs",
        "Other",
    ] {
        assert!(
            names.contains(&expected),
            "default categories missing {expected}"
        );
    }
}
