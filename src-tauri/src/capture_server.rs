//! A tiny localhost HTTP server that the browser extension posts captured
//! download URLs to. Listens on 127.0.0.1:53472.

use axum::{extract::State, routing::get, routing::post, Json, Router};
use serde::Deserialize;
use tauri::{AppHandle, Emitter};

use crate::downloader::{self, DownloadItem, Downloads};

/// Port the companion browser extension talks to.
pub const PORT: u16 = 53472;

#[derive(Clone)]
struct Ctx {
    app: AppHandle,
    downloads: Downloads,
}

#[derive(Deserialize)]
struct CaptureReq {
    url: String,
    filename: Option<String>,
}

pub async fn serve(app: AppHandle, downloads: Downloads) -> anyhow::Result<()> {
    let ctx = Ctx { app, downloads };

    let router = Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/capture", post(capture))
        // Allows the extension (different origin) to POST here.
        .layer(tower_http::cors::CorsLayer::permissive())
        .with_state(ctx);

    let listener = tokio::net::TcpListener::bind(("127.0.0.1", PORT)).await?;
    axum::serve(listener, router).await?;
    Ok(())
}

async fn capture(State(ctx): State<Ctx>, Json(req): Json<CaptureReq>) -> Json<DownloadItem> {
    let id = uuid::Uuid::new_v4().to_string();
    let filename = req
        .filename
        .unwrap_or_else(|| downloader::filename_from_url(&req.url));

    let item = DownloadItem {
        id: id.clone(),
        url: req.url.clone(),
        filename,
        total_size: 0,
        downloaded: 0,
        status: "queued".into(),
    };
    ctx.downloads.lock().await.insert(id.clone(), item.clone());
    let _ = ctx.app.emit("download-progress", item.clone());

    let app = ctx.app.clone();
    let downloads = ctx.downloads.clone();
    tauri::async_runtime::spawn(async move {
        let _ = downloader::run(app, downloads, id).await;
    });

    Json(item)
}
