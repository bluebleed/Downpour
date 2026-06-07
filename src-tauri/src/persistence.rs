//! Persistence layer for saving download state, queue, and settings across app restarts.
//!
//! Uses JSON file storage in the app data directory with debounced writes.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, Notify};

use crate::models::{DownloadItem, DownloadStatus, SegmentState};
use crate::settings::AppSettings;

/// File names used for JSON persistence.
const DOWNLOADS_FILE: &str = "downloads.json";
const SETTINGS_FILE: &str = "settings.json";
const SEGMENTS_DIR: &str = "segments";

/// Debounce interval for writes.
const DEBOUNCE_MS: u64 = 500;

// ─── Internal wrapper for debounced download writes ────────────────────────────

#[derive(Clone)]
struct DebouncedWriter {
    dirty: Arc<Mutex<bool>>,
    notify: Arc<Notify>,
}

impl DebouncedWriter {
    fn new() -> Self {
        Self {
            dirty: Arc::new(Mutex::new(false)),
            notify: Arc::new(Notify::new()),
        }
    }

    /// Mark as dirty and notify the background flush task.
    async fn mark_dirty(&self) {
        let mut dirty = self.dirty.lock().await;
        *dirty = true;
        self.notify.notify_one();
    }
}

// ─── Persistence Layer ─────────────────────────────────────────────────────────

/// Persists download state, segment offsets, and settings as JSON files.
///
/// Storage layout:
/// ```text
/// <data_dir>/downpour/
/// ├── downloads.json      — Vec<DownloadItem>
/// ├── settings.json       — AppSettings
/// └── segments/
///     └── <id>.json       — Vec<SegmentState>
/// ```
#[derive(Clone)]
pub struct PersistenceLayer {
    db_path: PathBuf,
    download_writer: DebouncedWriter,
    downloads_cache: Arc<Mutex<Vec<DownloadItem>>>,
}

impl PersistenceLayer {
    /// Create a new persistence layer, initializing the storage directory.
    ///
    /// Uses `dirs::data_dir()` (falling back to `dirs::config_dir()`) and creates
    /// a "downpour" subdirectory within it.
    pub fn new() -> Result<Self> {
        let base = dirs::data_dir()
            .or_else(dirs::config_dir)
            .context("could not determine app data directory")?;

        let db_path = base.join("downpour");
        std::fs::create_dir_all(&db_path)
            .with_context(|| format!("failed to create data dir: {}", db_path.display()))?;
        std::fs::create_dir_all(db_path.join(SEGMENTS_DIR))
            .with_context(|| format!("failed to create segments dir: {}", db_path.display()))?;

        let layer = Self {
            db_path,
            download_writer: DebouncedWriter::new(),
            downloads_cache: Arc::new(Mutex::new(Vec::new())),
        };

        Ok(layer)
    }

    /// Create a persistence layer at a specific path (useful for testing).
    pub fn with_path(db_path: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(&db_path)
            .with_context(|| format!("failed to create data dir: {}", db_path.display()))?;
        std::fs::create_dir_all(db_path.join(SEGMENTS_DIR))
            .with_context(|| format!("failed to create segments dir: {}", db_path.display()))?;

        let layer = Self {
            db_path,
            download_writer: DebouncedWriter::new(),
            downloads_cache: Arc::new(Mutex::new(Vec::new())),
        };

        Ok(layer)
    }

    /// Start the background debounced writer task. Call once during app setup.
    pub fn start_background_writer(&self) {
        let writer = self.download_writer.clone();
        let cache = self.downloads_cache.clone();
        let path = self.db_path.join(DOWNLOADS_FILE);

        // Use Tauri's managed runtime rather than `tokio::spawn`: this is called
        // from the synchronous Tauri `.setup()` closure, where there is no Tokio
        // reactor in context (so a bare `tokio::spawn` panics with "there is no
        // reactor running"). `tauri::async_runtime::spawn` always resolves the
        // app-wide runtime regardless of the calling thread's context.
        tauri::async_runtime::spawn(async move {
            loop {
                writer.notify.notified().await;

                // Wait for the debounce period — if more writes come in during
                // this window, the delay resets.
                loop {
                    tokio::select! {
                        _ = tokio::time::sleep(Duration::from_millis(DEBOUNCE_MS)) => {
                            break; // debounce period elapsed, flush now
                        }
                        _ = writer.notify.notified() => {
                            // Another write came in, restart the timer
                            continue;
                        }
                    }
                }

                // Flush to disk
                let mut dirty = writer.dirty.lock().await;
                if *dirty {
                    *dirty = false;
                    let items = cache.lock().await.clone();
                    if let Err(e) = write_json_atomic(&path, &items) {
                        eprintln!("persistence: failed to flush downloads: {e:?}");
                    }
                }
            }
        });
    }

    /// Save or update a single download item.
    ///
    /// Updates the in-memory cache and schedules a debounced write.
    pub async fn save_download(&self, item: &DownloadItem) -> Result<()> {
        let mut cache = self.downloads_cache.lock().await;
        if let Some(existing) = cache.iter_mut().find(|d| d.id == item.id) {
            *existing = item.clone();
        } else {
            cache.push(item.clone());
        }
        drop(cache);
        self.download_writer.mark_dirty().await;
        Ok(())
    }

    /// Load all downloads from disk.
    ///
    /// - Restores items with "downloading" status as "paused".
    /// - If the file is corrupted, renames it with `.corrupt` suffix and returns empty.
    pub async fn load_all_downloads(&self) -> Result<Vec<DownloadItem>> {
        let path = self.db_path.join(DOWNLOADS_FILE);

        if !path.exists() {
            return Ok(Vec::new());
        }

        let items: Vec<DownloadItem> = load_json_with_recovery(&path).unwrap_or_default();

        // Restore "downloading" items as "paused" — they were active when the app exited.
        let items: Vec<DownloadItem> = items
            .into_iter()
            .map(|mut item| {
                if item.status == DownloadStatus::Downloading {
                    item.status = DownloadStatus::Paused;
                }
                item
            })
            .collect();

        // Populate the in-memory cache
        let mut cache = self.downloads_cache.lock().await;
        *cache = items.clone();

        Ok(items)
    }

    /// Save segment state for a specific download.
    pub async fn save_segment_state(&self, id: &str, segments: &[SegmentState]) -> Result<()> {
        let path = self.db_path.join(SEGMENTS_DIR).join(format!("{id}.json"));
        write_json_atomic(&path, &segments)?;
        Ok(())
    }

    /// Load segment state for a specific download.
    ///
    /// Returns empty vec if no segment file exists or if it's corrupted.
    ///
    /// Part of the persistence interface from the design. Resume currently
    /// reconstructs offsets from the in-memory item, so this standalone loader
    /// is not yet exercised in the production wiring but is retained as public
    /// API (and covered by unit tests).
    #[allow(dead_code)]
    pub async fn load_segments(&self, id: &str) -> Result<Vec<SegmentState>> {
        let path = self.db_path.join(SEGMENTS_DIR).join(format!("{id}.json"));

        if !path.exists() {
            return Ok(Vec::new());
        }

        match load_json_with_recovery(&path) {
            Ok(segments) => Ok(segments),
            Err(_) => Ok(Vec::new()),
        }
    }

    /// Save application settings to disk (immediate write, not debounced).
    pub async fn save_settings(&self, settings: &AppSettings) -> Result<()> {
        let path = self.db_path.join(SETTINGS_FILE);
        write_json_atomic(&path, settings)?;
        Ok(())
    }

    /// Load application settings from disk.
    ///
    /// - On first launch (file doesn't exist), returns `AppSettings::default()`.
    /// - If the file is corrupted, renames with `.corrupt` suffix and returns defaults.
    /// - If the persisted `download_dir` no longer exists, falls back to OS default.
    pub async fn load_settings(&self) -> Result<AppSettings> {
        let path = self.db_path.join(SETTINGS_FILE);

        if !path.exists() {
            return Ok(AppSettings::default());
        }

        let mut settings: AppSettings = match load_json_with_recovery(&path) {
            Ok(s) => s,
            Err(_) => return Ok(AppSettings::default()),
        };

        // If the persisted download_dir no longer exists, fall back to OS default.
        if !settings.download_dir.exists() {
            settings.download_dir = dirs::download_dir().unwrap_or_else(std::env::temp_dir);
        }

        Ok(settings)
    }

    /// Delete a download and its segment state from disk.
    pub async fn delete_download(&self, id: &str) -> Result<()> {
        // Remove from in-memory cache
        let mut cache = self.downloads_cache.lock().await;
        cache.retain(|d| d.id != id);
        drop(cache);

        // Schedule debounced write for the downloads list
        self.download_writer.mark_dirty().await;

        // Remove segment file if it exists
        let seg_path = self.db_path.join(SEGMENTS_DIR).join(format!("{id}.json"));
        if seg_path.exists() {
            std::fs::remove_file(&seg_path)
                .with_context(|| format!("failed to delete segment file for {id}"))?;
        }

        Ok(())
    }

    /// Get the storage directory path.
    ///
    /// Used by tests to write fixtures directly; retained as public API.
    #[allow(dead_code)]
    pub fn db_path(&self) -> &Path {
        &self.db_path
    }
}

// ─── Helper functions ──────────────────────────────────────────────────────────

/// Write data as JSON to a file atomically (write to temp, then rename).
fn write_json_atomic<T: Serialize>(path: &Path, data: &T) -> Result<()> {
    let json = serde_json::to_string_pretty(data).context("failed to serialize data to JSON")?;

    let tmp_path = path.with_extension("tmp");
    std::fs::write(&tmp_path, json.as_bytes())
        .with_context(|| format!("failed to write temp file: {}", tmp_path.display()))?;

    std::fs::rename(&tmp_path, path)
        .with_context(|| format!("failed to rename temp to: {}", path.display()))?;

    Ok(())
}

/// Load JSON from a file with corruption recovery.
///
/// If parsing fails, renames the file with `.corrupt` suffix and returns an error.
fn load_json_with_recovery<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read file: {}", path.display()))?;

    match serde_json::from_str::<T>(&content) {
        Ok(data) => Ok(data),
        Err(e) => {
            // Corrupted file: rename with .corrupt suffix
            let corrupt_path = path.with_extension("corrupt");
            eprintln!(
                "persistence: corrupted file detected at {}, renaming to {}",
                path.display(),
                corrupt_path.display()
            );
            let _ = std::fs::rename(path, &corrupt_path);
            Err(anyhow::anyhow!("corrupted JSON at {}: {e}", path.display()))
        }
    }
}

// ─── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{DownloadItem, DownloadStatus, SegmentState, SegmentStatus};
    use tempfile::TempDir;

    fn test_persistence() -> (PersistenceLayer, TempDir) {
        let tmp = TempDir::new().unwrap();
        let layer = PersistenceLayer::with_path(tmp.path().to_path_buf()).unwrap();
        (layer, tmp)
    }

    #[tokio::test]
    async fn test_save_and_load_download() {
        let (layer, _tmp) = test_persistence();

        let item = DownloadItem::new(
            "test-1".into(),
            "https://example.com/file.zip".into(),
            "file.zip".into(),
        );

        layer.save_download(&item).await.unwrap();

        // Force flush by writing directly (bypass debounce for test)
        let cache = layer.downloads_cache.lock().await.clone();
        let path = layer.db_path.join(DOWNLOADS_FILE);
        write_json_atomic(&path, &cache).unwrap();

        let loaded = layer.load_all_downloads().await.unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].id, "test-1");
        assert_eq!(loaded[0].filename, "file.zip");
    }

    #[tokio::test]
    async fn test_downloading_restored_as_paused() {
        let (layer, _tmp) = test_persistence();

        let mut item = DownloadItem::new(
            "dl-1".into(),
            "https://example.com/big.bin".into(),
            "big.bin".into(),
        );
        item.status = DownloadStatus::Downloading;

        // Write directly to disk
        let path = layer.db_path.join(DOWNLOADS_FILE);
        write_json_atomic(&path, &vec![item]).unwrap();

        let loaded = layer.load_all_downloads().await.unwrap();
        assert_eq!(loaded[0].status, DownloadStatus::Paused);
    }

    #[tokio::test]
    async fn test_corrupted_file_recovery() {
        let (layer, _tmp) = test_persistence();

        // Write garbage to downloads file
        let path = layer.db_path.join(DOWNLOADS_FILE);
        std::fs::write(&path, "not valid json {{{{").unwrap();

        let loaded = layer.load_all_downloads().await.unwrap();
        assert!(loaded.is_empty());

        // Verify corrupt file was renamed
        assert!(path.with_extension("corrupt").exists());
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn test_settings_default_on_first_launch() {
        let (layer, _tmp) = test_persistence();

        let settings = layer.load_settings().await.unwrap();
        assert_eq!(settings.max_concurrent, 3);
        assert_eq!(settings.default_segments, 4);
        assert_eq!(settings.speed_limit, 0);
    }

    #[tokio::test]
    async fn test_settings_round_trip() {
        let (layer, _tmp) = test_persistence();

        let settings = AppSettings {
            max_concurrent: 5,
            default_segments: 8,
            speed_limit: 1_000_000,
            ..AppSettings::default()
        };

        layer.save_settings(&settings).await.unwrap();
        let loaded = layer.load_settings().await.unwrap();

        assert_eq!(loaded.max_concurrent, 5);
        assert_eq!(loaded.default_segments, 8);
        assert_eq!(loaded.speed_limit, 1_000_000);
    }

    #[tokio::test]
    async fn test_settings_download_dir_fallback() {
        let (layer, _tmp) = test_persistence();

        let settings = AppSettings {
            download_dir: PathBuf::from("/nonexistent/path/that/does/not/exist"),
            ..AppSettings::default()
        };

        layer.save_settings(&settings).await.unwrap();
        let loaded = layer.load_settings().await.unwrap();

        // Should fall back to OS default, not the nonexistent path
        assert_ne!(
            loaded.download_dir,
            PathBuf::from("/nonexistent/path/that/does/not/exist")
        );
    }

    #[tokio::test]
    async fn test_save_and_load_segments() {
        let (layer, _tmp) = test_persistence();

        let segments = vec![
            SegmentState {
                index: 0,
                start: 0,
                end: 499,
                downloaded: 250,
                status: SegmentStatus::Downloading,
            },
            SegmentState {
                index: 1,
                start: 500,
                end: 999,
                downloaded: 100,
                status: SegmentStatus::Pending,
            },
        ];

        layer.save_segment_state("dl-1", &segments).await.unwrap();
        let loaded = layer.load_segments("dl-1").await.unwrap();

        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].downloaded, 250);
        assert_eq!(loaded[1].start, 500);
    }

    #[tokio::test]
    async fn test_load_segments_missing_file() {
        let (layer, _tmp) = test_persistence();

        let loaded = layer.load_segments("nonexistent").await.unwrap();
        assert!(loaded.is_empty());
    }

    #[tokio::test]
    async fn test_delete_download() {
        let (layer, _tmp) = test_persistence();

        let item = DownloadItem::new(
            "del-1".into(),
            "https://example.com/file.zip".into(),
            "file.zip".into(),
        );
        layer.save_download(&item).await.unwrap();

        let segments = vec![SegmentState {
            index: 0,
            start: 0,
            end: 999,
            downloaded: 500,
            status: SegmentStatus::Downloading,
        }];
        layer.save_segment_state("del-1", &segments).await.unwrap();

        layer.delete_download("del-1").await.unwrap();

        // Verify cache is empty
        let cache = layer.downloads_cache.lock().await;
        assert!(cache.is_empty());
        drop(cache);

        // Verify segment file is gone
        let seg_path = layer.db_path.join(SEGMENTS_DIR).join("del-1.json");
        assert!(!seg_path.exists());
    }

    #[tokio::test]
    async fn test_corrupted_settings_recovery() {
        let (layer, _tmp) = test_persistence();

        // Write garbage to settings file
        let path = layer.db_path.join(SETTINGS_FILE);
        std::fs::write(&path, "{{invalid json!!").unwrap();

        let settings = layer.load_settings().await.unwrap();
        // Should return defaults
        assert_eq!(settings.max_concurrent, 3);
        assert_eq!(settings.default_segments, 4);

        // Verify corrupt file was renamed
        assert!(path.with_extension("corrupt").exists());
    }
}

// ─── Property tests: settings persistence round-trip ─────────────────────────────

#[cfg(test)]
mod settings_roundtrip_tests {
    use super::*;
    use crate::settings::CategoryRule;
    use proptest::prelude::*;
    use tempfile::TempDir;

    /// Strategy for an arbitrary `CategoryRule` with small, bounded collections.
    fn category_rule_strategy() -> impl Strategy<Value = CategoryRule> {
        (
            "[a-zA-Z0-9 ]{0,20}",
            prop::collection::vec("\\.[a-z0-9]{1,5}", 0..6),
            prop::collection::vec("[a-z]{1,8}/[a-z]{1,8}", 0..4),
            "[a-zA-Z0-9 ]{0,20}",
        )
            .prop_map(
                |(category, extensions, mime_patterns, subfolder)| CategoryRule {
                    category,
                    extensions,
                    mime_patterns,
                    subfolder,
                },
            )
    }

    /// Strategy for an arbitrary, valid `AppSettings` paired with a directory-name
    /// fragment.
    ///
    /// `download_dir` is left as a placeholder here; the test materializes a real
    /// directory inside a temp dir using the returned fragment so that the path
    /// exists on disk. This matters because `load_settings` substitutes the OS
    /// default download directory whenever the persisted `download_dir` does not
    /// exist, which would otherwise prevent that single field from round-tripping.
    fn app_settings_strategy() -> impl Strategy<Value = (AppSettings, String)> {
        let numbers = (1usize..=10, 1u32..=32, any::<u64>(), any::<u64>());
        let flags = (any::<bool>(), any::<bool>(), any::<bool>(), any::<bool>());
        let collections = (
            prop::collection::vec(category_rule_strategy(), 0..5),
            prop::collection::vec("\\.[a-z0-9]{1,5}", 0..8),
            prop::option::of("[a-zA-Z0-9_./-]{1,30}"),
            prop::option::of("[a-zA-Z0-9_./-]{1,30}"),
        );
        let dir_fragment = "[a-zA-Z0-9_-]{1,12}";

        (numbers, flags, collections, dir_fragment).prop_map(
            |(
                (max_concurrent, default_segments, speed_limit, capture_min_size),
                (auto_categorize, auto_start_queue, minimize_to_tray, notifications_enabled),
                (categories, capture_extensions, ytdlp_path, ffmpeg_path),
                dir_fragment,
            )| {
                let settings = AppSettings {
                    // Replaced by the test with a directory that actually exists.
                    download_dir: PathBuf::new(),
                    max_concurrent,
                    default_segments,
                    speed_limit,
                    auto_categorize,
                    categories,
                    auto_start_queue,
                    resume_on_startup: false,
                    minimize_to_tray,
                    notifications_enabled,
                    confirm_on_delete: true,
                    capture_min_size,
                    capture_extensions,
                    ytdlp_path: ytdlp_path.map(PathBuf::from),
                    ffmpeg_path: ffmpeg_path.map(PathBuf::from),
                };
                (settings, dir_fragment)
            },
        )
    }

    /// Assert every field of two `AppSettings` is equal (the type does not derive
    /// `PartialEq`, so compare field-by-field).
    fn assert_settings_eq(
        loaded: &AppSettings,
        original: &AppSettings,
    ) -> Result<(), TestCaseError> {
        prop_assert_eq!(&loaded.download_dir, &original.download_dir);
        prop_assert_eq!(loaded.max_concurrent, original.max_concurrent);
        prop_assert_eq!(loaded.default_segments, original.default_segments);
        prop_assert_eq!(loaded.speed_limit, original.speed_limit);
        prop_assert_eq!(loaded.auto_categorize, original.auto_categorize);
        prop_assert_eq!(loaded.auto_start_queue, original.auto_start_queue);
        prop_assert_eq!(loaded.minimize_to_tray, original.minimize_to_tray);
        prop_assert_eq!(loaded.notifications_enabled, original.notifications_enabled);
        prop_assert_eq!(loaded.capture_min_size, original.capture_min_size);
        prop_assert_eq!(&loaded.capture_extensions, &original.capture_extensions);
        prop_assert_eq!(&loaded.ytdlp_path, &original.ytdlp_path);
        prop_assert_eq!(&loaded.ffmpeg_path, &original.ffmpeg_path);

        prop_assert_eq!(loaded.categories.len(), original.categories.len());
        for (a, b) in loaded.categories.iter().zip(original.categories.iter()) {
            prop_assert_eq!(&a.category, &b.category);
            prop_assert_eq!(&a.extensions, &b.extensions);
            prop_assert_eq!(&a.mime_patterns, &b.mime_patterns);
            prop_assert_eq!(&a.subfolder, &b.subfolder);
        }
        Ok(())
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]

        /// Property 10: Persistence round-trip for settings.
        ///
        /// For any valid AppSettings configuration, saving to disk via
        /// `save_settings` and loading it back via `load_settings` produces an
        /// equivalent AppSettings with identical field values.
        ///
        /// The generated `download_dir` is created on disk so it survives the
        /// load-time existence check and round-trips like every other field.
        ///
        /// **Validates: Requirement 5.4**
        #[test]
        fn prop_settings_persistence_round_trip(
            (settings_template, dir_fragment) in app_settings_strategy()
        ) {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();

            rt.block_on(async move {
                let tmp = TempDir::new().unwrap();

                // Materialize an existing download directory (prefixed to avoid
                // Windows reserved device names such as CON/NUL).
                let download_dir = tmp.path().join(format!("downloads_{dir_fragment}"));
                std::fs::create_dir_all(&download_dir).unwrap();

                let mut original = settings_template;
                original.download_dir = download_dir;

                let layer = PersistenceLayer::with_path(tmp.path().join("data")).unwrap();
                layer.save_settings(&original).await.unwrap();
                let loaded = layer.load_settings().await.unwrap();

                assert_settings_eq(&loaded, &original)
            })?;
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Property 9: Persistence round-trip for downloads (Task 3.2)
//
// This module is intentionally separate from the `tests` module above so that
// it can be added without disturbing the existing unit tests.
// ─────────────────────────────────────────────────────────────────────────────
#[cfg(test)]
mod prop_round_trip_tests {
    use super::*;
    use crate::models::{DownloadItem, DownloadStatus, DownloadType, SegmentState, SegmentStatus};
    use proptest::prelude::*;
    use proptest::test_runner::TestCaseResult;
    use std::collections::HashMap;
    use tempfile::TempDir;

    // ─── Generators ─────────────────────────────────────────────────────────────

    fn arb_download_status() -> impl Strategy<Value = DownloadStatus> {
        prop_oneof![
            Just(DownloadStatus::Queued),
            Just(DownloadStatus::Downloading),
            Just(DownloadStatus::Paused),
            Just(DownloadStatus::Complete),
            Just(DownloadStatus::Error),
            Just(DownloadStatus::Merging),
        ]
    }

    fn arb_segment_status() -> impl Strategy<Value = SegmentStatus> {
        prop_oneof![
            Just(SegmentStatus::Pending),
            Just(SegmentStatus::Downloading),
            Just(SegmentStatus::Complete),
            Just(SegmentStatus::Error),
            Just(SegmentStatus::Paused),
        ]
    }

    fn arb_download_type() -> impl Strategy<Value = DownloadType> {
        prop_oneof![
            Just(DownloadType::Http),
            Just(DownloadType::Media),
            Just(DownloadType::Batch),
        ]
    }

    prop_compose! {
        fn arb_segment()(
            index in any::<u32>(),
            start in any::<u64>(),
            extra in any::<u64>(),
            downloaded in any::<u64>(),
            status in arb_segment_status(),
        ) -> SegmentState {
            // Keep end >= start so the byte range stays valid; serialization
            // fidelity itself is independent of the ordering.
            let end = start.saturating_add(extra);
            SegmentState { index, start, end, downloaded, status }
        }
    }

    prop_compose! {
        fn arb_download_item()(
            id in "[a-zA-Z0-9_-]{1,32}",
            url in ".{0,64}",
            filename in ".{0,64}",
            total_size in any::<u64>(),
            downloaded in any::<u64>(),
            status in arb_download_status(),
            category in proptest::option::of(".{0,24}"),
            created_at in any::<u64>(),
            completed_at in proptest::option::of(any::<u64>()),
            speed in any::<u64>(),
            eta in proptest::option::of(any::<u64>()),
            segments in proptest::collection::vec(arb_segment(), 0..6),
            error_message in proptest::option::of(".{0,48}"),
            headers in proptest::collection::hash_map("[a-zA-Z0-9-]{1,16}", ".{0,32}", 0..6),
            cookies in proptest::option::of(".{0,48}"),
            referer in proptest::option::of(".{0,48}"),
            is_resumable in any::<bool>(),
            download_type in arb_download_type(),
            segment_count in any::<u32>(),
            media_format_id in proptest::option::of("[a-zA-Z0-9+]{1,12}"),
        ) -> DownloadItem {
            DownloadItem {
                id,
                url,
                filename,
                total_size,
                downloaded,
                status,
                category,
                created_at,
                completed_at,
                speed,
                eta,
                segments,
                error_message,
                headers,
                cookies,
                referer,
                is_resumable,
                download_type,
                segment_count,
                media_format_id,
                output_path: None,
                output_template: None,
            }
        }
    }

    // ─── Field-by-field comparison helpers ───────────────────────────────────────

    fn assert_segments_equal(expected: &[SegmentState], got: &[SegmentState]) -> TestCaseResult {
        prop_assert_eq!(expected.len(), got.len(), "segment count differs");
        for (e, g) in expected.iter().zip(got.iter()) {
            prop_assert_eq!(e.index, g.index);
            prop_assert_eq!(e.start, g.start);
            prop_assert_eq!(e.end, g.end);
            prop_assert_eq!(e.downloaded, g.downloaded);
            prop_assert_eq!(&e.status, &g.status);
        }
        Ok(())
    }

    /// Assert every field of `got` matches `expected`, accounting for the one
    /// documented load-time transformation (downloading -> paused).
    fn assert_item_round_trip(expected: &DownloadItem, got: &DownloadItem) -> TestCaseResult {
        // Requirement 5.2: an item that was "downloading" at exit is restored
        // as "paused". Every other field must survive the round-trip verbatim.
        let expected_status = if expected.status == DownloadStatus::Downloading {
            DownloadStatus::Paused
        } else {
            expected.status.clone()
        };

        prop_assert_eq!(&got.id, &expected.id);
        prop_assert_eq!(&got.url, &expected.url);
        prop_assert_eq!(&got.filename, &expected.filename);
        prop_assert_eq!(got.total_size, expected.total_size);
        prop_assert_eq!(got.downloaded, expected.downloaded);
        prop_assert_eq!(&got.status, &expected_status);
        prop_assert_eq!(&got.category, &expected.category);
        prop_assert_eq!(got.created_at, expected.created_at);
        prop_assert_eq!(&got.completed_at, &expected.completed_at);
        prop_assert_eq!(got.speed, expected.speed);
        prop_assert_eq!(&got.eta, &expected.eta);
        prop_assert_eq!(&got.error_message, &expected.error_message);
        prop_assert_eq!(&got.headers, &expected.headers);
        prop_assert_eq!(&got.cookies, &expected.cookies);
        prop_assert_eq!(&got.referer, &expected.referer);
        prop_assert_eq!(got.is_resumable, expected.is_resumable);
        prop_assert_eq!(&got.download_type, &expected.download_type);
        prop_assert_eq!(got.segment_count, expected.segment_count);
        prop_assert_eq!(&got.media_format_id, &expected.media_format_id);
        assert_segments_equal(&expected.segments, &got.segments)?;
        Ok(())
    }

    // ─── Property 9 ───────────────────────────────────────────────────────────────

    proptest! {
        // Each case touches a fresh temp directory on disk; keep the case count
        // modest so the suite stays fast.
        #![proptest_config(ProptestConfig::with_cases(64))]

        /// Property 9: Persistence round-trip for downloads.
        ///
        /// For any valid DownloadItem with arbitrary segment states, saving it
        /// through the persistence layer and loading it back SHALL produce an
        /// equivalent DownloadItem with identical field values (modulo the
        /// documented "downloading" -> "paused" restore rule).
        ///
        /// **Validates: Requirements 5.1, 5.2**
        #[test]
        fn prop_download_persistence_round_trip(item in arb_download_item()) {
            let rt = tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap();

            rt.block_on(async move {
                let tmp = TempDir::new().unwrap();
                let layer = PersistenceLayer::with_path(tmp.path().to_path_buf()).unwrap();

                // Save via the public API (updates the in-memory cache), then
                // flush to disk exactly as the debounced background writer does.
                layer.save_download(&item).await.unwrap();
                let cache = layer.downloads_cache.lock().await.clone();
                let path = layer.db_path().join(DOWNLOADS_FILE);
                write_json_atomic(&path, &cache).unwrap();

                // Deserialize back from disk.
                let loaded = layer.load_all_downloads().await.unwrap();
                prop_assert_eq!(loaded.len(), 1);
                assert_item_round_trip(&item, &loaded[0])?;
                Ok(())
            })?;
        }
    }

    // ─── Complementary unit test: a fully-populated, complex item ─────────────────

    #[tokio::test]
    async fn round_trip_preserves_all_fields_for_complex_item() {
        let tmp = TempDir::new().unwrap();
        let layer = PersistenceLayer::with_path(tmp.path().to_path_buf()).unwrap();

        let mut headers = HashMap::new();
        headers.insert("User-Agent".to_string(), "Downpour/1.0".to_string());
        headers.insert("Accept".to_string(), "*/*".to_string());

        let item = DownloadItem {
            id: "complex-1".into(),
            url: "https://example.com/big.iso".into(),
            filename: "big.iso".into(),
            total_size: 1_073_741_824,
            downloaded: 536_870_912,
            status: DownloadStatus::Paused,
            category: Some("Software".into()),
            created_at: 1_700_000_000,
            completed_at: None,
            speed: 2_500_000,
            eta: Some(214),
            segments: vec![
                SegmentState {
                    index: 0,
                    start: 0,
                    end: 499,
                    downloaded: 500,
                    status: SegmentStatus::Complete,
                },
                SegmentState {
                    index: 1,
                    start: 500,
                    end: 999,
                    downloaded: 123,
                    status: SegmentStatus::Paused,
                },
            ],
            error_message: None,
            headers,
            cookies: Some("session=abc123".into()),
            referer: Some("https://example.com/downloads".into()),
            is_resumable: true,
            download_type: DownloadType::Http,
            segment_count: 4,
            media_format_id: None,
            output_path: Some("/downloads/Software/big.iso".into()),
            output_template: None,
        };

        layer.save_download(&item).await.unwrap();
        let cache = layer.downloads_cache.lock().await.clone();
        let path = layer.db_path().join(DOWNLOADS_FILE);
        write_json_atomic(&path, &cache).unwrap();

        let loaded = layer.load_all_downloads().await.unwrap();
        assert_eq!(loaded.len(), 1);
        let got = &loaded[0];

        assert_eq!(got.id, item.id);
        assert_eq!(got.url, item.url);
        assert_eq!(got.filename, item.filename);
        assert_eq!(got.total_size, item.total_size);
        assert_eq!(got.downloaded, item.downloaded);
        assert_eq!(got.status, DownloadStatus::Paused);
        assert_eq!(got.category, item.category);
        assert_eq!(got.created_at, item.created_at);
        assert_eq!(got.completed_at, item.completed_at);
        assert_eq!(got.speed, item.speed);
        assert_eq!(got.eta, item.eta);
        assert_eq!(got.error_message, item.error_message);
        assert_eq!(got.headers, item.headers);
        assert_eq!(got.cookies, item.cookies);
        assert_eq!(got.referer, item.referer);
        assert_eq!(got.is_resumable, item.is_resumable);
        assert_eq!(got.download_type, item.download_type);
        assert_eq!(got.segment_count, item.segment_count);
        assert_eq!(got.segments.len(), 2);
        assert_eq!(got.segments[0].status, SegmentStatus::Complete);
        assert_eq!(got.segments[1].downloaded, 123);
    }
}
