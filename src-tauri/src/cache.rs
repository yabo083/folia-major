// src-tauri/src/cache.rs
// M6 缓存层：audio/cover 磁盘缓存，路径与元数据行为对齐 electron/main.cjs 的缓存段。
// 路径：<cacheRoot>/audio/<sha256(cacheKey)>.bin + .json；cover 同构。cacheRoot 来自 M1 配置的缓存目录。

use crate::settings::{configured_cache_directory, SettingsStore};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Manager, State};

const DEFAULT_AUDIO_MIME_TYPE: &str = "audio/mpeg";
const DEFAULT_COVER_MIME_TYPE: &str = "application/octet-stream";

// 依据缓存 key 生成 64 位十六进制 sha256 文件名基（镜像 getAudioCacheBaseName）。
fn sha256_hex(input: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    let digest = hasher.finalize();
    let mut output = String::with_capacity(digest.len() * 2);
    for byte in digest {
        output.push_str(&format!("{byte:02x}"));
    }
    output
}

// 解析当前缓存根目录（配置或默认）。
fn cache_root(app: &AppHandle, store: &SettingsStore) -> Result<PathBuf, String> {
    let app_data_dir = app
        .path()
        .app_data_dir()
        .map_err(|error| format!("resolve app data directory: {error}"))?;
    Ok(configured_cache_directory(store, &app_data_dir))
}

fn audio_directory(root: &Path) -> PathBuf {
    root.join("audio")
}

fn cover_directory(root: &Path) -> PathBuf {
    root.join("cover")
}

// 返回 kind 目录下某个 cacheKey 的 .bin/.json 路径对。
fn cache_paths(root: &Path, kind: &str, cache_key: &str) -> (PathBuf, PathBuf) {
    let directory = root.join(kind);
    let base_name = sha256_hex(cache_key);
    (
        directory.join(format!("{base_name}.bin")),
        directory.join(format!("{base_name}.json")),
    )
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

// 递归创建文件所在目录。
fn ensure_parent_directory(path: &Path) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("cache path has no parent: {}", path.display()))?;
    fs::create_dir_all(parent).map_err(|error| format!("create cache directory: {error}"))
}

fn not_found_entry() -> Value {
    json!({ "found": false, "data": null, "mimeType": null })
}

// 读取音频 meta 中的 mimeType；缺失/非法时回退 audio/mpeg（镜像 readAudioCacheEntry）。
fn read_audio_mime_type(meta_path: &Path) -> String {
    let raw_meta = fs::read_to_string(meta_path).unwrap_or_default();
    if let Ok(meta) = serde_json::from_str::<Value>(&raw_meta) {
        if let Some(mime_type) = meta.get("mimeType").and_then(Value::as_str) {
            if !mime_type.trim().is_empty() {
                return mime_type.to_string();
            }
        }
    }
    DEFAULT_AUDIO_MIME_TYPE.to_string()
}

fn read_audio_entry(root: &Path, cache_key: &str) -> Value {
    let (data_path, meta_path) = cache_paths(root, "audio", cache_key);
    let data = match fs::read(&data_path) {
        Ok(data) => data,
        Err(_) => return not_found_entry(),
    };
    json!({
        "found": true,
        "data": data,
        "mimeType": read_audio_mime_type(&meta_path),
    })
}

fn write_audio_entry(
    root: &Path,
    cache_key: &str,
    data: &[u8],
    mime_type: &str,
) -> Result<(), String> {
    let (data_path, meta_path) = cache_paths(root, "audio", cache_key);
    ensure_parent_directory(&data_path)?;
    let mime = if mime_type.trim().is_empty() {
        DEFAULT_AUDIO_MIME_TYPE
    } else {
        mime_type
    };
    let meta = json!({
        "cacheKey": cache_key,
        "mimeType": mime,
        "size": data.len(),
        "updatedAt": unix_ms(),
    });
    fs::write(&data_path, data).map_err(|error| format!("write audio cache data: {error}"))?;
    let meta_content = serde_json::to_vec_pretty(&meta)
        .map_err(|error| format!("serialize audio cache meta: {error}"))?;
    fs::write(&meta_path, meta_content)
        .map_err(|error| format!("write audio cache meta: {error}"))?;
    Ok(())
}

// 读取 cover 条目：空载荷或元数据校验失败（cacheKey/mimeType/size 不匹配）时删除 .bin+.json 并视为未命中。
fn read_cover_entry(root: &Path, cache_key: &str) -> Value {
    let (data_path, meta_path) = cache_paths(root, "cover", cache_key);
    let data = match fs::read(&data_path) {
        Ok(data) => data,
        Err(_) => return not_found_entry(),
    };
    if data.is_empty() {
        let _ = fs::remove_file(&data_path);
        let _ = fs::remove_file(&meta_path);
        return not_found_entry();
    }
    let valid_mime_type = fs::read_to_string(&meta_path).ok().and_then(|raw| {
        let meta: Value = serde_json::from_str(&raw).ok()?;
        let key_matches = meta
            .get("cacheKey")
            .and_then(Value::as_str)
            .map(|key| key == cache_key)
            .unwrap_or(false);
        let mime_type = meta.get("mimeType").and_then(Value::as_str);
        let mime_valid = mime_type
            .map(|mime| mime.starts_with("image/"))
            .unwrap_or(false);
        let size_matches = meta
            .get("size")
            .and_then(Value::as_u64)
            .map(|size| size as usize == data.len())
            .unwrap_or(false);
        if key_matches && mime_valid && size_matches {
            mime_type.map(|mime| mime.to_string())
        } else {
            None
        }
    });
    match valid_mime_type {
        Some(mime_type) => json!({ "found": true, "data": data, "mimeType": mime_type }),
        None => {
            let _ = fs::remove_file(&data_path);
            let _ = fs::remove_file(&meta_path);
            not_found_entry()
        }
    }
}

// 写入 cover：拒绝空载荷与非 image/* 类型（镜像 writeCoverCacheEntry 的校验）。
fn write_cover_entry(
    root: &Path,
    cache_key: &str,
    data: &[u8],
    mime_type: &str,
) -> Result<(), String> {
    if data.is_empty() {
        return Err("Cannot persist an empty cover payload".to_string());
    }
    if !mime_type.starts_with("image/") {
        return Err("Cover cache only accepts image payloads".to_string());
    }
    let (data_path, meta_path) = cache_paths(root, "cover", cache_key);
    ensure_parent_directory(&data_path)?;
    let meta = json!({
        "cacheKey": cache_key,
        "mimeType": mime_type,
        "size": data.len(),
        "updatedAt": unix_ms(),
    });
    fs::write(&data_path, data).map_err(|error| format!("write cover cache data: {error}"))?;
    let meta_content = serde_json::to_vec_pretty(&meta)
        .map_err(|error| format!("serialize cover cache meta: {error}"))?;
    fs::write(&meta_path, meta_content)
        .map_err(|error| format!("write cover cache meta: {error}"))?;
    Ok(())
}

// 删除 cover 的 .bin+.json（忽略缺失）。
fn remove_cover_entry(root: &Path, cache_key: &str) -> bool {
    let (data_path, meta_path) = cache_paths(root, "cover", cache_key);
    let _ = fs::remove_file(&data_path);
    let _ = fs::remove_file(&meta_path);
    true
}

// 统计目录内 .bin 文件的字节总和（镜像 getAudioCacheUsageBytes/getCoverCacheUsageBytes）。
fn directory_usage_bytes(directory: &Path) -> u64 {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(_) => return 0,
    };
    entries
        .filter_map(|entry| entry.ok())
        .filter(|entry| {
            entry
                .file_type()
                .map(|kind| kind.is_file())
                .unwrap_or(false)
                && entry.file_name().to_string_lossy().ends_with(".bin")
        })
        .filter_map(|entry| fs::metadata(entry.path()).ok())
        .map(|metadata| metadata.len())
        .sum()
}

// 统计目录内 .bin 文件的数量与字节总和（镜像 getAudioCacheStats）。
fn directory_stats(directory: &Path) -> Value {
    let mut total_size = 0u64;
    let mut total_count = 0u64;
    if let Ok(entries) = fs::read_dir(directory) {
        for entry in entries.flatten() {
            if !entry
                .file_type()
                .map(|kind| kind.is_file())
                .unwrap_or(false)
            {
                continue;
            }
            if !entry.file_name().to_string_lossy().ends_with(".bin") {
                continue;
            }
            if let Ok(metadata) = fs::metadata(entry.path()) {
                total_size += metadata.len();
                total_count += 1;
            }
        }
    }
    json!({ "size": total_size, "count": total_count })
}

// 递归删除整个目录；目录不存在视为成功。
fn clear_directory(directory: &Path) -> bool {
    match fs::remove_dir_all(directory) {
        Ok(_) => true,
        Err(error) if error.kind() == ErrorKind::NotFound => true,
        Err(_) => false,
    }
}

#[tauri::command]
// 读取音频缓存条目：未命中返回 { found:false }，meta 缺失/非法时 mimeType 回退 audio/mpeg。
pub fn get_audio_cache(
    app: AppHandle,
    state: State<'_, SettingsStore>,
    cache_key: String,
) -> Result<Value, String> {
    let root = cache_root(&app, &state)?;
    Ok(read_audio_entry(&root, &cache_key))
}

#[tauri::command]
// 判断音频缓存是否命中（仅检查 .bin 是否存在）。
pub fn has_audio_cache(
    app: AppHandle,
    state: State<'_, SettingsStore>,
    cache_key: String,
) -> Result<bool, String> {
    let root = cache_root(&app, &state)?;
    let (data_path, _) = cache_paths(&root, "audio", &cache_key);
    Ok(data_path.exists())
}

#[tauri::command]
// 写入音频缓存：data 为字节数组，.bin 与 .json（meta：cacheKey/mimeType/size/updatedAt）同步落盘。
pub fn save_audio_cache(
    app: AppHandle,
    state: State<'_, SettingsStore>,
    cache_key: String,
    data: Vec<u8>,
    mime_type: Option<String>,
) -> Result<bool, String> {
    let root = cache_root(&app, &state)?;
    write_audio_entry(
        &root,
        &cache_key,
        &data,
        mime_type.as_deref().unwrap_or(DEFAULT_AUDIO_MIME_TYPE),
    )?;
    Ok(true)
}

#[tauri::command]
// 返回音频缓存目录 .bin 文件的字节总和。
pub fn get_audio_cache_usage(
    app: AppHandle,
    state: State<'_, SettingsStore>,
) -> Result<u64, String> {
    let root = cache_root(&app, &state)?;
    Ok(directory_usage_bytes(&audio_directory(&root)))
}

#[tauri::command]
// 返回音频缓存统计 { size, count }（仅 .bin 文件）。
pub fn get_audio_cache_stats(
    app: AppHandle,
    state: State<'_, SettingsStore>,
) -> Result<Value, String> {
    let root = cache_root(&app, &state)?;
    Ok(directory_stats(&audio_directory(&root)))
}

#[tauri::command]
// 递归删除整个音频缓存目录。
pub fn clear_audio_cache(app: AppHandle, state: State<'_, SettingsStore>) -> Result<bool, String> {
    let root = cache_root(&app, &state)?;
    Ok(clear_directory(&audio_directory(&root)))
}

#[tauri::command]
// 读取封面缓存条目：空载荷或元数据校验失败时清理脏文件并返回未命中。
pub fn get_cover_cache(
    app: AppHandle,
    state: State<'_, SettingsStore>,
    cache_key: String,
) -> Result<Value, String> {
    let root = cache_root(&app, &state)?;
    Ok(read_cover_entry(&root, &cache_key))
}

#[tauri::command]
// 写入封面缓存：拒绝空载荷与非 image/* 类型。
pub fn save_cover_cache(
    app: AppHandle,
    state: State<'_, SettingsStore>,
    cache_key: String,
    data: Vec<u8>,
    mime_type: Option<String>,
) -> Result<bool, String> {
    let root = cache_root(&app, &state)?;
    let mime_type = mime_type.unwrap_or_else(|| DEFAULT_COVER_MIME_TYPE.to_string());
    write_cover_entry(&root, &cache_key, &data, &mime_type)?;
    Ok(true)
}

#[tauri::command]
// 删除单个封面缓存条目（.bin+.json）。
pub fn remove_cover_cache(
    app: AppHandle,
    state: State<'_, SettingsStore>,
    cache_key: String,
) -> Result<bool, String> {
    let root = cache_root(&app, &state)?;
    Ok(remove_cover_entry(&root, &cache_key))
}

#[tauri::command]
// 返回封面缓存目录 .bin 文件的字节总和。
pub fn get_cover_cache_usage(
    app: AppHandle,
    state: State<'_, SettingsStore>,
) -> Result<u64, String> {
    let root = cache_root(&app, &state)?;
    Ok(directory_usage_bytes(&cover_directory(&root)))
}

#[tauri::command]
// 递归删除整个封面缓存目录。
pub fn clear_cover_cache(app: AppHandle, state: State<'_, SettingsStore>) -> Result<bool, String> {
    let root = cache_root(&app, &state)?;
    Ok(clear_directory(&cover_directory(&root)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_root() -> PathBuf {
        std::env::temp_dir().join(format!(
            "folia-cache-test-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[test]
    fn sha256_filename_is_stable_hex() {
        let first = sha256_hex("album://123");
        let second = sha256_hex("album://123");
        assert_eq!(first.len(), 64);
        assert!(first.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(first, second);
        assert_ne!(sha256_hex("album://123"), sha256_hex("album://124"));
    }

    #[test]
    fn audio_cache_round_trip_survives_reopen() {
        let root = temp_root();
        write_audio_entry(&root, "key-1", &[1, 2, 3, 4], "audio/mp4").unwrap();

        let entry = read_audio_entry(&root, "key-1");
        assert_eq!(entry["found"], true);
        assert_eq!(entry["data"], json!([1, 2, 3, 4]));
        assert_eq!(entry["mimeType"], "audio/mp4");

        let (data_path, meta_path) = cache_paths(&root, "audio", "key-1");
        assert!(data_path.exists());
        assert!(meta_path.exists());

        let meta: Value = serde_json::from_str(&fs::read_to_string(&meta_path).unwrap()).unwrap();
        assert_eq!(meta["cacheKey"], "key-1");
        assert_eq!(meta["size"], 4);
    }

    #[test]
    fn audio_mime_type_defaults_when_meta_missing_or_invalid() {
        let root = temp_root();
        let (data_path, _) = cache_paths(&root, "audio", "key-no-meta");
        ensure_parent_directory(&data_path).unwrap();
        fs::write(&data_path, [9, 9]).unwrap();
        assert_eq!(
            read_audio_entry(&root, "key-no-meta")["mimeType"],
            "audio/mpeg"
        );

        let (data_path, meta_path) = cache_paths(&root, "audio", "key-bad-meta");
        ensure_parent_directory(&data_path).unwrap();
        fs::write(&data_path, [9, 9]).unwrap();
        fs::write(&meta_path, "{ not json").unwrap();
        assert_eq!(
            read_audio_entry(&root, "key-bad-meta")["mimeType"],
            "audio/mpeg"
        );
    }

    #[test]
    fn audio_mime_type_defaults_when_empty() {
        let root = temp_root();
        write_audio_entry(&root, "key-empty-mime", &[1], "").unwrap();
        assert_eq!(
            read_audio_entry(&root, "key-empty-mime")["mimeType"],
            "audio/mpeg"
        );
    }

    #[test]
    fn audio_cache_miss_returns_not_found() {
        let root = temp_root();
        let entry = read_audio_entry(&root, "missing");
        assert_eq!(entry["found"], false);
        assert_eq!(entry["data"], Value::Null);
        assert_eq!(entry["mimeType"], Value::Null);
    }

    #[test]
    fn has_audio_cache_reflects_disk_state() {
        let root = temp_root();
        let (data_path, _) = cache_paths(&root, "audio", "key-has");
        ensure_parent_directory(&data_path).unwrap();
        assert!(!data_path.exists());
        fs::write(&data_path, [1]).unwrap();
        assert!(data_path.exists());
    }

    #[test]
    fn audio_usage_and_stats_count_only_bin_files() {
        let root = temp_root();
        write_audio_entry(&root, "a", &[1, 2, 3], "audio/mpeg").unwrap();
        write_audio_entry(&root, "b", &[1, 2, 3, 4, 5], "audio/mpeg").unwrap();
        let directory = audio_directory(&root);
        fs::write(directory.join("not-a-cache.txt"), [0; 100]).unwrap();
        fs::write(directory.join("orphan.json"), b"{}").unwrap();

        assert_eq!(directory_usage_bytes(&directory), 8);
        let stats = directory_stats(&directory);
        assert_eq!(stats["size"], 8);
        assert_eq!(stats["count"], 2);
    }

    #[test]
    fn audio_usage_is_zero_for_missing_directory() {
        let root = temp_root();
        assert_eq!(directory_usage_bytes(&audio_directory(&root)), 0);
        assert_eq!(
            directory_stats(&audio_directory(&root)),
            json!({ "size": 0, "count": 0 })
        );
    }

    #[test]
    fn clear_directory_removes_everything_and_is_idempotent() {
        let root = temp_root();
        let directory = audio_directory(&root);
        write_audio_entry(&root, "a", &[1], "audio/mpeg").unwrap();
        assert!(clear_directory(&directory));
        assert!(!directory.exists());
        assert!(clear_directory(&directory));
    }

    #[test]
    fn cover_round_trip_with_image_mime() {
        let root = temp_root();
        write_cover_entry(&root, "cover-1", &[10, 20, 30], "image/jpeg").unwrap();
        let entry = read_cover_entry(&root, "cover-1");
        assert_eq!(entry["found"], true);
        assert_eq!(entry["data"], json!([10, 20, 30]));
        assert_eq!(entry["mimeType"], "image/jpeg");
    }

    #[test]
    fn cover_rejects_empty_payload() {
        let root = temp_root();
        let error = write_cover_entry(&root, "cover-empty", &[], "image/jpeg").unwrap_err();
        assert!(error.contains("empty cover payload"));
    }

    #[test]
    fn cover_rejects_non_image_mime() {
        let root = temp_root();
        let error = write_cover_entry(&root, "cover-mime", &[1, 2], "application/octet-stream")
            .unwrap_err();
        assert!(error.contains("image payloads"));
    }

    #[test]
    fn cover_invalid_metadata_is_cleaned_and_reported_missing() {
        let root = temp_root();
        let (data_path, meta_path) = cache_paths(&root, "cover", "cover-bad");
        ensure_parent_directory(&data_path).unwrap();
        fs::write(&data_path, [1, 2, 3]).unwrap();
        fs::write(
            &meta_path,
            r#"{ "cacheKey": "different", "mimeType": "image/png", "size": 99 }"#,
        )
        .unwrap();

        let entry = read_cover_entry(&root, "cover-bad");
        assert_eq!(entry["found"], false);
        assert!(!data_path.exists());
        assert!(!meta_path.exists());
    }

    #[test]
    fn cover_empty_bin_is_cleaned_and_reported_missing() {
        let root = temp_root();
        let (data_path, meta_path) = cache_paths(&root, "cover", "cover-empty-file");
        ensure_parent_directory(&data_path).unwrap();
        fs::write(&data_path, []).unwrap();
        fs::write(&meta_path, r#"{}"#).unwrap();

        let entry = read_cover_entry(&root, "cover-empty-file");
        assert_eq!(entry["found"], false);
        assert!(!data_path.exists());
    }

    #[test]
    fn cover_remove_deletes_bin_and_meta() {
        let root = temp_root();
        write_cover_entry(&root, "cover-rm", &[1], "image/png").unwrap();
        let (data_path, meta_path) = cache_paths(&root, "cover", "cover-rm");
        assert!(remove_cover_entry(&root, "cover-rm"));
        assert!(!data_path.exists());
        assert!(!meta_path.exists());
        assert!(remove_cover_entry(&root, "cover-rm"));
    }
}
