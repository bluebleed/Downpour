use crate::downloader;
use crate::models::{DownloadItem, Downloads};
use crate::queue::QueueManager;

/// Create a download from a URL and enqueue it. The queue scheduler starts it
/// when a concurrency permit is available (Req 3.1).
#[tauri::command]
pub async fn start_download(
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
pub async fn list_downloads(
    state: tauri::State<'_, Downloads>,
) -> Result<Vec<DownloadItem>, String> {
    let map = state.inner().lock().await;
    Ok(map.values().cloned().collect())
}

/// Pause a single active download (Req 2.1).
#[tauri::command]
pub async fn pause_download(
    queue: tauri::State<'_, QueueManager>,
    id: String,
) -> Result<(), String> {
    queue.pause(&id).await.map_err(|e| format!("{e:#}"))
}

/// Resume a single paused download; the scheduler resumes it from saved offsets
/// (Req 2.2).
#[tauri::command]
pub async fn resume_download(
    queue: tauri::State<'_, QueueManager>,
    id: String,
) -> Result<(), String> {
    queue.resume(&id).await.map_err(|e| format!("{e:#}"))
}

/// Cancel a download and discard its partial file.
#[tauri::command]
pub async fn cancel_download(
    queue: tauri::State<'_, QueueManager>,
    id: String,
) -> Result<(), String> {
    queue.cancel(&id).await.map_err(|e| format!("{e:#}"))
}

/// Remove a download from the queue without deleting any completed file.
#[tauri::command]
pub async fn remove_download(
    queue: tauri::State<'_, QueueManager>,
    id: String,
) -> Result<(), String> {
    queue.remove(&id).await.map_err(|e| format!("{e:#}"))
}

/// Remove paused queue records without touching files on disk.
#[tauri::command]
pub async fn clear_paused_downloads(
    queue: tauri::State<'_, QueueManager>,
) -> Result<usize, String> {
    Ok(queue.clear_paused().await)
}

/// Move a download to a new position in the queue (Req 3.3).
#[tauri::command]
pub async fn reorder_download(
    queue: tauri::State<'_, QueueManager>,
    id: String,
    position: usize,
) -> Result<(), String> {
    queue
        .reorder(&id, position)
        .await
        .map_err(|e| format!("{e:#}"))
}
