use crate::models::Downloads;
use crate::queue::QueueManager;
use std::path::{Path, PathBuf};
use tauri::Manager;

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
            // CRT argv splitter — paths with spaces (e.g. "John Doe") break the
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
pub async fn open_download_file(
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
pub async fn reveal_download_file(
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
pub async fn delete_download_file(
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

#[tauri::command]
pub async fn hide_window(app: tauri::AppHandle) -> Result<(), String> {
    if let Some(window) = app.get_webview_window("main") {
        window.hide().map_err(|e| e.to_string())?;
    }
    Ok(())
}
