use crate::categorizer::Categorizer;
use crate::persistence::PersistenceLayer;
use crate::queue::QueueManager;
use crate::settings::AppSettings;
use crate::{build_media_extractor, CategorizerState, MediaState, MinimizeToTray, SettingsState};
use std::sync::atomic::Ordering;

/// Return the current application settings.
#[tauri::command]
pub async fn get_settings(
    settings: tauri::State<'_, SettingsState>,
) -> Result<AppSettings, String> {
    Ok(settings.lock().await.clone())
}

/// Validate and apply a full settings update, persisting it and propagating the
/// relevant values to the live components without an app restart (Req 11.5).
#[tauri::command]
pub async fn update_settings(
    settings: tauri::State<'_, SettingsState>,
    queue: tauri::State<'_, QueueManager>,
    persistence: tauri::State<'_, PersistenceLayer>,
    media: tauri::State<'_, MediaState>,
    categorizer: tauri::State<'_, CategorizerState>,
    minimize_to_tray: tauri::State<'_, MinimizeToTray>,
    new_settings: AppSettings,
) -> Result<AppSettings, String> {
    // Validate the bounded numeric fields (Req 11.1, 11.2, 11.3).
    AppSettings::validate_max_concurrent(new_settings.max_concurrent).map_err(|e| e.to_string())?;
    AppSettings::validate_segments(new_settings.default_segments).map_err(|e| e.to_string())?;
    AppSettings::validate_speed_limit(new_settings.speed_limit).map_err(|e| e.to_string())?;
    Categorizer::validate_rules(&new_settings.categories).map_err(|e| e.to_string())?;

    // Persist first so a write failure aborts the change.
    persistence
        .save_settings(&new_settings)
        .await
        .map_err(|e| format!("{e:#}"))?;

    // Apply to live components (Req 11.5).
    queue.set_max_concurrent(new_settings.max_concurrent).await;
    queue.set_speed_limit(new_settings.speed_limit);
    queue.set_download_dir(new_settings.download_dir.clone());
    *media.lock().await = build_media_extractor(&new_settings);
    *categorizer.lock().await = Categorizer::from_settings(&new_settings);
    minimize_to_tray.store(new_settings.minimize_to_tray, Ordering::SeqCst);
    *settings.lock().await = new_settings.clone();

    Ok(new_settings)
}

/// Set the global speed limit in bytes/sec (0 = unlimited). Accepts a signed
/// value from the UI and rejects negatives (Req 11.3, 4.3).
#[tauri::command]
pub async fn set_speed_limit(
    settings: tauri::State<'_, SettingsState>,
    queue: tauri::State<'_, QueueManager>,
    persistence: tauri::State<'_, PersistenceLayer>,
    bytes_per_sec: i64,
) -> Result<(), String> {
    let validated =
        AppSettings::validate_speed_limit_signed(bytes_per_sec).map_err(|e| e.to_string())?;

    queue.set_speed_limit(validated);

    let snapshot = {
        let mut s = settings.lock().await;
        s.speed_limit = validated;
        s.clone()
    };
    persistence
        .save_settings(&snapshot)
        .await
        .map_err(|e| format!("{e:#}"))?;
    Ok(())
}

/// Set the maximum concurrent downloads (1-10), resizing the queue semaphore
/// live (Req 3.4, 11.1).
#[tauri::command]
pub async fn set_max_concurrent(
    settings: tauri::State<'_, SettingsState>,
    queue: tauri::State<'_, QueueManager>,
    persistence: tauri::State<'_, PersistenceLayer>,
    value: usize,
) -> Result<(), String> {
    let validated = AppSettings::validate_max_concurrent(value).map_err(|e| e.to_string())?;

    queue.set_max_concurrent(validated).await;

    let snapshot = {
        let mut s = settings.lock().await;
        s.max_concurrent = validated;
        s.clone()
    };
    persistence
        .save_settings(&snapshot)
        .await
        .map_err(|e| format!("{e:#}"))?;
    Ok(())
}
