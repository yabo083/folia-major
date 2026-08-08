// src-tauri/src/settings.rs
// M1 设置与存储：settings JSON 读写落盘、locale 校验、缓存目录配置（默认/自定义/选择/重置）。
// 键名与 electron-store 完全一致；get_settings 返回 getPublicSettings() 语义（boolean 归一 + 默认值）。

use serde_json::{json, Map, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use tauri::{AppHandle, Manager, State};
use tauri_plugin_dialog::DialogExt;

pub const APP_LOCALE_KEY: &str = "APP_LOCALE";
const SETTINGS_DIRECTORY: &str = "folia";
const SETTINGS_FILE: &str = "settings.json";
const CACHE_DIRECTORY: &str = "media-cache";

// 桌面偏好设置键（electron/main.cjs 常量；窗口/缓存模块共用）。
pub const MINIMIZE_TO_TRAY: &str = "MINIMIZE_TO_TRAY";
pub const HIDE_TASKBAR_ICON: &str = "HIDE_TASKBAR_ICON";
pub const REMOTE_CONTROL_ALWAYS_ON_TOP: &str = "REMOTE_CONTROL_ALWAYS_ON_TOP";
pub const REMOTE_CONTROL_SKIP_TASKBAR: &str = "REMOTE_CONTROL_SKIP_TASKBAR";
pub const MAIN_WINDOW_ALWAYS_ON_TOP: &str = "MAIN_WINDOW_ALWAYS_ON_TOP";
pub const TRANSPARENT_PLAYER_BACKGROUND: &str = "TRANSPARENT_PLAYER_BACKGROUND";
pub const DISCORD_RICH_PRESENCE_ENABLED: &str = "DISCORD_RICH_PRESENCE_ENABLED";
pub const VOICE_INPUT_PAUSE_ENABLED: &str = "VOICE_INPUT_PAUSE_ENABLED";
pub const UPDATE_CHANNEL: &str = "UPDATE_CHANNEL";
pub const DISCORD_RICH_PRESENCE_APPLICATION_ID: &str = "DISCORD_RICH_PRESENCE_APPLICATION_ID";
pub const CACHE_DIRECTORY_SETTING_KEY: &str = "CACHE_DIRECTORY";
const REMOTE_ALWAYS_ON_TOP_DEFAULT_MIGRATION: &str =
    "REMOTE_CONTROL_ALWAYS_ON_TOP_DEFAULT_FALSE_MIGRATED";

// getPublicSettings() 中强制归一为 boolean 的键（readStoredBoolean 默认值同 Electron）。
const PUBLIC_BOOLEAN_DEFAULTS: &[(&str, bool)] = &[
    (MINIMIZE_TO_TRAY, false),
    (HIDE_TASKBAR_ICON, false),
    // Remote control must not cover native dialogs or other applications by default.
    // Users can still enable this explicitly from the remote toolbar.
    (REMOTE_CONTROL_ALWAYS_ON_TOP, false),
    (REMOTE_CONTROL_SKIP_TASKBAR, false),
    (MAIN_WINDOW_ALWAYS_ON_TOP, false),
    (TRANSPARENT_PLAYER_BACKGROUND, false),
    (DISCORD_RICH_PRESENCE_ENABLED, false),
    (VOICE_INPUT_PAUSE_ENABLED, false),
];

// save-settings 中 Boolean(value) 归一化的键（Electron 语义；注意不含 MAIN_WINDOW_ALWAYS_ON_TOP）。
const SAVE_BOOLEAN_COERCED_KEYS: &[&str] = &[
    MINIMIZE_TO_TRAY,
    HIDE_TASKBAR_ICON,
    REMOTE_CONTROL_ALWAYS_ON_TOP,
    REMOTE_CONTROL_SKIP_TASKBAR,
    TRANSPARENT_PLAYER_BACKGROUND,
    DISCORD_RICH_PRESENCE_ENABLED,
    VOICE_INPUT_PAUSE_ENABLED,
];

const NATIVE_BLUR_KEY: &str = "enable_player_page_native_blur";
const DEFAULT_UPDATE_CHANNEL: &str = "realeco";
const VALID_UPDATE_CHANNELS: &[&str] = &["realeco", "limo", "cielo", "internal"];

pub struct SettingsStore {
    path: PathBuf,
    values: Mutex<Map<String, Value>>,
}

impl SettingsStore {
    pub fn open(app_data_dir: impl AsRef<Path>) -> Result<Self, String> {
        let path = app_data_dir
            .as_ref()
            .join(SETTINGS_DIRECTORY)
            .join(SETTINGS_FILE);
        let mut values = load_settings(&path)?;
        if !values.contains_key(REMOTE_ALWAYS_ON_TOP_DEFAULT_MIGRATION)
            && matches!(
                values.get(REMOTE_CONTROL_ALWAYS_ON_TOP),
                Some(Value::Bool(true))
            )
        {
            values.insert(REMOTE_CONTROL_ALWAYS_ON_TOP.to_string(), Value::Bool(false));
            values.insert(
                REMOTE_ALWAYS_ON_TOP_DEFAULT_MIGRATION.to_string(),
                Value::Bool(true),
            );
            write_settings(&path, &values)?;
        }
        Ok(Self {
            path,
            values: Mutex::new(values),
        })
    }

    pub fn all(&self) -> Result<Value, String> {
        let values = self
            .values
            .lock()
            .map_err(|_| "settings lock poisoned".to_string())?;
        Ok(Value::Object(values.clone()))
    }

    pub fn get(&self, key: &str) -> Option<Value> {
        self.values.lock().ok()?.get(key).cloned()
    }

    pub fn get_bool(&self, key: &str) -> bool {
        self.get(key)
            .and_then(|value| value.as_bool())
            .unwrap_or(false)
    }

    pub fn has(&self, key: &str) -> bool {
        self.values
            .lock()
            .ok()
            .map(|values| values.contains_key(key))
            .unwrap_or(false)
    }

    pub fn set(&self, key: String, value: Value) -> Result<Value, String> {
        if key.trim().is_empty() {
            return Err("settings key must not be empty".to_string());
        }
        let mut values = self
            .values
            .lock()
            .map_err(|_| "settings lock poisoned".to_string())?;
        let mut next_values = values.clone();
        next_values.insert(key, value);
        write_settings(&self.path, &next_values)?;
        *values = next_values.clone();
        Ok(Value::Object(next_values))
    }

    pub fn delete(&self, key: &str) -> Result<bool, String> {
        let mut values = self
            .values
            .lock()
            .map_err(|_| "settings lock poisoned".to_string())?;
        if !values.contains_key(key) {
            return Ok(false);
        }
        let mut next_values = values.clone();
        next_values.remove(key);
        write_settings(&self.path, &next_values)?;
        *values = next_values.clone();
        Ok(true)
    }

    fn set_locale(&self, locale: String) -> Result<String, String> {
        if matches!(locale.as_str(), "zh-CN" | "en" | "in") {
            self.set(APP_LOCALE_KEY.to_string(), Value::String(locale.clone()))?;
        }
        Ok(locale)
    }

    // 镜像 Electron readStoredBoolean：boolean/string/number 归一，其余回退默认值。
    fn read_stored_boolean(&self, key: &str, fallback: bool) -> bool {
        match self.get(key) {
            Some(Value::Bool(value)) => value,
            Some(Value::String(value)) => {
                let normalized = value.trim().to_lowercase();
                if normalized == "true" {
                    return true;
                }
                if normalized == "false" {
                    return false;
                }
                fallback
            }
            Some(Value::Number(value)) => value.as_i64().map(|v| v != 0).unwrap_or(fallback),
            _ => fallback,
        }
    }

    // 返回 getPublicSettings() 等价对象：全部存储值 + boolean 归一键 + 默认 UPDATE_CHANNEL。
    fn public_settings(&self) -> Result<Value, String> {
        let mut map = self
            .all()?
            .as_object()
            .cloned()
            .ok_or_else(|| "settings values must be a JSON object".to_string())?;
        for (key, fallback) in PUBLIC_BOOLEAN_DEFAULTS {
            map.insert(
                key.to_string(),
                Value::Bool(self.read_stored_boolean(key, *fallback)),
            );
        }
        let native_blur = matches!(self.get(NATIVE_BLUR_KEY), Some(Value::Bool(true)));
        map.insert(NATIVE_BLUR_KEY.to_string(), Value::Bool(native_blur));
        let channel = resolve_update_channel(self.get(UPDATE_CHANNEL));
        map.insert(UPDATE_CHANNEL.to_string(), Value::String(channel));
        Ok(Value::Object(map))
    }
}

fn load_settings(path: &Path) -> Result<Map<String, Value>, String> {
    if !path.exists() {
        return Ok(Map::new());
    }
    let content = fs::read_to_string(path).map_err(|error| format!("read settings: {error}"))?;
    let value: Value =
        serde_json::from_str(&content).map_err(|error| format!("parse settings: {error}"))?;
    value
        .as_object()
        .cloned()
        .ok_or_else(|| "settings file must contain a JSON object".to_string())
}

fn write_settings(path: &Path, values: &Map<String, Value>) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "settings path has no parent".to_string())?;
    fs::create_dir_all(parent).map_err(|error| format!("create settings directory: {error}"))?;
    let temporary_path = path.with_extension("json.tmp");
    let content = serde_json::to_vec_pretty(&Value::Object(values.clone()))
        .map_err(|error| format!("serialize settings: {error}"))?;
    fs::write(&temporary_path, content).map_err(|error| format!("write settings: {error}"))?;
    if let Err(error) = fs::rename(&temporary_path, path) {
        #[cfg(windows)]
        {
            fs::remove_file(path).map_err(|remove_error| {
                format!("replace settings ({error}); remove old file: {remove_error}")
            })?;
            fs::rename(&temporary_path, path)
                .map_err(|rename_error| format!("replace settings: {rename_error}"))?;
        }
        #[cfg(not(windows))]
        {
            let _ = fs::remove_file(&temporary_path);
            return Err(format!("replace settings: {error}"));
        }
    }
    Ok(())
}

// 默认缓存目录：app_data_dir/media-cache（对应 Electron getDefaultCacheDirectory 的 userData/media-cache）。
pub fn default_cache_directory(app_data_dir: &Path) -> PathBuf {
    app_data_dir.join(CACHE_DIRECTORY)
}

// 配置的缓存目录：CACHE_DIRECTORY 为非空字符串时使用之，否则回退默认（镜像 getConfiguredCacheDirectory）。
pub fn configured_cache_directory(store: &SettingsStore, app_data_dir: &Path) -> PathBuf {
    match store.get(CACHE_DIRECTORY_SETTING_KEY) {
        Some(Value::String(path)) if !path.trim().is_empty() => PathBuf::from(path),
        _ => default_cache_directory(app_data_dir),
    }
}

// normalizeUpdateChannelSelection：仅 realeco/limo/cielo 可被显式保存。
fn normalize_update_channel(value: &Value) -> Option<String> {
    let channel = value.as_str()?.trim().to_lowercase();
    if channel == "realeco" || channel == "limo" || channel == "cielo" {
        Some(channel)
    } else {
        None
    }
}

// getCurrentReleaseChannel().id：存储通道有效则用之，否则按版本回退 realeco。
fn resolve_update_channel(stored: Option<Value>) -> String {
    match stored {
        Some(Value::String(channel)) => {
            let normalized = channel.trim().to_lowercase();
            if VALID_UPDATE_CHANNELS.contains(&normalized.as_str()) {
                return normalized;
            }
            DEFAULT_UPDATE_CHANNEL.to_string()
        }
        _ => DEFAULT_UPDATE_CHANNEL.to_string(),
    }
}

// JS Boolean(value) 语义：0/""/null/false → false，其余 → true。
pub(crate) fn as_bool_value(value: &Value) -> bool {
    match value {
        Value::Bool(value) => *value,
        Value::Number(value) => value.as_i64().map(|v| v != 0).unwrap_or(true),
        Value::String(value) => !value.is_empty(),
        Value::Null => false,
        _ => true,
    }
}

#[tauri::command]
// 返回 getPublicSettings() 等价对象（含 boolean 归一键与默认 UPDATE_CHANNEL），不修改持久化数据。
pub fn get_settings(state: State<'_, SettingsStore>) -> Result<Value, String> {
    state.public_settings()
}

#[tauri::command]
// 持久化一个设置并按 getPublicSettings() 语义返回；保留 Electron 的特殊键行为
// （DISCORD_RICH_PRESENCE_APPLICATION_ID 只读返回），并在 M7 相关键变更时联动
// Stage 服务器同步与远程控制窗口样式（同 electron/main.cjs save-settings 钩子）。
pub fn save_settings(
    key: String,
    value: Value,
    app: tauri::AppHandle,
    state: State<'_, SettingsStore>,
) -> Result<Value, String> {
    if key == DISCORD_RICH_PRESENCE_APPLICATION_ID {
        return state.public_settings();
    }
    let next_value = if key == UPDATE_CHANNEL {
        match normalize_update_channel(&value) {
            Some(channel) => Value::String(channel),
            None => return state.public_settings(),
        }
    } else if SAVE_BOOLEAN_COERCED_KEYS.contains(&key.as_str()) {
        Value::Bool(as_bool_value(&value))
    } else {
        value
    };
    state.set(key.clone(), next_value.clone())?;

    // M7 联动：Stage 模式 source 变化时同步启动/停止服务器（异步，不阻塞保存）。
    if key == crate::stage::STAGE_MODE_SOURCE_KEY {
        if let Some(stage) = app.try_state::<crate::stage::StageState>() {
            let stage = stage.inner().clone();
            let app = app.clone();
            std::thread::spawn(move || {
                let _ = stage.sync_and_serve(&app);
            });
        }
    }
    if key == REMOTE_CONTROL_ALWAYS_ON_TOP || key == REMOTE_CONTROL_SKIP_TASKBAR {
        crate::remote::apply_remote_window_settings(&app);
    }
    if key == DISCORD_RICH_PRESENCE_ENABLED {
        // M8 联动：Discord 开关变化 → 重新评估连接并广播 playback-sync 状态。
        crate::discord::refresh(&app);
        crate::remote::broadcast_playback_sync_bridge_status(&app);
    }
    if key == VOICE_INPUT_PAUSE_ENABLED {
        // M8 联动：语音输入暂停开关变化 → 启动/停止轮询并推送状态。
        crate::voice::sync_state(&app);
    }
    // M10 联动：更新相关设置变化 → 触发检查/下载/重置状态（镜像 main.cjs save-settings）。
    if key == crate::updater::ENABLE_UPDATE_CHECK_KEY
        || key == crate::updater::ENABLE_AUTO_UPDATE_KEY
        || key == UPDATE_CHANNEL
    {
        crate::updater::on_setting_saved(&app, key.as_str(), &next_value);
    }

    state.public_settings()
}

#[tauri::command]
// 持久化受支持的 locale（zh-CN/en/in），无效值不落盘；始终返回传入值。
pub fn set_app_locale(locale: String, state: State<'_, SettingsStore>) -> Result<String, String> {
    state.set_locale(locale)
}

#[tauri::command]
// 返回当前缓存目录（配置或默认）及其 isDefault 标记；不创建目录（对齐 Electron get-cache-directory）。
pub fn get_cache_directory(
    app: AppHandle,
    state: State<'_, SettingsStore>,
) -> Result<Value, String> {
    let app_data_dir = app
        .path()
        .app_data_dir()
        .map_err(|error| format!("resolve app data directory: {error}"))?;
    let path = configured_cache_directory(&state, &app_data_dir);
    Ok(json!({
        "path": path.to_string_lossy(),
        "isDefault": !state.has(CACHE_DIRECTORY_SETTING_KEY),
    }))
}

fn pick_cache_directory(app: &AppHandle) -> Option<PathBuf> {
    app.dialog()
        .file()
        .set_title("Choose cache directory")
        .blocking_pick_folder()
        .and_then(|file_path| file_path.into_path().ok())
}

#[tauri::command]
// 弹出原生目录选择框；选择后持久化 CACHE_DIRECTORY，返回 { canceled, path, isDefault }。
pub fn choose_cache_directory(
    app: AppHandle,
    state: State<'_, SettingsStore>,
) -> Result<Value, String> {
    let app_data_dir = app
        .path()
        .app_data_dir()
        .map_err(|error| format!("resolve app data directory: {error}"))?;
    let current = configured_cache_directory(&state, &app_data_dir);
    let is_default = !state.has(CACHE_DIRECTORY_SETTING_KEY);
    let selected = pick_cache_directory(&app);
    match selected {
        Some(path) => {
            state.set(
                CACHE_DIRECTORY_SETTING_KEY.to_string(),
                Value::String(path.to_string_lossy().to_string()),
            )?;
            Ok(json!({ "canceled": false, "path": path.to_string_lossy(), "isDefault": false }))
        }
        None => Ok(
            json!({ "canceled": true, "path": current.to_string_lossy(), "isDefault": is_default }),
        ),
    }
}

#[tauri::command]
// 删除 CACHE_DIRECTORY 设置，恢复默认缓存目录。
pub fn reset_cache_directory(
    app: AppHandle,
    state: State<'_, SettingsStore>,
) -> Result<Value, String> {
    let _ = state.delete(CACHE_DIRECTORY_SETTING_KEY)?;
    let app_data_dir = app
        .path()
        .app_data_dir()
        .map_err(|error| format!("resolve app data directory: {error}"))?;
    let path = configured_cache_directory(&state, &app_data_dir);
    Ok(json!({ "path": path.to_string_lossy(), "isDefault": true }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir() -> PathBuf {
        std::env::temp_dir().join(format!(
            "folia-settings-test-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[test]
    fn missing_settings_are_empty() {
        let root = temp_dir();
        let store = SettingsStore::open(&root).unwrap();
        assert_eq!(store.all().unwrap(), serde_json::json!({}));
    }

    #[test]
    fn settings_round_trip_survives_reopen() {
        let root = temp_dir();
        let store = SettingsStore::open(&root).unwrap();
        store.set("enabled".into(), Value::Bool(true)).unwrap();
        store
            .set("name".into(), Value::String("Folia".into()))
            .unwrap();
        store.set("count".into(), serde_json::json!(42)).unwrap();
        store
            .set(
                "nested".into(),
                serde_json::json!({ "active": false, "items": [1, 2] }),
            )
            .unwrap();

        let reopened = SettingsStore::open(&root).unwrap();
        assert_eq!(
            reopened.all().unwrap(),
            serde_json::json!({
                "enabled": true,
                "name": "Folia",
                "count": 42,
                "nested": { "active": false, "items": [1, 2] }
            })
        );
    }

    #[test]
    fn overwrite_preserves_other_keys() {
        let root = temp_dir();
        let store = SettingsStore::open(&root).unwrap();
        store
            .set("key".into(), Value::String("old".into()))
            .unwrap();
        store.set("other".into(), Value::Bool(true)).unwrap();
        store
            .set("key".into(), Value::String("new".into()))
            .unwrap();
        assert_eq!(
            store.all().unwrap(),
            serde_json::json!({ "key": "new", "other": true })
        );
    }

    #[test]
    fn app_locale_is_persisted() {
        let root = temp_dir();
        let store = SettingsStore::open(&root).unwrap();
        assert_eq!(store.set_locale("zh-CN".into()).unwrap(), "zh-CN");
        assert_eq!(
            SettingsStore::open(&root).unwrap().all().unwrap(),
            serde_json::json!({ "APP_LOCALE": "zh-CN" })
        );
    }

    #[test]
    fn invalid_locale_is_not_persisted() {
        let root = temp_dir();
        let store = SettingsStore::open(&root).unwrap();
        assert_eq!(store.set_locale("fr-FR".into()).unwrap(), "fr-FR");
        assert_eq!(
            SettingsStore::open(&root).unwrap().all().unwrap(),
            serde_json::json!({})
        );
    }

    #[test]
    fn delete_removes_key() {
        let root = temp_dir();
        let store = SettingsStore::open(&root).unwrap();
        store.set("key".into(), Value::Bool(true)).unwrap();
        assert!(store.has("key"));
        assert!(store.delete("key").unwrap());
        assert!(!store.has("key"));
        assert_eq!(
            SettingsStore::open(&root).unwrap().all().unwrap(),
            serde_json::json!({})
        );
    }

    #[test]
    fn default_cache_directory_is_media_cache() {
        let root = temp_dir();
        assert_eq!(default_cache_directory(&root), root.join("media-cache"));
    }

    #[test]
    fn configured_cache_directory_falls_back_to_default() {
        let root = temp_dir();
        let store = SettingsStore::open(&root).unwrap();
        assert_eq!(
            configured_cache_directory(&store, &root),
            root.join("media-cache")
        );
    }

    #[test]
    fn configured_cache_directory_uses_setting() {
        let root = temp_dir();
        let store = SettingsStore::open(&root).unwrap();
        store
            .set(
                CACHE_DIRECTORY_SETTING_KEY.to_string(),
                Value::String("D:\\Custom Cache".into()),
            )
            .unwrap();
        assert_eq!(
            configured_cache_directory(&store, &root),
            PathBuf::from("D:\\Custom Cache")
        );
    }

    #[test]
    fn configured_cache_directory_ignores_blank_setting() {
        let root = temp_dir();
        let store = SettingsStore::open(&root).unwrap();
        store
            .set(
                CACHE_DIRECTORY_SETTING_KEY.to_string(),
                Value::String("   ".into()),
            )
            .unwrap();
        assert_eq!(
            configured_cache_directory(&store, &root),
            root.join("media-cache")
        );
    }

    #[test]
    fn public_settings_boolean_defaults_and_coercion() {
        let root = temp_dir();
        let store = SettingsStore::open(&root).unwrap();
        store
            .set(MINIMIZE_TO_TRAY.to_string(), Value::String("true".into()))
            .unwrap();
        store
            .set(HIDE_TASKBAR_ICON.to_string(), Value::Number(1.into()))
            .unwrap();
        store
            .set("custom".into(), Value::String("kept".into()))
            .unwrap();

        let public = store.public_settings().unwrap();
        assert_eq!(public[MINIMIZE_TO_TRAY], true);
        assert_eq!(public[HIDE_TASKBAR_ICON], true);
        assert_eq!(public[REMOTE_CONTROL_ALWAYS_ON_TOP], false);
        assert_eq!(public[REMOTE_CONTROL_SKIP_TASKBAR], false);
        assert_eq!(public["custom"], "kept");
        assert_eq!(public[UPDATE_CHANNEL], DEFAULT_UPDATE_CHANNEL);
        assert_eq!(public[NATIVE_BLUR_KEY], false);
    }

    #[test]
    fn migrates_legacy_remote_always_on_top_default_once() {
        let root = temp_dir();
        let store = SettingsStore::open(&root).unwrap();
        store
            .set(REMOTE_CONTROL_ALWAYS_ON_TOP.to_string(), Value::Bool(true))
            .unwrap();

        let migrated = SettingsStore::open(&root).unwrap();
        assert!(!migrated.get_bool(REMOTE_CONTROL_ALWAYS_ON_TOP));
        assert_eq!(
            migrated.get(REMOTE_ALWAYS_ON_TOP_DEFAULT_MIGRATION),
            Some(Value::Bool(true))
        );

        migrated
            .set(REMOTE_CONTROL_ALWAYS_ON_TOP.to_string(), Value::Bool(true))
            .unwrap();
        assert!(SettingsStore::open(&root)
            .unwrap()
            .get_bool(REMOTE_CONTROL_ALWAYS_ON_TOP));
    }

    #[test]
    fn public_settings_updates_channel_from_stored_value() {
        let root = temp_dir();
        let store = SettingsStore::open(&root).unwrap();
        store
            .set(UPDATE_CHANNEL.to_string(), Value::String("limo".into()))
            .unwrap();
        let public = store.public_settings().unwrap();
        assert_eq!(public[UPDATE_CHANNEL], "limo");
    }

    #[test]
    fn normalize_update_channel_accepts_only_known_channels() {
        assert_eq!(
            normalize_update_channel(&json!("RealEco")),
            Some("realeco".to_string())
        );
        assert_eq!(
            normalize_update_channel(&json!("limo")),
            Some("limo".to_string())
        );
        assert_eq!(normalize_update_channel(&json!("unknown")), None);
        assert_eq!(normalize_update_channel(&json!(123)), None);
    }

    #[test]
    fn save_boolean_coercion_matches_electron() {
        let root = temp_dir();
        let store = SettingsStore::open(&root).unwrap();
        store.set("key".into(), Value::String("x".into())).unwrap();
        // SettingsStore::set 原样存储；Boolean 归一由 save_settings 命令通过 SAVE_BOOLEAN_COERCED_KEYS 完成。
        let saved = store
            .set(HIDE_TASKBAR_ICON.to_string(), Value::String("false".into()))
            .unwrap();
        assert_eq!(saved[HIDE_TASKBAR_ICON], "false");
        assert!(SAVE_BOOLEAN_COERCED_KEYS.contains(&HIDE_TASKBAR_ICON));
    }

    #[test]
    fn as_bool_value_mirrors_js_boolean() {
        assert!(!as_bool_value(&Value::Bool(false)));
        assert!(as_bool_value(&Value::Bool(true)));
        assert!(!as_bool_value(&Value::Null));
        assert!(!as_bool_value(&Value::String(String::new())));
        assert!(as_bool_value(&Value::String("true".into())));
        assert!(!as_bool_value(&serde_json::json!(0)));
        assert!(as_bool_value(&serde_json::json!(1)));
        assert!(as_bool_value(&serde_json::json!([])));
    }
}
