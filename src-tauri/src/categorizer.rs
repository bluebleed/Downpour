//! Auto-categorizer for sorting completed downloads into category folders.
//!
//! Matches file extensions and MIME types to categories, then moves files
//! to the appropriate subfolder within the download directory.
//!
//! Matching order (Requirement 7.1):
//!   1. file extension
//!   2. server-reported MIME type
//!   3. the catch-all "Other" category
//!
//! Not yet wired into the Tauri command surface (see task 13.1), so the public
//! API is allowed to be unused for now.
#![allow(dead_code)]

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Serialize;
use tauri::{AppHandle, Emitter};
use tokio::fs;

use crate::settings::{AppSettings, CategoryRule};

/// Name of the catch-all category used when no extension or MIME rule matches.
pub const OTHER_CATEGORY: &str = "Other";

/// Maximum number of user-defined category rules (Requirement 11.6).
pub const MAX_USER_CATEGORIES: usize = 20;

/// Maximum number of file extensions per category rule (Requirement 11.6).
pub const MAX_EXTENSIONS_PER_CATEGORY: usize = 50;

/// Event name emitted to the UI when categorization fails (Requirement 7.7).
const CATEGORIZATION_ERROR_EVENT: &str = "categorization-error";

// ─── Errors ──────────────────────────────────────────────────────────────────

/// Validation error for user-supplied category rules (Requirement 11.6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuleValidationError {
    /// More than [`MAX_USER_CATEGORIES`] rules were supplied.
    TooManyCategories { count: usize, max: usize },
    /// A category had more than [`MAX_EXTENSIONS_PER_CATEGORY`] extensions.
    TooManyExtensions {
        category: String,
        count: usize,
        max: usize,
    },
}

impl std::fmt::Display for RuleValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooManyCategories { count, max } => {
                write!(f, "too many categories: {count} supplied, maximum is {max}")
            }
            Self::TooManyExtensions {
                category,
                count,
                max,
            } => write!(
                f,
                "category '{category}' has {count} extensions, maximum is {max}"
            ),
        }
    }
}

impl std::error::Error for RuleValidationError {}

/// Payload emitted to the UI when a categorization move fails (Requirement 7.7).
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CategorizationError {
    /// Absolute path of the file that could not be moved.
    pub file: String,
    /// The category the file would have been moved into.
    pub category: String,
    /// Human-readable failure description.
    pub message: String,
}

// ─── Categorizer ───────────────────────────────────────────────────────────────

/// Sorts completed downloads into category subfolders based on extension/MIME.
pub struct Categorizer {
    rules: Vec<CategoryRule>,
    download_dir: PathBuf,
    enabled: bool,
}

impl Categorizer {
    /// Create a categorizer with explicit rules and download directory.
    pub fn new(download_dir: PathBuf, rules: Vec<CategoryRule>, enabled: bool) -> Self {
        Self {
            rules,
            download_dir,
            enabled,
        }
    }

    /// Build a categorizer from application settings.
    pub fn from_settings(settings: &AppSettings) -> Self {
        Self::new(
            settings.download_dir.clone(),
            settings.categories.clone(),
            settings.auto_categorize,
        )
    }

    /// Whether auto-categorization is enabled (Requirement 7.5).
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Validate user-supplied category rules against the configured limits.
    ///
    /// Enforces at most [`MAX_USER_CATEGORIES`] categories, each with at most
    /// [`MAX_EXTENSIONS_PER_CATEGORY`] extensions (Requirement 11.6).
    pub fn validate_rules(rules: &[CategoryRule]) -> Result<(), RuleValidationError> {
        if rules.len() > MAX_USER_CATEGORIES {
            return Err(RuleValidationError::TooManyCategories {
                count: rules.len(),
                max: MAX_USER_CATEGORIES,
            });
        }
        for rule in rules {
            if rule.extensions.len() > MAX_EXTENSIONS_PER_CATEGORY {
                return Err(RuleValidationError::TooManyExtensions {
                    category: rule.category.clone(),
                    count: rule.extensions.len(),
                    max: MAX_EXTENSIONS_PER_CATEGORY,
                });
            }
        }
        Ok(())
    }

    /// Determine the category for a file by extension first, then MIME type,
    /// falling back to "Other" (Requirements 7.1, 7.4).
    ///
    /// Returns `None` only when no rules are configured at all (no "Other"
    /// rule to fall back to). With the default category set this always
    /// returns `Some`.
    pub fn categorize(&self, filename: &str, mime: Option<&str>) -> Option<&str> {
        // 1. Match by file extension.
        if let Some(ext) = extension_of(filename) {
            for rule in &self.rules {
                if rule.extensions.iter().any(|e| e.eq_ignore_ascii_case(&ext)) {
                    return Some(&rule.category);
                }
            }
        }

        // 2. Fall back to the server-reported MIME type.
        if let Some(raw) = mime {
            let mime = raw.trim().to_ascii_lowercase();
            if !mime.is_empty() {
                for rule in &self.rules {
                    if rule
                        .mime_patterns
                        .iter()
                        .any(|p| mime.starts_with(&p.to_ascii_lowercase()))
                    {
                        return Some(&rule.category);
                    }
                }
            }
        }

        // 3. Fall back to the catch-all "Other" category.
        self.rules
            .iter()
            .find(|r| r.category == OTHER_CATEGORY)
            .map(|r| r.category.as_str())
    }

    /// Move a completed file into its category subfolder, creating the
    /// subfolder if needed and resolving filename conflicts with an
    /// incrementing numeric suffix (Requirements 7.3, 7.6).
    ///
    /// Returns the final destination path on success.
    pub async fn move_to_category(&self, file_path: &Path, category: &str) -> Result<PathBuf> {
        let file_name = file_path
            .file_name()
            .context("source path has no file name")?
            .to_owned();

        // Resolve the subfolder for this category (defaulting to the category name).
        let subfolder = self
            .rules
            .iter()
            .find(|r| r.category == category)
            .map(|r| r.subfolder.clone())
            .unwrap_or_else(|| category.to_string());

        let target_dir = self.download_dir.join(&subfolder);
        fs::create_dir_all(&target_dir)
            .await
            .with_context(|| format!("failed to create category folder {target_dir:?}"))?;

        let dest = unique_destination(&target_dir, &file_name).await;

        // Try a fast rename first; fall back to copy+remove for cross-device moves.
        if fs::rename(file_path, &dest).await.is_err() {
            fs::copy(file_path, &dest)
                .await
                .with_context(|| format!("failed to copy {file_path:?} to {dest:?}"))?;
            fs::remove_file(file_path)
                .await
                .with_context(|| format!("failed to remove source {file_path:?}"))?;
        }

        Ok(dest)
    }

    /// Categorize a completed download and move it into place.
    ///
    /// - Skips entirely when auto-categorization is disabled, leaving the file
    ///   in the download directory (Requirement 7.5).
    /// - On a filesystem error, leaves the file where it is and emits a
    ///   `categorization-error` event to the UI (Requirement 7.7).
    ///
    /// Returns the new path when the file was moved, or `None` otherwise.
    pub async fn process(
        &self,
        app: &AppHandle,
        file_path: &Path,
        mime: Option<&str>,
    ) -> Option<PathBuf> {
        if !self.enabled {
            return None;
        }

        let filename = file_path.file_name().and_then(|n| n.to_str())?;
        let category = self.categorize(filename, mime)?.to_string();

        match self.move_to_category(file_path, &category).await {
            Ok(dest) => Some(dest),
            Err(e) => {
                let _ = app.emit(
                    CATEGORIZATION_ERROR_EVENT,
                    CategorizationError {
                        file: file_path.to_string_lossy().into_owned(),
                        category,
                        message: format!("{e:#}"),
                    },
                );
                None
            }
        }
    }
}

// ─── Helpers ───────────────────────────────────────────────────────────────────

/// Extract a lowercased, dot-prefixed extension from a file name
/// (e.g. `"Movie.MP4"` → `".mp4"`). Returns `None` when there is no extension.
fn extension_of(filename: &str) -> Option<String> {
    Path::new(filename)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| format!(".{}", e.to_ascii_lowercase()))
}

/// Produce a non-colliding destination path inside `dir` for `file_name`.
///
/// If `dir/file_name` already exists, appends an incrementing suffix such as
/// `"file (1).ext"`, `"file (2).ext"` until a free name is found.
async fn unique_destination(dir: &Path, file_name: &std::ffi::OsStr) -> PathBuf {
    let candidate = dir.join(file_name);
    if !path_exists(&candidate).await {
        return candidate;
    }

    let name = Path::new(file_name);
    let stem = name
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_string();
    let ext = name.extension().and_then(|e| e.to_str());

    let mut counter: u64 = 1;
    loop {
        let new_name = match ext {
            Some(ext) => format!("{stem} ({counter}).{ext}"),
            None => format!("{stem} ({counter})"),
        };
        let candidate = dir.join(new_name);
        if !path_exists(&candidate).await {
            return candidate;
        }
        counter += 1;
    }
}

/// Async existence check that treats errors (e.g. permission denied) as "exists"
/// to avoid clobbering a path we cannot inspect.
async fn path_exists(path: &Path) -> bool {
    fs::try_exists(path).await.unwrap_or(true)
}

// ─── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn categorizer() -> Categorizer {
        Categorizer::new(
            PathBuf::from("/tmp/downloads"),
            AppSettings::default_categories(),
            true,
        )
    }

    // ─── categorize(): extension matching ────────────────────────────────────

    #[test]
    fn categorize_matches_extension() {
        let c = categorizer();
        assert_eq!(c.categorize("movie.mp4", None), Some("Videos"));
        assert_eq!(c.categorize("song.mp3", None), Some("Music"));
        assert_eq!(c.categorize("photo.png", None), Some("Images"));
        assert_eq!(c.categorize("report.pdf", None), Some("Documents"));
        assert_eq!(c.categorize("bundle.zip", None), Some("Archives"));
        assert_eq!(c.categorize("setup.exe", None), Some("Programs"));
    }

    #[test]
    fn categorize_is_case_insensitive() {
        let c = categorizer();
        assert_eq!(c.categorize("VIDEO.MP4", None), Some("Videos"));
        assert_eq!(c.categorize("Photo.JPG", None), Some("Images"));
    }

    #[test]
    fn categorize_extension_takes_precedence_over_mime() {
        let c = categorizer();
        // Extension says Images, MIME says video — extension wins.
        assert_eq!(c.categorize("image.png", Some("video/mp4")), Some("Images"));
    }

    // ─── categorize(): MIME fallback ─────────────────────────────────────────

    #[test]
    fn categorize_falls_back_to_mime() {
        let c = categorizer();
        // Unknown extension, but MIME identifies it as video.
        assert_eq!(c.categorize("clip.bin", Some("video/webm")), Some("Videos"));
        assert_eq!(c.categorize("track.bin", Some("audio/mpeg")), Some("Music"));
        assert_eq!(
            c.categorize("doc.bin", Some("application/pdf")),
            Some("Documents")
        );
    }

    #[test]
    fn categorize_mime_with_charset_parameter() {
        let c = categorizer();
        assert_eq!(
            c.categorize("page.bin", Some("text/plain; charset=utf-8")),
            Some("Documents")
        );
    }

    // ─── categorize(): "Other" fallback (totality) ───────────────────────────

    #[test]
    fn categorize_unknown_returns_other() {
        let c = categorizer();
        assert_eq!(c.categorize("mystery.xyz", None), Some("Other"));
        assert_eq!(
            c.categorize("data.bin", Some("application/x-unknown")),
            Some("Other")
        );
    }

    #[test]
    fn categorize_no_extension_returns_other() {
        let c = categorizer();
        assert_eq!(c.categorize("README", None), Some("Other"));
    }

    #[test]
    fn categorize_multi_dot_uses_last_extension() {
        let c = categorizer();
        assert_eq!(c.categorize("archive.tar.gz", None), Some("Archives"));
    }

    // ─── validate_rules(): Requirement 11.6 ──────────────────────────────────

    fn rule_with_extensions(name: &str, count: usize) -> CategoryRule {
        CategoryRule {
            category: name.to_string(),
            extensions: (0..count).map(|i| format!(".e{i}")).collect(),
            mime_patterns: Vec::new(),
            subfolder: name.to_string(),
        }
    }

    #[test]
    fn validate_rules_accepts_within_limits() {
        let rules: Vec<CategoryRule> = (0..MAX_USER_CATEGORIES)
            .map(|i| rule_with_extensions(&format!("Cat{i}"), MAX_EXTENSIONS_PER_CATEGORY))
            .collect();
        assert_eq!(Categorizer::validate_rules(&rules), Ok(()));
    }

    #[test]
    fn validate_rules_rejects_too_many_categories() {
        let rules: Vec<CategoryRule> = (0..MAX_USER_CATEGORIES + 1)
            .map(|i| rule_with_extensions(&format!("Cat{i}"), 1))
            .collect();
        assert_eq!(
            Categorizer::validate_rules(&rules),
            Err(RuleValidationError::TooManyCategories {
                count: MAX_USER_CATEGORIES + 1,
                max: MAX_USER_CATEGORIES,
            })
        );
    }

    #[test]
    fn validate_rules_rejects_too_many_extensions() {
        let rules = vec![rule_with_extensions("Big", MAX_EXTENSIONS_PER_CATEGORY + 1)];
        assert_eq!(
            Categorizer::validate_rules(&rules),
            Err(RuleValidationError::TooManyExtensions {
                category: "Big".to_string(),
                count: MAX_EXTENSIONS_PER_CATEGORY + 1,
                max: MAX_EXTENSIONS_PER_CATEGORY,
            })
        );
    }

    // ─── move_to_category(): move + subfolder creation ───────────────────────

    #[tokio::test]
    async fn move_creates_subfolder_and_moves_file() {
        let tmp = tempfile::tempdir().unwrap();
        let c = Categorizer::new(
            tmp.path().to_path_buf(),
            AppSettings::default_categories(),
            true,
        );

        let src = tmp.path().join("movie.mp4");
        fs::write(&src, b"video-bytes").await.unwrap();

        let dest = c.move_to_category(&src, "Videos").await.unwrap();

        assert_eq!(dest, tmp.path().join("Videos").join("movie.mp4"));
        assert!(path_exists(&dest).await);
        assert!(!path_exists(&src).await, "source should have been moved");
        assert_eq!(fs::read(&dest).await.unwrap(), b"video-bytes");
    }

    // ─── move_to_category(): conflict handling (Requirement 7.6) ─────────────

    #[tokio::test]
    async fn move_resolves_filename_conflicts_with_incrementing_suffix() {
        let tmp = tempfile::tempdir().unwrap();
        let c = Categorizer::new(
            tmp.path().to_path_buf(),
            AppSettings::default_categories(),
            true,
        );
        let videos = tmp.path().join("Videos");

        // First move: lands as movie.mp4
        let src1 = tmp.path().join("movie.mp4");
        fs::write(&src1, b"one").await.unwrap();
        let dest1 = c.move_to_category(&src1, "Videos").await.unwrap();
        assert_eq!(dest1, videos.join("movie.mp4"));

        // Second move of the same name: becomes "movie (1).mp4"
        let src2 = tmp.path().join("sub").join("movie.mp4");
        fs::create_dir_all(src2.parent().unwrap()).await.unwrap();
        fs::write(&src2, b"two").await.unwrap();
        let dest2 = c.move_to_category(&src2, "Videos").await.unwrap();
        assert_eq!(dest2, videos.join("movie (1).mp4"));

        // Third move: becomes "movie (2).mp4"
        let src3 = tmp.path().join("sub2").join("movie.mp4");
        fs::create_dir_all(src3.parent().unwrap()).await.unwrap();
        fs::write(&src3, b"three").await.unwrap();
        let dest3 = c.move_to_category(&src3, "Videos").await.unwrap();
        assert_eq!(dest3, videos.join("movie (2).mp4"));

        // All three files coexist with their original contents.
        assert_eq!(fs::read(videos.join("movie.mp4")).await.unwrap(), b"one");
        assert_eq!(
            fs::read(videos.join("movie (1).mp4")).await.unwrap(),
            b"two"
        );
        assert_eq!(
            fs::read(videos.join("movie (2).mp4")).await.unwrap(),
            b"three"
        );
    }

    #[tokio::test]
    async fn move_conflict_for_file_without_extension() {
        let tmp = tempfile::tempdir().unwrap();
        let c = Categorizer::new(
            tmp.path().to_path_buf(),
            AppSettings::default_categories(),
            true,
        );

        let src1 = tmp.path().join("README");
        fs::write(&src1, b"a").await.unwrap();
        let dest1 = c.move_to_category(&src1, "Other").await.unwrap();
        assert_eq!(dest1, tmp.path().join("Other").join("README"));

        let src2 = tmp.path().join("sub").join("README");
        fs::create_dir_all(src2.parent().unwrap()).await.unwrap();
        fs::write(&src2, b"b").await.unwrap();
        let dest2 = c.move_to_category(&src2, "Other").await.unwrap();
        assert_eq!(dest2, tmp.path().join("Other").join("README (1)"));
    }

    // ─── Property 13: Auto-categorizer totality ──────────────────────────────

    proptest! {
        /// Property 13: For ANY filename and ANY optional MIME type, a
        /// categorizer built from the default category set always returns
        /// `Some(category)` — categorization never fails to produce a result,
        /// falling back to "Other" when no extension or MIME rule matches.
        ///
        /// **Validates: Requirements 7.1, 7.3, 7.4**
        #[test]
        fn prop_categorize_is_total(
            filename in ".*",
            mime in proptest::option::of(".*"),
        ) {
            let c = categorizer();
            let result = c.categorize(&filename, mime.as_deref());

            // Totality: a result is always produced for the default rule set.
            prop_assert!(
                result.is_some(),
                "categorize returned None for filename={filename:?} mime={mime:?}"
            );

            // The returned category must be one of the configured categories.
            let category = result.unwrap();
            let known: Vec<String> = AppSettings::default_categories()
                .iter()
                .map(|r| r.category.clone())
                .collect();
            prop_assert!(
                known.iter().any(|c| c == category),
                "categorize returned unknown category {category:?} for filename={filename:?}"
            );
        }
    }
}
