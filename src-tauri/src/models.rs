//! Shared data models and types used across the download manager.
//!
//! All types use `#[serde(rename_all = "camelCase")]` for frontend (JS) compatibility.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

// ─── Download Status ───────────────────────────────────────────────────────────

/// The lifecycle status of a download.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DownloadStatus {
    #[default]
    Queued,
    Downloading,
    Paused,
    Complete,
    Error,
    Merging,
}

impl std::fmt::Display for DownloadStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::Queued => "queued",
            Self::Downloading => "downloading",
            Self::Paused => "paused",
            Self::Complete => "complete",
            Self::Error => "error",
            Self::Merging => "merging",
        };
        write!(f, "{s}")
    }
}

// ─── Segment Status ────────────────────────────────────────────────────────────

/// Status of an individual download segment.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SegmentStatus {
    #[default]
    Pending,
    Downloading,
    Complete,
    Error,
    Paused,
}

// ─── Segment State ─────────────────────────────────────────────────────────────

/// Tracks the state of a single byte-range segment within a download.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SegmentState {
    /// Zero-based index of this segment.
    pub index: u32,
    /// Start byte offset (inclusive).
    pub start: u64,
    /// End byte offset (inclusive).
    pub end: u64,
    /// Number of bytes downloaded so far in this segment.
    pub downloaded: u64,
    /// Current status of this segment.
    pub status: SegmentStatus,
}

// ─── Download Type ─────────────────────────────────────────────────────────────

/// The type/source of a download.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DownloadType {
    #[default]
    Http,
    Media,
    Batch,
}

// ─── Download Item ─────────────────────────────────────────────────────────────

/// Full state of a single download, emitted to the frontend via events.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DownloadItem {
    pub id: String,
    pub url: String,
    pub filename: String,
    pub total_size: u64,
    pub downloaded: u64,
    pub status: DownloadStatus,
    /// Auto-categorized label (e.g. "Videos", "Documents").
    pub category: Option<String>,
    /// Unix timestamp (seconds) when the download was created.
    pub created_at: u64,
    /// Unix timestamp (seconds) when the download completed.
    pub completed_at: Option<u64>,
    /// Current download speed in bytes/sec.
    pub speed: u64,
    /// Estimated seconds remaining (None if total_size unknown or speed is 0).
    pub eta: Option<u64>,
    /// Per-segment progress tracking.
    pub segments: Vec<SegmentState>,
    /// Error description when status is Error.
    pub error_message: Option<String>,
    /// Custom HTTP headers forwarded from browser capture.
    pub headers: HashMap<String, String>,
    /// Cookie header value forwarded from browser capture.
    pub cookies: Option<String>,
    /// Referer URL from the originating page.
    pub referer: Option<String>,
    /// Whether the server supports HTTP Range (resume).
    pub is_resumable: bool,
    /// The source type of this download.
    pub download_type: DownloadType,
    /// Number of segments this download is split into.
    pub segment_count: u32,
    /// For `Media` downloads, the yt-dlp format id selected by the user. Carried
    /// through the queue so the scheduler can dispatch to the media extractor.
    #[serde(default)]
    pub media_format_id: Option<String>,
    /// Final path of the downloaded file on disk, set on completion (and updated
    /// if the auto-categorizer moves it). Lets the UI open the file or reveal it
    /// in its folder. `None` until the download completes.
    #[serde(default)]
    pub output_path: Option<PathBuf>,
}

impl DownloadItem {
    /// Create a new download item with sensible defaults.
    pub fn new(id: String, url: String, filename: String) -> Self {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        Self {
            id,
            url,
            filename,
            total_size: 0,
            downloaded: 0,
            status: DownloadStatus::Queued,
            category: None,
            created_at: now,
            completed_at: None,
            speed: 0,
            eta: None,
            segments: Vec::new(),
            error_message: None,
            headers: HashMap::new(),
            cookies: None,
            referer: None,
            is_resumable: false,
            download_type: DownloadType::Http,
            segment_count: 4,
            media_format_id: None,
            output_path: None,
        }
    }
}

// ─── Paused State ──────────────────────────────────────────────────────────────

/// Snapshot of segment offsets captured when a download is paused.
/// Used to resume downloads from where they left off.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PausedState {
    /// The download ID this state belongs to.
    pub id: String,
    /// Total bytes downloaded at the time of pause.
    pub downloaded: u64,
    /// Per-segment byte offsets at the time of pause.
    pub segment_offsets: Vec<SegmentState>,
}

// ─── Download Config ───────────────────────────────────────────────────────────

/// Per-download configuration controlling parallelism and retry behavior.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DownloadConfig {
    /// Number of parallel segments (1-32).
    pub segments: u32,
    /// Speed limit in bytes/sec; 0 means unlimited.
    pub speed_limit: u64,
    /// Number of retry attempts per segment on failure.
    pub retry_count: u32,
    /// Delay between retries in milliseconds.
    pub retry_delay_ms: u64,
}

impl Default for DownloadConfig {
    fn default() -> Self {
        Self {
            segments: 4,
            speed_limit: 0,
            retry_count: 3,
            retry_delay_ms: 1000,
        }
    }
}

// ─── Queue Config ──────────────────────────────────────────────────────────────

/// Configuration for the download queue scheduler.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QueueConfig {
    /// Maximum number of concurrent active downloads (1-10).
    pub max_concurrent: usize,
    /// Maximum retry attempts for a failed download.
    pub max_retries: u32,
    /// Whether to automatically start queued downloads.
    pub auto_start: bool,
    /// Global speed limit in bytes/sec; 0 means unlimited.
    pub speed_limit_global: u64,
    /// Directory where downloaded files are saved. Seeded from
    /// `AppSettings.download_dir`; the queue tracks live updates via
    /// `QueueManager::set_download_dir`.
    #[serde(default = "default_download_dir")]
    pub download_dir: PathBuf,
}

/// Fallback download directory: the OS downloads folder, or the temp dir if it
/// cannot be resolved. Mirrors `downloader::downloads_dir`.
fn default_download_dir() -> PathBuf {
    dirs::download_dir().unwrap_or_else(std::env::temp_dir)
}

impl Default for QueueConfig {
    fn default() -> Self {
        Self {
            max_concurrent: 3,
            max_retries: 3,
            auto_start: true,
            speed_limit_global: 0,
            download_dir: default_download_dir(),
        }
    }
}

// ─── Type Aliases ──────────────────────────────────────────────────────────────

/// Shared, thread-safe registry of all downloads.
pub type Downloads = Arc<Mutex<HashMap<String, DownloadItem>>>;

/// Shared map of active download cancellation tokens, keyed by download ID.
pub type CancelTokens = Arc<Mutex<HashMap<String, tokio_util::sync::CancellationToken>>>;
