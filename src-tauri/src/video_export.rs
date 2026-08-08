// src-tauri/src/video_export.rs
// M9 视频导出：保存对话框（sanitize + one-time token）、主窗口准备/恢复、原始二进制写文件。
// 行为对齐 electron/main.cjs 的 video-export 段（choose/prepare/restore/write 四命令）。
//
// WebView2 无法提供 Electron desktopCapturer 的 source id，捕获源由
// public/electron-shim.js 用 getDisplayMedia 适配（见 PLAN.md 风险与已知缺口）。
// 本模块只负责：
//   - 保存对话框：扩展名/文件名/展示名 sanitize，默认 Videos 目录，返回 writeToken；
//     save_file 回调 + oneshot，非阻塞主线程事件循环；
//   - 仅当主窗口存在时返回哨兵 capture source（系统选择器仍由用户决定，不能静默强制 Folia）；
//   - 主窗口 prepare/restore（快照一次、幂等、不跨窗口操作持锁；restore 先消费快照，
//     重建/缺失的主窗口不会把过期 bounds 留给下一次导出）；
//   - 原始 IPC 写文件：只接受 Raw body + 有效一次性 token，写 token 绑定的 Rust 选择路径。
// 五个命令都以调用窗口 label 做发送方信任校验：非主窗口一律拒绝（对齐 Electron 可信 sender）。

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use serde::Serialize;
use tauri::ipc::InvokeBody;
use tauri::{AppHandle, Manager};
#[cfg(desktop)]
use tauri::{PhysicalPosition, PhysicalSize};
use tauri_plugin_dialog::DialogExt;

use crate::window::MAIN_WINDOW_LABEL;

/// 哨兵 source id：Rust 与 public/electron-shim.js 必须保持一致的共享契约常量。
/// shim 只拦截 chromeMediaSource=desktop 且 chromeMediaSourceId 等于此值的
/// 遗留 getUserMedia 调用并返回缓存的 getDisplayMedia 流。
pub const VIDEO_EXPORT_SENTINEL_SOURCE_ID: &str = "__folia_tauri_getdisplaymedia_sentinel__";

/// write token 请求头（shim 在原始 IPC 请求里附带）。
const WRITE_TOKEN_HEADER: &str = "x-folia-video-export-token";
/// write token 最大长度（防滥用，token 本身为固定 41 字节 hex）。
const WRITE_TOKEN_MAX_LEN: usize = 128;
/// 一次性 token 保留时长：6 小时，覆盖超长曲目导出（选择→准备→录制→写入）。
const TOKEN_TTL: Duration = Duration::from_secs(6 * 60 * 60);
/// 滞留 token 最大数量（写失败/流程中断时的有界保留）。
const TOKEN_MAX_COUNT: usize = 8;

fn system_now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0)
}

/// 信任边界：五个视频导出命令只允许主窗口调用（对齐 Electron 侧"可信 sender 校验"）。
/// 远程/其他窗口即使拿到命令名也会被拒绝，不能只靠前端隐藏入口。
fn require_main_window_label(label: &str) -> Result<(), String> {
    if label == MAIN_WINDOW_LABEL {
        Ok(())
    } else {
        Err(format!(
            "video export command must be invoked from the main window, got label '{label}'"
        ))
    }
}

// ---------------------------------------------------------------------------
// 纯函数：sanitize 与尺寸校验（对齐 electron/main.cjs）
// ---------------------------------------------------------------------------

/// 扩展名白名单：mp4 之外一律归一为 webm（同 Electron safeExtension）。
fn sanitize_extension(extension: &str) -> &'static str {
    if extension == "mp4" {
        "mp4"
    } else {
        "webm"
    }
}

/// Windows 文件名非法字符（`<>:"/\|?*`）与 C0 控制字符（Electron 正则等价物）。
fn is_invalid_file_char(c: char) -> bool {
    matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') || (c as u32) < 0x20
}

/// 把非法文件名/控制字符替换为 `_`；剥离 Windows 不允许的结尾点号/空格。
fn sanitize_file_name(input: &str) -> String {
    let replaced: String = input
        .chars()
        .map(|c| if is_invalid_file_char(c) { '_' } else { c })
        .collect();
    let trimmed = replaced.trim_end_matches(['.', ' ']);
    if trimmed.is_empty() {
        "folia-export".to_string()
    } else {
        trimmed.to_string()
    }
}

/// 展示名：空串回退默认（同 Electron safeDisplayName）。
fn sanitize_display_name(display_name: &str, extension: &str) -> String {
    let trimmed = display_name.trim();
    if trimmed.is_empty() {
        if extension == "mp4" {
            "MP4 Video".to_string()
        } else {
            "WebM Video".to_string()
        }
    } else {
        trimmed.to_string()
    }
}

/// 默认文件名：sanitize 后强制匹配扩展名（同 Electron safeDefaultName/defaultFileName）。
fn build_save_file_name(default_name: &str, extension: &str) -> String {
    let safe = if default_name.trim().is_empty() {
        format!("folia-export.{extension}")
    } else {
        let sanitized = sanitize_file_name(default_name.trim());
        if sanitized.ends_with(&format!(".{extension}")) {
            return sanitized;
        }
        sanitized
    };
    if safe.ends_with(&format!(".{extension}")) {
        return safe;
    }
    let stem = match safe.rfind('.') {
        Some(idx) if idx > 0 => &safe[..idx],
        _ => safe.as_str(),
    };
    format!("{stem}.{extension}")
}

/// 尺寸校验：round 后必须有限且 >=320，超过 3840 钳制（同 Electron sanitizeVideoExportSize）。
/// 桌面专属（video_export_prepare_window 使用）；另有纯逻辑单元测试直接验证。
#[cfg(any(desktop, test))]
fn sanitize_export_size(width: f64, height: f64) -> Option<(u32, u32)> {
    let width = width.round();
    let height = height.round();
    if !width.is_finite() || !height.is_finite() || width < 320.0 || height < 320.0 {
        return None;
    }
    Some((width.min(3840.0) as u32, height.min(3840.0) as u32))
}

// ---------------------------------------------------------------------------
// 一次性 write token：token -> 保存对话框选中的确切路径
// ---------------------------------------------------------------------------

struct TokenEntry {
    token: String,
    path: PathBuf,
    expires_at_ms: u128,
}

/// 有界、过期即弃的一次性 token 存储（path 绑定 + consume 一次即清）。
pub struct VideoExportTokenStore {
    inner: Mutex<Vec<TokenEntry>>,
    ttl_ms: u128,
    max_count: usize,
    clock: Box<dyn Fn() -> u128 + Send + Sync>,
}

impl VideoExportTokenStore {
    pub fn new() -> Self {
        Self::with_clock(TOKEN_TTL, TOKEN_MAX_COUNT, Box::new(system_now_ms))
    }

    // 测试用：注入 ttl 与时钟。
    pub fn with_clock(
        ttl: Duration,
        max_count: usize,
        clock: Box<dyn Fn() -> u128 + Send + Sync>,
    ) -> Self {
        Self {
            inner: Mutex::new(Vec::new()),
            ttl_ms: ttl.as_millis(),
            max_count,
            clock,
        }
    }

    /// 记录 path 并返回不透明 token；注册前清理过期与超出上限的最旧项。
    pub fn register(&self, path: PathBuf) -> String {
        let now = (self.clock)();
        let mut entries = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        entries.retain(|entry| entry.expires_at_ms >= now);
        while entries.len() >= self.max_count {
            entries.remove(0);
        }
        let token = format!("ve-{:016x}-{:016x}", now, rand::random::<u64>());
        entries.push(TokenEntry {
            token: token.clone(),
            path,
            expires_at_ms: now + self.ttl_ms,
        });
        token
    }

    /// 一次性消费：token 匹配且未过期则返回绑定 path 并移除；否则 None。
    pub fn consume(&self, token: &str) -> Option<PathBuf> {
        let now = (self.clock)();
        let mut entries = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let index = entries
            .iter()
            .position(|entry| entry.token == token && entry.expires_at_ms >= now)?;
        Some(entries.remove(index).path)
    }

    /// 清空全部 token（应用退出/极端场景用；正常场景靠 TTL + 有界保留）。
    #[allow(dead_code)]
    pub fn clear(&self) {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
    }
}

impl Default for VideoExportTokenStore {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// 主窗口 prepare/restore 快照模型（纯逻辑可单测）
// 桌面专属：移动端没有可捕获/恢复的 WebviewWindow 全屏/最大化状态。
// ---------------------------------------------------------------------------

/// 恢复动作：fullscreen 优先于 maximized，否则仅恢复 bounds。
#[cfg(any(desktop, test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RestoreAction {
    Bounds,
    Maximize,
    Fullscreen,
}

/// 导出准备前保存的主窗口原始状态（外部位移 + 内部尺寸 + 最大化/全屏）。
#[cfg(any(desktop, test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct WindowRestoreSnapshot {
    position: (i32, i32),
    inner_size: (u32, u32),
    maximized: bool,
    fullscreen: bool,
}

#[cfg(any(desktop, test))]
impl WindowRestoreSnapshot {
    /// 捕获主窗口原始状态；任一查询失败都如实报错（不做 unwrap_or(false) 静默降级），
    /// 因为错误的最大化/全屏状态会在恢复时把窗口恢复到错误的形状。
    fn capture(window: &tauri::WebviewWindow) -> Result<Self, String> {
        let position = window
            .outer_position()
            .map_err(|error| format!("capture video export window position: {error}"))?;
        let size = window
            .inner_size()
            .map_err(|error| format!("capture video export window size: {error}"))?;
        let maximized = window
            .is_maximized()
            .map_err(|error| format!("capture video export maximize state: {error}"))?;
        let fullscreen = window
            .is_fullscreen()
            .map_err(|error| format!("capture video export fullscreen state: {error}"))?;
        Ok(Self {
            position: (position.x, position.y),
            inner_size: (size.width, size.height),
            maximized,
            fullscreen,
        })
    }

    /// 决定恢复优先级：全屏 > 最大化 > 仅 bounds（Electron setFullScreen else maximize 语义）。
    fn restore_action(&self) -> RestoreAction {
        if self.fullscreen {
            RestoreAction::Fullscreen
        } else if self.maximized {
            RestoreAction::Maximize
        } else {
            RestoreAction::Bounds
        }
    }

    /// 恢复主窗口原始状态（桌面专属：fullscreen/maximize 在移动端 WebviewWindow 上不存在）。
    #[cfg(desktop)]
    fn apply(&self, window: &tauri::WebviewWindow) -> Result<(), String> {
        window
            .set_position(PhysicalPosition::new(self.position.0, self.position.1))
            .map_err(|error| format!("restore video export window position: {error}"))?;
        window
            .set_size(PhysicalSize::new(self.inner_size.0, self.inner_size.1))
            .map_err(|error| format!("restore video export window size: {error}"))?;
        match self.restore_action() {
            RestoreAction::Fullscreen => window
                .set_fullscreen(true)
                .map_err(|error| format!("restore video export fullscreen: {error}")),
            RestoreAction::Maximize => window
                .maximize()
                .map_err(|error| format!("restore video export maximize: {error}")),
            RestoreAction::Bounds => Ok(()),
        }
    }
}

/// 主窗口导出状态：快照只保存一次、恢复只消费一次（幂等）。
/// 桌面专属：移动端该状态无消费者，字段与方法均不编译。
#[cfg(any(desktop, test))]
pub struct VideoExportWindowState {
    inner: Mutex<Option<WindowRestoreSnapshot>>,
}

#[cfg(any(desktop, test))]
impl VideoExportWindowState {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(None),
        }
    }

    /// 取走快照（恢复用，exactly-once）；没有快照时返回 None。
    fn take(&self) -> Option<WindowRestoreSnapshot> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
    }

    /// 仅当尚未保存过快照时保存一份（prepared 前调用）。
    fn save_if_absent(&self, snapshot: WindowRestoreSnapshot) {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if inner.is_none() {
            *inner = Some(snapshot);
        }
    }

    /// 恢复失败后放回原快照（槽位应已被 take 清空），允许调用方重试。
    /// 只在槽位仍为空时写入，避免覆盖并发写入的新快照。
    fn reinsert(&self, snapshot: WindowRestoreSnapshot) {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if inner.is_none() {
            *inner = Some(snapshot);
        }
    }
}

#[cfg(any(desktop, test))]
impl Default for VideoExportWindowState {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// 原子写文件（临时文件 + rename，失败清理临时文件）
// ---------------------------------------------------------------------------

const MAX_TEMP_FILE_ATTEMPTS: u32 = 16;

fn write_atomic(path: &Path, data: &[u8]) -> Result<(), String> {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .ok_or_else(|| "invalid video export file name".to_string())?;
    let parent = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    if !parent.is_dir() {
        return Err(format!(
            "video export directory does not exist: {}",
            parent.display()
        ));
    }
    // 随机后缀 + create_new 重试：并发写入同一目录不会互相覆盖临时文件。
    let mut attempts = 0u32;
    loop {
        attempts += 1;
        if attempts > MAX_TEMP_FILE_ATTEMPTS {
            return Err("unable to allocate a unique video export temp file".to_string());
        }
        let nonce = rand::random::<u64>();
        let temp_path = parent.join(format!(".{file_name}.folia-{nonce:016x}.tmp"));
        let file = match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(format!("create video export temp file: {error}"));
            }
        };
        let result = (|| -> Result<(), String> {
            let mut file = file;
            file.write_all(data)
                .map_err(|error| format!("write video export temp file: {error}"))?;
            file.sync_all()
                .map_err(|error| format!("sync video export temp file: {error}"))?;
            drop(file);
            std::fs::rename(&temp_path, path)
                .map_err(|error| format!("finalize video export file: {error}"))?;
            Ok(())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&temp_path);
        }
        return result;
    }
}

// ---------------------------------------------------------------------------
// Tauri 命令（snake_case，shim 同名调用）
// ---------------------------------------------------------------------------

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChooseVideoExportPathResult {
    canceled: bool,
    file_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    write_token: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VideoExportSource {
    id: String,
    name: String,
}

#[tauri::command]
// 弹出保存对话框：sanitize 扩展名/文件名/展示名，默认 Videos 目录；
// 选中后登记一次性 writeToken（shim 用原始 IPC 写文件时绑定确切路径）。
// 非阻塞：save_file 回调 + oneshot，不阻塞 Tauri 主线程事件循环。
// 只接受主窗口调用（发送方信任校验）。
pub async fn video_export_choose_path(
    window: tauri::WebviewWindow,
    app: AppHandle,
    state: tauri::State<'_, VideoExportTokenStore>,
    default_name: String,
    extension: String,
    display_name: String,
) -> Result<ChooseVideoExportPathResult, String> {
    require_main_window_label(window.label())?;
    let safe_extension = sanitize_extension(&extension);
    let safe_display_name = sanitize_display_name(&display_name, safe_extension);
    let file_name = build_save_file_name(&default_name, safe_extension);
    let videos_dir = app.path().video_dir().ok();
    let mut builder = app
        .dialog()
        .file()
        .add_filter(&safe_display_name, &[safe_extension])
        .set_file_name(&file_name);
    if let Some(dir) = videos_dir {
        builder = builder.set_directory(dir);
    }
    let (tx, rx) = tokio::sync::oneshot::channel();
    builder.save_file(move |file_path| {
        let path = file_path.and_then(|file_path| file_path.into_path().ok());
        let _ = tx.send(path);
    });
    let Some(file_path) = rx
        .await
        .map_err(|_| "video export save dialog closed before a path was selected".to_string())?
    else {
        return Ok(ChooseVideoExportPathResult {
            canceled: true,
            file_path: None,
            write_token: None,
        });
    };
    let write_token = state.register(file_path.clone());
    Ok(ChooseVideoExportPathResult {
        canceled: false,
        file_path: Some(file_path.to_string_lossy().into_owned()),
        write_token: Some(write_token),
    })
}

#[tauri::command]
// 仅当主窗口存在时返回哨兵 capture source；系统选择器由用户决定，不能静默强制 Folia。
// 只接受主窗口调用。
pub fn video_export_get_main_window_source(
    window: tauri::WebviewWindow,
    app: AppHandle,
) -> Result<Option<VideoExportSource>, String> {
    require_main_window_label(window.label())?;
    let exists = app.get_webview_window(MAIN_WINDOW_LABEL).is_some();
    Ok(exists.then(|| VideoExportSource {
        id: VIDEO_EXPORT_SENTINEL_SOURCE_ID.to_string(),
        name: "Folia".to_string(),
    }))
}

#[tauri::command]
// 为导出准备主窗口：先快照原始状态，快照失败绝不改动窗口；退出全屏/最大化时
// 任一失败都如实报错（返回 Err），避免"prepared=true 但窗口状态错误"。
// 只接受主窗口调用。
#[cfg(desktop)]
pub fn video_export_prepare_window(
    window: tauri::WebviewWindow,
    app: AppHandle,
    state: tauri::State<'_, VideoExportWindowState>,
    size: Option<ValueSize>,
) -> Result<bool, String> {
    require_main_window_label(window.label())?;
    let Some((width, height)) = size.and_then(|size| sanitize_export_size(size.width, size.height))
    else {
        return Ok(false);
    };
    let Some(window) = app.get_webview_window(MAIN_WINDOW_LABEL) else {
        return Ok(false);
    };
    // 无法记录可恢复快照时不做任何窗口改动，如实报错（查询失败不再静默降级为 false）。
    let snapshot = WindowRestoreSnapshot::capture(&window)?;
    // 快照只保存一次；持锁只做内存拷贝，窗口操作在锁外进行。
    state.save_if_absent(snapshot);
    if window
        .is_fullscreen()
        .map_err(|error| format!("query video export fullscreen: {error}"))?
    {
        window
            .set_fullscreen(false)
            .map_err(|error| format!("exit video export fullscreen: {error}"))?;
    }
    if window
        .is_maximized()
        .map_err(|error| format!("query video export maximize: {error}"))?
    {
        window
            .unmaximize()
            .map_err(|error| format!("exit video export maximize: {error}"))?;
    }
    window
        .set_size(PhysicalSize::new(width, height))
        .map_err(|error| format!("prepare video export window size: {error}"))?;
    let _ = window.center();
    let _ = window.set_focus();
    Ok(true)
}

#[tauri::command]
// 恢复主窗口：**先消费**快照（exactly-once）再查找主窗口——重建/缺失的主窗口
// 绝不能把过期 bounds 留给下一次导出。无快照/窗口不可用时如实返回 false；
// 只有对同一个仍存在的主窗口应用失败时才放回快照供重试。
// 只接受主窗口调用。
#[cfg(desktop)]
pub fn video_export_restore_window(
    window: tauri::WebviewWindow,
    app: AppHandle,
    state: tauri::State<'_, VideoExportWindowState>,
) -> Result<bool, String> {
    require_main_window_label(window.label())?;
    let Some(snapshot) = state.take() else {
        return Ok(false);
    };
    let Some(window) = app.get_webview_window(MAIN_WINDOW_LABEL) else {
        // 快照已消费且不放回：主窗口缺失/重建时，旧 bounds 已过期，放回会污染下一次导出。
        return Ok(false);
    };
    if let Err(error) = snapshot.apply(&window) {
        state.reinsert(snapshot);
        return Err(error);
    }
    Ok(true)
}

#[tauri::command]
// 原始二进制写文件：只接受 Raw body + 有效一次性 token 头，写 token 绑定的 Rust 选择路径。
// 拒绝 JSON body / 缺失 / 未知 / 复用 / 过期 token；写临时文件 + rename，失败清理。
// 只接受主窗口调用（发送方信任校验，防远程/注入窗口直接落盘）。
pub fn video_export_write_file(
    window: tauri::WebviewWindow,
    app: AppHandle,
    request: tauri::ipc::Request<'_>,
) -> Result<bool, String> {
    require_main_window_label(window.label())?;
    let InvokeBody::Raw(bytes) = request.body() else {
        return Err("video export write requires a raw bytes IPC body".to_string());
    };
    let token = request
        .headers()
        .get(WRITE_TOKEN_HEADER)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    if token.is_empty() || token.len() > WRITE_TOKEN_MAX_LEN || !token.is_ascii() {
        return Err("missing or invalid video export write token".to_string());
    }
    let path = app
        .state::<VideoExportTokenStore>()
        .consume(token)
        .ok_or_else(|| "invalid, expired, or reused video export write token".to_string())?;
    write_atomic(&path, bytes)?;
    Ok(true)
}

/// prepare_window 的入参形状（camelCase `size`，字段即 width/height）。
#[derive(serde::Deserialize)]
#[cfg(desktop)]
pub struct ValueSize {
    width: f64,
    height: f64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;

    fn fake_clock(now: Arc<AtomicU64>) -> Box<dyn Fn() -> u128 + Send + Sync> {
        Box::new(move || now.load(Ordering::Relaxed) as u128)
    }

    // -- sanitize：扩展名 / 文件名 / 展示名 ----------------------------------

    #[test]
    fn main_window_label_guard_rejects_other_windows() {
        // 发送方信任校验：只有主窗口能调用视频导出命令，远程等窗口一律拒绝。
        assert!(require_main_window_label(MAIN_WINDOW_LABEL).is_ok());
        assert!(require_main_window_label("remote").is_err());
        assert!(require_main_window_label("").is_err());
        assert!(require_main_window_label("Main").is_err()); // label 大小写敏感
    }

    #[test]
    fn extension_is_mp4_or_webm() {
        assert_eq!(sanitize_extension("mp4"), "mp4");
        assert_eq!(sanitize_extension("webm"), "webm");
        assert_eq!(sanitize_extension("mkv"), "webm");
        assert_eq!(sanitize_extension(""), "webm");
    }

    #[test]
    fn file_name_sanitizes_invalid_windows_chars_and_control_chars() {
        assert_eq!(
            sanitize_file_name("a<b>c:d\"e/f\\g|h?i*j"),
            "a_b_c_d_e_f_g_h_i_j"
        );
        assert_eq!(sanitize_file_name("bad\u{0}\u{1f}name"), "bad__name");
    }

    #[test]
    fn file_name_strips_trailing_dots_and_spaces() {
        assert_eq!(sanitize_file_name("song.."), "song");
        assert_eq!(sanitize_file_name("song  "), "song");
        assert_eq!(sanitize_file_name("..."), "folia-export");
    }

    #[test]
    fn save_file_name_enforces_matching_extension() {
        assert_eq!(build_save_file_name("song", "mp4"), "song.mp4");
        assert_eq!(build_save_file_name("song.mp4", "mp4"), "song.mp4");
        assert_eq!(build_save_file_name("song.webm", "mp4"), "song.mp4");
        assert_eq!(build_save_file_name("song", "webm"), "song.webm");
        assert_eq!(build_save_file_name("", "mp4"), "folia-export.mp4");
        assert_eq!(build_save_file_name("  ", "webm"), "folia-export.webm");
    }

    #[test]
    fn save_file_name_sanitizes_before_appending() {
        assert_eq!(build_save_file_name("a:b:c", "mp4"), "a_b_c.mp4");
    }

    #[test]
    fn display_name_falls_back_to_default() {
        assert_eq!(sanitize_display_name("MP4 Video", "mp4"), "MP4 Video");
        assert_eq!(sanitize_display_name("", "mp4"), "MP4 Video");
        assert_eq!(sanitize_display_name("  ", "webm"), "WebM Video");
        assert_eq!(sanitize_display_name("My Format", "webm"), "My Format");
    }

    // -- 尺寸校验 -------------------------------------------------------------

    #[test]
    fn export_size_rounds_clamps_and_rejects() {
        assert_eq!(sanitize_export_size(1920.0, 1080.0), Some((1920, 1080)));
        assert_eq!(sanitize_export_size(1920.4, 1080.6), Some((1920, 1081)));
        assert_eq!(sanitize_export_size(5000.0, 5000.0), Some((3840, 3840)));
        assert_eq!(sanitize_export_size(319.0, 1080.0), None);
        assert_eq!(sanitize_export_size(320.0, 319.0), None);
        assert_eq!(sanitize_export_size(f64::NAN, 1080.0), None);
        assert_eq!(sanitize_export_size(f64::INFINITY, 1080.0), None);
    }

    // -- 恢复状态模型 ----------------------------------------------------------

    #[test]
    fn restore_action_prefers_fullscreen_over_maximize() {
        let full = WindowRestoreSnapshot {
            fullscreen: true,
            maximized: false,
            ..Default::default()
        };
        assert_eq!(full.restore_action(), RestoreAction::Fullscreen);

        let full_and_max = WindowRestoreSnapshot {
            fullscreen: true,
            maximized: true,
            ..Default::default()
        };
        assert_eq!(full_and_max.restore_action(), RestoreAction::Fullscreen);
    }

    #[test]
    fn restore_action_falls_back_to_maximize_then_bounds() {
        let maximized = WindowRestoreSnapshot {
            maximized: true,
            ..Default::default()
        };
        assert_eq!(maximized.restore_action(), RestoreAction::Maximize);

        let bounds = WindowRestoreSnapshot::default();
        assert_eq!(bounds.restore_action(), RestoreAction::Bounds);
    }

    #[test]
    fn snapshot_is_saved_only_once() {
        let state = VideoExportWindowState::new();
        let first = WindowRestoreSnapshot {
            position: (1, 2),
            inner_size: (1200, 800),
            ..Default::default()
        };
        let second = WindowRestoreSnapshot {
            position: (9, 9),
            inner_size: (300, 300),
            ..Default::default()
        };
        state.save_if_absent(first);
        state.save_if_absent(second);
        // 第二次保存被忽略：快照只记录准备前的原始状态。
        assert_eq!(state.take(), Some(first));
        assert_eq!(state.take(), None);
    }

    #[test]
    fn failed_restore_reinserts_snapshot_for_retry() {
        let state = VideoExportWindowState::new();
        let snapshot = WindowRestoreSnapshot {
            position: (10, 20),
            inner_size: (1200, 800),
            maximized: true,
            fullscreen: false,
        };
        state.save_if_absent(snapshot);

        // 模拟 restore：take 后 apply 失败，调用方必须能把原快照放回以便重试。
        let taken = state.take();
        assert_eq!(taken, Some(snapshot));
        assert_eq!(state.take(), None);
        state.reinsert(snapshot);
        assert_eq!(state.take(), Some(snapshot));
        assert_eq!(state.take(), None);
    }

    #[test]
    fn reinsert_never_overwrites_a_newer_snapshot() {
        let state = VideoExportWindowState::new();
        let stale = WindowRestoreSnapshot {
            position: (1, 1),
            ..Default::default()
        };
        let fresh = WindowRestoreSnapshot {
            position: (2, 2),
            ..Default::default()
        };
        // 槽位已有新快照（例如重试期间新 prepare 已写入）时，放回旧快照不得覆盖。
        state.save_if_absent(fresh);
        state.reinsert(stale);
        assert_eq!(state.take(), Some(fresh));
    }

    #[test]
    fn restore_without_window_consumes_snapshot_and_does_not_reinsert() {
        // restore 必须先消费快照再检查主窗口：主窗口缺失/重建时旧 bounds 已过期，
        // 快照不得放回，否则会污染下一次导出（用 stale bounds 覆盖新窗口）。
        let state = VideoExportWindowState::new();
        let snapshot = WindowRestoreSnapshot {
            position: (10, 20),
            inner_size: (1200, 800),
            ..Default::default()
        };
        state.save_if_absent(snapshot);
        // 模拟"取走快照后发现主窗口不存在"：取走即清空，且调用方不会 reinsert。
        assert_eq!(state.take(), Some(snapshot));
        assert_eq!(state.take(), None);
    }

    // -- token：过期 / 一次性 / path 绑定 / 有界 -------------------------------

    #[test]
    fn token_is_consumed_exactly_once_and_binds_path() {
        let now = Arc::new(AtomicU64::new(1000));
        let store = VideoExportTokenStore::with_clock(
            Duration::from_secs(600),
            TOKEN_MAX_COUNT,
            fake_clock(now.clone()),
        );
        let path = PathBuf::from("C:\\Videos\\export.mp4");
        let token = store.register(path.clone());
        assert_eq!(store.consume(&token), Some(path));
        assert_eq!(store.consume(&token), None);
    }

    #[test]
    fn token_rejects_unknown_and_expired() {
        let now = Arc::new(AtomicU64::new(1000));
        let store = VideoExportTokenStore::with_clock(
            Duration::from_millis(600),
            TOKEN_MAX_COUNT,
            fake_clock(now.clone()),
        );
        assert_eq!(store.consume("ve-0000000000000000-0000000000000000"), None);

        let token = store.register(PathBuf::from("C:\\Videos\\x.webm"));
        now.store(1601, Ordering::Relaxed); // 超过 600ms ttl
        assert_eq!(store.consume(&token), None);
    }

    #[test]
    fn token_ttl_is_six_hours_with_boundary() {
        let now = Arc::new(AtomicU64::new(0));
        let store =
            VideoExportTokenStore::with_clock(TOKEN_TTL, TOKEN_MAX_COUNT, fake_clock(now.clone()));
        assert_eq!(TOKEN_TTL, Duration::from_secs(6 * 60 * 60));
        let six_hours_ms = 6 * 60 * 60 * 1000u64;

        let token = store.register(PathBuf::from("p1"));
        let token2 = store.register(PathBuf::from("p2"));
        // 恰好到期仍有效（expires_at >= now）。
        now.store(six_hours_ms, Ordering::Relaxed);
        assert_eq!(store.consume(&token), Some(PathBuf::from("p1")));
        // 超过即过期。
        now.store(six_hours_ms + 1, Ordering::Relaxed);
        assert_eq!(store.consume(&token2), None);
    }

    #[test]
    fn token_does_not_cross_bind_to_other_paths() {
        let store = VideoExportTokenStore::new();
        let first = store.register(PathBuf::from("C:\\Videos\\a.mp4"));
        let second = store.register(PathBuf::from("C:\\Videos\\b.mp4"));
        // 每个 token 各自绑定自己的 path，互不串换。
        assert_eq!(
            store.consume(&first),
            Some(PathBuf::from("C:\\Videos\\a.mp4"))
        );
        assert_eq!(
            store.consume(&second),
            Some(PathBuf::from("C:\\Videos\\b.mp4"))
        );
    }

    #[test]
    fn token_retention_is_bounded_and_prunes_expired() {
        let now = Arc::new(AtomicU64::new(1000));
        let store =
            VideoExportTokenStore::with_clock(Duration::from_secs(600), 3, fake_clock(now.clone()));
        let t1 = store.register(PathBuf::from("p1"));
        let t2 = store.register(PathBuf::from("p2"));
        let t3 = store.register(PathBuf::from("p3"));
        // 超过 max_count：最旧的被挤掉。
        let t4 = store.register(PathBuf::from("p4"));
        assert_eq!(store.consume(&t1), None);
        assert_eq!(store.consume(&t2), Some(PathBuf::from("p2")));
        assert_eq!(store.consume(&t3), Some(PathBuf::from("p3")));
        assert_eq!(store.consume(&t4), Some(PathBuf::from("p4")));
    }

    // -- 原始 IPC 校验（raw-vs-JSON、token 缺失/复用） ------------------------

    #[test]
    fn write_validation_rejects_json_body_missing_token_and_invalid_tokens() {
        // 与命令内联的校验语义独立复现（Request 不可在单测中直接构造）：
        // JSON body 必须拒绝。
        fn validate(body_raw: bool, token: Option<&str>) -> Result<(), String> {
            if !body_raw {
                return Err("video export write requires a raw bytes IPC body".to_string());
            }
            let token = token.unwrap_or("");
            if token.is_empty() || token.len() > WRITE_TOKEN_MAX_LEN || !token.is_ascii() {
                return Err("missing or invalid video export write token".to_string());
            }
            Ok(())
        }
        assert!(validate(false, Some("ve-abc")).is_err());
        assert!(validate(true, None).is_err());
        assert!(validate(true, Some("")).is_err());
        assert!(validate(true, Some("non ascii ü")).is_err());
        assert!(validate(true, Some("ve-abc")).is_ok());
    }

    #[test]
    fn write_atomic_writes_content_and_leaves_no_temp_files() {
        let dir = std::env::temp_dir().join(format!("folia-ve-test-{}", system_now_ms()));
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("out.mp4");
        let payload = b"hello video bytes";
        write_atomic(&target, payload).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), payload);
        // 成功路径不应残留 .folia-*.tmp。
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains(".folia-") && name.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "leftover temp files: {leftovers:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_atomic_fails_cleanly_when_parent_missing() {
        let dir = std::env::temp_dir().join(format!("folia-ve-missing-{}", system_now_ms()));
        let target = dir.join("out.mp4");
        let result = write_atomic(&target, b"data");
        assert!(result.is_err());
        assert!(!dir.exists());
    }

    #[test]
    fn write_atomic_overwrites_existing_target() {
        let dir = std::env::temp_dir().join(format!("folia-ve-overwrite-{}", system_now_ms()));
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("out.mp4");
        std::fs::write(&target, b"old").unwrap();
        write_atomic(&target, b"new").unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"new");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_atomic_uses_unique_temp_names_across_rapid_writes() {
        // 随机后缀 + create_new：同一目录内连续/并发写入不会复用临时文件名。
        let dir = std::env::temp_dir().join(format!("folia-ve-rapid-{}", system_now_ms()));
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("out.mp4");
        for round in 0..8u8 {
            write_atomic(&target, &[round]).unwrap();
        }
        assert_eq!(std::fs::read(&target).unwrap(), &[7u8]);
        // 每次写入成功，且不残留任何 .folia-*.tmp。
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains(".folia-") && name.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "leftover temp files: {leftovers:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
