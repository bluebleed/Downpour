use crate::models::DownloadItem;
use crate::queue::QueueManager;

/// Pause every active download and suspend the scheduler (Req 3.5).
#[tauri::command]
pub async fn pause_all(queue: tauri::State<'_, QueueManager>) -> Result<(), String> {
    queue.pause_all().await.map_err(|e| format!("{e:#}"))
}

/// Resume all paused downloads and restart scheduling (Req 3.6).
#[tauri::command]
pub async fn resume_all(queue: tauri::State<'_, QueueManager>) -> Result<(), String> {
    queue.resume_all().await.map_err(|e| format!("{e:#}"))
}

/// Snapshot of all downloads in queue order (Req 12.3).
#[tauri::command]
pub async fn get_queue_state(
    queue: tauri::State<'_, QueueManager>,
) -> Result<Vec<DownloadItem>, String> {
    Ok(queue.get_queue_state().await)
}
