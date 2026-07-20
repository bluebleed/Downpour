//! Media extraction via yt-dlp and ffmpeg for permitted video/image downloads.
//!
//! Orchestrates external processes for metadata extraction and media downloading
//! while respecting the responsible-use boundary.
//!
//! # Responsible-use boundary
//!
//! This module NEVER passes flags to yt-dlp whose purpose is to bypass DRM,
//! geo-restrictions, or to harvest browser cookies to access content the user
//! is not entitled to. The forbidden-flags guard ([`contains_forbidden_flag`])
//! is a hard requirement (Requirement 8.6) and every constructed command is
//! validated through [`MediaExtractor::validate_args`] before spawning.
//!
//! The progress parser ([`parse_progress_line`]) and the forbidden-flags guard
//! are kept as pure, side-effect-free functions so they can be unit- and
//! property-tested without spawning any external processes.
#![allow(dead_code)]

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc::Sender, Mutex};
use tokio_util::sync::CancellationToken;

// ─── Constants ──────────────────────────────────────────────────────────────

/// Timeout applied to the `--dump-json` info-extraction process (Requirement 8.1).
const INFO_TIMEOUT: Duration = Duration::from_secs(30);

/// Timeout for enumerating a playlist/channel (flat, so fast even when large).
const PLAYLIST_TIMEOUT: Duration = Duration::from_secs(60);

/// Maximum progress events emitted per second per media download (Requirement 8.3).
const MAX_PROGRESS_EVENTS_PER_SEC: u64 = 3;

/// Minimum interval between forwarded progress events (333ms ⇒ 3/sec).
const PROGRESS_INTERVAL: Duration = Duration::from_millis(1000 / MAX_PROGRESS_EVENTS_PER_SEC);

/// Grace period to wait for a child to exit after SIGTERM before force-killing,
/// and the bound within which no orphan child may remain (Requirements 8.4, 8.8).
const TERMINATE_GRACE: Duration = Duration::from_secs(5);

/// Number of trailing stderr lines retained for error reporting (Requirement 8.7).
const STDERR_TAIL_LINES: usize = 5;

/// Flags that must never be passed to yt-dlp.
///
/// - `--allow-unplayable-formats` — DRM-bypass.
/// - `--cookies-from-browser` — credential extraction from the user's browser.
/// - `--geo-bypass*` — geo-restriction bypass.
///
/// **Validates: Requirement 8.6**
pub const FORBIDDEN_FLAGS: &[&str] = &[
    "--allow-unplayable-formats",
    "--cookies-from-browser",
    "--geo-bypass",
    "--geo-bypass-country",
    "--geo-bypass-ip-block",
];

// ─── Data models ────────────────────────────────────────────────────────────

/// Availability of the external binaries the extractor depends on.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExtractorStatus {
    /// Whether the yt-dlp binary exists at the configured path.
    pub ytdlp_available: bool,
    /// Whether the ffmpeg binary exists at the configured path.
    pub ffmpeg_available: bool,
}

impl ExtractorStatus {
    /// Both binaries are present and the extractor is ready to use.
    pub fn is_ready(&self) -> bool {
        self.ytdlp_available && self.ffmpeg_available
    }

    /// A human-readable error (with setup instructions) describing which binary
    /// is missing, or `None` when both are available (Requirement 8.5).
    pub fn missing_binary_error(&self) -> Option<String> {
        let mut parts: Vec<String> = Vec::new();
        if !self.ytdlp_available {
            parts.push(
                "yt-dlp (install from https://github.com/yt-dlp/yt-dlp#installation)".to_string(),
            );
        }
        if !self.ffmpeg_available {
            parts.push("ffmpeg (install from https://ffmpeg.org/download.html)".to_string());
        }

        if parts.is_empty() {
            return None;
        }

        Some(format!(
            "Missing required {}: {}. Install the missing binary and set its path in Settings.",
            if parts.len() == 1 {
                "binary"
            } else {
                "binaries"
            },
            parts.join("; "),
        ))
    }
}

/// Metadata for a single media item, returned by [`MediaExtractor::extract_info`].
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MediaInfo {
    pub title: String,
    pub thumbnail: Option<String>,
    /// Duration in whole seconds, when known.
    pub duration: Option<u64>,
    pub formats: Vec<MediaFormat>,
    /// The platform/extractor name reported by yt-dlp (e.g. "Youtube").
    pub platform: String,
}

/// A single selectable format/quality for a media item.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MediaFormat {
    pub format_id: String,
    pub ext: String,
    /// Human-readable quality label (e.g. "1080p", "audio only").
    pub quality: String,
    pub filesize: Option<u64>,
    pub has_video: bool,
    pub has_audio: bool,
}

/// A single entry in a playlist/channel, from a flat (no per-video formats)
/// enumeration. The `url` is the canonical per-video URL yt-dlp can re-fetch.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlaylistEntry {
    pub url: String,
    pub title: String,
    /// Duration in whole seconds, when known.
    pub duration: Option<u64>,
    /// 1-based position in the playlist.
    pub index: u32,
}

/// A playlist/channel and its entries, from a flat enumeration.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlaylistInfo {
    pub title: String,
    pub uploader: String,
    pub entries: Vec<PlaylistEntry>,
}

/// A throttled progress update parsed from yt-dlp stdout.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MediaProgress {
    /// Download completion percentage in `[0.0, 100.0]`.
    pub percent: f64,
    /// Current speed in bytes/sec, when reported.
    pub speed_bps: Option<u64>,
    /// Estimated seconds remaining, when reported.
    pub eta_secs: Option<u64>,
    /// Total size in bytes, when reported.
    pub total_bytes: Option<u64>,
}

// ─── Pure helpers (unit/property testable, no I/O) ────────────────────────────

/// Returns `true` if `arg` is one of the forbidden flags.
///
/// Handles the `--flag=value` form by comparing only the part before `=`.
///
/// **Validates: Requirement 8.6**
pub fn is_forbidden_flag(arg: &str) -> bool {
    let normalized = arg.split('=').next().unwrap_or(arg);
    FORBIDDEN_FLAGS.contains(&normalized)
}

/// Returns `true` if any argument in `args` is a forbidden flag.
///
/// **Validates: Requirement 8.6**
pub fn contains_forbidden_flag<S: AsRef<str>>(args: &[S]) -> bool {
    args.iter().any(|a| is_forbidden_flag(a.as_ref()))
}

/// Parse a human-readable size token (e.g. `"2.50MiB"`, `"1.00GiB"`, `"512B"`)
/// into a byte count. Returns `None` for `"Unknown"`, `"NA"`, or unparseable
/// input.
pub fn parse_size_to_bytes(token: &str) -> Option<u64> {
    let token = token.trim();
    if token.is_empty()
        || token.eq_ignore_ascii_case("unknown")
        || token.eq_ignore_ascii_case("na")
        || token == "~"
    {
        return None;
    }

    // Split the numeric prefix from the (optional) unit suffix.
    let split = token
        .find(|c: char| c.is_ascii_alphabetic())
        .unwrap_or(token.len());
    let (num, unit) = token.split_at(split);
    let value: f64 = num.trim().parse().ok()?;
    if !value.is_finite() || value < 0.0 {
        return None;
    }

    let mult: f64 = match unit.trim() {
        "" | "B" => 1.0,
        "KiB" => 1024.0,
        "MiB" => 1024.0 * 1024.0,
        "GiB" => 1024.0 * 1024.0 * 1024.0,
        "TiB" => 1024.0_f64.powi(4),
        "KB" | "kB" => 1_000.0,
        "MB" => 1_000_000.0,
        "GB" => 1_000_000_000.0,
        "TB" => 1_000_000_000_000.0,
        _ => return None,
    };

    Some((value * mult) as u64)
}

/// Parse a yt-dlp ETA token (`"MM:SS"` or `"HH:MM:SS"`) into seconds.
/// Returns `None` for `"Unknown"`, `"NA"`, or unparseable input.
pub fn parse_eta(token: &str) -> Option<u64> {
    let token = token.trim();
    if !token.contains(':') {
        return None;
    }

    let mut secs: u64 = 0;
    for part in token.split(':') {
        let v: u64 = part.trim().parse().ok()?;
        secs = secs.checked_mul(60)?.checked_add(v)?;
    }
    Some(secs)
}

/// Parse a single yt-dlp progress line into a [`MediaProgress`].
///
/// Recognizes lines of the form:
/// `[download]  10.5% of  100.00MiB at  2.50MiB/s ETA 00:42`
///
/// Returns `None` for any line that is not a `[download]` progress line or that
/// carries no percentage. Speed and ETA are optional (yt-dlp may report
/// `"Unknown"`), in which case they are `None`.
///
/// **Validates: Requirement 8.3 (Property 14)**
pub fn parse_progress_line(line: &str) -> Option<MediaProgress> {
    let line = line.trim();
    let rest = line.strip_prefix("[download]")?;

    let tokens: Vec<&str> = rest.split_whitespace().collect();

    let mut percent: Option<f64> = None;
    let mut speed_bps: Option<u64> = None;
    let mut eta_secs: Option<u64> = None;
    let mut total_bytes: Option<u64> = None;

    if let Some(of_idx) = tokens.iter().position(|&t| t == "of") {
        if let Some(size_tok) = tokens.get(of_idx + 1) {
            let normalized = size_tok.strip_prefix('~').unwrap_or(size_tok);
            total_bytes = parse_size_to_bytes(normalized);
        }
    }

    for (i, tok) in tokens.iter().enumerate() {
        if percent.is_none() {
            if let Some(num) = tok.strip_suffix('%') {
                if let Ok(p) = num.parse::<f64>() {
                    if p.is_finite() {
                        percent = Some(p.clamp(0.0, 100.0));
                    }
                }
            }
        }
        if let Some(num) = tok.strip_suffix("/s") {
            speed_bps = parse_size_to_bytes(num);
        }
        if *tok == "ETA" {
            if let Some(next) = tokens.get(i + 1) {
                eta_secs = parse_eta(next);
            }
        }
    }

    percent.map(|percent| MediaProgress {
        percent,
        speed_bps,
        eta_secs,
        total_bytes,
    })
}

/// A filesystem path yt-dlp announced for the output file, classified by how
/// authoritative it is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OutputLine {
    /// A per-stream or single-file destination (`[download] Destination: …`).
    Destination(String),
    /// The definitive merged / post-processed output, which supersedes any
    /// per-stream destination (`[Merger] Merging formats into …` or
    /// `[ExtractAudio] Destination: …`).
    Final(String),
}

/// Parse a yt-dlp stdout line that announces the output file path, if any.
///
/// Recognizes the destination, merge, and "already downloaded" lines so callers
/// can learn the real on-disk filename (yt-dlp names files from an output
/// template like `%(title)s.%(ext)s`, so the final name isn't known up front).
pub fn parse_output_line(line: &str) -> Option<OutputLine> {
    let line = line.trim();

    if let Some(rest) = line.strip_prefix("[Merger] Merging formats into ") {
        let path = rest.trim().trim_matches('"');
        if !path.is_empty() {
            return Some(OutputLine::Final(path.to_string()));
        }
    }
    if let Some(rest) = line.strip_prefix("[ExtractAudio] Destination: ") {
        let path = rest.trim();
        if !path.is_empty() {
            return Some(OutputLine::Final(path.to_string()));
        }
    }
    if let Some(rest) = line.strip_prefix("[download] Destination: ") {
        let path = rest.trim();
        if !path.is_empty() {
            return Some(OutputLine::Destination(path.to_string()));
        }
    }
    if let Some(rest) = line.strip_prefix("[download] ") {
        if let Some(path) = rest.strip_suffix(" has already been downloaded") {
            let path = path.trim();
            if !path.is_empty() {
                return Some(OutputLine::Destination(path.to_string()));
            }
        }
    }
    None
}

/// Map a yt-dlp format JSON object into a [`MediaFormat`].
fn format_from_json(v: &serde_json::Value) -> Option<MediaFormat> {
    let format_id = v.get("format_id")?.as_str()?.to_string();
    let ext = v
        .get("ext")
        .and_then(|e| e.as_str())
        .unwrap_or("")
        .to_string();

    let vcodec = v.get("vcodec").and_then(|c| c.as_str()).unwrap_or("none");
    let acodec = v.get("acodec").and_then(|c| c.as_str()).unwrap_or("none");
    let has_video = vcodec != "none" && !vcodec.is_empty();
    let has_audio = acodec != "none" && !acodec.is_empty();

    let quality = v
        .get("format_note")
        .and_then(|n| n.as_str())
        .filter(|n| !n.is_empty())
        .map(String::from)
        .or_else(|| {
            v.get("height")
                .and_then(|h| h.as_u64())
                .map(|h| format!("{h}p"))
        })
        .unwrap_or_else(|| match (has_video, has_audio) {
            (false, true) => "audio only".to_string(),
            (true, false) => "video only".to_string(),
            _ => "unknown".to_string(),
        });

    let filesize = v
        .get("filesize")
        .and_then(|s| s.as_u64())
        .or_else(|| v.get("filesize_approx").and_then(|s| s.as_u64()));

    Some(MediaFormat {
        format_id,
        ext,
        quality,
        filesize,
        has_video,
        has_audio,
    })
}

/// Parse the JSON object produced by `yt-dlp --dump-json` into [`MediaInfo`].
fn media_info_from_json(json: &str) -> Result<MediaInfo> {
    let trimmed = json.trim();
    if trimmed.is_empty() {
        return Err(anyhow!("yt-dlp produced no JSON output"));
    }

    // yt-dlp emits one compact JSON object per line (JSONL). Try the whole blob
    // first (handles a single pretty-printed object), then fall back to the
    // first non-empty line (handles multi-line JSONL where each line is an item).
    let v: serde_json::Value = serde_json::from_str(trimmed).or_else(|_| {
        let line = trimmed
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .ok_or_else(|| anyhow!("yt-dlp produced no JSON output"))?;
        serde_json::from_str(line).context("failed to parse yt-dlp --dump-json output")
    })?;

    let title = v
        .get("title")
        .and_then(|t| t.as_str())
        .unwrap_or("Untitled")
        .to_string();

    let thumbnail = v
        .get("thumbnail")
        .and_then(|t| t.as_str())
        .map(String::from);

    let duration = v
        .get("duration")
        .and_then(|d| d.as_f64())
        .filter(|d| d.is_finite() && *d >= 0.0)
        .map(|d| d as u64);

    let platform = v
        .get("extractor_key")
        .or_else(|| v.get("extractor"))
        .and_then(|e| e.as_str())
        .unwrap_or("generic")
        .to_string();

    let formats = v
        .get("formats")
        .and_then(|f| f.as_array())
        .map(|arr| arr.iter().filter_map(format_from_json).collect())
        .unwrap_or_default();

    Ok(MediaInfo {
        title,
        thumbnail,
        duration,
        formats,
        platform,
    })
}

/// Parse the JSON-Lines output of `yt-dlp --flat-playlist --dump-json` into a
/// [`PlaylistInfo`]. Each non-empty line is one entry; playlist-level title and
/// uploader are read from the entries (yt-dlp repeats them on each line).
/// Lines that don't parse or lack a usable URL are skipped.
pub fn playlist_from_jsonl(stdout: &str) -> PlaylistInfo {
    let mut entries = Vec::new();
    let mut title = String::new();
    let mut uploader = String::new();

    for (i, line) in stdout.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let v: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        if title.is_empty() {
            if let Some(t) = v
                .get("playlist_title")
                .or_else(|| v.get("playlist"))
                .and_then(|x| x.as_str())
                .filter(|s| !s.is_empty())
            {
                title = t.to_string();
            }
        }
        if uploader.is_empty() {
            if let Some(u) = v
                .get("playlist_uploader")
                .or_else(|| v.get("uploader"))
                .and_then(|x| x.as_str())
                .filter(|s| !s.is_empty())
            {
                uploader = u.to_string();
            }
        }

        let url = match v
            .get("url")
            .or_else(|| v.get("webpage_url"))
            .and_then(|x| x.as_str())
            .filter(|s| !s.is_empty())
        {
            Some(u) => u.to_string(),
            None => continue,
        };
        let entry_title = v
            .get("title")
            .and_then(|x| x.as_str())
            .filter(|s| !s.is_empty())
            .unwrap_or("Untitled")
            .to_string();
        let duration = v
            .get("duration")
            .and_then(|x| x.as_f64())
            .filter(|d| d.is_finite() && *d >= 0.0)
            .map(|d| d as u64);
        let index = v
            .get("playlist_index")
            .and_then(|x| x.as_u64())
            .map(|x| x as u32)
            .unwrap_or((i + 1) as u32);

        entries.push(PlaylistEntry {
            url,
            title: entry_title,
            duration,
            index,
        });
    }

    PlaylistInfo {
        title,
        uploader,
        entries,
    }
}

// ─── MediaExtractor ──────────────────────────────────────────────────────────

/// Orchestrates yt-dlp and ffmpeg for permitted media downloads.
#[derive(Clone, Debug)]
pub struct MediaExtractor {
    ytdlp_path: PathBuf,
    ffmpeg_path: PathBuf,
}

impl MediaExtractor {
    /// Create an extractor from explicit binary paths.
    pub fn new(ytdlp_path: PathBuf, ffmpeg_path: PathBuf) -> Self {
        Self {
            ytdlp_path,
            ffmpeg_path,
        }
    }

    /// Verify that both yt-dlp and ffmpeg binaries are available (Requirement 8.5).
    ///
    /// An explicit path is checked directly; a bare command name (e.g. `yt-dlp`)
    /// is resolved against `PATH`, mirroring how the process is actually spawned
    /// — so binaries installed on `PATH` are detected without a configured path.
    pub async fn check_availability(&self) -> ExtractorStatus {
        ExtractorStatus {
            ytdlp_available: binary_available(&self.ytdlp_path),
            ffmpeg_available: binary_available(&self.ffmpeg_path),
        }
    }

    /// Build the argument list for `--dump-json` info extraction.
    ///
    /// Cookies (when provided) are forwarded via a `Cookie:` HTTP header using
    /// `--add-header` — never via `--cookies-from-browser`.
    fn build_info_args(&self, url: &str, cookies: Option<&str>) -> Result<Vec<String>> {
        let mut args: Vec<String> = vec![
            "--dump-json".into(),
            "--no-playlist".into(),
            "--no-warnings".into(),
            "--no-progress".into(),
        ];
        Self::push_cookie_header(&mut args, cookies);
        args.push(url.to_string());
        Self::validate_args(&args)?;
        Ok(args)
    }

    /// Build the argument list for a format download.
    ///
    /// Cookies (when provided) are forwarded via a `Cookie:` HTTP header using
    /// `--add-header` — never via `--cookies-from-browser`.
    fn build_download_args(
        &self,
        url: &str,
        format_id: &str,
        output_path: &Path,
        path_out_file: &Path,
        cookies: Option<&str>,
    ) -> Result<Vec<String>> {
        let mut args: Vec<String> = vec![
            "-f".into(),
            format_id.to_string(),
            "-o".into(),
            output_path.to_string_lossy().into_owned(),
            "--no-playlist".into(),
            "--newline".into(),
            // When a separate video + audio stream are merged, prefer an MP4
            // container so the result is broadly playable (e.g. on Windows)
            // rather than the WebM yt-dlp would otherwise pick for Opus audio.
            "--merge-output-format".into(),
            "mp4".into(),
            "--ffmpeg-location".into(),
            self.ffmpeg_path.to_string_lossy().into_owned(),
            // Have yt-dlp write the final post-processed path to a file. This is
            // the authoritative filename (Unicode-safe, unlike parsing stdout on
            // Windows where the console encoding can mangle non-ASCII titles).
            "--print-to-file".into(),
            "after_move:filepath".into(),
            path_out_file.to_string_lossy().into_owned(),
        ];
        Self::push_cookie_header(&mut args, cookies);
        args.push(url.to_string());
        Self::validate_args(&args)?;
        Ok(args)
    }

    /// Append a `Cookie:` request header via `--add-header` when cookies are
    /// supplied and non-empty.
    fn push_cookie_header(args: &mut Vec<String>, cookies: Option<&str>) {
        if let Some(cookies) = cookies {
            let cookies = cookies.trim();
            if !cookies.is_empty() {
                args.push("--add-header".into());
                args.push(format!("Cookie:{cookies}"));
            }
        }
    }

    /// Defense-in-depth guard: reject any command containing a forbidden flag
    /// before it is ever spawned (Requirement 8.6).
    fn validate_args(args: &[String]) -> Result<()> {
        if let Some(flag) = args.iter().find(|a| is_forbidden_flag(a)) {
            return Err(anyhow!(
                "refusing to run yt-dlp with forbidden flag: {flag}"
            ));
        }
        Ok(())
    }

    /// Extract media metadata without downloading content (Requirement 8.1).
    ///
    /// Spawns `yt-dlp --dump-json` with a 30-second timeout. On a non-zero exit
    /// the last few stderr lines are included in the error (Requirement 8.7).
    pub async fn extract_info(&self, url: &str, cookies: Option<&str>) -> Result<MediaInfo> {
        let status = self.check_availability().await;
        if !status.ytdlp_available {
            return Err(anyhow!(ExtractorStatus {
                ytdlp_available: false,
                ffmpeg_available: status.ffmpeg_available,
            }
            .missing_binary_error()
            .unwrap_or_else(|| "yt-dlp is unavailable".to_string())));
        }

        let args = self.build_info_args(url, cookies)?;
        let child = self.spawn(&args)?;

        // kill_on_drop ensures the child is reaped if the timeout future is
        // dropped, so no orphan process can outlive this call (Requirement 8.4).
        let output = match tokio::time::timeout(INFO_TIMEOUT, child.wait_with_output()).await {
            Ok(out) => out.context("failed to run yt-dlp for info extraction")?,
            Err(_) => {
                return Err(anyhow!(
                    "yt-dlp info extraction timed out after {}s",
                    INFO_TIMEOUT.as_secs()
                ));
            }
        };

        if !output.status.success() {
            let tail = last_lines(&String::from_utf8_lossy(&output.stderr), STDERR_TAIL_LINES);
            return Err(anyhow!(
                "yt-dlp info extraction failed ({}): {}",
                output.status,
                tail
            ));
        }

        media_info_from_json(&String::from_utf8_lossy(&output.stdout))
    }

    /// Enumerate a playlist/channel without fetching per-video formats (fast even
    /// for large lists). When `limit` is set, only the first `limit` entries are
    /// returned (`--playlist-end`), which the UI uses for the soft cap.
    pub async fn extract_playlist(
        &self,
        url: &str,
        cookies: Option<&str>,
        limit: Option<usize>,
    ) -> Result<PlaylistInfo> {
        let status = self.check_availability().await;
        if !status.ytdlp_available {
            return Err(anyhow!(ExtractorStatus {
                ytdlp_available: false,
                ffmpeg_available: status.ffmpeg_available,
            }
            .missing_binary_error()
            .unwrap_or_else(|| "yt-dlp is unavailable".to_string())));
        }

        let mut args: Vec<String> = vec![
            "--flat-playlist".into(),
            "--dump-json".into(),
            "--no-warnings".into(),
            "--no-progress".into(),
        ];
        if let Some(n) = limit {
            args.push("--playlist-end".into());
            args.push(n.to_string());
        }
        Self::push_cookie_header(&mut args, cookies);
        args.push(url.to_string());
        Self::validate_args(&args)?;

        let child = self.spawn(&args)?;
        let output = match tokio::time::timeout(PLAYLIST_TIMEOUT, child.wait_with_output()).await {
            Ok(out) => out.context("failed to run yt-dlp for playlist extraction")?,
            Err(_) => {
                return Err(anyhow!(
                    "yt-dlp playlist extraction timed out after {}s",
                    PLAYLIST_TIMEOUT.as_secs()
                ));
            }
        };

        if !output.status.success() {
            let tail = last_lines(&String::from_utf8_lossy(&output.stderr), STDERR_TAIL_LINES);
            return Err(anyhow!(
                "yt-dlp playlist extraction failed ({}): {}",
                output.status,
                tail
            ));
        }

        let info = playlist_from_jsonl(&String::from_utf8_lossy(&output.stdout));
        if info.entries.is_empty() {
            return Err(anyhow!(
                "no playlist entries found (is this a playlist URL?)"
            ));
        }
        Ok(info)
    }

    /// Download the selected format to `output_path`, forwarding throttled
    /// progress updates over `progress_tx` (Requirements 8.2, 8.3).
    ///
    /// On cancellation the child process tree is terminated with SIGTERM, then
    /// force-killed after a 5s grace period, and partial files are removed
    /// (Requirement 8.8). On a non-zero exit the last 5 stderr lines are
    /// reported and partial files are cleaned up (Requirement 8.7).
    /// Returns the final on-disk path yt-dlp wrote (parsed from its output), or
    /// `None` if it could not be determined.
    pub async fn download(
        &self,
        url: &str,
        format_id: &str,
        output_path: &Path,
        progress_tx: Sender<MediaProgress>,
        cancel: CancellationToken,
    ) -> Result<Option<PathBuf>> {
        let status = self.check_availability().await;
        if let Some(err) = status.missing_binary_error() {
            return Err(anyhow!(err));
        }

        // A throwaway file yt-dlp writes the final output path into (see
        // `build_download_args`). Unique per run so concurrent downloads don't
        // clobber each other's result.
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let path_out_file = std::env::temp_dir().join(format!(
            "downpour-ytpath-{}-{}.txt",
            std::process::id(),
            unique
        ));

        let args = self.build_download_args(url, format_id, output_path, &path_out_file, None)?;
        let mut child = self.spawn(&args)?;

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("failed to capture yt-dlp stdout"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| anyhow!("failed to capture yt-dlp stderr"))?;

        // Retain only the last few stderr lines for error reporting.
        let stderr_tail: Arc<Mutex<VecDeque<String>>> = Arc::new(Mutex::new(VecDeque::new()));
        let stderr_tail2 = stderr_tail.clone();
        let stderr_task = tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let mut q = stderr_tail2.lock().await;
                if q.len() == STDERR_TAIL_LINES {
                    q.pop_front();
                }
                q.push_back(line);
            }
        });

        // Read stdout progress lines, throttling forwarded events to 3/sec, and
        // track the output path yt-dlp announces so the caller learns the real
        // filename (a `[Merger]`/`[ExtractAudio]` line supersedes `Destination`).
        let mut lines = BufReader::new(stdout).lines();
        let mut last_emit: Option<Instant> = None;
        let mut destination: Option<String> = None;
        let mut final_output: Option<String> = None;

        loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    terminate_child(&mut child).await;
                    stderr_task.abort();
                    cleanup_partial(output_path).await;
                    let _ = tokio::fs::remove_file(&path_out_file).await;
                    return Err(anyhow!("media download cancelled"));
                }
                next = lines.next_line() => {
                    match next {
                        Ok(Some(line)) => {
                            if let Some(progress) = parse_progress_line(&line) {
                                let now = Instant::now();
                                let due = last_emit
                                    .map(|t| now.duration_since(t) >= PROGRESS_INTERVAL)
                                    .unwrap_or(true);
                                if due {
                                    last_emit = Some(now);
                                    // A closed receiver should not abort the download.
                                    let _ = progress_tx.send(progress).await;
                                }
                            } else if let Some(out) = parse_output_line(&line) {
                                match out {
                                    OutputLine::Final(p) => final_output = Some(p),
                                    OutputLine::Destination(p) => destination = Some(p),
                                }
                            }
                        }
                        Ok(None) => break, // EOF: process closed stdout
                        Err(_) => break,
                    }
                }
            }
        }

        // Wait for yt-dlp to finish. After stdout closes it may still be
        // post-processing — e.g. ffmpeg merging the chosen video stream with the
        // best audio — which routinely takes far longer than the SIGTERM
        // kill-grace. Wait without that tight bound while still honouring
        // cancellation, so a merge is never killed mid-flight (Requirements 8.4, 8.8).
        let exit = tokio::select! {
            _ = cancel.cancelled() => {
                terminate_child(&mut child).await;
                stderr_task.abort();
                cleanup_partial(output_path).await;
                return Err(anyhow!("media download cancelled"));
            }
            res = child.wait() => res.context("failed waiting for yt-dlp to exit")?,
        };

        let _ = stderr_task.await;

        if !exit.success() {
            let tail = {
                let q = stderr_tail.lock().await;
                q.iter().cloned().collect::<Vec<_>>().join("\n")
            };
            cleanup_partial(output_path).await;
            let _ = tokio::fs::remove_file(&path_out_file).await;
            return Err(anyhow!("yt-dlp exited with {}: {}", exit, tail));
        }

        // Prefer the path yt-dlp wrote to the print-to-file (authoritative and
        // Unicode-safe); fall back to the stdout-parsed path if that file is
        // missing or empty for some reason.
        let from_file = tokio::fs::read_to_string(&path_out_file)
            .await
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .map(PathBuf::from);
        let _ = tokio::fs::remove_file(&path_out_file).await;

        Ok(from_file.or_else(|| final_output.or(destination).map(PathBuf::from)))
    }

    /// Spawn yt-dlp with piped stdio. On Unix the child is placed in its own
    /// process group so the whole tree (yt-dlp + ffmpeg) can be signalled.
    fn spawn(&self, args: &[String]) -> Result<Child> {
        let mut cmd = Command::new(&self.ytdlp_path);
        cmd.args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        #[cfg(unix)]
        cmd.process_group(0);

        cmd.spawn().context("failed to spawn yt-dlp process")
    }
}

// ─── Process termination & cleanup ────────────────────────────────────────────

/// Remove partial output files left behind by a cancelled or failed download.
async fn cleanup_partial(output_path: &Path) {
    let _ = tokio::fs::remove_file(output_path).await;
    // yt-dlp writes in-progress data to a sibling `.part` file.
    let part = PathBuf::from(format!("{}.part", output_path.display()));
    let _ = tokio::fs::remove_file(part).await;
}

/// Terminate a child process tree: SIGTERM, wait up to 5s, then force-kill
/// (Requirement 8.8). On Windows, kill the tree via `taskkill /T /F`.
async fn terminate_child(child: &mut Child) {
    let Some(pid) = child.id() else {
        // Already exited.
        let _ = child.wait().await;
        return;
    };

    #[cfg(unix)]
    {
        // Signal the whole process group (negative pid) so ffmpeg children are
        // included; fall back to the direct pid.
        unsafe {
            libc::kill(-(pid as i32), libc::SIGTERM);
            libc::kill(pid as i32, libc::SIGTERM);
        }

        let deadline = Instant::now() + TERMINATE_GRACE;
        loop {
            if let Ok(Some(_)) = child.try_wait() {
                return;
            }
            if Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        unsafe {
            libc::kill(-(pid as i32), libc::SIGKILL);
        }
        let _ = child.kill().await;
        let _ = child.wait().await;
    }

    #[cfg(windows)]
    {
        // Windows has no SIGTERM; terminate the process tree forcefully.
        let _ = Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await;
        let _ = child.kill().await;
        let _ = child.wait().await;
    }
}

/// Whether an external binary can be found: at an explicit path, or — for a bare
/// command name — somewhere on `PATH` (trying Windows executable extensions).
/// Mirrors `std::process::Command`'s program resolution so the availability
/// check agrees with what spawning will actually do.
fn binary_available(program: &Path) -> bool {
    // An explicit path (absolute or containing a directory component) is checked
    // directly; only a bare name is resolved against PATH.
    let is_bare = program
        .parent()
        .map(|p| p.as_os_str().is_empty())
        .unwrap_or(true);
    if !is_bare {
        return program.is_file();
    }

    let Some(paths) = std::env::var_os("PATH") else {
        return false;
    };
    let name = program.to_string_lossy();
    for dir in std::env::split_paths(&paths) {
        if dir.join(&*name).is_file() {
            return true;
        }
        #[cfg(windows)]
        for ext in ["exe", "cmd", "bat"] {
            if dir.join(format!("{name}.{ext}")).is_file() {
                return true;
            }
        }
    }
    false
}

/// Return the last `n` non-empty lines of `text`, joined by newlines.
fn last_lines(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].join("\n")
}

// ─── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Progress parser unit tests ──────────────────────────────────────────

    #[test]
    fn parses_full_progress_line() {
        let p = parse_progress_line("[download]  10.5% of  100.00MiB at  2.50MiB/s ETA 00:42")
            .expect("should parse");
        assert_eq!(p.percent, 10.5);
        assert_eq!(p.speed_bps, Some((2.5 * 1024.0 * 1024.0) as u64));
        assert_eq!(p.eta_secs, Some(42));
        assert_eq!(p.total_bytes, Some(104_857_600));
    }

    #[test]
    fn parses_completed_line_without_eta() {
        let p = parse_progress_line("[download] 100% of 1.00MiB in 00:01").expect("should parse");
        assert_eq!(p.percent, 100.0);
        assert_eq!(p.eta_secs, None);
        assert_eq!(p.total_bytes, Some(1_048_576));
    }

    #[test]
    fn parses_line_with_unknown_speed_and_eta() {
        let p = parse_progress_line("[download]  50.0% of ~5.00MiB at Unknown speed ETA Unknown")
            .expect("should parse");
        assert_eq!(p.percent, 50.0);
        assert_eq!(p.speed_bps, None);
        assert_eq!(p.eta_secs, None);
        assert_eq!(p.total_bytes, Some(5_242_880));
    }

    #[test]
    fn parses_hms_eta() {
        let p = parse_progress_line("[download]  1.0% of 1.00GiB at 1.00MiB/s ETA 01:02:03")
            .expect("should parse");
        assert_eq!(p.eta_secs, Some(3723));
        assert_eq!(p.total_bytes, Some(1_073_741_824));
    }

    #[test]
    fn ignores_non_download_lines() {
        assert!(parse_progress_line("[info] Writing video metadata").is_none());
        assert!(parse_progress_line("[ffmpeg] Merging formats into output.mp4").is_none());
        assert!(parse_progress_line("").is_none());
    }

    #[test]
    fn ignores_download_destination_line() {
        // A "[download] Destination: ..." line has no percentage and must not parse.
        assert!(parse_progress_line("[download] Destination: video.mp4").is_none());
    }

    // ── Output-path parser unit tests ────────────────────────────────────────

    #[test]
    fn parses_download_destination_path() {
        assert_eq!(
            parse_output_line("[download] Destination: C:\\dl\\My Video.f137.mp4"),
            Some(OutputLine::Destination(
                "C:\\dl\\My Video.f137.mp4".to_string()
            ))
        );
    }

    #[test]
    fn merger_path_is_final_and_unquoted() {
        assert_eq!(
            parse_output_line("[Merger] Merging formats into \"/dl/My Video.mp4\""),
            Some(OutputLine::Final("/dl/My Video.mp4".to_string()))
        );
    }

    #[test]
    fn extract_audio_destination_is_final() {
        assert_eq!(
            parse_output_line("[ExtractAudio] Destination: /dl/Song.mp3"),
            Some(OutputLine::Final("/dl/Song.mp3".to_string()))
        );
    }

    #[test]
    fn already_downloaded_line_yields_destination() {
        assert_eq!(
            parse_output_line("[download] /dl/My Video.mp4 has already been downloaded"),
            Some(OutputLine::Destination("/dl/My Video.mp4".to_string()))
        );
    }

    #[test]
    fn non_output_lines_yield_none() {
        assert!(
            parse_output_line("[download]  42.0% of 10.00MiB at 1.00MiB/s ETA 00:05").is_none()
        );
        assert!(parse_output_line("[info] Writing video metadata").is_none());
        assert!(parse_output_line("").is_none());
    }

    #[test]
    fn size_parser_handles_units() {
        assert_eq!(parse_size_to_bytes("512B"), Some(512));
        assert_eq!(parse_size_to_bytes("1.00KiB"), Some(1024));
        assert_eq!(parse_size_to_bytes("2.50MiB"), Some(2_621_440));
        assert_eq!(parse_size_to_bytes("1.00GiB"), Some(1_073_741_824));
        assert_eq!(parse_size_to_bytes("Unknown"), None);
        assert_eq!(parse_size_to_bytes("NA"), None);
    }

    #[test]
    fn eta_parser_handles_formats() {
        assert_eq!(parse_eta("00:42"), Some(42));
        assert_eq!(parse_eta("01:30"), Some(90));
        assert_eq!(parse_eta("01:02:03"), Some(3723));
        assert_eq!(parse_eta("Unknown"), None);
        assert_eq!(parse_eta("NA"), None);
    }

    // ── Forbidden-flags guard unit tests ────────────────────────────────────

    #[test]
    fn detects_each_forbidden_flag() {
        assert!(is_forbidden_flag("--allow-unplayable-formats"));
        assert!(is_forbidden_flag("--cookies-from-browser"));
        assert!(is_forbidden_flag("--geo-bypass"));
        assert!(is_forbidden_flag("--geo-bypass-country"));
        assert!(is_forbidden_flag("--geo-bypass-ip-block"));
    }

    #[test]
    fn detects_forbidden_flag_with_value() {
        assert!(is_forbidden_flag("--cookies-from-browser=chrome"));
        assert!(is_forbidden_flag("--geo-bypass-country=US"));
    }

    #[test]
    fn allows_permitted_flags() {
        assert!(!is_forbidden_flag("--dump-json"));
        assert!(!is_forbidden_flag("--add-header"));
        assert!(!is_forbidden_flag("-f"));
        assert!(!is_forbidden_flag("--cookies")); // file-based cookies are allowed
    }

    #[test]
    fn contains_forbidden_flag_scans_args() {
        let bad = vec![
            "-f".to_string(),
            "best".to_string(),
            "--cookies-from-browser".to_string(),
        ];
        assert!(contains_forbidden_flag(&bad));

        let good = vec![
            "-f".to_string(),
            "best".to_string(),
            "--newline".to_string(),
        ];
        assert!(!contains_forbidden_flag(&good));
    }

    #[test]
    fn built_info_args_never_contain_forbidden_flags() {
        let ext = MediaExtractor::new(PathBuf::from("yt-dlp"), PathBuf::from("ffmpeg"));
        let args = ext
            .build_info_args("https://example.com/v/1", Some("session=abc"))
            .expect("info args build");
        assert!(!contains_forbidden_flag(&args));
        // Cookies are forwarded as a header, not via --cookies-from-browser.
        assert!(args.iter().any(|a| a == "--add-header"));
        assert!(args.iter().any(|a| a == "Cookie:session=abc"));
    }

    #[test]
    fn built_download_args_never_contain_forbidden_flags() {
        let ext = MediaExtractor::new(PathBuf::from("yt-dlp"), PathBuf::from("ffmpeg"));
        let args = ext
            .build_download_args(
                "https://example.com/v/1",
                "137+140",
                Path::new("/tmp/out.mp4"),
                Path::new("/tmp/pathout.txt"),
                Some("session=abc"),
            )
            .expect("download args build");
        assert!(!contains_forbidden_flag(&args));
        assert!(args.iter().any(|a| a == "--ffmpeg-location"));
    }

    // ── ExtractorStatus unit tests ──────────────────────────────────────────

    #[test]
    fn status_reports_missing_binaries() {
        let none = ExtractorStatus {
            ytdlp_available: false,
            ffmpeg_available: false,
        };
        let err = none.missing_binary_error().unwrap();
        assert!(err.contains("yt-dlp"));
        assert!(err.contains("ffmpeg"));

        let only_ffmpeg_missing = ExtractorStatus {
            ytdlp_available: true,
            ffmpeg_available: false,
        };
        let err = only_ffmpeg_missing.missing_binary_error().unwrap();
        assert!(err.contains("ffmpeg"));
        assert!(!err.contains("yt-dlp"));

        let ready = ExtractorStatus {
            ytdlp_available: true,
            ffmpeg_available: true,
        };
        assert!(ready.is_ready());
        assert!(ready.missing_binary_error().is_none());
    }

    // ── MediaInfo JSON parsing unit test ────────────────────────────────────

    #[test]
    fn parses_media_info_from_dump_json() {
        let json = r#"{
            "title": "Sample",
            "thumbnail": "https://example.com/t.jpg",
            "duration": 123.0,
            "extractor_key": "Generic",
            "formats": [
                {"format_id": "140", "ext": "m4a", "vcodec": "none", "acodec": "mp4a", "filesize": 1000},
                {"format_id": "137", "ext": "mp4", "vcodec": "avc1", "acodec": "none", "height": 1080}
            ]
        }"#;
        let info = media_info_from_json(json).expect("parse");
        assert_eq!(info.title, "Sample");
        assert_eq!(info.duration, Some(123));
        assert_eq!(info.platform, "Generic");
        assert_eq!(info.formats.len(), 2);

        let audio = &info.formats[0];
        assert!(audio.has_audio && !audio.has_video);
        assert_eq!(audio.quality, "audio only");
        assert_eq!(audio.filesize, Some(1000));

        let video = &info.formats[1];
        assert!(video.has_video && !video.has_audio);
        assert_eq!(video.quality, "1080p");
    }

    #[test]
    fn parses_flat_playlist_jsonl() {
        // Two entries as emitted by `yt-dlp --flat-playlist --dump-json`.
        let jsonl = concat!(
            r#"{"url":"https://www.youtube.com/watch?v=AAA","title":"First","duration":3674.0,"playlist_index":1,"playlist_title":"My List","playlist_uploader":"Chan"}"#,
            "\n",
            r#"{"url":"https://www.youtube.com/watch?v=BBB","title":"Second","duration":22258.0,"playlist_index":2,"playlist_title":"My List"}"#,
            "\n",
        );
        let info = playlist_from_jsonl(jsonl);
        assert_eq!(info.title, "My List");
        assert_eq!(info.uploader, "Chan");
        assert_eq!(info.entries.len(), 2);
        assert_eq!(info.entries[0].url, "https://www.youtube.com/watch?v=AAA");
        assert_eq!(info.entries[0].title, "First");
        assert_eq!(info.entries[0].duration, Some(3674));
        assert_eq!(info.entries[0].index, 1);
        assert_eq!(info.entries[1].index, 2);
    }

    #[test]
    fn flat_playlist_skips_unparseable_and_urlless_lines() {
        let jsonl = concat!(
            "not json\n",
            r#"{"title":"no url here"}"#,
            "\n",
            r#"{"url":"https://x/y","title":"ok"}"#,
            "\n",
        );
        let info = playlist_from_jsonl(jsonl);
        assert_eq!(info.entries.len(), 1);
        assert_eq!(info.entries[0].title, "ok");
        // Falls back to line-order index when playlist_index is absent.
        assert_eq!(info.entries[0].index, 3);
    }

    #[test]
    fn last_lines_returns_tail() {
        let text = "a\nb\n\nc\nd\ne\nf";
        assert_eq!(last_lines(text, 5), "b\nc\nd\ne\nf");
        assert_eq!(last_lines("only", 5), "only");
        assert_eq!(last_lines("", 5), "");
    }

    // ── Property 15: Media extractor never passes forbidden flags ────────────
    //
    // *For any* media download configuration, the constructed yt-dlp command
    // SHALL never include DRM-bypass flags or --cookies-from-browser.
    //
    // **Validates: Requirement 8.6**

    use proptest::prelude::*;

    /// A "benign" argument-list element guaranteed never to be a forbidden flag:
    /// every generated string is prefixed with `v`, so it can never start with
    /// `--` and therefore can never equal any entry in `FORBIDDEN_FLAGS`.
    fn arb_safe_arg() -> impl Strategy<Value = String> {
        "[a-zA-Z0-9._=:/-]{0,16}".prop_map(|s| format!("v{s}"))
    }

    /// A plausible https URL for a media page. Constrained to the http(s) space
    /// so the generator never accidentally produces a forbidden flag.
    fn arb_url() -> impl Strategy<Value = String> {
        "https?://[a-z0-9.-]{1,24}(/[a-zA-Z0-9._-]{0,16}){0,3}"
    }

    /// A plausible yt-dlp format selector (e.g. "best", "137+140", "22").
    fn arb_format_id() -> impl Strategy<Value = String> {
        "[a-zA-Z0-9+_-]{1,12}"
    }

    /// An arbitrary cookie header value (may be empty / whitespace).
    fn arb_cookies() -> impl Strategy<Value = Option<String>> {
        proptest::option::of(".{0,48}")
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(512))]

        /// Property 15 (part 1): for arbitrary url / format_id / cookies inputs,
        /// the argument lists produced by `build_info_args` and
        /// `build_download_args` NEVER contain a forbidden flag.
        ///
        /// **Validates: Requirement 8.6**
        #[test]
        fn prop_built_args_never_contain_forbidden_flags(
            url in arb_url(),
            format_id in arb_format_id(),
            cookies in arb_cookies(),
        ) {
            let ext = MediaExtractor::new(PathBuf::from("yt-dlp"), PathBuf::from("ffmpeg"));
            let cookies_ref = cookies.as_deref();

            // build_* runs validate_args internally; a benign config must build
            // successfully and never carry a forbidden flag.
            let info_args = ext
                .build_info_args(&url, cookies_ref)
                .expect("benign info config must build");
            prop_assert!(
                !contains_forbidden_flag(&info_args),
                "info args leaked a forbidden flag: {info_args:?}"
            );

            let download_args = ext
                .build_download_args(
                    &url,
                    &format_id,
                    Path::new("/tmp/out.mp4"),
                    Path::new("/tmp/pathout.txt"),
                    cookies_ref,
                )
                .expect("benign download config must build");
            prop_assert!(
                !contains_forbidden_flag(&download_args),
                "download args leaked a forbidden flag: {download_args:?}"
            );

            // Cookies are forwarded via --add-header, never --cookies-from-browser.
            prop_assert!(!info_args.iter().any(|a| a == "--cookies-from-browser"));
            prop_assert!(!download_args.iter().any(|a| a == "--cookies-from-browser"));
        }

        /// Property 15 (part 2): `contains_forbidden_flag` detects a forbidden
        /// flag injected at ANY position in an otherwise-benign argument list,
        /// including the `--flag=value` form.
        ///
        /// **Validates: Requirement 8.6**
        #[test]
        fn prop_contains_forbidden_flag_detects_injection(
            mut args in proptest::collection::vec(arb_safe_arg(), 0..16),
            flag_idx in 0..FORBIDDEN_FLAGS.len(),
            with_value in any::<bool>(),
            value in "[a-zA-Z0-9]{0,8}",
        ) {
            // A list built only from safe args must be clean to begin with.
            prop_assert!(
                !contains_forbidden_flag(&args),
                "safe args were unexpectedly flagged: {args:?}"
            );

            // Construct the forbidden flag, optionally in --flag=value form.
            let base = FORBIDDEN_FLAGS[flag_idx];
            let injected = if with_value {
                format!("{base}={value}")
            } else {
                base.to_string()
            };

            // Insert it at an arbitrary position within the list.
            let pos = if args.is_empty() { 0 } else { flag_idx % (args.len() + 1) };
            args.insert(pos.min(args.len()), injected);

            prop_assert!(
                contains_forbidden_flag(&args),
                "failed to detect injected forbidden flag in {args:?}"
            );
        }
    }

    // ── Property 14: yt-dlp progress parser correctness ──────────────────────
    //
    // *For any* well-formed yt-dlp progress output line, the parser SHALL
    // correctly extract the download percentage, speed, and ETA values.
    //
    // **Validates: Requirement 8.3**

    /// Size units yt-dlp prints, paired with their byte multiplier.
    const SIZE_UNITS: &[(&str, f64)] = &[
        ("B", 1.0),
        ("KiB", 1024.0),
        ("MiB", 1024.0 * 1024.0),
        ("GiB", 1024.0 * 1024.0 * 1024.0),
    ];

    /// Generate a size token (e.g. `"2.50MiB"`). Used only for the `of <size>`
    /// field, which the parser ignores, so no expected value is tracked.
    fn arb_size_token() -> impl Strategy<Value = String> {
        prop_oneof![
            8 => (0.0f64..4096.0, 0usize..SIZE_UNITS.len()).prop_map(|(v, idx)| {
                let (unit, _) = SIZE_UNITS[idx];
                format!("{v:.2}{unit}")
            }),
            1 => Just("Unknown".to_string()),
            1 => Just("~5.00MiB".to_string()),
        ]
    }

    /// Generate the speed segment between `at` and `ETA`, paired with the
    /// bytes/sec the parser must recover. A real speed ends in `/s`; an unknown
    /// speed is the bare word `Unknown` (no `/s`), which parses to `None`.
    fn arb_speed_segment() -> impl Strategy<Value = (String, Option<u64>)> {
        prop_oneof![
            8 => (0.0f64..4096.0, 0usize..SIZE_UNITS.len()).prop_map(|(v, idx)| {
                let (unit, mult) = SIZE_UNITS[idx];
                let num_str = format!("{v:.2}");
                let num: f64 = num_str.parse().unwrap();
                (format!("{num_str}{unit}/s"), Some((num * mult) as u64))
            }),
            2 => Just(("Unknown".to_string(), None)),
        ]
    }

    /// Generate an ETA token (`MM:SS` or `HH:MM:SS`) with the seconds the parser
    /// must recover, or the `Unknown` sentinel (value `None`).
    fn arb_eta_token() -> impl Strategy<Value = (String, Option<u64>)> {
        prop_oneof![
            6 => (0u64..60, 0u64..60)
                .prop_map(|(m, s)| (format!("{m:02}:{s:02}"), Some(m * 60 + s))),
            3 => (0u64..24, 0u64..60, 0u64..60)
                .prop_map(|(h, m, s)| (format!("{h:02}:{m:02}:{s:02}"), Some((h * 60 + m) * 60 + s))),
            2 => Just(("Unknown".to_string(), None)),
        ]
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(512))]

        /// Property 14: for any `[download]` line synthesized from arbitrary
        /// (percent, size, speed, eta) components, the parser recovers the
        /// percentage (clamped to `[0, 100]`), the speed, and the ETA, and
        /// never panics.
        ///
        /// **Validates: Requirement 8.3**
        #[test]
        fn property_14_recovers_progress_fields(
            // Range straddles the [0, 100] bounds so clamping is exercised.
            raw_percent in -20.0f64..150.0,
            size_token in arb_size_token(),
            (speed_segment, expected_speed) in arb_speed_segment(),
            (eta_token, expected_eta) in arb_eta_token(),
            // Arbitrary leading whitespace must not affect parsing.
            lead in "[ \t]{0,4}",
        ) {
            let pct_str = format!("{raw_percent:.1}");
            let pct_num: f64 = pct_str.parse().unwrap();
            let expected_percent = pct_num.clamp(0.0, 100.0);

            let line = format!(
                "{lead}[download]  {pct_str}% of  {size_token} at  {speed_segment} ETA {eta_token}"
            );

            let progress = parse_progress_line(&line)
                .expect("a well-formed [download] line with a percentage must parse");

            prop_assert!((0.0..=100.0).contains(&progress.percent));
            prop_assert_eq!(progress.percent, expected_percent);
            prop_assert_eq!(progress.speed_bps, expected_speed);
            prop_assert_eq!(progress.eta_secs, expected_eta);
        }

        /// Property 14 (totality): the parser never panics on arbitrary input,
        /// every non-`[download]` line yields `None`, and any produced
        /// percentage is always within `[0, 100]`.
        ///
        /// **Validates: Requirement 8.3**
        #[test]
        fn property_14_non_download_lines_return_none(line in ".*") {
            // Must not panic for any input.
            let result = parse_progress_line(&line);

            if !line.trim_start().starts_with("[download]") {
                prop_assert!(result.is_none());
            }

            if let Some(p) = result {
                prop_assert!((0.0..=100.0).contains(&p.percent));
            }
        }
    }
}
