//! The download engine: probes a URL, then downloads it in parallel segments
//! (HTTP `Range` requests) when the server supports it, falling back to a single
//! stream otherwise. Progress is emitted to the UI via the `download-progress` event.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use anyhow::{anyhow, Result};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter};
use tokio::io::{AsyncSeekExt, AsyncWriteExt};
use tokio::sync::Mutex;

/// Number of parallel segments to split a download into.
const SEGMENTS: u64 = 4;

#[derive(Clone, Serialize, Deserialize)]
pub struct DownloadItem {
    pub id: String,
    pub url: String,
    pub filename: String,
    pub total_size: u64,
    pub downloaded: u64,
    /// queued | downloading | complete | error | paused
    pub status: String,
}

/// Shared, thread-safe registry of all downloads.
pub type Downloads = Arc<Mutex<HashMap<String, DownloadItem>>>;

/// Best-effort filename from a URL (strips query string).
pub fn filename_from_url(url: &str) -> String {
    url.rsplit('/')
        .next()
        .map(|s| s.split('?').next().unwrap_or(s))
        .filter(|s| !s.is_empty())
        .unwrap_or("download.bin")
        .to_string()
}

/// Where files are saved. TODO: make this user-configurable.
pub fn downloads_dir() -> PathBuf {
    dirs::download_dir().unwrap_or_else(std::env::temp_dir)
}

async fn emit(app: &AppHandle, downloads: &Downloads, id: &str) {
    if let Some(item) = downloads.lock().await.get(id).cloned() {
        let _ = app.emit("download-progress", item);
    }
}

/// Run a download to completion.
pub async fn run(app: AppHandle, downloads: Downloads, id: String) -> Result<()> {
    let (url, filename) = {
        let map = downloads.lock().await;
        let it = map.get(&id).ok_or_else(|| anyhow!("unknown download id"))?;
        (it.url.clone(), it.filename.clone())
    };

    let client = reqwest::Client::builder().build()?;

    // --- Probe: total size + range support ---
    let head = client.head(&url).send().await?;
    let total = head.content_length().unwrap_or(0);
    let supports_range = head
        .headers()
        .get(reqwest::header::ACCEPT_RANGES)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.eq_ignore_ascii_case("bytes"))
        .unwrap_or(false);

    {
        let mut map = downloads.lock().await;
        if let Some(it) = map.get_mut(&id) {
            it.total_size = total;
            it.status = "downloading".into();
        }
    }
    emit(&app, &downloads, &id).await;

    let dest = downloads_dir().join(&filename);
    let downloaded = Arc::new(AtomicU64::new(0));

    if supports_range && total > 0 {
        // Pre-allocate the destination file.
        let file = tokio::fs::File::create(&dest).await?;
        file.set_len(total).await?;
        drop(file);

        let seg = total / SEGMENTS;
        let mut tasks = Vec::new();
        for i in 0..SEGMENTS {
            let start = i * seg;
            let end = if i == SEGMENTS - 1 {
                total - 1
            } else {
                (i + 1) * seg - 1
            };
            let client = client.clone();
            let url = url.clone();
            let dest = dest.clone();
            let counter = downloaded.clone();
            tasks.push(tokio::spawn(async move {
                download_segment(client, url, dest, start, end, counter).await
            }));
        }

        // Periodically push progress to the UI while segments run.
        let reporter = {
            let app = app.clone();
            let downloads = downloads.clone();
            let id = id.clone();
            let counter = downloaded.clone();
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                    {
                        let mut map = downloads.lock().await;
                        if let Some(it) = map.get_mut(&id) {
                            it.downloaded = counter.load(Ordering::Relaxed);
                        }
                    }
                    emit(&app, &downloads, &id).await;
                }
            })
        };

        for t in tasks {
            t.await??;
        }
        reporter.abort();
    } else {
        // Fallback: single stream (no parallelism / size may be unknown).
        let resp = client.get(&url).send().await?;
        let mut file = tokio::fs::File::create(&dest).await?;
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            file.write_all(&chunk).await?;
            let done =
                downloaded.fetch_add(chunk.len() as u64, Ordering::Relaxed) + chunk.len() as u64;
            {
                let mut map = downloads.lock().await;
                if let Some(it) = map.get_mut(&id) {
                    it.downloaded = done;
                }
            }
            emit(&app, &downloads, &id).await;
        }
        file.flush().await?;
    }

    {
        let mut map = downloads.lock().await;
        if let Some(it) = map.get_mut(&id) {
            it.downloaded = it.total_size.max(it.downloaded);
            it.status = "complete".into();
        }
    }
    emit(&app, &downloads, &id).await;
    Ok(())
}

/// Download a single byte range into the correct offset of `dest`.
async fn download_segment(
    client: reqwest::Client,
    url: String,
    dest: PathBuf,
    start: u64,
    end: u64,
    counter: Arc<AtomicU64>,
) -> Result<()> {
    let resp = client
        .get(&url)
        .header(reqwest::header::RANGE, format!("bytes={start}-{end}"))
        .send()
        .await?;

    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .open(&dest)
        .await?;
    file.seek(std::io::SeekFrom::Start(start)).await?;

    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        file.write_all(&chunk).await?;
        counter.fetch_add(chunk.len() as u64, Ordering::Relaxed);
    }
    file.flush().await?;
    Ok(())
}
