//! Application settings and configuration management.
//!
//! Defines `AppSettings` and related types with validation for all configurable values.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// A category rule mapping a category name to associated file extensions.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CategoryRule {
    /// Human-readable category name (e.g. "Videos", "Documents").
    pub category: String,
    /// File extensions belonging to this category (e.g. [".mp4", ".mkv"]).
    pub extensions: Vec<String>,
    /// MIME type patterns for fallback matching.
    pub mime_patterns: Vec<String>,
    /// Subfolder name relative to the download directory.
    pub subfolder: String,
}

/// All user-configurable application settings.
///
/// Persisted to disk and restored on launch.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppSettings {
    /// Root directory where downloads are saved.
    pub download_dir: PathBuf,
    /// Maximum concurrent downloads (1-10).
    pub max_concurrent: usize,
    /// Default number of segments per download (1-32).
    pub default_segments: u32,
    /// Global speed limit in bytes/sec; 0 means unlimited.
    pub speed_limit: u64,
    /// Whether completed downloads are auto-sorted into category folders.
    pub auto_categorize: bool,
    /// Category rules for auto-categorization.
    pub categories: Vec<CategoryRule>,
    /// Whether queued downloads start automatically.
    pub auto_start_queue: bool,
    /// Whether interrupted downloads auto-resume on app launch. When `false`
    /// (the default), restored downloads stay paused until the user clicks
    /// "Resume All"; when `true`, the scheduler resumes them automatically after
    /// `restore_from_disk`. Defaulted via serde so older settings files load.
    #[serde(default)]
    pub resume_on_startup: bool,
    /// Whether closing the window minimizes to system tray.
    pub minimize_to_tray: bool,
    /// Whether to show desktop notifications on completion/error.
    pub notifications_enabled: bool,
    /// Whether to ask for confirmation before deleting a downloaded file from
    /// disk. Defaults to `true`; serde-defaulted so older settings files load
    /// with confirmation enabled.
    #[serde(default = "default_true")]
    pub confirm_on_delete: bool,
    /// Minimum file size (bytes) for browser capture to intercept.
    pub capture_min_size: u64,
    /// File extensions the browser extension will capture (empty = all).
    pub capture_extensions: Vec<String>,
    /// Path to the yt-dlp binary.
    pub ytdlp_path: Option<PathBuf>,
    /// Path to the ffmpeg binary.
    pub ffmpeg_path: Option<PathBuf>,
}

/// Serde default for boolean settings that should default to `true` when absent
/// from an older settings file.
fn default_true() -> bool {
    true
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            download_dir: dirs::download_dir().unwrap_or_else(std::env::temp_dir),
            max_concurrent: 3,
            default_segments: 4,
            speed_limit: 0,
            auto_categorize: true,
            categories: Self::default_categories(),
            auto_start_queue: true,
            resume_on_startup: false,
            minimize_to_tray: false,
            notifications_enabled: true,
            confirm_on_delete: true,
            capture_min_size: 1_048_576, // 1 MB
            capture_extensions: Vec::new(),
            ytdlp_path: None,
            ffmpeg_path: None,
        }
    }
}

/// Validation error for settings fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidationError {
    pub field: String,
    pub message: String,
}

impl std::fmt::Display for ValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.field, self.message)
    }
}

impl std::error::Error for ValidationError {}

impl AppSettings {
    /// Validate `max_concurrent` is within [1, 10].
    pub fn validate_max_concurrent(value: usize) -> Result<usize, ValidationError> {
        if (1..=10).contains(&value) {
            Ok(value)
        } else {
            Err(ValidationError {
                field: "max_concurrent".into(),
                message: format!("must be between 1 and 10, got {value}"),
            })
        }
    }

    /// Validate `default_segments` is within [1, 32].
    pub fn validate_segments(value: u32) -> Result<u32, ValidationError> {
        if (1..=32).contains(&value) {
            Ok(value)
        } else {
            Err(ValidationError {
                field: "default_segments".into(),
                message: format!("must be between 1 and 32, got {value}"),
            })
        }
    }

    /// Validate speed limit: must be 0 (unlimited) or a positive value.
    pub fn validate_speed_limit(value: u64) -> Result<u64, ValidationError> {
        // u64 is always >= 0, so any value is valid (0 = unlimited, >0 = limit).
        Ok(value)
    }

    /// Validate a speed limit supplied as a signed integer.
    ///
    /// Values arriving from the UI / Tauri command boundary are JSON numbers and may be
    /// negative. Per Requirement 11.3, the speed limit must be `0` (unlimited) or a
    /// positive integer; negative values are rejected.
    pub fn validate_speed_limit_signed(value: i64) -> Result<u64, ValidationError> {
        if value < 0 {
            Err(ValidationError {
                field: "speed_limit".into(),
                message: format!("must be 0 (unlimited) or a positive integer, got {value}"),
            })
        } else {
            Ok(value as u64)
        }
    }

    /// Apply a validated max_concurrent value.
    pub fn set_max_concurrent(&mut self, value: usize) -> Result<(), ValidationError> {
        let validated = Self::validate_max_concurrent(value)?;
        self.max_concurrent = validated;
        Ok(())
    }

    /// Apply a validated segments value.
    pub fn set_default_segments(&mut self, value: u32) -> Result<(), ValidationError> {
        let validated = Self::validate_segments(value)?;
        self.default_segments = validated;
        Ok(())
    }

    /// Apply a validated speed limit.
    pub fn set_speed_limit(&mut self, value: u64) -> Result<(), ValidationError> {
        let validated = Self::validate_speed_limit(value)?;
        self.speed_limit = validated;
        Ok(())
    }

    /// Apply a speed limit supplied as a signed integer (e.g. from the UI).
    ///
    /// On rejection (negative value), the previous `speed_limit` is retained.
    pub fn set_speed_limit_signed(&mut self, value: i64) -> Result<(), ValidationError> {
        let validated = Self::validate_speed_limit_signed(value)?;
        self.speed_limit = validated;
        Ok(())
    }

    /// Default category rules matching the design spec.
    pub fn default_categories() -> Vec<CategoryRule> {
        vec![
            CategoryRule {
                category: "Videos".into(),
                extensions: vec![
                    ".mp4", ".mkv", ".avi", ".mov", ".wmv", ".flv", ".webm", ".m4v",
                ]
                .into_iter()
                .map(String::from)
                .collect(),
                mime_patterns: vec!["video/".into()],
                subfolder: "Videos".into(),
            },
            CategoryRule {
                category: "Music".into(),
                extensions: vec![".mp3", ".flac", ".aac", ".ogg", ".wav", ".wma", ".m4a"]
                    .into_iter()
                    .map(String::from)
                    .collect(),
                mime_patterns: vec!["audio/".into()],
                subfolder: "Music".into(),
            },
            CategoryRule {
                category: "Images".into(),
                extensions: vec![
                    ".jpg", ".jpeg", ".png", ".gif", ".bmp", ".svg", ".webp", ".ico", ".tiff",
                ]
                .into_iter()
                .map(String::from)
                .collect(),
                mime_patterns: vec!["image/".into()],
                subfolder: "Images".into(),
            },
            CategoryRule {
                category: "Documents".into(),
                extensions: vec![
                    ".pdf", ".doc", ".docx", ".xls", ".xlsx", ".ppt", ".pptx", ".txt", ".rtf",
                    ".odt", ".csv",
                ]
                .into_iter()
                .map(String::from)
                .collect(),
                mime_patterns: vec![
                    "application/pdf".into(),
                    "application/msword".into(),
                    "text/".into(),
                ],
                subfolder: "Documents".into(),
            },
            CategoryRule {
                category: "Archives".into(),
                extensions: vec![".zip", ".rar", ".7z", ".tar", ".gz", ".bz2", ".xz", ".iso"]
                    .into_iter()
                    .map(String::from)
                    .collect(),
                mime_patterns: vec![
                    "application/zip".into(),
                    "application/x-rar".into(),
                    "application/x-7z-compressed".into(),
                ],
                subfolder: "Archives".into(),
            },
            CategoryRule {
                category: "Programs".into(),
                extensions: vec![".exe", ".msi", ".dmg", ".deb", ".rpm", ".appimage", ".apk"]
                    .into_iter()
                    .map(String::from)
                    .collect(),
                mime_patterns: vec!["application/x-executable".into()],
                subfolder: "Programs".into(),
            },
            CategoryRule {
                category: "Other".into(),
                extensions: Vec::new(),
                mime_patterns: Vec::new(),
                subfolder: "Other".into(),
            },
        ]
    }
}

// ─── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    // Valid range constants (mirrors the validation rules).
    const MAX_CONCURRENT_MIN: usize = 1;
    const MAX_CONCURRENT_MAX: usize = 10;
    const SEGMENTS_MIN: u32 = 1;
    const SEGMENTS_MAX: u32 = 32;

    // ─── Unit tests: boundary values ────────────────────────────────────────────

    #[test]
    fn max_concurrent_accepts_boundaries() {
        assert_eq!(AppSettings::validate_max_concurrent(1), Ok(1));
        assert_eq!(AppSettings::validate_max_concurrent(10), Ok(10));
    }

    #[test]
    fn max_concurrent_rejects_just_outside_boundaries() {
        assert!(AppSettings::validate_max_concurrent(0).is_err());
        assert!(AppSettings::validate_max_concurrent(11).is_err());
    }

    #[test]
    fn segments_accepts_boundaries() {
        assert_eq!(AppSettings::validate_segments(1), Ok(1));
        assert_eq!(AppSettings::validate_segments(32), Ok(32));
    }

    #[test]
    fn segments_rejects_just_outside_boundaries() {
        assert!(AppSettings::validate_segments(0).is_err());
        assert!(AppSettings::validate_segments(33).is_err());
    }

    #[test]
    fn speed_limit_accepts_zero_and_positive() {
        assert_eq!(AppSettings::validate_speed_limit_signed(0), Ok(0));
        assert_eq!(AppSettings::validate_speed_limit_signed(1024), Ok(1024));
    }

    #[test]
    fn speed_limit_rejects_negative() {
        assert!(AppSettings::validate_speed_limit_signed(-1).is_err());
    }

    // ─── resume_on_startup: default off + forward-compatible deserialization ──────

    #[test]
    fn resume_on_startup_defaults_off() {
        // Matches the spec: restored downloads stay paused until the user resumes.
        assert!(!AppSettings::default().resume_on_startup);
    }

    #[test]
    fn resume_on_startup_missing_field_deserializes_to_false() {
        // An older settings file written before this field existed must still load,
        // defaulting the new flag to false (preserving prior behavior).
        let json = r#"{
            "downloadDir": "/tmp/dl",
            "maxConcurrent": 3,
            "defaultSegments": 4,
            "speedLimit": 0,
            "autoCategorize": true,
            "categories": [],
            "autoStartQueue": true,
            "minimizeToTray": false,
            "notificationsEnabled": true,
            "captureMinSize": 1048576,
            "captureExtensions": [],
            "ytdlpPath": null,
            "ffmpegPath": null
        }"#;
        let parsed: AppSettings = serde_json::from_str(json).unwrap();
        assert!(!parsed.resume_on_startup);
    }

    #[test]
    fn resume_on_startup_round_trips_when_enabled() {
        let settings = AppSettings {
            resume_on_startup: true,
            ..Default::default()
        };
        let json = serde_json::to_string(&settings).unwrap();
        let back: AppSettings = serde_json::from_str(&json).unwrap();
        assert!(back.resume_on_startup);
    }

    // ─── Unit tests: rejection retains the previous setting ───────────────────────

    #[test]
    fn rejected_max_concurrent_retains_previous_value() {
        let mut settings = AppSettings::default();
        let previous = settings.max_concurrent;
        let err = settings.set_max_concurrent(99);
        assert!(err.is_err());
        assert_eq!(settings.max_concurrent, previous);
    }

    #[test]
    fn rejected_segments_retains_previous_value() {
        let mut settings = AppSettings::default();
        let previous = settings.default_segments;
        let err = settings.set_default_segments(100);
        assert!(err.is_err());
        assert_eq!(settings.default_segments, previous);
    }

    #[test]
    fn rejected_speed_limit_retains_previous_value() {
        let mut settings = AppSettings {
            speed_limit: 5000,
            ..Default::default()
        };
        let err = settings.set_speed_limit_signed(-42);
        assert!(err.is_err());
        assert_eq!(settings.speed_limit, 5000);
    }

    // ─── Property 19: Settings validation rejects out-of-bounds values ────────────
    //
    // For any integer value:
    //   - max_concurrent is accepted iff it is in [1, 10]
    //   - default_segments is accepted iff it is in [1, 32]
    //   - speed_limit is accepted iff it is >= 0
    //
    // **Validates: Requirements 11.1, 11.2, 11.3**

    proptest! {
        /// max_concurrent: accepted iff value ∈ [1, 10].
        #[test]
        fn prop_max_concurrent_accepted_iff_in_range(value in any::<usize>()) {
            let in_range = (MAX_CONCURRENT_MIN..=MAX_CONCURRENT_MAX).contains(&value);
            let result = AppSettings::validate_max_concurrent(value);
            prop_assert_eq!(result.is_ok(), in_range);
            if in_range {
                // On success the validated value equals the input.
                prop_assert_eq!(result.unwrap(), value);
            }
        }

        /// max_concurrent: a rejected value retains the previous setting.
        #[test]
        fn prop_max_concurrent_rejection_retains_previous(
            start in MAX_CONCURRENT_MIN..=MAX_CONCURRENT_MAX,
            value in any::<usize>(),
        ) {
            let in_range = (MAX_CONCURRENT_MIN..=MAX_CONCURRENT_MAX).contains(&value);
            prop_assume!(!in_range);
            let mut settings = AppSettings {
                max_concurrent: start,
                ..Default::default()
            };
            let result = settings.set_max_concurrent(value);
            prop_assert!(result.is_err());
            prop_assert_eq!(settings.max_concurrent, start);
        }

        /// default_segments: accepted iff value ∈ [1, 32].
        #[test]
        fn prop_segments_accepted_iff_in_range(value in any::<u32>()) {
            let in_range = (SEGMENTS_MIN..=SEGMENTS_MAX).contains(&value);
            let result = AppSettings::validate_segments(value);
            prop_assert_eq!(result.is_ok(), in_range);
            if in_range {
                prop_assert_eq!(result.unwrap(), value);
            }
        }

        /// default_segments: a rejected value retains the previous setting.
        #[test]
        fn prop_segments_rejection_retains_previous(
            start in SEGMENTS_MIN..=SEGMENTS_MAX,
            value in any::<u32>(),
        ) {
            let in_range = (SEGMENTS_MIN..=SEGMENTS_MAX).contains(&value);
            prop_assume!(!in_range);
            let mut settings = AppSettings {
                default_segments: start,
                ..Default::default()
            };
            let result = settings.set_default_segments(value);
            prop_assert!(result.is_err());
            prop_assert_eq!(settings.default_segments, start);
        }

        /// speed_limit: accepted iff value >= 0 (negative values rejected).
        #[test]
        fn prop_speed_limit_accepted_iff_non_negative(value in any::<i64>()) {
            let non_negative = value >= 0;
            let result = AppSettings::validate_speed_limit_signed(value);
            prop_assert_eq!(result.is_ok(), non_negative);
            if non_negative {
                // Accepted values round-trip to the same magnitude.
                prop_assert_eq!(result.unwrap(), value as u64);
            }
        }

        /// speed_limit: a rejected (negative) value retains the previous setting.
        #[test]
        fn prop_speed_limit_rejection_retains_previous(
            start in any::<u64>(),
            value in i64::MIN..0i64,
        ) {
            let mut settings = AppSettings {
                speed_limit: start,
                ..Default::default()
            };
            let result = settings.set_speed_limit_signed(value);
            prop_assert!(result.is_err());
            prop_assert_eq!(settings.speed_limit, start);
        }
    }
}
