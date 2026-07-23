mod capture_server;
pub mod categorizer;
pub mod commands;
pub mod downloader;
mod media_extractor;
pub mod models;
pub mod persistence;
pub mod queue;
pub mod settings;
pub mod speed_limiter;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{Emitter, Listener, Manager, WindowEvent};
use tauri_plugin_notification::NotificationExt;
use tokio::sync::Mutex;

use categorizer::Categorizer;
use media_extractor::MediaExtractor;
use models::{CancelTokens, DownloadItem, DownloadStatus, Downloads, QueueConfig};
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

/// Build the system tray icon with Show, Hide, Pause All, Resume All, and Quit.
///
/// Left-clicking the tray icon shows and focuses the main window; the menu items
/// give explicit show/hide/quit control. Pairs with the window-close handler,
/// which hides to the tray instead of quitting when `minimize_to_tray` is on.
fn build_tray(app: &tauri::App, queue: QueueManager) -> tauri::Result<()> {
    let show_i = MenuItem::with_id(app, "tray_show", "Show Downpour", true, None::<&str>)?;
    let hide_i = MenuItem::with_id(app, "tray_hide", "Hide to Tray", true, None::<&str>)?;
    let pause_i = MenuItem::with_id(app, "tray_pause_all", "Pause All", true, None::<&str>)?;
    let resume_i = MenuItem::with_id(app, "tray_resume_all", "Resume All", true, None::<&str>)?;
    let quit_i = MenuItem::with_id(app, "tray_quit", "Quit Downpour", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&show_i, &hide_i, &pause_i, &resume_i, &quit_i])?;

    let mut builder = TrayIconBuilder::with_id("main-tray")
        .tooltip("Downpour")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(move |app, event| match event.id.as_ref() {
            "tray_show" => show_main_window(app),
            "tray_hide" => {
                if let Some(window) = app.get_webview_window("main") {
                    let _ = window.hide();
                }
            }
            "tray_pause_all" => {
                let queue = queue.clone();
                tauri::async_runtime::spawn(async move {
                    if let Err(e) = queue.pause_all().await {
                        eprintln!("failed to pause all downloads from tray: {e:#}");
                    }
                });
            }
            "tray_resume_all" => {
                let queue = queue.clone();
                tauri::async_runtime::spawn(async move {
                    if let Err(e) = queue.resume_all().await {
                        eprintln!("failed to resume all downloads from tray: {e:#}");
                    }
                });
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

            // Build the system tray (Show / Hide / Pause / Resume / Quit + click-to-show).
            if let Err(e) = build_tray(app, queue.clone()) {
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
            commands::download::start_download,
            commands::download::list_downloads,
            commands::download::pause_download,
            commands::download::resume_download,
            commands::download::cancel_download,
            commands::download::remove_download,
            commands::download::clear_paused_downloads,
            commands::download::reorder_download,
            commands::queue::pause_all,
            commands::queue::resume_all,
            commands::queue::get_queue_state,
            commands::system::open_download_file,
            commands::system::reveal_download_file,
            commands::system::delete_download_file,
            commands::settings::get_settings,
            commands::settings::update_settings,
            commands::settings::set_speed_limit,
            commands::settings::set_max_concurrent,
            commands::media::extract_media_info,
            commands::media::start_media_download,
            commands::media::extract_playlist_info,
            commands::media::start_media_batch,
            commands::media::cancel_media_download,
            commands::system::hide_window,
        ])
        .run(tauri::generate_context!())
        .expect("error while running Downpour");
}
