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
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{Emitter, Listener, Manager, WindowEvent};
use tauri_plugin_notification::NotificationExt;
use tokio::sync::Mutex;

use categorizer::Categorizer;
use media_extractor::{MediaExtractor, MediaInfo, PlaylistInfo};
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

/// Live "minimize to tray on window close" flag. Seeded from
/// `AppSettings.minimize_to_tray` and updated by `update_settings`, so the
/// window-close handler (a synchronous closure) can read it without locking the
/// async settings mutex.
type MinimizeToTray = Arc<AtomicBool>;

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
    minimize_to_tray: tauri::State<'_, MinimizeToTray>,
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
    queue.set_download_dir(new_settings.download_dir.clone());
    *media.lock().await = build_media_extractor(&new_settings);
    *categorizer.lock().await = Categorizer::from_settings(&new_settings);
    minimize_to_tray.store(new_settings.minimize_to_tray, Ordering::SeqCst);
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

// ─── Open / reveal completed files ───────────────────────────────────────────────

/// Resolve the on-disk path of a completed download: its recorded `output_path`
/// (set on completion / after categorization), falling back to
/// `download_dir/filename`. Errors if the file is missing.
async fn resolve_download_path(
    downloads: &Downloads,
    queue: &QueueManager,
    id: &str,
) -> Result<PathBuf, String> {
    let item = {
        let map = downloads.lock().await;
        map.get(id)
            .cloned()
            .ok_or_else(|| format!("unknown download id: {id}"))?
    };
    let path = item
        .output_path
        .clone()
        .unwrap_or_else(|| queue.download_dir().join(&item.filename));
    if !path.exists() {
        return Err(format!("file not found: {}", path.display()));
    }
    Ok(path)
}

/// Launch a path with the OS default handler, or (when `reveal`) open its parent
/// folder with the file selected.
fn os_open(path: &Path, reveal: bool) -> Result<(), String> {
    let mut command = {
        #[cfg(target_os = "windows")]
        {
            // explorer's `/select,` syntax is parsed by explorer itself, not the
            // CRT argv splitter — paths with spaces (e.g. "Yash Verma") break the
            // default Command quoting and explorer opens the wrong folder. Build
            // the argument verbatim with `raw_arg` and quote the path ourselves.
            use std::os::windows::process::CommandExt;
            let mut c = std::process::Command::new("explorer");
            if reveal {
                c.raw_arg(format!("/select,\"{}\"", path.display()));
            } else {
                c.raw_arg(format!("\"{}\"", path.display()));
            }
            c
        }
        #[cfg(target_os = "macos")]
        {
            let mut c = std::process::Command::new("open");
            if reveal {
                c.arg("-R");
            }
            c.arg(path);
            c
        }
        #[cfg(all(unix, not(target_os = "macos")))]
        {
            // No portable "reveal + select"; open the containing folder instead.
            let target = if reveal {
                path.parent().unwrap_or(path)
            } else {
                path
            };
            let mut c = std::process::Command::new("xdg-open");
            c.arg(target);
            c
        }
    };
    command.spawn().map_err(|e| e.to_string())?;
    Ok(())
}

/// Open a completed download with the OS default application.
#[tauri::command]
async fn open_download_file(
    downloads: tauri::State<'_, Downloads>,
    queue: tauri::State<'_, QueueManager>,
    id: String,
) -> Result<(), String> {
    let path = resolve_download_path(&downloads, &queue, &id).await?;
    os_open(&path, false)
}

/// Reveal a completed download in its containing folder (file selected where the
/// platform supports it).
#[tauri::command]
async fn reveal_download_file(
    downloads: tauri::State<'_, Downloads>,
    queue: tauri::State<'_, QueueManager>,
    id: String,
) -> Result<(), String> {
    let path = resolve_download_path(&downloads, &queue, &id).await?;
    os_open(&path, true)
}

/// Delete a download's file from disk and remove the entry from the queue. The
/// file removal is best-effort (a missing file is not an error); the entry is
/// always removed.
#[tauri::command]
async fn delete_download_file(
    downloads: tauri::State<'_, Downloads>,
    queue: tauri::State<'_, QueueManager>,
    id: String,
) -> Result<(), String> {
    let item = { downloads.lock().await.get(&id).cloned() };
    if let Some(item) = item {
        let path = item
            .output_path
            .clone()
            .unwrap_or_else(|| queue.download_dir().join(&item.filename));
        let _ = tokio::fs::remove_file(&path).await;
    }
    queue.remove(&id).await.map_err(|e| format!("{e:#}"))
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
    title: Option<String>,
) -> Result<DownloadItem, String> {
    let id = uuid::Uuid::new_v4().to_string();

    // The yt-dlp output template: a user-provided filename names the file exactly,
    // otherwise yt-dlp names it from the video title + extension.
    let template = filename
        .clone()
        .unwrap_or_else(|| "%(title)s.%(ext)s".to_string());

    // The display name shown on the card *while downloading*: the user filename if
    // given, else the real title we already fetched (so the card never shows the
    // raw `%(title)s.%(ext)s` template). Replaced with the exact on-disk name on
    // completion.
    let display = filename
        .or(title)
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "Media download".to_string());

    let mut item = DownloadItem::new(id, url, display);
    item.download_type = DownloadType::Media;
    item.media_format_id = Some(format_id);
    item.output_template = Some(template);
    // Track progress as a percentage (0-100) since yt-dlp reports percent, not bytes.
    item.total_size = 100;

    queue
        .enqueue(item.clone())
        .await
        .map_err(|e| format!("{e:#}"))?;
    Ok(item)
}

/// Enumerate a playlist/channel (flat — no per-video formats, so it's fast).
/// `limit` caps how many entries are returned for the UI soft-cap.
#[tauri::command]
async fn extract_playlist_info(
    media: tauri::State<'_, MediaState>,
    url: String,
    cookies: Option<String>,
    limit: Option<usize>,
) -> Result<PlaylistInfo, String> {
    let extractor = media.lock().await.clone();
    extractor
        .extract_playlist(&url, cookies.as_deref(), limit)
        .await
        .map_err(|e| format!("{e:#}"))
}

/// One selected playlist entry to enqueue as a media download.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct BatchEntry {
    url: String,
    title: String,
    index: u32,
}

/// Enqueue many playlist entries at once. Each becomes a `Media` download with
/// the shared `format_selector` (a yt-dlp format expression, e.g. a quality
/// preset). When `index_prefix` is set the file is named `NN - <title>` so the
/// playlist order is preserved on disk. Returns the enqueued items.
#[tauri::command]
async fn start_media_batch(
    queue: tauri::State<'_, QueueManager>,
    entries: Vec<BatchEntry>,
    format_selector: String,
    index_prefix: bool,
) -> Result<Vec<DownloadItem>, String> {
    let mut started = Vec::new();
    for entry in entries {
        let id = uuid::Uuid::new_v4().to_string();
        let template = if index_prefix {
            format!("{:02} - %(title)s.%(ext)s", entry.index)
        } else {
            "%(title)s.%(ext)s".to_string()
        };
        let display = if entry.title.trim().is_empty() {
            "Media download".to_string()
        } else {
            entry.title.clone()
        };

        let mut item = DownloadItem::new(id, entry.url, display);
        item.download_type = DownloadType::Media;
        item.media_format_id = Some(format_selector.clone());
        item.output_template = Some(template);
        item.total_size = 100;

        if queue.enqueue(item.clone()).await.is_ok() {
            started.push(item);
        }
    }
    Ok(started)
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

/// Listen for download completions and route completed downloads through the
/// auto-categorizer (Req 7.1). Both HTTP and media downloads are categorized:
/// media now reports its real `output_path` on completion, so its file can be
/// moved into the matching category folder (e.g. `Videos/`) just like HTTP.
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

        if item.status != DownloadStatus::Complete {
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

            let cat = categorizer.lock().await;
            if !cat.is_enabled() {
                return;
            }
            // Use the real path the download wrote (set on completion for both
            // HTTP and media), falling back to the configured dir + filename.
            let file_path = item
                .output_path
                .clone()
                .unwrap_or_else(|| cat.download_dir().join(&item.filename));
            let label = cat.categorize(&item.filename, None).map(str::to_string);
            let moved = cat.process(&app, &file_path, None).await;
            drop(cat);

            if moved.is_none() {
                return;
            }

            // Record the resolved category and the file's new location, then persist.
            let snapshot = {
                let mut map = downloads.lock().await;
                let it = match map.get_mut(&item.id) {
                    Some(it) => it,
                    None => return,
                };
                it.category = label;
                it.output_path = moved;
                it.clone()
            };
            let _ = persistence.save_download(&snapshot).await;
            let _ = app.emit("download-progress", snapshot);
        });
    });
}

// ─── Completion → desktop notification wiring ─────────────────────────────────────

/// Listen for download completions and show a native desktop notification when
/// `AppSettings.notifications_enabled` is set. Dedupes per id so a download is
/// announced once even though `download-progress` fires repeatedly.
fn spawn_completion_notifier(app: tauri::AppHandle, settings: SettingsState) {
    let notified: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
    let listen_handle = app.clone();

    listen_handle.listen("download-progress", move |event| {
        let item: DownloadItem = match serde_json::from_str(event.payload()) {
            Ok(item) => item,
            Err(_) => return,
        };
        if item.status != DownloadStatus::Complete {
            return;
        }

        let app = app.clone();
        let settings = settings.clone();
        let notified = notified.clone();

        tauri::async_runtime::spawn(async move {
            // Announce each completed download only once.
            {
                let mut seen = notified.lock().await;
                if !seen.insert(item.id.clone()) {
                    return;
                }
            }
            if !settings.lock().await.notifications_enabled {
                return;
            }
            let _ = app
                .notification()
                .builder()
                .title("Download complete")
                .body(&item.filename)
                .show();
        });
    });
}

// ─── System tray ─────────────────────────────────────────────────────────────────

/// Build the system tray icon with a Show / Hide / Quit menu (Req: system tray).
///
/// Left-clicking the tray icon shows and focuses the main window; the menu items
/// give explicit show/hide/quit control. Pairs with the window-close handler,
/// which hides to the tray instead of quitting when `minimize_to_tray` is on.
fn build_tray(app: &tauri::App) -> tauri::Result<()> {
    let show_i = MenuItem::with_id(app, "tray_show", "Show Downpour", true, None::<&str>)?;
    let hide_i = MenuItem::with_id(app, "tray_hide", "Hide to Tray", true, None::<&str>)?;
    let quit_i = MenuItem::with_id(app, "tray_quit", "Quit Downpour", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&show_i, &hide_i, &quit_i])?;

    let mut builder = TrayIconBuilder::with_id("main-tray")
        .tooltip("Downpour")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id.as_ref() {
            "tray_show" => show_main_window(app),
            "tray_hide" => {
                if let Some(window) = app.get_webview_window("main") {
                    let _ = window.hide();
                }
            }
            "tray_quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            // Left-click (button released) brings the window back.
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                show_main_window(tray.app_handle());
            }
        });

    // Reuse the embedded window icon for the tray when one is available.
    if let Some(icon) = app.default_window_icon().cloned() {
        builder = builder.icon(icon);
    }

    builder.build(app)?;
    Ok(())
}

/// Show and focus the main window (used by the tray menu and icon click).
fn show_main_window(app: &tauri::AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
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
                download_dir: settings.download_dir.clone(),
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

            // Restore the queue from disk. By default nothing auto-starts (Req 5.2):
            // restored downloads stay paused until the user resumes. If the user
            // opted into `resume_on_startup`, auto-resume them after restore.
            let restore_queue = queue.clone();
            let resume_on_startup = settings.resume_on_startup;
            tauri::async_runtime::block_on(async move {
                if let Err(e) = restore_queue.restore_from_disk().await {
                    eprintln!("failed to restore queue from disk: {e:#}");
                } else if resume_on_startup {
                    if let Err(e) = restore_queue.resume_all().await {
                        eprintln!("failed to auto-resume downloads on startup: {e:#}");
                    }
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

            // Announce completed downloads via native notifications.
            spawn_completion_notifier(handle.clone(), settings_state.clone());

            // Build the system tray (Show / Hide / Quit + click-to-show).
            if let Err(e) = build_tray(app) {
                eprintln!("failed to build system tray: {e:#}");
            }

            // Minimize-to-tray: when enabled, intercept the window close and hide
            // the window instead of quitting. The flag is read live so toggling
            // the setting takes effect without a restart.
            let minimize_to_tray: MinimizeToTray =
                Arc::new(AtomicBool::new(settings.minimize_to_tray));
            if let Some(window) = app.get_webview_window("main") {
                let flag = minimize_to_tray.clone();
                let win = window.clone();
                window.on_window_event(move |event| {
                    if let WindowEvent::CloseRequested { api, .. } = event {
                        if flag.load(Ordering::SeqCst) {
                            api.prevent_close();
                            let _ = win.hide();
                        }
                    }
                });
            }

            // Register everything as managed state for the command surface.
            app.manage(queue);
            app.manage(persistence);
            app.manage(limiter);
            app.manage(settings_state);
            app.manage(media_state);
            app.manage(categorizer_state);
            app.manage(minimize_to_tray);

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
            open_download_file,
            reveal_download_file,
            delete_download_file,
            get_settings,
            update_settings,
            set_speed_limit,
            set_max_concurrent,
            extract_media_info,
            start_media_download,
            extract_playlist_info,
            start_media_batch,
            cancel_media_download,
        ])
        .run(tauri::generate_context!())
        .expect("error while running Downpour");
}
