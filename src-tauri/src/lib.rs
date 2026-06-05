mod capture_server;
mod downloader;
mod queue;

use std::collections::HashMap;
use std::sync::Arc;

use tauri::{Emitter, Manager};
use tokio::sync::Mutex;

use downloader::{DownloadItem, Downloads};

/// Create a new download from a URL and start it in the background.
#[tauri::command]
async fn start_download(
    app: tauri::AppHandle,
    state: tauri::State<'_, Downloads>,
    url: String,
) -> Result<DownloadItem, String> {
    let downloads = state.inner().clone();

    let id = uuid::Uuid::new_v4().to_string();
    let item = DownloadItem {
        id: id.clone(),
        url: url.clone(),
        filename: downloader::filename_from_url(&url),
        total_size: 0,
        downloaded: 0,
        status: "queued".into(),
    };
    downloads.lock().await.insert(id.clone(), item.clone());

    let app2 = app.clone();
    let downloads2 = downloads.clone();
    let id2 = id.clone();
    tauri::async_runtime::spawn(async move {
        if let Err(e) = downloader::run(app2.clone(), downloads2.clone(), id2.clone()).await {
            eprintln!("download {id2} failed: {e:?}");
            let mut map = downloads2.lock().await;
            if let Some(it) = map.get_mut(&id2) {
                it.status = "error".into();
                let snapshot = it.clone();
                drop(map);
                let _ = app2.emit("download-progress", snapshot);
            }
        }
    });

    Ok(item)
}

/// Return all known downloads (used to hydrate the UI on launch).
#[tauri::command]
async fn list_downloads(state: tauri::State<'_, Downloads>) -> Result<Vec<DownloadItem>, String> {
    let downloads = state.inner().clone();
    let map = downloads.lock().await;
    Ok(map.values().cloned().collect())
}

pub fn run() {
    let downloads: Downloads = Arc::new(Mutex::new(HashMap::new()));

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_notification::init())
        .manage(downloads.clone())
        .setup(move |app| {
            // Start the local capture server the browser extension talks to.
            let handle = app.handle().clone();
            let downloads = downloads.clone();
            tauri::async_runtime::spawn(async move {
                if let Err(e) = capture_server::serve(handle, downloads).await {
                    eprintln!("capture server stopped: {e:?}");
                }
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![start_download, list_downloads])
        .run(tauri::generate_context!())
        .expect("error while running Downpour");
}
