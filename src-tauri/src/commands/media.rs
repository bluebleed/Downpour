use crate::media_extractor::{MediaInfo, PlaylistInfo};
use crate::models::{DownloadItem, DownloadType};
use crate::queue::QueueManager;
use crate::MediaState;

/// Extract media metadata (formats, title, …) without downloading (Req 8.1).
#[tauri::command]
pub async fn extract_media_info(
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
pub async fn start_media_download(
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
pub async fn extract_playlist_info(
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
pub struct BatchEntry {
    pub url: String,
    pub title: String,
    pub index: u32,
}

/// Enqueue many playlist entries at once. Each becomes a `Media` download with
/// the shared `format_selector` (a yt-dlp format expression, e.g. a quality
/// preset). When `index_prefix` is set the file is named `NN - <title>` so the
/// playlist order is preserved on disk. Returns the enqueued items.
#[tauri::command]
pub async fn start_media_batch(
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
pub async fn cancel_media_download(
    queue: tauri::State<'_, QueueManager>,
    id: String,
) -> Result<(), String> {
    queue.cancel(&id).await.map_err(|e| format!("{e:#}"))
}
