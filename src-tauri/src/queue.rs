//! Download queue manager — controls concurrency, ordering, and lifecycle.
//!
//! The [`QueueManager`] owns the scheduling layer that sits between the capture
//! server / UI commands and the download engine (`downloader.rs`). It enforces a
//! configurable maximum number of concurrent downloads using an [`Arc<Semaphore>`],
//! schedules queued items in FIFO order (honouring manual reordering), and reacts
//! to lifecycle events (pause, resume, complete, error, cancel) by freeing a permit
//! and starting the next queued download.
//!
//! Concurrency is enforced by two cooperating mechanisms:
//! 1. An `Arc<Semaphore>` whose permit count tracks `max_concurrent`. A permit is
//!    acquired before a download starts and released (dropped) when it finishes,
//!    pauses, errors, or is cancelled.
//! 2. An active-count gate (`active < max_concurrent`) checked before each start.
//!    This guarantees Requirement 3.4: when `max_concurrent` is lowered below the
//!    current active count, in-flight downloads are never cancelled and no new
//!    download starts until the active count drops below the new limit — even
//!    though dropped permits from the over-provisioned downloads briefly return to
//!    the semaphore.
//!
//! Pure scheduling logic (next-queued selection, reordering, summary building) is
//! factored into free functions so it can be unit-tested without a Tauri runtime.
#![allow(dead_code)]

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::{anyhow, Result};
use serde::Serialize;
use tauri::{AppHandle, Emitter};
use tokio::sync::{Mutex, Notify, OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;

use crate::downloader;
use crate::media_extractor::{MediaExtractor, MediaProgress};
use crate::models::{
    CancelTokens, DownloadItem, DownloadStatus, DownloadType, Downloads, PausedState, QueueConfig,
};
use crate::persistence::PersistenceLayer;
use crate::speed_limiter::SpeedLimiter;

/// Hard upper bound on concurrent downloads (mirrors the settings validation range).
const MAX_CONCURRENT_LIMIT: usize = 10;

/// How long the scheduler waits between ticks when idle (Req 3.2 liveness guarantee).
const SCHEDULER_TICK: Duration = Duration::from_secs(1);

/// Event emitted to the UI when the disk runs out of space during a download
/// (Req 10.4). The UI surfaces this so the user can free space and resume.
const DISK_FULL_EVENT: &str = "disk-full";

// ─── Queue item summary (queue-changed event payload) ────────────────────────────

/// A compact summary of a queued download, emitted in the `queue-changed` event.
///
/// Contains exactly the fields required by Requirement 12.3: id, filename, status,
/// and position within the ordered queue.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct QueueItemSummary {
    pub id: String,
    pub filename: String,
    pub status: DownloadStatus,
    pub position: usize,
}

/// Payload for the `disk-full` event (Req 10.4). Reports how many active
/// downloads were paused and a human-readable message for the UI.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DiskFullPayload {
    /// Ids of the downloads that were paused in response to the disk-full event.
    pub paused: Vec<String>,
    /// Human-readable message describing the condition.
    pub message: String,
}

// ─── Queue Manager ───────────────────────────────────────────────────────────────

/// The download queue manager. Cheaply cloneable (shares one `Arc`-backed core).
#[derive(Clone)]
pub struct QueueManager {
    inner: Arc<QueueInner>,
}

struct QueueInner {
    /// Tauri handle used to emit `queue-changed` / `download-progress` events.
    app: AppHandle,
    /// Shared registry of all downloads.
    downloads: Downloads,
    /// Per-download cancellation tokens (shared with the engine).
    cancel_tokens: CancelTokens,
    /// Persistence layer for queue/state recovery.
    persistence: PersistenceLayer,
    /// Global speed limiter shared across all active downloads (Req 4 / 10.4).
    limiter: SpeedLimiter,
    /// Media extractor (yt-dlp/ffmpeg) used to run `Media`-type downloads. Shared
    /// with the command layer so it tracks settings changes. `None` when media
    /// support is not configured (e.g. in tests).
    media: Option<Arc<Mutex<MediaExtractor>>>,
    /// Concurrency permits. Permit count tracks `max_concurrent`.
    semaphore: Arc<Semaphore>,
    /// Currently configured maximum concurrent downloads.
    max_concurrent: AtomicUsize,
    /// When set, the scheduler will not start any new downloads (pause_all).
    suspended: AtomicBool,
    /// Live directory where downloaded files are saved. Seeded from
    /// `AppSettings.download_dir`; updated by `set_download_dir` when settings
    /// change so downloads honour the configured location without a restart
    /// (Req 11.5). Reads clone the path and drop the guard immediately, so it is
    /// never held across an `.await`.
    download_dir: RwLock<PathBuf>,
    /// FIFO ordering of download ids; the source of truth for scheduling order.
    order: Mutex<Vec<String>>,
    /// Notifies the scheduler loop that the queue state changed.
    notify: Arc<Notify>,
    /// Cancels the scheduler loop on shutdown.
    scheduler_cancel: CancellationToken,
}

impl QueueManager {
    /// Create a new queue manager from a [`QueueConfig`] and shared state.
    pub fn new(
        app: AppHandle,
        downloads: Downloads,
        cancel_tokens: CancelTokens,
        persistence: PersistenceLayer,
        config: QueueConfig,
    ) -> Self {
        let max = config.max_concurrent.clamp(1, MAX_CONCURRENT_LIMIT);
        let inner = QueueInner {
            app,
            downloads,
            cancel_tokens,
            persistence,
            limiter: SpeedLimiter::new(config.speed_limit_global),
            media: None,
            semaphore: Arc::new(Semaphore::new(max)),
            max_concurrent: AtomicUsize::new(max),
            suspended: AtomicBool::new(false),
            download_dir: RwLock::new(config.download_dir),
            order: Mutex::new(Vec::new()),
            notify: Arc::new(Notify::new()),
            scheduler_cancel: CancellationToken::new(),
        };
        Self {
            inner: Arc::new(inner),
        }
    }

    /// Attach the media extractor used to run `Media`-type downloads. Call once
    /// during app setup (before [`start_scheduler`](Self::start_scheduler)).
    ///
    /// Returns `self` so it can be chained at the construction site.
    pub fn with_media_extractor(mut self, media: Arc<Mutex<MediaExtractor>>) -> Self {
        // The inner core is only shared once the manager is cloned, so at the
        // setup call site the `Arc` is still unique and this assignment succeeds.
        if let Some(inner) = Arc::get_mut(&mut self.inner) {
            inner.media = Some(media);
        }
        self
    }

    /// The global speed limiter, so other components can share the same instance.
    pub fn limiter(&self) -> SpeedLimiter {
        self.inner.limiter.clone()
    }

    /// The directory downloads are currently saved into (the live value of
    /// `AppSettings.download_dir`).
    pub fn download_dir(&self) -> PathBuf {
        self.inner
            .download_dir
            .read()
            .expect("download_dir lock poisoned")
            .clone()
    }

    /// Update the directory new and resumed downloads are saved into, applying a
    /// settings change without an app restart (Req 11.5). In-flight downloads
    /// keep writing to their original destination until they finish.
    pub fn set_download_dir(&self, dir: PathBuf) {
        *self
            .inner
            .download_dir
            .write()
            .expect("download_dir lock poisoned") = dir;
    }

    // ─── Public queue operations ─────────────────────────────────────────────────

    /// Add a download to the end of the queue.
    ///
    /// The item is inserted into the shared registry, appended to the FIFO order,
    /// persisted, and the scheduler is notified (it may start the download
    /// immediately if a permit is available). Returns the item's id.
    pub async fn enqueue(&self, mut item: DownloadItem) -> Result<String> {
        let id = item.id.clone();
        item.status = DownloadStatus::Queued;

        {
            let mut map = self.inner.downloads.lock().await;
            if map.contains_key(&id) {
                return Err(anyhow!("download id already exists: {id}"));
            }
            map.insert(id.clone(), item.clone());
        }
        {
            let mut order = self.inner.order.lock().await;
            if !order.contains(&id) {
                order.push(id.clone());
            }
        }

        self.inner.persistence.save_download(&item).await.ok();
        // Enqueuing a new download is an explicit "start working" signal — lift any
        // scheduler suspension left over from startup restore or a previous
        // pause_all so the new download actually starts (it would otherwise sit in
        // `Queued` forever). Restored *paused* downloads stay paused: the scheduler
        // only starts `Queued` items, so this never auto-resumes interrupted work.
        self.inner.suspended.store(false, Ordering::SeqCst);
        self.emit_queue_changed().await;
        self.inner.notify.notify_one();
        Ok(id)
    }

    /// Pause a single download, releasing its concurrency permit (Req 2.3).
    ///
    /// If the download is actively running it has a cancellation token and is
    /// paused through the engine (segment tasks cancelled, offsets recorded). If it
    /// is merely `Queued` (captured/added but not yet started by the scheduler)
    /// there is no token to cancel, so it is simply marked `Paused` directly —
    /// pausing a not-yet-started download must not error.
    pub async fn pause(&self, id: &str) -> Result<()> {
        let is_active = self.inner.cancel_tokens.lock().await.contains_key(id);
        if is_active {
            downloader::pause_download(
                &self.inner.app,
                &self.inner.downloads,
                id,
                &self.inner.cancel_tokens,
                &self.inner.persistence,
            )
            .await?;
        } else {
            let snapshot = {
                let mut map = self.inner.downloads.lock().await;
                let item = map
                    .get_mut(id)
                    .ok_or_else(|| anyhow!("unknown download id: {id}"))?;
                // Only pause items that are pending/active; leave terminal states
                // (complete) untouched.
                if matches!(
                    item.status,
                    DownloadStatus::Queued | DownloadStatus::Downloading
                ) {
                    item.status = DownloadStatus::Paused;
                    item.speed = 0;
                    item.eta = None;
                }
                item.clone()
            };
            self.inner.persistence.save_download(&snapshot).await.ok();
        }
        self.emit_queue_changed().await;
        Ok(())
    }

    /// Resume a single download by re-queuing it for the scheduler.
    ///
    /// `Paused` items are re-queued and resumed from their saved segment offsets;
    /// `Error` items are re-queued for a fresh retry (the UI's "Retry" action
    /// routes here). The scheduler picks the item up subject to the concurrency
    /// limit.
    pub async fn resume(&self, id: &str) -> Result<()> {
        {
            let mut map = self.inner.downloads.lock().await;
            let item = map
                .get_mut(id)
                .ok_or_else(|| anyhow!("unknown download id: {id}"))?;
            if matches!(item.status, DownloadStatus::Paused | DownloadStatus::Error) {
                item.status = DownloadStatus::Queued;
                item.error_message = None;
            }
        }
        // Resuming is an explicit "go" signal: lift any scheduler suspension left
        // over from startup restore or pause_all so this download actually starts.
        self.inner.suspended.store(false, Ordering::SeqCst);
        self.persist(id).await;
        self.emit_queue_changed().await;
        self.inner.notify.notify_one();
        Ok(())
    }

    /// Cancel a download: abort it if active, discard its partial file, and remove
    /// it from the queue entirely.
    pub async fn cancel(&self, id: &str) -> Result<()> {
        self.stop_if_active(id).await;

        // Discard the partial file on disk (cancel = discard incomplete download).
        if let Some(filename) = self.filename_of(id).await {
            let dest = self.download_dir().join(&filename);
            let _ = tokio::fs::remove_file(&dest).await;
        }

        self.remove_record(id).await?;
        self.emit_queue_changed().await;
        self.inner.notify.notify_one();
        Ok(())
    }

    /// Remove a download from the queue without deleting its destination file.
    ///
    /// If the download is active it is first aborted (its permit is released by the
    /// running task wrapper). A completed file is left untouched on disk.
    pub async fn remove(&self, id: &str) -> Result<()> {
        self.stop_if_active(id).await;
        self.remove_record(id).await?;
        self.emit_queue_changed().await;
        self.inner.notify.notify_one();
        Ok(())
    }

    /// Move a download to a new position in the queue. Only affects scheduling
    /// order for items that are still `Queued` (Req 3.3).
    pub async fn reorder(&self, id: &str, position: usize) -> Result<()> {
        {
            let mut order = self.inner.order.lock().await;
            if !order.iter().any(|x| x == id) {
                return Err(anyhow!("unknown download id: {id}"));
            }
            reorder_vec(&mut order, id, position);
        }
        self.emit_queue_changed().await;
        self.inner.notify.notify_one();
        Ok(())
    }

    /// Pause all active downloads and prevent any new ones from starting (Req 3.5).
    ///
    /// Active downloads are paused (their `PausedState` recorded); queued downloads
    /// are set to `Paused`. The `suspended` flag stops the scheduler from starting
    /// anything until [`resume_all`](Self::resume_all) is called.
    pub async fn pause_all(&self) -> Result<()> {
        self.inner.suspended.store(true, Ordering::SeqCst);

        // Snapshot ids by current status so we don't hold the lock across awaits.
        let (active, queued): (Vec<String>, Vec<String>) = {
            let map = self.inner.downloads.lock().await;
            let active = map
                .values()
                .filter(|i| i.status == DownloadStatus::Downloading)
                .map(|i| i.id.clone())
                .collect();
            let queued = map
                .values()
                .filter(|i| i.status == DownloadStatus::Queued)
                .map(|i| i.id.clone())
                .collect();
            (active, queued)
        };

        // Pause each active download (records offsets, releases its permit).
        for id in &active {
            let _ = downloader::pause_download(
                &self.inner.app,
                &self.inner.downloads,
                id,
                &self.inner.cancel_tokens,
                &self.inner.persistence,
            )
            .await;
        }

        // Set queued downloads to paused so they don't auto-start.
        {
            let mut map = self.inner.downloads.lock().await;
            for id in &queued {
                if let Some(item) = map.get_mut(id) {
                    item.status = DownloadStatus::Paused;
                }
            }
        }
        for id in &queued {
            self.persist(id).await;
        }

        self.emit_queue_changed().await;
        Ok(())
    }

    /// Transition all paused downloads back to `Queued` and resume scheduling,
    /// starting up to `max_concurrent` downloads in queue order (Req 3.6).
    pub async fn resume_all(&self) -> Result<()> {
        self.inner.suspended.store(false, Ordering::SeqCst);

        let paused: Vec<String> = {
            let mut map = self.inner.downloads.lock().await;
            let ids: Vec<String> = map
                .values()
                .filter(|i| i.status == DownloadStatus::Paused)
                .map(|i| i.id.clone())
                .collect();
            for id in &ids {
                if let Some(item) = map.get_mut(id) {
                    item.status = DownloadStatus::Queued;
                }
            }
            ids
        };
        for id in &paused {
            self.persist(id).await;
        }

        self.emit_queue_changed().await;
        self.inner.notify.notify_one();
        Ok(())
    }

    /// Adjust the maximum concurrent downloads, resizing the semaphore dynamically
    /// (Req 3.4). Increasing adds permits and immediately starts queued downloads;
    /// decreasing forgets available permits and lets active downloads finish.
    pub async fn set_max_concurrent(&self, n: usize) {
        let new = n.clamp(1, MAX_CONCURRENT_LIMIT);
        let old = self.inner.max_concurrent.swap(new, Ordering::SeqCst);

        if new > old {
            self.inner.semaphore.add_permits(new - old);
        } else if new < old {
            // Removes up to (old - new) *available* permits. Permits held by
            // active downloads are unaffected; the active-count gate prevents
            // over-scheduling once they return.
            self.inner.semaphore.forget_permits(old - new);
        }

        // Increasing the limit should wake the scheduler to fill the new slots.
        self.inner.notify.notify_one();
    }

    /// Set the global speed limit (bytes/sec, 0 = unlimited). Applied to the shared
    /// limiter so all active downloads coordinate immediately (Req 4.3).
    pub fn set_speed_limit(&self, bytes_per_sec: u64) {
        self.inner.limiter.set_rate(bytes_per_sec);
    }

    /// The currently configured maximum concurrent downloads.
    pub fn max_concurrent(&self) -> usize {
        self.inner.max_concurrent.load(Ordering::SeqCst)
    }

    /// Snapshot of all downloads in queue order.
    pub async fn get_queue_state(&self) -> Vec<DownloadItem> {
        let map = self.inner.downloads.lock().await;
        let order = self.inner.order.lock().await;
        order.iter().filter_map(|id| map.get(id).cloned()).collect()
    }

    // ─── Persistence recovery (Req 5.1, 5.2, 5.3) ────────────────────────────────

    /// Restore the queue from disk on app start (Req 5.2, 5.3).
    ///
    /// Loads every persisted download, maps any item that was `Downloading` at
    /// exit to `Paused` (it was interrupted and must not auto-start), rebuilds the
    /// FIFO order from the persisted sequence, and populates the in-memory
    /// registry. Restored downloads are **not** auto-started: paused items stay
    /// paused until the user explicitly resumes them, and the suspended flag is
    /// engaged so the scheduler does not pick up any stray `Queued` item before
    /// the user acts.
    ///
    /// Returns the number of downloads restored.
    pub async fn restore_from_disk(&self) -> Result<usize> {
        let loaded = self.inner.persistence.load_all_downloads().await?;
        let (restored, order) = build_restore(loaded);
        let count = restored.len();

        {
            let mut map = self.inner.downloads.lock().await;
            for item in restored {
                map.insert(item.id.clone(), item);
            }
        }
        {
            let mut existing = self.inner.order.lock().await;
            *existing = order;
        }

        // Do not auto-start restored downloads. Suspend the scheduler so any
        // `Queued` item that slipped through stays put until the user resumes.
        self.inner.suspended.store(true, Ordering::SeqCst);

        self.emit_queue_changed().await;
        Ok(count)
    }

    /// React to the disk filling up during a download (Req 10.4).
    ///
    /// Pauses every active download (recording their `PausedState`), prevents new
    /// downloads from starting, and emits a `disk-full` error event to the UI so
    /// the user can free space and resume. Returns the ids that were paused.
    pub async fn handle_disk_full(&self, message: impl Into<String>) -> Result<Vec<String>> {
        // Reuse pause_all: it suspends the scheduler, pauses active downloads
        // (recording offsets), and sets queued items to paused.
        let active: Vec<String> = {
            let map = self.inner.downloads.lock().await;
            active_ids(&map)
        };

        self.pause_all().await?;

        let payload = DiskFullPayload {
            paused: active.clone(),
            message: message.into(),
        };
        let _ = self.inner.app.emit(DISK_FULL_EVENT, payload);

        Ok(active)
    }

    // ─── Scheduler ───────────────────────────────────────────────────────────────

    /// Start the background scheduler loop. Call once during app setup.
    ///
    /// The loop reacts to queue-change notifications or a 1s tick, then starts as
    /// many queued downloads as permits and the concurrency limit allow.
    pub fn start_scheduler(&self) {
        let this = self.clone();
        tauri::async_runtime::spawn(async move {
            loop {
                tokio::select! {
                    _ = this.inner.notify.notified() => {}
                    _ = tokio::time::sleep(SCHEDULER_TICK) => {}
                    _ = this.inner.scheduler_cancel.cancelled() => break,
                }

                if this.inner.suspended.load(Ordering::SeqCst) {
                    continue;
                }

                // Start as many downloads as currently possible.
                while this.try_start_one().await {}
            }
        });
    }

    /// Stop the scheduler loop.
    pub fn shutdown(&self) {
        self.inner.scheduler_cancel.cancel();
    }

    /// Attempt to start the next queued download. Returns `true` if one started.
    async fn try_start_one(&self) -> bool {
        if self.inner.suspended.load(Ordering::SeqCst) {
            return false;
        }
        let max = self.inner.max_concurrent.load(Ordering::SeqCst);

        // Choose the next queued id under lock, then release locks before awaiting.
        let chosen = {
            let map = self.inner.downloads.lock().await;
            let order = self.inner.order.lock().await;
            if count_active(&map) >= max {
                None
            } else {
                next_queued_id(&order, &map)
            }
        };
        let id = match chosen {
            Some(id) => id,
            None => return false,
        };

        // Acquire a concurrency permit (held for the lifetime of the download).
        let permit = match self.inner.semaphore.clone().try_acquire_owned() {
            Ok(p) => p,
            Err(_) => return false,
        };

        // Mark the item as downloading immediately so it isn't picked twice.
        {
            let mut map = self.inner.downloads.lock().await;
            match map.get_mut(&id) {
                Some(item) if item.status == DownloadStatus::Queued => {
                    item.status = DownloadStatus::Downloading;
                }
                // Status changed out from under us — abandon this start.
                _ => return false,
            }
        }

        self.spawn_running(id, permit);
        // Req 12.3: the "started" transition (Queued → Downloading) must emit a
        // queue-changed event so the UI reflects the new active download.
        self.emit_queue_changed().await;
        true
    }

    /// Spawn the task that runs (or resumes) a download and releases its permit.
    fn spawn_running(&self, id: String, permit: OwnedSemaphorePermit) {
        let this = self.clone();
        tauri::async_runtime::spawn(async move {
            this.run_download(id, permit).await;
        });
    }

    /// Run or resume a single download, then release the permit and reschedule.
    ///
    /// The permit is held for the entire download and dropped at the end of this
    /// function — covering completion, pause, error, and cancellation — which
    /// frees a slot for the next queued item (Req 3.2, 10.5).
    async fn run_download(&self, id: String, permit: OwnedSemaphorePermit) {
        let inner = &self.inner;

        // Media downloads are dispatched to the yt-dlp extractor rather than the
        // HTTP engine, but still hold the concurrency permit for their lifetime.
        let is_media = {
            let map = inner.downloads.lock().await;
            map.get(&id).map(is_media_download).unwrap_or(false)
        };
        if is_media {
            self.run_media_download(&id).await;
            // Release the permit (slot freed) and reschedule the next item.
            drop(permit);
            self.emit_queue_changed().await;
            inner.notify.notify_one();
            return;
        }

        // Decide fresh-start vs resume-from-offsets based on recorded progress.
        let resume_state = {
            let map = inner.downloads.lock().await;
            map.get(&id).and_then(paused_state_for_resume)
        };

        let dest_dir = self.download_dir();
        let token = CancellationToken::new();
        let outcome = match resume_state {
            Some(state) => {
                downloader::resume_download(
                    inner.app.clone(),
                    inner.downloads.clone(),
                    id.clone(),
                    state,
                    inner.limiter.clone(),
                    token,
                    inner.cancel_tokens.clone(),
                    inner.persistence.clone(),
                    dest_dir,
                )
                .await
            }
            None => {
                downloader::run(
                    inner.app.clone(),
                    inner.downloads.clone(),
                    id.clone(),
                    inner.limiter.clone(),
                    token,
                    inner.cancel_tokens.clone(),
                    dest_dir,
                )
                .await
            }
        };

        if let Err(e) = outcome {
            // Mark errored and emit, mirroring the engine's progress contract.
            let mut map = inner.downloads.lock().await;
            if let Some(item) = map.get_mut(&id) {
                item.status = DownloadStatus::Error;
                item.error_message = Some(format!("{e:?}"));
                item.speed = 0;
                item.eta = None;
                let snapshot = item.clone();
                drop(map);
                let _ = inner.app.emit("download-progress", snapshot);
            }
        }

        // Persist the final state.
        self.persist(&id).await;

        // Release the permit (slot freed) and reschedule the next queued item.
        drop(permit);
        self.emit_queue_changed().await;
        inner.notify.notify_one();
    }

    /// Run a `Media`-type download via the yt-dlp extractor, forwarding throttled
    /// progress to the UI and honouring cancellation through the shared
    /// `cancel_tokens` registry (so [`cancel`](Self::cancel) / pause work). The
    /// concurrency permit is held by the caller for the whole call, so media
    /// downloads count against `max_concurrent` like HTTP downloads.
    async fn run_media_download(&self, id: &str) {
        let inner = &self.inner;

        // Media support must be configured; otherwise the item cannot run.
        let extractor = match &inner.media {
            Some(media) => media.lock().await.clone(),
            None => {
                self.mark_media_error(id, "media downloads are not configured")
                    .await;
                return;
            }
        };

        // Pull the URL, yt-dlp output template, and chosen format from the item.
        // The template (not the display filename) drives yt-dlp's `-o` naming.
        let (url, template, format_id) = {
            let map = inner.downloads.lock().await;
            match map.get(id) {
                Some(it) => (
                    it.url.clone(),
                    it.output_template
                        .clone()
                        .unwrap_or_else(|| it.filename.clone()),
                    it.media_format_id.clone(),
                ),
                None => return,
            }
        };
        let format_id = match format_id {
            Some(f) => f,
            None => {
                self.mark_media_error(id, "media download is missing a selected format")
                    .await;
                return;
            }
        };
        let output_path = self.download_dir().join(&template);

        // Register a cancellation token so cancel/pause can stop this download.
        let cancel = CancellationToken::new();
        inner
            .cancel_tokens
            .lock()
            .await
            .insert(id.to_string(), cancel.clone());

        // Forward throttled progress updates to the UI (percentage model: the
        // item's total_size is 100, downloaded tracks percent).
        let (tx, mut rx) = tokio::sync::mpsc::channel::<MediaProgress>(16);
        let app_progress = inner.app.clone();
        let downloads_progress = inner.downloads.clone();
        let id_progress = id.to_string();
        let progress_task = tauri::async_runtime::spawn(async move {
            while let Some(progress) = rx.recv().await {
                let mut map = downloads_progress.lock().await;
                if let Some(it) = map.get_mut(&id_progress) {
                    it.downloaded = progress.percent.round().clamp(0.0, 100.0) as u64;
                    it.speed = progress.speed_bps.unwrap_or(0);
                    it.eta = progress.eta_secs;
                    let snapshot = it.clone();
                    drop(map);
                    let _ = app_progress.emit("download-progress", snapshot);
                }
            }
        });

        let outcome = extractor
            .download(&url, &format_id, &output_path, tx, cancel)
            .await;

        // Stop the progress forwarder and drop the cancel token.
        let _ = progress_task.await;
        inner.cancel_tokens.lock().await.remove(id);

        // On success, resolve the real file path and its size on disk *before*
        // taking the registry lock (metadata is async). yt-dlp names the file from
        // a template, so the path it reported is the source of truth for the card
        // name and for Open/Reveal/Delete.
        let completed = match &outcome {
            Ok(final_path) => {
                let path = final_path.clone().unwrap_or_else(|| output_path.clone());
                let size = tokio::fs::metadata(&path).await.map(|m| m.len()).ok();
                Some((path, size))
            }
            Err(_) => None,
        };

        // Record the terminal state, mirroring the engine's progress contract.
        {
            let mut map = inner.downloads.lock().await;
            if let Some(it) = map.get_mut(id) {
                match completed {
                    Some((path, size)) => {
                        it.status = DownloadStatus::Complete;
                        it.speed = 0;
                        it.eta = Some(0);
                        it.completed_at = Some(
                            std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .unwrap_or_default()
                                .as_secs(),
                        );
                        if let Some(name) = path.file_name() {
                            it.filename = name.to_string_lossy().into_owned();
                        }
                        it.output_path = Some(path);
                        // Replace the percentage model (total_size == 100) with the
                        // real byte size so the card shows the actual file size.
                        match size {
                            Some(bytes) if bytes > 0 => {
                                it.total_size = bytes;
                                it.downloaded = bytes;
                            }
                            _ => it.downloaded = it.total_size,
                        }
                    }
                    None => {
                        let err = match &outcome {
                            Err(e) => format!("{e:#}"),
                            Ok(_) => String::new(),
                        };
                        it.status = DownloadStatus::Error;
                        it.error_message = Some(err);
                        it.speed = 0;
                        it.eta = None;
                    }
                }
                let snapshot = it.clone();
                drop(map);
                let _ = inner.app.emit("download-progress", snapshot);
            }
        }

        self.persist(id).await;
    }

    /// Mark a media download as errored with `message` and emit the update.
    async fn mark_media_error(&self, id: &str, message: &str) {
        let inner = &self.inner;
        let mut map = inner.downloads.lock().await;
        if let Some(it) = map.get_mut(id) {
            it.status = DownloadStatus::Error;
            it.error_message = Some(message.to_string());
            it.speed = 0;
            it.eta = None;
            let snapshot = it.clone();
            drop(map);
            let _ = inner.app.emit("download-progress", snapshot);
        }
    }

    // ─── Internal helpers ──────────────────────────────────────────────────────

    /// Cancel an active download's token (if any) and wait briefly for it to wind
    /// down so its permit is released before we mutate the registry.
    async fn stop_if_active(&self, id: &str) {
        let token = {
            let tokens = self.inner.cancel_tokens.lock().await;
            tokens.get(id).cloned()
        };
        if let Some(token) = token {
            token.cancel();
            // Poll until the engine has stopped touching this download (status no
            // longer Downloading) or a short deadline elapses.
            let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
            loop {
                let still_active = {
                    let map = self.inner.downloads.lock().await;
                    map.get(id)
                        .map(|i| i.status == DownloadStatus::Downloading)
                        .unwrap_or(false)
                };
                if !still_active || tokio::time::Instant::now() >= deadline {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    }

    /// Remove a download from the registry, the order list, and persistence.
    async fn remove_record(&self, id: &str) -> Result<()> {
        {
            let mut map = self.inner.downloads.lock().await;
            map.remove(id);
        }
        {
            let mut order = self.inner.order.lock().await;
            order.retain(|x| x != id);
        }
        self.inner.persistence.delete_download(id).await.ok();
        Ok(())
    }

    /// The filename of a download, if it exists in the registry.
    async fn filename_of(&self, id: &str) -> Option<String> {
        self.inner
            .downloads
            .lock()
            .await
            .get(id)
            .map(|i| i.filename.clone())
    }

    /// Persist the current state of a single download (best-effort).
    async fn persist(&self, id: &str) {
        let snapshot = {
            let map = self.inner.downloads.lock().await;
            map.get(id).cloned()
        };
        if let Some(item) = snapshot {
            self.inner.persistence.save_download(&item).await.ok();
        }
    }

    /// Build and emit the `queue-changed` event (Req 12.3).
    async fn emit_queue_changed(&self) {
        let summaries = {
            let map = self.inner.downloads.lock().await;
            let order = self.inner.order.lock().await;
            build_summaries(&order, &map)
        };
        let _ = self.inner.app.emit("queue-changed", summaries);
    }

    /// Replace the FIFO order (used by persistence recovery in later tasks).
    pub async fn set_order(&self, ids: Vec<String>) {
        let mut order = self.inner.order.lock().await;
        *order = ids;
    }
}

// ─── Pure scheduling helpers (unit-tested) ───────────────────────────────────────

/// Count downloads currently in the `Downloading` state.
pub fn count_active(map: &HashMap<String, DownloadItem>) -> usize {
    map.values()
        .filter(|i| i.status == DownloadStatus::Downloading)
        .count()
}

/// Collect the ids of all downloads currently in the `Downloading` state.
fn active_ids(map: &HashMap<String, DownloadItem>) -> Vec<String> {
    map.values()
        .filter(|i| i.status == DownloadStatus::Downloading)
        .map(|i| i.id.clone())
        .collect()
}

/// Prepare persisted downloads for restoration (Req 5.2, 5.3). Pure so it can be
/// unit-tested without a Tauri runtime or disk I/O.
///
/// - Any item still marked `Downloading` (i.e. it was active when the app exited)
///   is mapped to `Paused` so it does not auto-start and the user can resume it.
/// - The FIFO order is taken from the persisted sequence as-is, preserving the
///   previously saved queue position. Terminal items (`Complete`) keep their
///   place so history still renders in order.
///
/// Returns the transformed items alongside the rebuilt order vector.
pub fn build_restore(loaded: Vec<DownloadItem>) -> (Vec<DownloadItem>, Vec<String>) {
    let restored: Vec<DownloadItem> = loaded
        .into_iter()
        .map(|mut item| {
            if item.status == DownloadStatus::Downloading {
                item.status = DownloadStatus::Paused;
            }
            item
        })
        .collect();
    let order = restored.iter().map(|i| i.id.clone()).collect();
    (restored, order)
}

/// Whether a download should be dispatched to the media (yt-dlp) path rather
/// than the HTTP engine. Pure so the scheduler's branch is unit-testable.
pub fn is_media_download(item: &DownloadItem) -> bool {
    item.download_type == DownloadType::Media
}

/// The id of the first `Queued` download in FIFO `order`, or `None` if there is no
/// queued download ready to start.
pub fn next_queued_id(order: &[String], map: &HashMap<String, DownloadItem>) -> Option<String> {
    order
        .iter()
        .find(|id| {
            map.get(*id)
                .map(|i| i.status == DownloadStatus::Queued)
                .unwrap_or(false)
        })
        .cloned()
}

/// Move `id` to `position` within `order` (clamped to the valid range). No-op if
/// `id` is not present.
pub fn reorder_vec(order: &mut Vec<String>, id: &str, position: usize) {
    if let Some(cur) = order.iter().position(|x| x == id) {
        let item = order.remove(cur);
        let pos = position.min(order.len());
        order.insert(pos, item);
    }
}

/// Build a [`PausedState`] for an item that has recoverable progress, so the
/// scheduler resumes it from saved offsets instead of restarting. Returns `None`
/// for fresh downloads (no recorded progress), which start from scratch.
fn paused_state_for_resume(item: &DownloadItem) -> Option<PausedState> {
    if item.downloaded > 0 && !item.segments.is_empty() {
        Some(PausedState {
            id: item.id.clone(),
            downloaded: item.downloaded,
            segment_offsets: item.segments.clone(),
        })
    } else {
        None
    }
}

/// Build the ordered list of [`QueueItemSummary`] for the `queue-changed` event.
fn build_summaries(order: &[String], map: &HashMap<String, DownloadItem>) -> Vec<QueueItemSummary> {
    order
        .iter()
        .filter_map(|id| map.get(id))
        .enumerate()
        .map(|(position, item)| QueueItemSummary {
            id: item.id.clone(),
            filename: item.filename.clone(),
            status: item.status.clone(),
            position,
        })
        .collect()
}

// ─── Tests ───────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{DownloadStatus, SegmentState, SegmentStatus};

    fn item(id: &str, status: DownloadStatus) -> DownloadItem {
        let mut it = DownloadItem::new(id.into(), format!("https://e/{id}"), format!("{id}.bin"));
        it.status = status;
        it
    }

    fn map_of(items: Vec<DownloadItem>) -> HashMap<String, DownloadItem> {
        items.into_iter().map(|i| (i.id.clone(), i)).collect()
    }

    // ─── count_active ───────────────────────────────────────────────────────────

    #[test]
    fn count_active_counts_only_downloading() {
        let map = map_of(vec![
            item("a", DownloadStatus::Downloading),
            item("b", DownloadStatus::Queued),
            item("c", DownloadStatus::Downloading),
            item("d", DownloadStatus::Paused),
            item("e", DownloadStatus::Complete),
        ]);
        assert_eq!(count_active(&map), 2);
    }

    #[test]
    fn count_active_empty_is_zero() {
        assert_eq!(count_active(&HashMap::new()), 0);
    }

    // ─── next_queued_id ───────────────────────────────────────────────────────────

    #[test]
    fn next_queued_returns_first_queued_in_order() {
        let order = vec!["a".into(), "b".into(), "c".into()];
        let map = map_of(vec![
            item("a", DownloadStatus::Complete),
            item("b", DownloadStatus::Queued),
            item("c", DownloadStatus::Queued),
        ]);
        assert_eq!(next_queued_id(&order, &map), Some("b".to_string()));
    }

    #[test]
    fn next_queued_respects_order_not_insertion() {
        // Order puts "c" before "b"; the first *queued* in order wins.
        let order = vec!["a".into(), "c".into(), "b".into()];
        let map = map_of(vec![
            item("a", DownloadStatus::Downloading),
            item("b", DownloadStatus::Queued),
            item("c", DownloadStatus::Queued),
        ]);
        assert_eq!(next_queued_id(&order, &map), Some("c".to_string()));
    }

    #[test]
    fn next_queued_none_when_nothing_queued() {
        let order = vec!["a".into(), "b".into()];
        let map = map_of(vec![
            item("a", DownloadStatus::Downloading),
            item("b", DownloadStatus::Paused),
        ]);
        assert_eq!(next_queued_id(&order, &map), None);
    }

    #[test]
    fn next_queued_skips_ids_missing_from_map() {
        let order = vec!["ghost".into(), "b".into()];
        let map = map_of(vec![item("b", DownloadStatus::Queued)]);
        assert_eq!(next_queued_id(&order, &map), Some("b".to_string()));
    }

    // ─── reorder_vec ───────────────────────────────────────────────────────────────

    #[test]
    fn reorder_moves_to_front() {
        let mut order: Vec<String> = vec!["a".into(), "b".into(), "c".into()];
        reorder_vec(&mut order, "c", 0);
        assert_eq!(order, vec!["c", "a", "b"]);
    }

    #[test]
    fn reorder_moves_to_middle() {
        let mut order: Vec<String> = vec!["a".into(), "b".into(), "c".into(), "d".into()];
        reorder_vec(&mut order, "a", 2);
        assert_eq!(order, vec!["b", "c", "a", "d"]);
    }

    #[test]
    fn reorder_position_beyond_end_clamps_to_back() {
        let mut order: Vec<String> = vec!["a".into(), "b".into(), "c".into()];
        reorder_vec(&mut order, "a", 99);
        assert_eq!(order, vec!["b", "c", "a"]);
    }

    #[test]
    fn reorder_unknown_id_is_noop() {
        let mut order: Vec<String> = vec!["a".into(), "b".into()];
        reorder_vec(&mut order, "z", 0);
        assert_eq!(order, vec!["a", "b"]);
    }

    // ─── paused_state_for_resume ─────────────────────────────────────────────────

    #[test]
    fn fresh_download_has_no_resume_state() {
        let it = item("a", DownloadStatus::Queued);
        assert!(paused_state_for_resume(&it).is_none());
    }

    #[test]
    fn download_with_progress_yields_resume_state() {
        let mut it = item("a", DownloadStatus::Queued);
        it.downloaded = 500;
        it.segments = vec![SegmentState {
            index: 0,
            start: 0,
            end: 999,
            downloaded: 500,
            status: SegmentStatus::Paused,
        }];
        let state = paused_state_for_resume(&it).expect("expected resume state");
        assert_eq!(state.id, "a");
        assert_eq!(state.downloaded, 500);
        assert_eq!(state.segment_offsets.len(), 1);
    }

    #[test]
    fn progress_without_segments_has_no_resume_state() {
        let mut it = item("a", DownloadStatus::Queued);
        it.downloaded = 500; // single-stream progress but no recorded segments
        assert!(paused_state_for_resume(&it).is_none());
    }

    // ─── build_summaries ───────────────────────────────────────────────────────────

    #[test]
    fn summaries_follow_order_with_positions() {
        let order = vec!["b".into(), "a".into()];
        let map = map_of(vec![
            item("a", DownloadStatus::Queued),
            item("b", DownloadStatus::Downloading),
        ]);
        let summaries = build_summaries(&order, &map);
        assert_eq!(summaries.len(), 2);
        assert_eq!(summaries[0].id, "b");
        assert_eq!(summaries[0].position, 0);
        assert_eq!(summaries[0].status, DownloadStatus::Downloading);
        assert_eq!(summaries[1].id, "a");
        assert_eq!(summaries[1].position, 1);
    }

    #[test]
    fn summaries_skip_ids_missing_from_map() {
        let order = vec!["a".into(), "ghost".into(), "b".into()];
        let map = map_of(vec![
            item("a", DownloadStatus::Queued),
            item("b", DownloadStatus::Queued),
        ]);
        let summaries = build_summaries(&order, &map);
        // "ghost" is skipped; positions are re-numbered over present items.
        assert_eq!(summaries.len(), 2);
        assert_eq!(summaries[0].id, "a");
        assert_eq!(summaries[0].position, 0);
        assert_eq!(summaries[1].id, "b");
        assert_eq!(summaries[1].position, 1);
    }

    // ─── active_ids ──────────────────────────────────────────────────────────────

    #[test]
    fn active_ids_collects_only_downloading() {
        let map = map_of(vec![
            item("a", DownloadStatus::Downloading),
            item("b", DownloadStatus::Queued),
            item("c", DownloadStatus::Downloading),
            item("d", DownloadStatus::Paused),
        ]);
        let mut ids = active_ids(&map);
        ids.sort();
        assert_eq!(ids, vec!["a".to_string(), "c".to_string()]);
    }

    #[test]
    fn active_ids_empty_when_none_downloading() {
        let map = map_of(vec![
            item("a", DownloadStatus::Paused),
            item("b", DownloadStatus::Complete),
        ]);
        assert!(active_ids(&map).is_empty());
    }

    // ─── is_media_download (scheduler dispatch branch) ───────────────────────────

    #[test]
    fn media_item_dispatches_to_media_path() {
        let mut it = item("m", DownloadStatus::Queued);
        it.download_type = DownloadType::Media;
        it.media_format_id = Some("137+140".into());
        assert!(is_media_download(&it));
    }

    #[test]
    fn http_item_does_not_dispatch_to_media_path() {
        let it = item("h", DownloadStatus::Queued);
        assert_eq!(it.download_type, DownloadType::Http);
        assert!(!is_media_download(&it));
    }

    #[test]
    fn media_item_is_scheduled_in_fifo_like_any_other() {
        // A Media item is selected by the scheduler exactly like an HTTP item;
        // the media/HTTP distinction only affects how run_download dispatches it,
        // not whether/when it is picked (so it still counts against max_concurrent).
        let order = vec!["a".into(), "m".into(), "c".into()];
        let mut media = item("m", DownloadStatus::Queued);
        media.download_type = DownloadType::Media;
        let map = map_of(vec![
            item("a", DownloadStatus::Complete),
            media,
            item("c", DownloadStatus::Queued),
        ]);
        assert_eq!(next_queued_id(&order, &map), Some("m".to_string()));
    }

    // ─── build_restore (Req 5.2, 5.3) ────────────────────────────────────────────

    #[test]
    fn restore_maps_downloading_to_paused() {
        let loaded = vec![
            item("a", DownloadStatus::Downloading),
            item("b", DownloadStatus::Queued),
        ];
        let (restored, _order) = build_restore(loaded);
        let by_id: HashMap<_, _> = restored.iter().map(|i| (i.id.clone(), i)).collect();
        assert_eq!(by_id["a"].status, DownloadStatus::Paused);
        // Queued items are left untouched (they don't auto-start; scheduler is suspended).
        assert_eq!(by_id["b"].status, DownloadStatus::Queued);
    }

    #[test]
    fn restore_preserves_persisted_order() {
        let loaded = vec![
            item("c", DownloadStatus::Paused),
            item("a", DownloadStatus::Queued),
            item("b", DownloadStatus::Downloading),
        ];
        let (_restored, order) = build_restore(loaded);
        assert_eq!(order, vec!["c", "a", "b"]);
    }

    #[test]
    fn restore_does_not_change_terminal_states() {
        let loaded = vec![
            item("a", DownloadStatus::Complete),
            item("b", DownloadStatus::Error),
            item("c", DownloadStatus::Paused),
        ];
        let (restored, _order) = build_restore(loaded);
        let by_id: HashMap<_, _> = restored.iter().map(|i| (i.id.clone(), i)).collect();
        assert_eq!(by_id["a"].status, DownloadStatus::Complete);
        assert_eq!(by_id["b"].status, DownloadStatus::Error);
        assert_eq!(by_id["c"].status, DownloadStatus::Paused);
    }

    #[test]
    fn restore_empty_yields_empty() {
        let (restored, order) = build_restore(vec![]);
        assert!(restored.is_empty());
        assert!(order.is_empty());
    }

    #[test]
    fn restore_preserves_segment_offsets_for_paused_items() {
        // An item that was downloading keeps its recorded progress so it can be
        // resumed (not restarted) once the user resumes it.
        let mut it = item("a", DownloadStatus::Downloading);
        it.downloaded = 4096;
        it.segments = vec![SegmentState {
            index: 0,
            start: 0,
            end: 8191,
            downloaded: 4096,
            status: SegmentStatus::Downloading,
        }];
        let (restored, _order) = build_restore(vec![it]);
        assert_eq!(restored[0].status, DownloadStatus::Paused);
        assert_eq!(restored[0].downloaded, 4096);
        assert_eq!(restored[0].segments.len(), 1);
        assert_eq!(restored[0].segments[0].downloaded, 4096);
    }

    // ─── Property 5: Queue scheduling respects FIFO order ───────────────────────────

    use proptest::prelude::*;

    /// One of the six download lifecycle statuses, generated arbitrarily so the
    /// property covers every mix of queued / non-queued items.
    fn any_status() -> impl Strategy<Value = DownloadStatus> {
        prop_oneof![
            Just(DownloadStatus::Queued),
            Just(DownloadStatus::Downloading),
            Just(DownloadStatus::Paused),
            Just(DownloadStatus::Complete),
            Just(DownloadStatus::Error),
            Just(DownloadStatus::Merging),
        ]
    }

    /// Independent reference implementation of FIFO next-queued selection: the
    /// first id in `order` that is present in `map` with status `Queued`.
    fn expected_next_queued(
        order: &[String],
        map: &HashMap<String, DownloadItem>,
    ) -> Option<String> {
        for id in order {
            if let Some(it) = map.get(id) {
                if it.status == DownloadStatus::Queued {
                    return Some(id.clone());
                }
            }
        }
        None
    }

    proptest! {
        /// Property 5: Queue scheduling respects FIFO order.
        ///
        /// For an arbitrary FIFO `order` and an arbitrary map of download states,
        /// `next_queued_id` always returns the earliest-in-order id whose status is
        /// `Queued` (skipping non-queued items and ids absent from the map), and
        /// returns `None` exactly when no queued item is reachable. This mirrors the
        /// scheduler starting downloads in insertion order as permits free up.
        ///
        /// **Validates: Requirement 3.2**
        #[test]
        fn prop_next_queued_is_first_queued_in_order(
            // A pool of statuses; index i becomes download id "d{i}".
            statuses in prop::collection::vec(any_status(), 0..12),
            // How the ids are arranged in the FIFO order list (a shuffled subset,
            // possibly with a few "ghost" ids missing from the map).
            order_seed in prop::collection::vec(0usize..16, 0..16),
        ) {
            // Build the download map: id "d{i}" -> item with the generated status.
            let map: HashMap<String, DownloadItem> = statuses
                .iter()
                .enumerate()
                .map(|(i, s)| {
                    let id = format!("d{i}");
                    (id.clone(), item(&id, s.clone()))
                })
                .collect();

            // Build the FIFO order from the seed, de-duplicating while preserving
            // first-seen position. Indices >= statuses.len() become ghost ids that
            // are absent from the map (exercising the skip-missing branch).
            let mut order: Vec<String> = Vec::new();
            for idx in &order_seed {
                let id = format!("d{idx}");
                if !order.contains(&id) {
                    order.push(id);
                }
            }

            let actual = next_queued_id(&order, &map);
            let expected = expected_next_queued(&order, &map);
            prop_assert_eq!(&actual, &expected);

            // Cross-check the structural guarantees of the returned value.
            match actual {
                Some(id) => {
                    // The chosen id is present, queued, and the earliest such id.
                    prop_assert_eq!(
                        map.get(&id).map(|i| i.status.clone()),
                        Some(DownloadStatus::Queued)
                    );
                    let chosen_pos = order.iter().position(|x| *x == id).unwrap();
                    for earlier in &order[..chosen_pos] {
                        let earlier_queued = map
                            .get(earlier)
                            .map(|i| i.status == DownloadStatus::Queued)
                            .unwrap_or(false);
                        prop_assert!(
                            !earlier_queued,
                            "id {} precedes chosen {} yet is queued",
                            earlier,
                            id
                        );
                    }
                }
                None => {
                    // None is returned only when no reachable item is queued.
                    let any_queued = order.iter().any(|id| {
                        map.get(id)
                            .map(|i| i.status == DownloadStatus::Queued)
                            .unwrap_or(false)
                    });
                    prop_assert!(!any_queued, "returned None but a queued item exists");
                }
            }
        }
    }

    // ─── Property 4: Queue concurrency invariant ─────────────────────────────────
    //
    // For any sequence of enqueue/start/pause/complete/cancel/error operations, the
    // number of downloads with status "downloading" SHALL never exceed the
    // configured max_concurrent limit.
    //
    // The only operation that ever *raises* the active count is a scheduler start;
    // pause, complete, cancel, and error only lower or hold it. So the invariant can
    // only be threatened by starts. This test models the scheduler's start decision
    // exactly as `QueueManager::try_start_one` does — the active-count gate
    // (`count_active(&map) < max`) combined with `next_queued_id` — then drives the
    // scheduler to saturation (mirroring `while this.try_start_one().await {}`) over
    // an arbitrary initial mix of states and an arbitrary max, asserting the active
    // count never exceeds the limit at any step.
    //
    // **Validates: Requirements 3.1, 10.4**

    /// Pure mirror of the scheduler's start decision in
    /// [`QueueManager::try_start_one`]: choose the next queued id only while the
    /// active-count gate permits another start.
    fn schedule_decision(
        order: &[String],
        map: &HashMap<String, DownloadItem>,
        max: usize,
    ) -> Option<String> {
        if count_active(map) >= max {
            None
        } else {
            next_queued_id(order, map)
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(512))]

        /// Property 4: greedily starting downloads through the scheduler's gate
        /// never lets the active count exceed max_concurrent — for any initial mix
        /// of states and any max in the supported range [1, MAX_CONCURRENT_LIMIT].
        #[test]
        fn prop_queue_concurrency_invariant(
            // A pool of statuses; index i becomes download id "d{i}". Includes
            // pre-existing Downloading items so the gate may already be at/over max.
            statuses in prop::collection::vec(any_status(), 0..20),
            max in 1usize..=MAX_CONCURRENT_LIMIT,
        ) {
            let mut map: HashMap<String, DownloadItem> = statuses
                .iter()
                .enumerate()
                .map(|(i, s)| {
                    let id = format!("d{i}");
                    (id.clone(), item(&id, s.clone()))
                })
                .collect();
            let order: Vec<String> = (0..statuses.len()).map(|i| format!("d{i}")).collect();

            let initial_active = count_active(&map);

            // Drive the scheduler to saturation, exactly as the real scheduler does
            // with `while this.try_start_one().await {}`. The bound guards against a
            // non-terminating loop if the gate logic were ever broken.
            let mut started = 0usize;
            let bound = map.len() + 1;
            while let Some(id) = schedule_decision(&order, &map, max) {
                // The scheduler only ever promotes a Queued item to Downloading.
                let entry = map.get_mut(&id).expect("chosen id must exist");
                prop_assert_eq!(entry.status.clone(), DownloadStatus::Queued);
                entry.status = DownloadStatus::Downloading;

                // INVARIANT: after every start, active count stays within the limit.
                prop_assert!(
                    count_active(&map) <= max,
                    "active {} exceeded max_concurrent {}",
                    count_active(&map),
                    max
                );

                started += 1;
                prop_assert!(started <= bound, "scheduler failed to terminate");
            }

            // After saturation the invariant still holds: the scheduler never
            // *raised* the active count above the limit. Any excess over `max` can
            // only be pre-existing over-provisioning (Req 3.4: lowering
            // max_concurrent lets in-flight downloads finish without cancellation),
            // never something a start created — every start above is gated by
            // count_active < max.
            let final_active = count_active(&map);
            prop_assert!(
                final_active <= max.max(initial_active),
                "scheduler raised active {} above the limit (max {}, initial {})",
                final_active,
                max,
                initial_active
            );
            prop_assert!(final_active >= initial_active.min(max));
            prop_assert!(
                final_active >= max || next_queued_id(&order, &map).is_none(),
                "scheduler stopped early: active {} < max {} but a queued item remains",
                final_active,
                max
            );
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Property 6: Queue reorder is respected (Task 7.5)
//
// Exercises the pure `reorder_vec` helper that backs `QueueManager::reorder`.
// A reorder must respect the new ordering for subsequent scheduling decisions,
// which means the moved id lands at its clamped target position while every
// other element keeps its relative order and no element is lost or duplicated.
//
// This module is intentionally separate from the `tests` module above so the
// property test can be added without disturbing the existing example tests.
// ─────────────────────────────────────────────────────────────────────────────
#[cfg(test)]
mod reorder_property_tests {
    use super::*;
    use proptest::prelude::*;

    /// A queue order of `n` distinct ids ("id0", "id1", …, "id{n-1}").
    /// Distinctness mirrors the real invariant: the FIFO `order` never holds
    /// duplicate download ids.
    fn distinct_order() -> impl Strategy<Value = Vec<String>> {
        (1usize..=12).prop_map(|n| (0..n).map(|i| format!("id{i}")).collect())
    }

    proptest! {
        // Property 6: Queue reorder is respected.
        //
        // For any queue order, any element in it, and any target position, after
        // `reorder_vec`:
        //   1. the multiset of elements is preserved (no loss, no duplication),
        //   2. the moved id lands at exactly min(position, len - 1),
        //   3. the relative order of all other elements is unchanged.
        //
        // **Validates: Requirement 3.3**
        #[test]
        fn prop_reorder_respects_new_ordering(
            order in distinct_order(),
            moved_idx in 0usize..12,
            position in 0usize..16,
        ) {
            // Constrain the chosen index to the generated order's length.
            let len = order.len();
            let moved_idx = moved_idx % len;
            let moved_id = order[moved_idx].clone();

            let original = order.clone();
            let mut result = order;
            reorder_vec(&mut result, &moved_id, position);

            // (1) Length is preserved and the element multiset is unchanged.
            prop_assert_eq!(result.len(), len);
            let mut before_sorted = original.clone();
            let mut after_sorted = result.clone();
            before_sorted.sort();
            after_sorted.sort();
            prop_assert_eq!(&before_sorted, &after_sorted, "elements lost or duplicated");

            // (2) The moved id lands at the clamped target position.
            let expected_pos = position.min(len - 1);
            let actual_pos = result.iter().position(|x| x == &moved_id).unwrap();
            prop_assert_eq!(
                actual_pos,
                expected_pos,
                "moved id at {} but expected min(position, len-1) = {}",
                actual_pos,
                expected_pos
            );

            // (3) The relative order of the other elements is unchanged: drop the
            // moved id from both sequences and they must be identical.
            let others_before: Vec<&String> =
                original.iter().filter(|x| *x != &moved_id).collect();
            let others_after: Vec<&String> =
                result.iter().filter(|x| *x != &moved_id).collect();
            prop_assert_eq!(others_before, others_after, "relative order of other items changed");
        }

        // Reordering an id that is not present in the queue is a no-op: the order
        // is left exactly as it was. This guards the `QueueManager::reorder`
        // contract that only known, queued ids move.
        //
        // **Validates: Requirement 3.3**
        #[test]
        fn prop_reorder_unknown_id_is_noop(
            order in distinct_order(),
            position in 0usize..16,
        ) {
            // "absent" can never collide with the generated "id{n}" ids.
            let original = order.clone();
            let mut result = order;
            reorder_vec(&mut result, "absent", position);
            prop_assert_eq!(result, original, "reordering an unknown id changed the order");
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Task 16.2 — End-to-end integration tests (queue + settings propagation)
//
// Flow 3 (concurrent download limit) and Flow 4 (settings change propagation)
// are verified here against the *real* concurrency primitives the QueueManager
// uses: an `Arc<Semaphore>` resized exactly as `set_max_concurrent` does, the
// real scheduling helpers (`next_queued_id` / `count_active`), and the real
// `SpeedLimiter`. The full `QueueManager` requires a concrete `tauri::AppHandle`
// to emit `queue-changed` events and so cannot be constructed in a headless
// test; this module reproduces its scheduling + live-tuning behaviour with the
// same components so the guarantees are exercised by automated tests.
// ─────────────────────────────────────────────────────────────────────────────
#[cfg(test)]
mod e2e_queue_integration_tests {
    use super::*;

    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    use tokio::sync::{Mutex, Semaphore};

    use crate::settings::AppSettings;
    use crate::speed_limiter::SpeedLimiter;

    fn queued_item(id: &str) -> DownloadItem {
        let mut it = DownloadItem::new(
            id.into(),
            format!("https://example.com/{id}"),
            format!("{id}.bin"),
        );
        it.status = DownloadStatus::Queued;
        it
    }

    // ─── Flow 3: concurrent download limit enforcement ───────────────────────────
    //
    // Enqueue more than `max_concurrent` items and drive a faithful scheduler
    // loop that uses the real `Arc<Semaphore>` (permits == max_concurrent), the
    // real `next_queued_id` selection, and a real concurrent "download" task per
    // item. A shared peak-tracking counter asserts the number of items in the
    // Downloading state NEVER exceeds the configured limit at any instant.
    //
    // **Validates: Requirements 3.1, 10.4**
    #[tokio::test]
    async fn e2e_concurrent_download_limit_is_enforced() {
        for max in 1usize..=4 {
            let total_items = max * 3 + 2; // always more than the limit
            let downloads: Downloads = Arc::new(Mutex::new(HashMap::new()));
            let order: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));

            {
                let mut map = downloads.lock().await;
                let mut ord = order.lock().await;
                for i in 0..total_items {
                    let id = format!("dl{i}");
                    map.insert(id.clone(), queued_item(&id));
                    ord.push(id);
                }
            }

            let semaphore = Arc::new(Semaphore::new(max));
            let live = Arc::new(AtomicUsize::new(0));
            let peak = Arc::new(AtomicUsize::new(0));
            let mut handles = Vec::new();

            // Scheduler loop: start every queued item as permits free up, exactly
            // like QueueManager::try_start_one (gate on count_active < max, pick the
            // next queued id, acquire a permit, mark Downloading).
            loop {
                let chosen = {
                    let map = downloads.lock().await;
                    let ord = order.lock().await;
                    if count_active(&map) >= max {
                        None
                    } else {
                        next_queued_id(&ord, &map)
                    }
                };
                let id = match chosen {
                    Some(id) => id,
                    None => {
                        // Nothing startable right now. If anything is still active
                        // wait for it; otherwise we're done.
                        if live.load(Ordering::SeqCst) == 0 {
                            let remaining = {
                                let map = downloads.lock().await;
                                let ord = order.lock().await;
                                next_queued_id(&ord, &map).is_some()
                            };
                            if !remaining {
                                break;
                            }
                        }
                        tokio::time::sleep(Duration::from_millis(5)).await;
                        continue;
                    }
                };

                let permit = match semaphore.clone().try_acquire_owned() {
                    Ok(p) => p,
                    Err(_) => {
                        tokio::time::sleep(Duration::from_millis(5)).await;
                        continue;
                    }
                };

                {
                    let mut map = downloads.lock().await;
                    if let Some(it) = map.get_mut(&id) {
                        it.status = DownloadStatus::Downloading;
                    }
                }

                let downloads_t = downloads.clone();
                let live_t = live.clone();
                let peak_t = peak.clone();
                handles.push(tokio::spawn(async move {
                    // Hold the permit for the lifetime of the "download".
                    let _permit = permit;
                    let now = live_t.fetch_add(1, Ordering::SeqCst) + 1;
                    peak_t.fetch_max(now, Ordering::SeqCst);

                    tokio::time::sleep(Duration::from_millis(20)).await;

                    {
                        let mut map = downloads_t.lock().await;
                        if let Some(it) = map.get_mut(&id) {
                            it.status = DownloadStatus::Complete;
                        }
                    }
                    live_t.fetch_sub(1, Ordering::SeqCst);
                }));
            }

            for h in handles {
                h.await.unwrap();
            }

            // Every item finished, and the peak concurrency never broke the limit.
            let peak = peak.load(Ordering::SeqCst);
            assert!(
                peak <= max,
                "peak concurrency {peak} exceeded max_concurrent {max}"
            );
            assert!(peak >= 1, "expected at least one concurrent download");

            let map = downloads.lock().await;
            assert!(
                map.values().all(|i| i.status == DownloadStatus::Complete),
                "all items should complete"
            );
        }
    }

    // ─── Flow 4: settings change propagation ─────────────────────────────────────
    //
    // Updating settings must apply to the live components without an app restart
    // (Req 11.5). This drives the exact mutations `set_max_concurrent` and
    // `set_speed_limit` perform on the live primitives:
    //   - the global `SpeedLimiter` rate (shared with every active download), and
    //   - the concurrency `Semaphore` permit count (add_permits / forget_permits),
    // after validating the new values through `AppSettings` (Req 11.1/11.3).
    //
    // **Validates: Requirement 11.5 (plus 11.1, 11.3 validation gating)**
    #[tokio::test]
    async fn e2e_settings_change_propagates_to_live_components() {
        // The live speed limiter, shared across downloads.
        let limiter = SpeedLimiter::new(0); // start unlimited
        assert_eq!(limiter.current_rate(), 0);

        // The live concurrency semaphore + tracked max, as held by QueueInner.
        let semaphore = Arc::new(Semaphore::new(3));
        let mut current_max = 3usize;
        assert_eq!(semaphore.available_permits(), 3);

        // --- Apply a settings update (max_concurrent 3 → 5, speed_limit 0 → 1 MB/s) ---
        let new = AppSettings {
            max_concurrent: 5,
            speed_limit: 1_048_576,
            ..AppSettings::default()
        };
        // Validation gates the update (Req 11.1 / 11.3).
        let validated_max =
            AppSettings::validate_max_concurrent(new.max_concurrent).expect("5 is in range");
        let validated_limit =
            AppSettings::validate_speed_limit(new.speed_limit).expect("non-negative");

        // Propagate to the live speed limiter (Req 4.3 / 11.5).
        limiter.set_rate(validated_limit);
        assert_eq!(
            limiter.current_rate(),
            1_048_576,
            "speed limiter should reflect the new rate immediately"
        );

        // Propagate to the live semaphore (increase → add permits), mirroring
        // QueueManager::set_max_concurrent.
        if validated_max > current_max {
            semaphore.add_permits(validated_max - current_max);
        } else if validated_max < current_max {
            semaphore.forget_permits(current_max - validated_max);
        }
        current_max = validated_max;
        assert_eq!(current_max, 5);
        assert_eq!(
            semaphore.available_permits(),
            5,
            "two extra permits should be available after raising max_concurrent"
        );

        // --- Lower max_concurrent 5 → 2 with two permits checked out ---
        let _p1 = semaphore.clone().try_acquire_owned().unwrap();
        let _p2 = semaphore.clone().try_acquire_owned().unwrap();
        assert_eq!(semaphore.available_permits(), 3);

        let lowered = AppSettings::validate_max_concurrent(2).expect("2 is in range");
        if lowered > current_max {
            semaphore.add_permits(lowered - current_max);
        } else if lowered < current_max {
            // Forget only *available* permits; in-flight ones are untouched
            // (Req 3.4: active downloads are never cancelled).
            semaphore.forget_permits(current_max - lowered);
        }
        current_max = lowered;
        assert_eq!(current_max, 2);
        // 3 available − 3 forgotten = 0 free; the 2 checked-out permits live on.
        assert_eq!(
            semaphore.available_permits(),
            0,
            "lowering the limit forgets available permits without cancelling active ones"
        );

        // Speed limit can also be turned back to unlimited live.
        limiter.set_rate(AppSettings::validate_speed_limit(0).unwrap());
        assert_eq!(limiter.current_rate(), 0);

        // An out-of-range update is rejected and must NOT touch live components.
        let before = limiter.current_rate();
        assert!(AppSettings::validate_max_concurrent(99).is_err());
        assert!(AppSettings::validate_speed_limit_signed(-1).is_err());
        assert_eq!(
            limiter.current_rate(),
            before,
            "a rejected settings update must not change live state"
        );
    }
}
