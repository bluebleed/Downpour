//! A tiny localhost HTTP server that the browser extension posts captured
//! download URLs to. Listens on 127.0.0.1:53472.
//!
//! The extension and this server share the `http://127.0.0.1:53472` contract
//! (see `extension/background.js`). A capture request carries the download URL
//! plus optional request context (cookies, headers, referer, …) so that
//! authenticated/protected downloads succeed once handed to the engine.

use std::collections::HashMap;
use std::time::Duration;

use axum::extract::rejection::JsonRejection;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use tauri::AppHandle;
use tauri_plugin_notification::NotificationExt;

use crate::downloader;
use crate::models::{DownloadItem, DownloadType};
use crate::queue::QueueManager;

/// Port the companion browser extension talks to.
pub const PORT: u16 = 53472;

/// Maximum allowed length (in characters) of a capture URL (Req 9.2/9.4).
const MAX_URL_LEN: usize = 2048;

/// Number of times to retry binding the capture port after the first attempt
/// fails before giving up (Req 9.5). Five retries produce the backoff sequence
/// 1s, 2s, 4s, 8s, 16s.
const MAX_BIND_RETRIES: u32 = 5;

#[derive(Clone)]
struct Ctx {
    queue: QueueManager,
}

/// Payload POSTed by the browser extension to `/capture`.
///
/// Only `url` is required. Every other field is optional so the server can
/// "proceed with whatever is present" (Req 6.7). Field names map to the
/// extension's camelCase JSON (`pageUrl`, `mimeType`, `isMedia`).
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CaptureReq {
    /// The download URL (required).
    url: String,
    /// Suggested filename from the browser.
    #[serde(default)]
    filename: Option<String>,
    /// Cookie header value scoped to the download domain.
    #[serde(default)]
    cookies: Option<String>,
    /// Additional HTTP headers to forward (may include `Referer`).
    #[serde(default)]
    headers: HashMap<String, String>,
    /// Referer URL from the initiating page.
    #[serde(default)]
    referer: Option<String>,
    /// URL of the page that triggered the download.
    #[serde(default)]
    page_url: Option<String>,
    /// MIME type reported by the browser.
    #[serde(default)]
    mime_type: Option<String>,
    /// Declared/detected file size in bytes.
    #[serde(default)]
    filesize: Option<u64>,
    /// Whether the extension classified this as a media download.
    #[serde(default)]
    is_media: Option<bool>,
}

/// Success response for a captured download (Req 9.3).
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct CaptureResp {
    id: String,
    status: &'static str,
}

/// Error response describing why a capture request was rejected.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ErrorResp {
    error: String,
}

/// Validate a capture URL (Req 9.2, 9.4).
///
/// Pure and side-effect free so it can be unit/property tested. A URL is valid
/// iff it is non-empty, at most [`MAX_URL_LEN`] characters, and uses the `http`
/// or `https` scheme (case-insensitive). Any other scheme (`file:`,
/// `javascript:`, `data:`, `ftp:`, …) is rejected with a descriptive reason.
pub fn validate_capture_url(url: &str) -> Result<(), String> {
    let len = url.chars().count();
    if url.trim().is_empty() {
        return Err("missing or empty \"url\" field".to_string());
    }
    if len > MAX_URL_LEN {
        return Err(format!(
            "url exceeds the maximum length of {MAX_URL_LEN} characters (got {len})"
        ));
    }

    // The scheme is everything before the first ':'.
    let scheme = match url.split_once(':') {
        Some((scheme, _)) => scheme.to_ascii_lowercase(),
        None => {
            return Err(
                "url is missing a scheme; only http and https URLs are supported".to_string(),
            )
        }
    };

    match scheme.as_str() {
        "http" | "https" => Ok(()),
        other => Err(format!(
            "unsupported URL scheme \"{other}:\"; only http and https URLs are supported"
        )),
    }
}

/// Map captured request context onto a fresh [`DownloadItem`].
///
/// Pure and side-effect free so it can be unit/property tested without a
/// running server or queue. Every captured field is preserved verbatim on the
/// resulting item (Req 6.3): cookies, headers, and referer flow straight
/// through, the declared `filesize` becomes `total_size` (a value of `None`
/// leaves the default of 0, meaning "unknown"), and a media hint switches the
/// `download_type` to [`DownloadType::Media`]. These are exactly the fields the
/// [`crate::downloader`] engine later forwards onto every HTTP request via
/// `build_request` (Req 6.4).
#[allow(clippy::too_many_arguments)]
pub fn build_captured_item(
    id: String,
    url: String,
    filename: String,
    cookies: Option<String>,
    headers: HashMap<String, String>,
    referer: Option<String>,
    filesize: Option<u64>,
    is_media: bool,
) -> DownloadItem {
    let mut item = DownloadItem::new(id, url, filename);
    item.cookies = cookies;
    item.headers = headers;
    item.referer = referer;
    if let Some(size) = filesize {
        item.total_size = size;
    }
    if is_media {
        item.download_type = DownloadType::Media;
    }
    item
}

pub async fn serve(app: AppHandle, queue: QueueManager) -> anyhow::Result<()> {
    let ctx = Ctx { queue };

    let router = Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/capture", post(capture))
        // Allows the extension (different origin) to POST here.
        .layer(tower_http::cors::CorsLayer::permissive())
        .with_state(ctx);

    // Req 9.5: if the port is already in use, retry binding up to
    // MAX_BIND_RETRIES times with exponential backoff (1s, 2s, 4s, 8s, 16s).
    // After all retries are exhausted, notify the user and return the error.
    let listener = match bind_with_retry().await {
        Ok(listener) => listener,
        Err(e) => {
            notify_bind_failure(&app);
            return Err(e);
        }
    };

    axum::serve(listener, router).await?;
    Ok(())
}

/// Compute the backoff delay before the Nth bind retry (Req 9.5).
///
/// Pure and side-effect free so it can be unit tested. Retry index 0 is the
/// first retry. The delay is `2^retry` seconds, producing the sequence
/// 1s, 2s, 4s, 8s, 16s for retries 0..5.
pub fn bind_backoff_delay(retry: u32) -> Duration {
    let secs = 1u64.saturating_mul(2u64.saturating_pow(retry));
    Duration::from_secs(secs)
}

/// Attempt to bind the capture port, retrying with exponential backoff on
/// failure (Req 9.5).
///
/// Makes one initial attempt followed by up to [`MAX_BIND_RETRIES`] retries,
/// sleeping for [`bind_backoff_delay`] between each. Returns the bound listener
/// on success, or the last bind error after all retries are exhausted.
async fn bind_with_retry() -> anyhow::Result<tokio::net::TcpListener> {
    let mut attempt = 0u32;
    loop {
        match tokio::net::TcpListener::bind(("127.0.0.1", PORT)).await {
            Ok(listener) => return Ok(listener),
            Err(e) => {
                if attempt >= MAX_BIND_RETRIES {
                    return Err(anyhow::Error::new(e).context(format!(
                        "failed to bind capture server to 127.0.0.1:{PORT} after {} attempts",
                        MAX_BIND_RETRIES + 1
                    )));
                }
                let delay = bind_backoff_delay(attempt);
                eprintln!(
                    "capture server: port {PORT} unavailable ({e}); retrying in {}s \
                     (attempt {}/{})",
                    delay.as_secs(),
                    attempt + 1,
                    MAX_BIND_RETRIES
                );
                tokio::time::sleep(delay).await;
                attempt += 1;
            }
        }
    }
}

/// Notify the user via a system notification that the capture server could not
/// bind its port after all retries were exhausted (Req 9.5).
fn notify_bind_failure(app: &AppHandle) {
    let result = app
        .notification()
        .builder()
        .title("Downpour: capture server unavailable")
        .body(format!(
            "Port {PORT} is already in use, so browser capture is disabled. \
             Close the conflicting application and restart Downpour."
        ))
        .show();
    if let Err(e) = result {
        eprintln!("failed to show capture-server notification: {e:?}");
    }
}

/// Handle a `/capture` POST: validate the payload, build a [`DownloadItem`]
/// carrying the captured context, and enqueue it in the [`QueueManager`].
///
/// Responds with `{ id, status: "queued" }` on success (Req 9.3) or a JSON
/// error body on malformed input / invalid URL (Req 9.4, 9.6) without creating
/// a download.
async fn capture(
    State(ctx): State<Ctx>,
    payload: Result<Json<CaptureReq>, JsonRejection>,
) -> Response {
    // Reject malformed JSON or a missing "url" field with a descriptive error
    // (Req 9.6). axum's `Json` rejection covers both cases.
    let req = match payload {
        Ok(Json(req)) => req,
        Err(rejection) => {
            return error(
                StatusCode::BAD_REQUEST,
                format!("malformed capture request: {rejection}"),
            );
        }
    };

    // Validate the URL (scheme, length, non-empty) before creating anything.
    if let Err(reason) = validate_capture_url(&req.url) {
        return error(StatusCode::BAD_REQUEST, reason);
    }

    let id = uuid::Uuid::new_v4().to_string();
    let filename = req
        .filename
        .as_ref()
        .map(|f| f.trim())
        .filter(|f| !f.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| downloader::filename_from_url(&req.url));

    // Build the item, attaching whatever captured context is present (Req 6.3, 6.7).
    let item = build_captured_item(
        id,
        req.url.clone(),
        filename,
        req.cookies.clone(),
        req.headers.clone(),
        req.referer.clone(),
        req.filesize,
        req.is_media == Some(true),
    );
    // `page_url` and `mime_type` are accepted for forward-compatibility; the
    // current DownloadItem model has no dedicated fields for them, so they are
    // tolerated and ignored rather than rejected (Req 6.7).
    let _ = (&req.page_url, &req.mime_type);

    // Hand off to the queue manager instead of spawning a download directly.
    match ctx.queue.enqueue(item).await {
        Ok(id) => Json(CaptureResp {
            id,
            status: "queued",
        })
        .into_response(),
        Err(e) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("failed to enqueue download: {e}"),
        ),
    }
}

/// Build a JSON error [`Response`] with the given status code and message.
fn error(code: StatusCode, message: String) -> Response {
    (code, Json(ErrorResp { error: message })).into_response()
}

// ─── Tests ───────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn accepts_http_and_https() {
        assert!(validate_capture_url("http://example.com/file.zip").is_ok());
        assert!(validate_capture_url("https://example.com/file.zip").is_ok());
        assert!(validate_capture_url("https://sub.example.com/a/b/c.iso?x=1#frag").is_ok());
    }

    #[test]
    fn scheme_check_is_case_insensitive() {
        assert!(validate_capture_url("HTTP://example.com").is_ok());
        assert!(validate_capture_url("HTTPS://example.com").is_ok());
        assert!(validate_capture_url("HtTpS://example.com").is_ok());
    }

    #[test]
    fn rejects_disallowed_schemes() {
        for url in [
            "file:///etc/passwd",
            "javascript:alert(1)",
            "data:text/html,<h1>x</h1>",
            "ftp://example.com/file.zip",
            "ws://example.com",
        ] {
            let err = validate_capture_url(url).unwrap_err();
            assert!(
                err.contains("only http and https"),
                "expected scheme rejection for {url}, got: {err}"
            );
        }
    }

    #[test]
    fn rejects_url_without_scheme() {
        let err = validate_capture_url("example.com/file.zip").unwrap_err();
        assert!(err.contains("scheme"), "got: {err}");
    }

    #[test]
    fn rejects_empty_or_whitespace_url() {
        assert!(validate_capture_url("").is_err());
        assert!(validate_capture_url("   ").is_err());
        assert!(validate_capture_url("\t\n").is_err());
    }

    #[test]
    fn accepts_url_at_max_length() {
        let url = format!(
            "https://e.com/{}",
            "a".repeat(MAX_URL_LEN - "https://e.com/".len())
        );
        assert_eq!(url.chars().count(), MAX_URL_LEN);
        assert!(validate_capture_url(&url).is_ok());
    }

    #[test]
    fn rejects_url_over_max_length() {
        let url = format!("https://e.com/{}", "a".repeat(MAX_URL_LEN));
        assert!(url.chars().count() > MAX_URL_LEN);
        let err = validate_capture_url(&url).unwrap_err();
        assert!(err.contains("maximum length"), "got: {err}");
    }

    // ── Port bind backoff (Req 9.5) ──────────────────────────────────────────────

    #[test]
    fn bind_backoff_produces_exponential_sequence() {
        // Req 9.5: 1s, 2s, 4s, 8s, 16s for the five retries.
        assert_eq!(bind_backoff_delay(0), Duration::from_secs(1));
        assert_eq!(bind_backoff_delay(1), Duration::from_secs(2));
        assert_eq!(bind_backoff_delay(2), Duration::from_secs(4));
        assert_eq!(bind_backoff_delay(3), Duration::from_secs(8));
        assert_eq!(bind_backoff_delay(4), Duration::from_secs(16));
    }

    #[test]
    fn bind_backoff_total_wait_matches_requirement() {
        // The five configured retries wait 1+2+4+8+16 = 31 seconds in total.
        let total: u64 = (0..MAX_BIND_RETRIES)
            .map(|r| bind_backoff_delay(r).as_secs())
            .sum();
        assert_eq!(total, 31);
        assert_eq!(MAX_BIND_RETRIES, 5);
    }

    #[test]
    fn bind_backoff_is_monotonic_and_never_panics() {
        // Saturating arithmetic keeps large attempt counts well-defined.
        for retry in 0..64u32 {
            let current = bind_backoff_delay(retry);
            let next = bind_backoff_delay(retry + 1);
            assert!(next >= current, "backoff decreased at retry {retry}");
        }
    }

    // Property 16: URL scheme validation
    // For any URL string, the Capture_Server SHALL accept it if and only if the
    // scheme is http or https, rejecting all other schemes (file, javascript,
    // data, ftp, etc.), as well as schemeless or over-length URLs.
    // **Validates: Requirement 9.4**

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(512))]

        /// Property 16: any well-formed `http://` or `https://` URL whose total
        /// length is within bounds is accepted (case-insensitive scheme).
        #[test]
        fn prop_accepts_http_and_https_within_bounds(
            scheme in prop_oneof!["http", "https", "HTTP", "HTTPS", "HtTp", "HtTpS"],
            // Keep the rest short enough that the full URL stays <= MAX_URL_LEN.
            rest in "[A-Za-z0-9./?=&#:_~%-]{0,512}",
        ) {
            let url = format!("{scheme}://{rest}");
            prop_assume!(url.chars().count() <= MAX_URL_LEN);
            prop_assert!(
                validate_capture_url(&url).is_ok(),
                "expected accept for {url:?}"
            );
        }

        /// Property 16: any URL whose scheme is neither http nor https is
        /// rejected (regardless of the rest of the URL).
        #[test]
        fn prop_rejects_non_http_schemes(
            scheme in "[A-Za-z][A-Za-z0-9+.-]{0,12}",
            rest in "[A-Za-z0-9./?=&#:_~%-]{0,256}",
        ) {
            let lower = scheme.to_ascii_lowercase();
            prop_assume!(lower != "http" && lower != "https");
            let url = format!("{scheme}:{rest}");
            prop_assume!(url.chars().count() <= MAX_URL_LEN);
            prop_assert!(
                validate_capture_url(&url).is_err(),
                "expected reject for non-http scheme {url:?}"
            );
        }

        /// Property 16: a URL with no scheme separator (`:`) is rejected.
        #[test]
        fn prop_rejects_schemeless_urls(
            body in "[A-Za-z0-9./?=&#_~%-]{1,256}",
        ) {
            // No ':' means no scheme; ensure the generator never produced one.
            prop_assume!(!body.contains(':'));
            prop_assume!(!body.trim().is_empty());
            prop_assert!(
                validate_capture_url(&body).is_err(),
                "expected reject for schemeless url {body:?}"
            );
        }

        /// Property 16: an otherwise-valid http/https URL that exceeds the
        /// maximum length is rejected.
        #[test]
        fn prop_rejects_over_length_urls(
            extra in 1usize..2048,
        ) {
            let prefix = "https://e.com/";
            let pad = MAX_URL_LEN - prefix.chars().count() + extra;
            let url = format!("{prefix}{}", "a".repeat(pad));
            prop_assert!(url.chars().count() > MAX_URL_LEN);
            prop_assert!(
                validate_capture_url(&url).is_err(),
                "expected reject for over-length url of {} chars",
                url.chars().count()
            );
        }
    }
}
