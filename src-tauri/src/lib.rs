mod capture_server;
pub mod categorizer;
pub mod downloader;
mod media_extractor;
pub mod models;
pub mod persistence;
pub mod queue;
pub mod settings;
pub mod speed_limiter;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use tauri::{Emitter, Listener, Manager};
use tokio::sync::Mutex;

use categorizer::Categorizer;
use media_extractor::{MediaExtractor, MediaInfo};
use models::{CancelTokens, DownloadItem, DownloadStatus, DownloadType, Downloads, QueueConfig};
use persistence::PersistenceLayer;
use queue::QueueManager;
use settings::AppSettings;

// ─── Managed-state type aliases ──────────────────────────────────────────────────

/// The current application settings, shared so commands can read and update them
/// at runtime without an app restart (Req 11.5).
type SettingsState = Arc<Mutex<AppSettings>>;

/// The media extractor, rebuilt when the yt-dlp/ffmpeg paths change in settings.
type MediaState = Arc<Mutex<MediaExtractor>>;

/// The auto-categorizer, rebuilt when category rules or the download dir change.
type CategorizerState = Arc<Mutex<Categorizer>>;

// ─── Helpers ─────────────────────────────────────────────────────────────────────

/// Build a [`MediaExtractor`] from the configured binary paths, falling back to
/// the bare `yt-dlp` / `ffmpeg` names (resolved on `PATH`) when unset.
fn build_media_extractor(settings: &AppSettings) -> MediaExtractor {
    let ytdlp = settings
        .ytdlp_path
        .clone()
        .unwrap_or_else(|| PathBuf::from("yt-dlp"));
    let ffmpeg = settings
        .ffmpeg_path
        .clone()
        .unwrap_or_else(|| PathBuf::from("ffmpeg"));
    MediaExtractor::new(ytdlp, ffmpeg)
}

// ─── HTTP download commands ────────────────────────────────────────────────────

/// Create a download from a URL and enqueue it. The queue scheduler starts it
/// when a concurrency permit is available (Req 3.1).
#[tauri::command]
async fn start_download(
    queue: tauri::State<'_, QueueManager>,
    url: String,
) -> Result<DownloadItem, String> {
    let id = uuid::Uuid::new_v4().to_string();
    let item = DownloadItem::new(id, url.clone(), downloader::filename_from_url(&url));
    queue
        .enqueue(item.clone())
        .await
        .map_err(|e| format!("{e:#}"))?;
    Ok(item)
}

/// Return all known downloads (used to hydrate the UI on launch).
#[tauri::command]
async fn list_downloads(state: tauri::State<'_, Downloads>) -> Result<Vec<DownloadItem>, String> {
    let map = state.inner().lock().await;
    Ok(map.values().cloned().collect())
}

/// Pause a single active download (Req 2.1).
#[tauri::command]
async fn pause_download(queue: tauri::State<'_, QueueManager>, id: String) -> Result<(), String> {
    queue.pause(&id).await.map_err(|e| format!("{e:#}"))
}

/// Resume a single paused download; the scheduler resumes it from saved offsets
/// (Req 2.2).
#[tauri::command]
async fn resume_download(queue: tauri::State<'_, QueueManager>, id: String) -> Result<(), String> {
    queue.resume(&id).await.map_err(|e| format!("{e:#}"))
}

/// Cancel a download and discard its partial file.
#[tauri::command]
async fn cancel_download(queue: tauri::State<'_, QueueManager>, id: String) -> Result<(), String> {
    queue.cancel(&id).await.map_err(|e| format!("{e:#}"))
}

/// Remove a download from the queue without deleting any completed file.
#[tauri::command]
async fn remove_download(queue: tauri::State<'_, QueueManager>, id: String) -> Result<(), String> {
    queue.remove(&id).await.map_err(|e| format!("{e:#}"))
}

/// Move a download to a new position in the queue (Req 3.3).
#[tauri::command]
async fn reorder_download(
    queue: tauri::State<'_, QueueManager>,
    id: String,
    position: usize,
) -> Result<(), String> {
    queue
        .reorder(&id, position)
        .await
        .map_err(|e| format!("{e:#}"))
}

// ─── Queue commands ──────────────────────────────────────────────────────────────

/// Pause every active download and suspend the scheduler (Req 3.5).
#[tauri::command]
async fn pause_all(queue: tauri::State<'_, QueueManager>) -> Result<(), String> {
    queue.pause_all().await.map_err(|e| format!("{e:#}"))
}

/// Resume all paused downloads and restart scheduling (Req 3.6).
#[tauri::command]
async fn resume_all(queue: tauri::State<'_, QueueManager>) -> Result<(), String> {
    queue.resume_all().await.map_err(|e| format!("{e:#}"))
}

/// Snapshot of all downloads in queue order (Req 12.3).
#[tauri::command]
async fn get_queue_state(
    queue: tauri::State<'_, QueueManager>,
) -> Result<Vec<DownloadItem>, String> {
    Ok(queue.get_queue_state().await)
}

// ─── Settings commands ───────────────────────────────────────────────────────────

/// Return the current application settings.
#[tauri::command]
async fn get_settings(settings: tauri::State<'_, SettingsState>) -> Result<AppSettings, String> {
    Ok(settings.lock().await.clone())
}

/// Validate and apply a full settings update, persisting it and propagating the
/// relevant values to the live components without an app restart (Req 11.5).
#[tauri::command]
async fn update_settings(
    settings: tauri::State<'_, SettingsState>,
    queue: tauri::State<'_, QueueManager>,
    persistence: tauri::State<'_, PersistenceLayer>,
    media: tauri::State<'_, MediaState>,
    categorizer: tauri::State<'_, CategorizerState>,
    new_settings: AppSettings,
) -> Result<AppSettings, String> {
    // Validate the bounded numeric fields (Req 11.1, 11.2, 11.3).
    AppSettings::validate_max_concurrent(new_settings.max_concurrent).map_err(|e| e.to_string())?;
    AppSettings::validate_segments(new_settings.default_segments).map_err(|e| e.to_string())?;
    AppSettings::validate_speed_limit(new_settings.speed_limit).map_err(|e| e.to_string())?;
    Categorizer::validate_rules(&new_settings.categories).map_err(|e| e.to_string())?;

    // Persist first so a write failure aborts the change.
    persistence
        .save_settings(&new_settings)
        .await
        .map_err(|e| format!("{e:#}"))?;

    // Apply to live components (Req 11.5).
    queue.set_max_concurrent(new_settings.max_concurrent).await;
    queue.set_speed_limit(new_settings.speed_limit);
    *media.lock().await = build_media_extractor(&new_settings);
    *categorizer.lock().await = Categorizer::from_settings(&new_settings);
    *settings.lock().await = new_settings.clone();

    Ok(new_settings)
}

/// Set the global speed limit in bytes/sec (0 = unlimited). Accepts a signed
/// value from the UI and rejects negatives (Req 11.3, 4.3).
#[tauri::command]
async fn set_speed_limit(
    settings: tauri::State<'_, SettingsState>,
    queue: tauri::State<'_, QueueManager>,
    persistence: tauri::State<'_, PersistenceLayer>,
    bytes_per_sec: i64,
) -> Result<(), String> {
    let validated =
        AppSettings::validate_speed_limit_signed(bytes_per_sec).map_err(|e| e.to_string())?;

    queue.set_speed_limit(validated);

    let snapshot = {
        let mut s = settings.lock().await;
        s.speed_limit = validated;
        s.clone()
    };
    persistence
        .save_settings(&snapshot)
        .await
        .map_err(|e| format!("{e:#}"))?;
    Ok(())
}

/// Set the maximum concurrent downloads (1-10), resizing the queue semaphore
/// live (Req 3.4, 11.1).
#[tauri::command]
async fn set_max_concurrent(
    settings: tauri::State<'_, SettingsState>,
    queue: tauri::State<'_, QueueManager>,
    persistence: tauri::State<'_, PersistenceLayer>,
    value: usize,
) -> Result<(), String> {
    let validated = AppSettings::validate_max_concurrent(value).map_err(|e| e.to_string())?;

    queue.set_max_concurrent(validated).await;

    let snapshot = {
        let mut s = settings.lock().await;
        s.max_concurrent = validated;
        s.clone()
    };
    persistence
        .save_settings(&snapshot)
        .await
        .map_err(|e| format!("{e:#}"))?;
    Ok(())
}

// ─── Media (yt-dlp) commands ─────────────────────────────────────────────────────

/// Extract media metadata (formats, title, …) without downloading (Req 8.1).
#[tauri::command]
async fn extract_media_info(
    media: tauri::State<'_, MediaState>,
    url: String,
    cookies: Option<String>,
) -> Result<MediaInfo, String> {
    let extractor = media.lock().await.clone();
    extractor
        .extract_info(&url, cookies.as_deref())
        .await
        .map_err(|e| format!("{e:#}"))
}

/// Start a media download for the chosen format by enqueueing a `Media`
/// download. The queue scheduler dispatches it to the yt-dlp extractor when a
/// concurrency permit is available, so media downloads respect `max_concurrent`
/// like HTTP downloads. Throttled progress is forwarded to the UI via
/// `download-progress` events (Req 8.2, 8.3).
#[tauri::command]
async fn start_media_download(
    queue: tauri::State<'_, QueueManager>,
    url: String,
    format_id: String,
    filename: Option<String>,
) -> Result<DownloadItem, String> {
    let id = uuid::Uuid::new_v4().to_string();
    // yt-dlp accepts an output template; default to title + extension.
    let out_name = filename.unwrap_or_else(|| "%(title)s.%(ext)s".to_string());

    let mut item = DownloadItem::new(id, url, out_name);
    item.download_type = DownloadType::Media;
    item.media_format_id = Some(format_id);
    // Track progress as a percentage (0-100) since yt-dlp reports percent, not bytes.
    item.total_size = 100;

    queue
        .enqueue(item.clone())
        .await
        .map_err(|e| format!("{e:#}"))?;
    Ok(item)
}

/// Cancel an in-flight (or queued) media download and discard its partial file
/// (Req 8.4, 8.8). Routes through the queue so the concurrency permit is freed
/// and the next queued download can start.
#[tauri::command]
async fn cancel_media_download(
    queue: tauri::State<'_, QueueManager>,
    id: String,
) -> Result<(), String> {
    queue.cancel(&id).await.map_err(|e| format!("{e:#}"))
}

// ─── Completion → auto-categorizer wiring ─────────────────────────────────────────

/// Listen for download completions and route completed HTTP downloads through
/// the auto-categorizer (Req 7.1). Media downloads use an output template, so
/// their final path is not known here and they are skipped.
fn spawn_completion_categorizer(
    app: tauri::AppHandle,
    downloads: Downloads,
    categorizer: CategorizerState,
    persistence: PersistenceLayer,
) {
    // Dedupe: `download-progress` may fire repeatedly; categorize each id once.
    let processed: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
    let listen_handle = app.clone();

    listen_handle.listen("download-progress", move |event| {
        let item: DownloadItem = match serde_json::from_str(event.payload()) {
            Ok(item) => item,
            Err(_) => return,
        };

        if item.status != DownloadStatus::Complete || item.download_type == DownloadType::Media {
            return;
        }

        let app = app.clone();
        let downloads = downloads.clone();
        let categorizer = categorizer.clone();
        let persistence = persistence.clone();
        let processed = processed.clone();

        tauri::async_runtime::spawn(async move {
            // Only categorize a given download once.
            {
                let mut seen = processed.lock().await;
                if !seen.insert(item.id.clone()) {
                    return;
                }
            }

            // The engine saves files into the OS downloads directory.
            let file_path = downloader::downloads_dir().join(&item.filename);

            let cat = categorizer.lock().await;
            if !cat.is_enabled() {
                return;
            }
            let label = cat.categorize(&item.filename, None).map(str::to_string);
            let moved = cat.process(&app, &file_path, None).await;
            drop(cat);

            if moved.is_none() {
                return;
            }

            // Record the resolved category and persist it.
            let snapshot = {
                let mut map = downloads.lock().await;
                let it = match map.get_mut(&item.id) {
                    Some(it) => it,
                    None => return,
                };
                it.category = label;
                it.clone()
            };
            let _ = persistence.save_download(&snapshot).await;
            let _ = app.emit("download-progress", snapshot);
        });
    });
}

// ─── App entry point ─────────────────────────────────────────────────────────────

pub fn run() {
    let downloads: Downloads = Arc::new(Mutex::new(HashMap::new()));
    let cancel_tokens: CancelTokens = Arc::new(Mutex::new(HashMap::new()));

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_notification::init())
        .manage(downloads.clone())
        .manage(cancel_tokens.clone())
        .setup(move |app| {
            let handle = app.handle().clone();

            // Initialize persistence (fall back to a temp dir if the data dir is
            // unavailable, so the command surface always has its managed state).
            let persistence = match PersistenceLayer::new() {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("failed to init persistence ({e:#}); using a temporary store");
                    PersistenceLayer::with_path(std::env::temp_dir().join("downpour"))
                        .expect("failed to create fallback persistence store")
                }
            };
            persistence.start_background_writer();

            // Load settings (defaults on first launch / corruption).
            let settings =
                tauri::async_runtime::block_on(persistence.load_settings()).unwrap_or_default();

            // Build the queue manager seeded from settings.
            let queue_config = QueueConfig {
                max_concurrent: settings.max_concurrent,
                max_retries: 3,
                auto_start: settings.auto_start_queue,
                speed_limit_global: settings.speed_limit,
            };
            // The media extractor is shared with the queue so `Media` downloads
            // run through the scheduler (respecting max_concurrent) and pick up
            // live settings changes made via `update_settings`.
            let media_state: MediaState = Arc::new(Mutex::new(build_media_extractor(&settings)));
            let queue = QueueManager::new(
                handle.clone(),
                downloads.clone(),
                cancel_tokens.clone(),
                persistence.clone(),
                queue_config,
            )
            .with_media_extractor(media_state.clone());

            // Restore the queue from disk without auto-starting anything (Req 5.2).
            let restore_queue = queue.clone();
            tauri::async_runtime::block_on(async move {
                if let Err(e) = restore_queue.restore_from_disk().await {
                    eprintln!("failed to restore queue from disk: {e:#}");
                }
            });

            // Start the scheduler loop.
            queue.start_scheduler();

            // Start the local capture server the browser extension talks to.
            let capture_queue = queue.clone();
            let capture_handle = handle.clone();
            tauri::async_runtime::spawn(async move {
                if let Err(e) = capture_server::serve(capture_handle, capture_queue).await {
                    eprintln!("capture server stopped: {e:#}");
                }
            });

            // Shared, runtime-mutable state.
            let settings_state: SettingsState = Arc::new(Mutex::new(settings.clone()));
            let categorizer_state: CategorizerState =
                Arc::new(Mutex::new(Categorizer::from_settings(&settings)));
            let limiter = queue.limiter();

            // Connect download completion to the auto-categorizer (Req 7.1).
            spawn_completion_categorizer(
                handle.clone(),
                downloads.clone(),
                categorizer_state.clone(),
                persistence.clone(),
            );

            // Register everything as managed state for the command surface.
            app.manage(queue);
            app.manage(persistence);
            app.manage(limiter);
            app.manage(settings_state);
            app.manage(media_state);
            app.manage(categorizer_state);

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            start_download,
            list_downloads,
            pause_download,
            resume_download,
            cancel_download,
            remove_download,
            reorder_download,
            pause_all,
            resume_all,
            get_queue_state,
            get_settings,
            update_settings,
            set_speed_limit,
            set_max_concurrent,
            extract_media_info,
            start_media_download,
            cancel_media_download,
        ])
        .run(tauri::generate_context!())
        .expect("error while running Downpour");
}
