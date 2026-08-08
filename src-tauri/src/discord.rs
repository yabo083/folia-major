//! M8 Discord Rich Presence.
//!
//! Mirrors `folia-major/electron/discordPresence.cjs` (used by the Electron app
//! through `@xhayper/discord-rpc`): builds a "Listening" activity from the
//! renderer playback snapshot, maintains it over the local Discord IPC named
//! pipe, and reports honest lifecycle / error status to the renderer on
//! `discord-presence-status-changed`.
//!
//! Wire protocol matches `@xhayper/discord-rpc` IPC transport exactly:
//! 8-byte little-endian frame header (opcode, payload length), JSON payload
//! unpadded; HANDSHAKE(0) → {v:1, client_id}, FRAME(1) carries
//! SET_ACTIVITY, PING(3) is answered with PONG(4). The IPC client is
//! implemented over `std::fs` named pipes — no Node/Electron sidecar, no extra
//! dependencies.

use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use tauri::{AppHandle, Manager};

#[cfg(windows)]
use std::os::windows::io::AsRawHandle;
#[cfg(windows)]
use windows::Win32::Foundation::HANDLE;
#[cfg(windows)]
use windows::Win32::System::IO::CancelSynchronousIo;

use crate::settings::{SettingsStore, DISCORD_RICH_PRESENCE_ENABLED};

pub const DEFAULT_DISCORD_APPLICATION_ID: &str = "1518508445483925645";

const DISCORD_PRESENCE_UPDATE_INTERVAL_MS: u128 = 15_000;
const DISCORD_ACTIVITY_TYPE_LISTENING: u8 = 2;
#[cfg(windows)]
const DISCORD_HANDSHAKE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
#[cfg(windows)]
const DISCORD_MAX_FRAME_BYTES: usize = 4 * 1024 * 1024;

// 命名管道 IPC 线格式常量：仅 Windows 原生传输使用。
#[cfg(windows)]
const OP_HANDSHAKE: u32 = 0;
#[cfg(windows)]
const OP_FRAME: u32 = 1;
#[cfg(windows)]
const OP_CLOSE: u32 = 2;
#[cfg(windows)]
const OP_PING: u32 = 3;
#[cfg(windows)]
const OP_PONG: u32 = 4;

// -- pure payload mapping (spec source: test/unit/discordPresence.test.ts) ----

pub fn normalize_discord_application_id(value: &str) -> String {
    let trimmed = value.trim();
    if (16..=24).contains(&trimmed.len()) && trimmed.bytes().all(|b| b.is_ascii_digit()) {
        trimmed.to_string()
    } else {
        String::new()
    }
}

pub fn normalize_discord_image_url(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let parsed = match url::Url::parse(trimmed) {
        Ok(url) => url,
        Err(_) => return String::new(),
    };
    if parsed.scheme() != "http" && parsed.scheme() != "https" {
        return String::new();
    }
    let hostname = parsed.host_str().unwrap_or("").to_lowercase();
    if hostname == "localhost"
        || hostname == "127.0.0.1"
        || hostname == "::1"
        || hostname.ends_with(".localhost")
    {
        return String::new();
    }
    let mut url = parsed;
    if url.scheme() == "http" {
        let _ = url.set_scheme("https");
    }
    url.to_string()
}

fn now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

fn get_snapshot_timestamp(snapshot: &Value) -> f64 {
    match snapshot.get("updatedAt").and_then(Value::as_f64) {
        Some(value) if value.is_finite() && value > 0.0 => value,
        _ => now_ms() as f64,
    }
}

fn truncate(value: &str, max: usize) -> String {
    value.chars().take(max).collect()
}

/// Builds the Discord activity payload; `None` when there is no active track
/// (mirrors `buildDiscordActivity` in discordPresence.cjs).
pub fn build_discord_activity(snapshot: &Value) -> Option<Value> {
    let has_track = snapshot
        .get("hasTrack")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let title = snapshot.get("title").and_then(Value::as_str).unwrap_or("");
    if !has_track || title.is_empty() {
        return None;
    }

    let title = truncate(title, 128);
    let artist = snapshot
        .get("artist")
        .and_then(Value::as_str)
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| truncate(s, 128))
        .unwrap_or_else(|| "Folia".to_string());
    let player_state = match snapshot.get("playerState").and_then(Value::as_str) {
        Some("PLAYING") => "PLAYING",
        _ => "PAUSED",
    };
    let duration = snapshot
        .get("duration")
        .and_then(Value::as_f64)
        .unwrap_or(f64::NAN);
    let current_time = snapshot
        .get("currentTime")
        .and_then(Value::as_f64)
        .unwrap_or(0.0)
        .max(0.0);
    let has_finite_duration = duration.is_finite() && duration > current_time + 1.0;
    let cover_image_url = snapshot
        .get("coverUrl")
        .and_then(Value::as_str)
        .map(normalize_discord_image_url)
        .filter(|url| !url.is_empty());

    let mut activity = serde_json::Map::new();
    activity.insert("name".to_string(), json!("Folia"));
    activity.insert("type".to_string(), json!(DISCORD_ACTIVITY_TYPE_LISTENING));
    activity.insert("details".to_string(), json!(title));
    activity.insert(
        "state".to_string(),
        json!(if player_state == "PLAYING" {
            artist.clone()
        } else {
            format!("Paused - {artist}")
        }),
    );
    activity.insert(
        "largeImageText".to_string(),
        json!(if cover_image_url.is_some() {
            title.clone()
        } else {
            "Folia".to_string()
        }),
    );
    activity.insert(
        "smallImageText".to_string(),
        json!(if player_state == "PLAYING" {
            "Playing"
        } else {
            "Paused"
        }),
    );
    activity.insert("instance".to_string(), json!(false));
    if let Some(url) = cover_image_url {
        activity.insert("largeImageKey".to_string(), json!(url));
    }
    if player_state == "PLAYING" && has_finite_duration {
        let sampled_at = get_snapshot_timestamp(snapshot);
        let start = (sampled_at - current_time * 1000.0).max(0.0);
        let end = sampled_at + (duration - current_time) * 1000.0;
        activity.insert("startTimestamp".to_string(), json!(start));
        activity.insert("endTimestamp".to_string(), json!(end));
    }
    Some(Value::Object(activity))
}

/// Internal dedupe key for the 15s update throttle (mirrors getActivityKey).
fn get_activity_key(activity: Option<&Value>) -> String {
    let Some(activity) = activity else {
        return "empty".to_string();
    };
    let timestamp_secs = |key: &str| {
        activity
            .get(key)
            .and_then(Value::as_f64)
            .map(|value| (value / 1000.0).round() as i64)
    };
    json!({
        "details": activity.get("details"),
        "state": activity.get("state"),
        "largeImageKey": activity.get("largeImageKey"),
        "startTimestamp": timestamp_secs("startTimestamp"),
        "endTimestamp": timestamp_secs("endTimestamp"),
    })
    .to_string()
}

// -- IPC transport abstraction ------------------------------------------------

/// Minimal transport seam so the controller is unit-testable without Discord.
pub(crate) trait RpcTransport: Send + Sync {
    /// `on_disconnect` 携带断开原因：`Some(message)` 为 Discord evt ERROR 的
    /// 真实错误消息，`None` 表示管道被动关闭（Discord 退出 / 连接中断）。
    fn connect(
        &self,
        app_id: &str,
        on_disconnect: Arc<dyn Fn(Option<String>) + Send + Sync>,
    ) -> Result<Arc<dyn RpcConnection + Send + Sync>, String>;
}

pub(crate) trait RpcConnection: Send + Sync {
    /// `None` clears the activity (SET_ACTIVITY with pid only).
    fn set_activity(&self, activity: Option<&Value>) -> Result<(), String>;
}

// -- production named-pipe transport -----------------------------------------

struct IpcTransport;

#[cfg(windows)]
impl RpcTransport for IpcTransport {
    fn connect(
        &self,
        app_id: &str,
        on_disconnect: Arc<dyn Fn(Option<String>) + Send + Sync>,
    ) -> Result<Arc<dyn RpcConnection + Send + Sync>, String> {
        IpcConnection::open(app_id, on_disconnect).map(|conn| Arc::new(conn) as _)
    }
}

#[cfg(not(windows))]
impl RpcTransport for IpcTransport {
    fn connect(
        &self,
        _app_id: &str,
        _on_disconnect: Arc<dyn Fn(Option<String>) + Send + Sync>,
    ) -> Result<Arc<dyn RpcConnection + Send + Sync>, String> {
        Err("Discord Rich Presence is only supported on Windows.".to_string())
    }
}

#[cfg(windows)]
struct IpcConnection {
    writer: Arc<Mutex<std::fs::File>>,
    reader: Option<std::thread::JoinHandle<()>>,
    /// reader 线程 id；Drop 在 reader 线程自身执行时跳过 join（join self 会 panic）。
    reader_tid: std::thread::ThreadId,
    /// 置位后 reader 尽快退出（即使管道静默）。
    cancel: Arc<std::sync::atomic::AtomicBool>,
}

#[cfg(windows)]
enum FirstFrame {
    Ready,
    Error(String),
}

/// reader 轮询打断间隔：CancelSynchronousIo 后 sleep 再重试，保证落在
/// read 系统调用间隙的 reader 也能被唤醒，join 必然在有限时间内返回。
#[cfg(windows)]
const DISCORD_READER_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(5);

/// 提取 evt == ERROR 帧的错误消息（握手后 Discord 拒绝 SET_ACTIVITY 等场景）。
#[cfg(windows)]
fn frame_error_message(payload: &[u8]) -> Option<String> {
    let value: Value = serde_json::from_slice(payload).ok()?;
    if value.get("evt").and_then(Value::as_str) != Some("ERROR") {
        return None;
    }
    Some(
        value
            .pointer("/data/message")
            .and_then(Value::as_str)
            .filter(|message| !message.is_empty())
            .unwrap_or("Discord reported an error")
            .to_string(),
    )
}

/// 首 OP_FRAME 必须严格为 evt READY：ERROR / 畸形 JSON / 意外事件一律视为失败。
#[cfg(windows)]
fn classify_first_frame(payload: &[u8]) -> FirstFrame {
    if let Some(message) = frame_error_message(payload) {
        return FirstFrame::Error(message);
    }
    let value = match serde_json::from_slice::<Value>(payload) {
        Ok(value) => value,
        Err(_) => {
            return FirstFrame::Error("Discord returned a malformed handshake response".to_string())
        }
    };
    if value.get("evt").and_then(Value::as_str) == Some("READY") {
        FirstFrame::Ready
    } else {
        FirstFrame::Error("Discord returned an unexpected first event".to_string())
    }
}

/// 置位 cancel 并反复 CancelSynchronousIo，直到 reader 线程退出。
/// 必须在 reader 线程自身以外的线程调用。
///
/// 打断句柄直接复用 `JoinHandle::as_raw_handle()` 持有的原生线程句柄
/// （`CreateThread` 返回 THREAD_ALL_ACCESS，含 THREAD_TERMINATE，满足
/// CancelSynchronousIo 要求），不依赖 OpenThread —— 不存在句柄获取失败的
/// 路径，因此也不存在"拿不到句柄只能盲 join"的死锁分支。句柄由 JoinHandle
/// 所有，join 时随其 Drop 关闭，这里不重复 CloseHandle。
#[cfg(windows)]
fn interrupt_reader(reader: &std::thread::JoinHandle<()>, cancel: &std::sync::atomic::AtomicBool) {
    let thread_handle = HANDLE(reader.as_raw_handle());
    cancel.store(true, std::sync::atomic::Ordering::SeqCst);
    loop {
        let _ = unsafe { CancelSynchronousIo(thread_handle) };
        if reader.is_finished() {
            break;
        }
        std::thread::sleep(DISCORD_READER_POLL_INTERVAL);
    }
}

/// 取消并 join reader 线程。open() 的各失败路径使用。
#[cfg(windows)]
fn stop_reader(reader: std::thread::JoinHandle<()>, cancel: &std::sync::atomic::AtomicBool) {
    interrupt_reader(&reader, cancel);
    let _ = reader.join();
}

/// 生产 open() 跨全部 10 个 pipe 的整体等待上限：让"管道瞬时忙"的场景
/// （例如 Discord 正在重连、或另一客户端刚抢走实例）在有限时间内等到可用
/// 实例，而不是立刻报 unavailable。加上 connect 内握手本身的
/// DISCORD_HANDSHAKE_TIMEOUT，单次 open 最坏有界于二者之和（后台线程执行）。
#[cfg(windows)]
const DISCORD_PIPE_OPEN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

#[cfg(windows)]
impl IpcConnection {
    fn open(
        app_id: &str,
        on_disconnect: Arc<dyn Fn(Option<String>) + Send + Sync>,
    ) -> Result<Self, String> {
        let deadline = std::time::Instant::now() + DISCORD_PIPE_OPEN_TIMEOUT;
        let file = (0..10u32)
            .find_map(|pipe_id| {
                let path = format!(r"\\?\pipe\discord-ipc-{}", pipe_id);
                open_pipe_bounded(&path, deadline).ok()
            })
            .ok_or_else(|| "Discord IPC pipe unavailable (is Discord running?).".to_string())?;
        Self::open_file(app_id, file, DISCORD_HANDSHAKE_TIMEOUT, on_disconnect)
    }

    /// 打开指定路径的命名管道并完成握手（生产 open 与测试共用核心逻辑）。
    #[cfg(test)]
    fn open_at(
        app_id: &str,
        pipe_path: &str,
        handshake_timeout: std::time::Duration,
        on_disconnect: Arc<dyn Fn(Option<String>) + Send + Sync>,
    ) -> Result<Self, String> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(pipe_path)
            .map_err(|_| "Discord IPC pipe unavailable (is Discord running?).".to_string())?;
        Self::open_file(app_id, file, handshake_timeout, on_disconnect)
    }

    fn open_file(
        app_id: &str,
        file: std::fs::File,
        handshake_timeout: std::time::Duration,
        on_disconnect: Arc<dyn Fn(Option<String>) + Send + Sync>,
    ) -> Result<Self, String> {
        let writer =
            Arc::new(Mutex::new(file.try_clone().map_err(|error| {
                format!("duplicate Discord pipe handle: {error}")
            })?));
        let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (first_tx, first_rx) = std::sync::mpsc::channel::<FirstFrame>();

        let handshake = json!({ "v": 1, "client_id": app_id });
        let body = serde_json::to_vec(&handshake)
            .map_err(|error| format!("serialize Discord handshake: {error}"))?;

        // 先写握手、再启动 reader：避免 reader 的阻塞读 pending 时阻塞本写
        // （Windows 字节管道同一实例上 pending 同步读会阻塞后续写）。
        write_frame(&writer, OP_HANDSHAKE, &body)?;

        let reader_writer = writer.clone();
        let reader_cancel = cancel.clone();
        let handle = std::thread::spawn(move || {
            let mut read_file = file;
            let mut buf = [0u8; 8];
            let mut first_frame_seen = false;
            let mut ready_received = false;
            let mut disconnect_reason: Option<String> = None;
            'reader: loop {
                if reader_cancel.load(std::sync::atomic::Ordering::SeqCst) {
                    break;
                }
                if read_exact(&mut read_file, &mut buf).is_err() {
                    break;
                }
                let op = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
                let len = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]) as usize;
                if len > DISCORD_MAX_FRAME_BYTES {
                    break;
                }
                let mut payload = vec![0u8; len];
                if read_exact(&mut read_file, &mut payload).is_err() {
                    break;
                }
                match op {
                    OP_FRAME if !first_frame_seen => {
                        first_frame_seen = true;
                        match classify_first_frame(&payload) {
                            FirstFrame::Ready => {
                                ready_received = true;
                                let _ = first_tx.send(FirstFrame::Ready);
                            }
                            FirstFrame::Error(message) => {
                                // 握手被拒绝：上报给 open() 的错误路径，随后退出。
                                let _ = first_tx.send(FirstFrame::Error(message));
                                break 'reader;
                            }
                        }
                    }
                    OP_FRAME => {
                        // 握手后的 evt ERROR：断开并携带真实错误消息更新状态。
                        if let Some(message) = frame_error_message(&payload) {
                            disconnect_reason = Some(message);
                            break 'reader;
                        }
                    }
                    OP_CLOSE => break 'reader,
                    OP_PING => {
                        let _ = write_frame(&reader_writer, OP_PONG, &payload);
                    }
                    _ => {}
                }
            }
            drop(first_tx);
            // 仅 READY 已交付才通知断开：握手前的退出（超时/拒绝/畸形）由 open()
            // 的错误路径负责，避免向 controller 记录虚假 pending 断开。
            if ready_received {
                on_disconnect(disconnect_reason);
            }
        });

        let reader_tid = handle.thread().id();

        match first_rx.recv_timeout(handshake_timeout) {
            Ok(FirstFrame::Ready) => Ok(Self {
                writer,
                reader: Some(handle),
                reader_tid,
                cancel,
            }),
            Ok(FirstFrame::Error(message)) => {
                stop_reader(handle, &cancel);
                Err(format!("Discord handshake failed: {message}"))
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                stop_reader(handle, &cancel);
                Err("Discord handshake timed out.".to_string())
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                stop_reader(handle, &cancel);
                Err("Discord IPC closed before handshake completed.".to_string())
            }
        }
    }
}

/// 有限等待地打开一个命名管道（生产 open 对每个 pipe 的调用）。
///
/// API 语义证据（MSDN CreateFileW / WaitNamedPipeW 文档）：
/// - CreateFileW(OPEN_EXISTING) 打开命名管道在当前调用模式下从不无限等待：
///   管道不存在 → 立即失败 ERROR_FILE_NOT_FOUND(2)；"有活跃实例但无监听
///   实例（所有实例都已连接）→ 立即失败 ERROR_PIPE_BUSY(231)"；有监听实例
///   → 立即连接成功。std::fs::OpenOptions 即同步 CreateFileW，因此快速失败
///   路径本身有界，不会挂起调用线程。
/// - WaitNamedPipeW 把"管道瞬时忙"变成明确有限等待：实例可用（服务端
///   pending ConnectNamedPipe）或 nTimeOut 到期（ERROR_SEM_TIMEOUT(121)）
///   即返回；管道不存在时无论超时多少都立即返回。返回 TRUE 只代表"当时至少
///   有一个实例可用"，随后 CreateFileW 仍可能因竞态失败，故循环重试直到
///   整体 deadline。
#[cfg(windows)]
fn open_pipe_bounded(path: &str, deadline: std::time::Instant) -> std::io::Result<std::fs::File> {
    use std::io::{Error, ErrorKind};
    loop {
        if let Ok(file) = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
        {
            return Ok(file);
        }
        if std::time::Instant::now() >= deadline {
            return Err(Error::new(
                ErrorKind::TimedOut,
                "Discord IPC pipe open timed out",
            ));
        }
        // WaitNamedPipeW 要求 \\server\pipe\pipename 形式；\\?\pipe\ 与
        // \\.\pipe\ 指向同一管道对象（pipe_reader_tests 的服务器用 \\.\pipe\
        // 创建、客户端用 \\?\pipe\ 连接，已在测试中证明等价）。
        let wait_path = path.replacen(r"\\?\pipe\", r"\\.\pipe\", 1);
        let wait_ms = deadline
            .saturating_duration_since(std::time::Instant::now())
            .as_millis()
            .clamp(1, u64::from(u32::MAX) as u128) as u32; // 0 = NMPWAIT_USE_DEFAULT_WAIT，须避开
        if wait_named_pipe(&wait_path, wait_ms) {
            continue; // 实例可能已被其他客户端抢走 → 回到 open 重试
        }
        return Err(Error::last_os_error());
    }
}

#[cfg(windows)]
fn wait_named_pipe(pipe_name: &str, timeout_ms: u32) -> bool {
    use windows::core::PCWSTR;
    use windows::Win32::System::Pipes::WaitNamedPipeW;
    let wide: Vec<u16> = pipe_name.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe { WaitNamedPipeW(PCWSTR(wide.as_ptr()), timeout_ms).as_bool() }
}

/// 销毁/禁用/超时/ERROR 时取消阻塞读并安全 join，绝不永久 detached。
#[cfg(windows)]
impl Drop for IpcConnection {
    fn drop(&mut self) {
        let is_self = self.reader_tid == std::thread::current().id();
        if let Some(reader) = self.reader.take() {
            if !is_self {
                interrupt_reader(&reader, &self.cancel);
                let _ = reader.join();
            }
            // is_self（reader 线程自身的自然关闭路径）→ 已退出主循环，无需 join。
        }
    }
}

#[cfg(windows)]
impl RpcConnection for IpcConnection {
    fn set_activity(&self, activity: Option<&Value>) -> Result<(), String> {
        let mut args = serde_json::Map::new();
        args.insert("pid".to_string(), json!(std::process::id()));
        if let Some(activity) = activity {
            args.insert(
                "activity".to_string(),
                discord_activity_wire_format(activity),
            );
        }
        let payload = json!({
            "cmd": "SET_ACTIVITY",
            "args": args,
            "nonce": generate_nonce(),
        });
        let body = serde_json::to_vec(&payload)
            .map_err(|error| format!("serialize Discord activity: {error}"))?;
        write_frame(&self.writer, OP_FRAME, &body)
    }
}

/// 把渲染层 camelCase 活动转换为 Discord RPC 线上格式（对应 @xhayper
/// ClientUser.setActivity 的字段变换）：timestamps.start/end、assets.large_image/
/// large_text/small_text、created_at。缺失字段按库的行为省略。
/// 生产代码仅 Windows 命名管道传输引用；另有跨平台纯逻辑单元测试直接验证。
#[cfg(any(windows, test))]
pub fn discord_activity_wire_format(activity: &Value) -> Value {
    let mut formatted = serde_json::Map::new();
    formatted.insert(
        "name".to_string(),
        activity
            .get("name")
            .cloned()
            .unwrap_or_else(|| json!("Folia")),
    );
    formatted.insert(
        "type".to_string(),
        activity.get("type").cloned().unwrap_or_else(|| json!(0)),
    );
    formatted.insert("created_at".to_string(), json!(now_ms()));
    formatted.insert(
        "instance".to_string(),
        activity
            .get("instance")
            .cloned()
            .unwrap_or_else(|| json!(false)),
    );
    if let Some(value) = activity
        .get("details")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
    {
        formatted.insert("details".to_string(), json!(value));
    }
    if let Some(value) = activity
        .get("state")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
    {
        formatted.insert("state".to_string(), json!(value));
    }
    let start = activity.get("startTimestamp").and_then(Value::as_f64);
    let end = activity.get("endTimestamp").and_then(Value::as_f64);
    if start.is_some() || end.is_some() {
        let mut timestamps = serde_json::Map::new();
        if let Some(value) = start {
            timestamps.insert("start".to_string(), json!(value));
        }
        if let Some(value) = end {
            timestamps.insert("end".to_string(), json!(value));
        }
        formatted.insert("timestamps".to_string(), Value::Object(timestamps));
    }
    let large_image = activity.get("largeImageKey").and_then(Value::as_str);
    let large_text = activity.get("largeImageText").and_then(Value::as_str);
    let small_text = activity.get("smallImageText").and_then(Value::as_str);
    if large_image.is_some() || large_text.is_some() || small_text.is_some() {
        let mut assets = serde_json::Map::new();
        if let Some(value) = large_image {
            assets.insert("large_image".to_string(), json!(value));
        }
        if let Some(value) = large_text {
            assets.insert("large_text".to_string(), json!(value));
        }
        if let Some(value) = small_text {
            assets.insert("small_text".to_string(), json!(value));
        }
        formatted.insert("assets".to_string(), Value::Object(assets));
    }
    Value::Object(formatted)
}

/// Windows-only: writes a named-pipe frame (used by the IPC transport and its
/// Windows test server).
#[cfg(windows)]
fn write_frame(writer: &Mutex<std::fs::File>, op: u32, body: &[u8]) -> Result<(), String> {
    use std::io::Write;
    let mut pipe = writer
        .lock()
        .map_err(|_| "Discord pipe writer poisoned".to_string())?;
    pipe.write_all(&op.to_le_bytes())
        .map_err(|error| format!("write Discord frame opcode: {error}"))?;
    pipe.write_all(&(body.len() as u32).to_le_bytes())
        .map_err(|error| format!("write Discord frame length: {error}"))?;
    pipe.write_all(body)
        .map_err(|error| format!("write Discord frame body: {error}"))?;
    Ok(())
}

/// Windows-only: reads an exact-length named-pipe payload.
#[cfg(windows)]
fn read_exact(file: &mut std::fs::File, buf: &mut [u8]) -> std::io::Result<()> {
    use std::io::Read;
    file.read_exact(buf)
}

/// Windows-only: frame nonce for SET_ACTIVITY commands.
#[cfg(windows)]
fn generate_nonce() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut bytes);
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

// -- controller ----------------------------------------------------------------

struct ActiveConnection {
    id: u64,
    app_id: String,
    conn: Arc<dyn RpcConnection + Send + Sync>,
}

struct ControllerState {
    status: Value,
    last_activity_key: String,
    last_update_at: u128,
    last_snapshot: Option<Value>,
    connecting: bool,
    connection: Option<ActiveConnection>,
    connection_broken: bool,
    next_connection_id: u64,
    /// connect() 阻塞期间（generation 尚未注册到 connection）收到的断开：
    /// 记录 (generation id, 错误)，由 ensure_connection 在注册时决策，避免
    /// READY 后、注册前管道断开导致误报 connected。
    pending_disconnect: Option<(u64, String)>,
}

impl Default for ControllerState {
    fn default() -> Self {
        Self {
            status: json!({
                "enabled": false,
                "configured": false,
                "connected": false,
                "error": Value::Null,
                "applicationId": Value::Null,
                "updatedAt": now_ms(),
            }),
            last_activity_key: String::new(),
            last_update_at: 0,
            last_snapshot: None,
            connecting: false,
            connection: None,
            connection_broken: false,
            next_connection_id: 0,
            pending_disconnect: None,
        }
    }
}

/// Arc core 单值，可廉价 Clone（async command 从 AppHandle 取 owned clone 后
/// 移入 spawn_blocking / 后台线程）。
#[derive(Clone)]
pub struct DiscordPresenceController {
    core: Arc<ControllerCore>,
}

struct ControllerCore {
    state: Mutex<ControllerState>,
    transport: Arc<dyn RpcTransport + Send + Sync>,
    get_application_id: Arc<dyn Fn() -> String + Send + Sync>,
    is_enabled: Arc<dyn Fn() -> bool + Send + Sync>,
    on_status: Arc<dyn Fn(&Value) + Send + Sync>,
    /// Weak self-reference for the IPC reader-thread disconnect callback
    /// (avoids a strong cycle; the reader thread releases it on pipe close).
    self_weak: Mutex<Option<std::sync::Weak<ControllerCore>>>,
}

impl ControllerCore {
    fn set_self_weak(&self, weak: std::sync::Weak<ControllerCore>) {
        *self.self_weak.lock().unwrap() = Some(weak);
    }
}

impl DiscordPresenceController {
    /// Production constructor wired to settings + the main-window event bus.
    pub fn new(app: &AppHandle) -> Self {
        let emit_app = app.clone();
        let is_enabled_app = app.clone();
        let on_status = Arc::new(move |status: &Value| {
            use tauri::Emitter;
            let _ = emit_app.emit_to("main", "discord-presence-status-changed", status.clone());
        });
        let core = Arc::new(ControllerCore {
            state: Mutex::new(ControllerState::default()),
            transport: Arc::new(IpcTransport),
            get_application_id: Arc::new(|| DEFAULT_DISCORD_APPLICATION_ID.to_string()),
            is_enabled: Arc::new(move || {
                is_enabled_app
                    .state::<SettingsStore>()
                    .get_bool(DISCORD_RICH_PRESENCE_ENABLED)
            }),
            on_status,
            self_weak: Mutex::new(None),
        });
        core.set_self_weak(Arc::downgrade(&core));
        Self { core }
    }

    #[cfg(test)]
    pub fn for_test(
        transport: Arc<dyn RpcTransport + Send + Sync>,
        is_enabled: Arc<dyn Fn() -> bool + Send + Sync>,
    ) -> (Self, Arc<Mutex<Vec<Value>>>) {
        Self::for_test_with_app_id(
            transport,
            is_enabled,
            Arc::new(|| DEFAULT_DISCORD_APPLICATION_ID.to_string()),
        )
    }

    /// 测试构造器 + 可配置的 application id（app_id 切换场景需要换 id）。
    #[cfg(test)]
    pub fn for_test_with_app_id(
        transport: Arc<dyn RpcTransport + Send + Sync>,
        is_enabled: Arc<dyn Fn() -> bool + Send + Sync>,
        get_application_id: Arc<dyn Fn() -> String + Send + Sync>,
    ) -> (Self, Arc<Mutex<Vec<Value>>>) {
        let statuses: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
        let recorder = statuses.clone();
        let core = Arc::new(ControllerCore {
            state: Mutex::new(ControllerState::default()),
            transport,
            get_application_id,
            is_enabled,
            on_status: Arc::new(move |status: &Value| {
                recorder.lock().unwrap().push(status.clone());
            }),
            self_weak: Mutex::new(None),
        });
        core.set_self_weak(Arc::downgrade(&core));
        (Self { core }, statuses)
    }

    pub fn get_status(&self) -> Value {
        self.core.get_status()
    }

    pub fn publish_snapshot(&self, snapshot: Value) -> Value {
        self.core.publish_snapshot(snapshot)
    }

    pub fn refresh(&self) -> Value {
        self.core.refresh()
    }

    pub fn destroy(&self) {
        self.core.destroy();
    }
}

impl ControllerCore {
    pub fn get_status(&self) -> Value {
        self.state.lock().unwrap().status.clone()
    }

    pub fn publish_snapshot(&self, snapshot: Value) -> Value {
        self.publish_inner(Some(snapshot))
    }

    pub fn refresh(&self) -> Value {
        let last = self.state.lock().unwrap().last_snapshot.clone();
        self.publish_inner(last)
    }

    pub fn destroy(&self) {
        // 取出连接并在锁外 drop：Drop 会取消 reader 并 join，不能在持锁时进行。
        let dropped = {
            let mut state = self.state.lock().unwrap();
            state.connection_broken = false;
            state.last_activity_key = String::new();
            state.last_update_at = 0;
            state.connection.take()
        };
        drop(dropped);
    }

    // 状态发布：字段级变更才推送事件（Electron publishStatus 语义），updatedAt 总是刷新。
    fn publish_status(&self, patch: &Value) -> Value {
        let mut state = self.state.lock().unwrap();
        let mut next = state.status.clone();
        if let Some(object) = patch.as_object() {
            for (key, value) in object {
                next[key] = value.clone();
            }
        }
        let changed = state.status["enabled"] != next["enabled"]
            || state.status["configured"] != next["configured"]
            || state.status["connected"] != next["connected"]
            || state.status["error"] != next["error"]
            || state.status["applicationId"] != next["applicationId"];
        next["updatedAt"] = json!(now_ms());
        state.status = next.clone();
        drop(state);
        if changed {
            (self.on_status)(&next);
        }
        next
    }

    fn current_connection(&self) -> Option<Arc<dyn RpcConnection + Send + Sync>> {
        self.state
            .lock()
            .unwrap()
            .connection
            .as_ref()
            .map(|active| active.conn.clone())
    }

    fn ensure_connection(&self, app_id: &str) {
        let should_connect = {
            let state = self.state.lock().unwrap();
            if state.connecting {
                return;
            }
            match &state.connection {
                Some(active) => active.app_id != app_id || state.connection_broken,
                None => true,
            }
        };
        if !should_connect {
            return;
        }

        let id = {
            let mut state = self.state.lock().unwrap();
            // 清掉上一个 generation 遗留的 pending 断开记录。
            state.pending_disconnect = None;
            state.connecting = true;
            state.next_connection_id += 1;
            state.next_connection_id
        };
        // 断开回调运行在 IPC 读线程；通过 Weak 引用 controller，读线程随管道
        // 关闭退出后释放（不构成强循环）。Some(message) 为 Discord evt ERROR
        // 的真实错误消息，None 表示管道被动关闭。
        let self_weak = self.self_weak.lock().unwrap().clone();
        let on_disconnect: Arc<dyn Fn(Option<String>) + Send + Sync> = Arc::new(move |reason| {
            if let Some(core) = self_weak.as_ref().and_then(|weak| weak.upgrade()) {
                let error = reason.unwrap_or_else(|| "Discord disconnected.".to_string());
                core.disconnect_with_error(&id, &error);
            }
        });
        let result = self.transport.connect(app_id, on_disconnect);

        match result {
            Err(error) => {
                // 连接失败：旧连接（app_id 切换场景）在锁外 drop，join 旧 reader。
                let dropped = {
                    let mut state = self.state.lock().unwrap();
                    state.connecting = false;
                    state.pending_disconnect = None;
                    let dropped = state.connection.take();
                    // 本次连接尝试作废，重置节流状态使 refresh 重连后能立即重发。
                    state.last_activity_key = String::new();
                    state.last_update_at = 0;
                    dropped
                };
                drop(dropped);
                let _ = self.publish_status(&json!({ "connected": false, "error": error }));
            }
            Ok(conn) => {
                // 所有旧/新连接的 drop 都放在 state 锁外：IpcConnection::Drop
                // 会 cancel+join reader，而 reader 退出时会回调
                // disconnect_with_error → 重新取同一把 state 锁。若持锁 drop，
                // drop 线程 join 等 reader、reader 回调等锁，形成 AB-BA 死锁。
                // 锁内只做状态登记，连接一律取到锁外再 drop。
                let (dropped_old, outcome) = {
                    let mut state = self.state.lock().unwrap();
                    state.connecting = false;
                    let dropped_old = state.connection.take();
                    match state.pending_disconnect.take() {
                        // READY 已收到但注册前管道断开/收到 ERROR → 不注册，
                        // 新 conn 与旧 conn 都到锁外 drop，报告真实错误。
                        Some((pending_id, error)) if pending_id == id => {
                            (dropped_old, Some((conn, error)))
                        }
                        _ => {
                            state.connection = Some(ActiveConnection {
                                id,
                                app_id: app_id.to_string(),
                                conn,
                            });
                            state.connection_broken = false;
                            // 对齐 Electron destroyClient：重连后重置节流，立即重发。
                            state.last_activity_key = String::new();
                            state.last_update_at = 0;
                            (dropped_old, None)
                        }
                    }
                }; // state 锁在此释放
                drop(dropped_old); // 锁外 cancel + join 旧 reader
                match outcome {
                    Some((conn, error)) => {
                        drop(conn); // 锁外 cancel + join 新 conn 的 reader
                        let _ = self.publish_status(&json!({ "connected": false, "error": error }));
                    }
                    None => {
                        let _ = self
                            .publish_status(&json!({ "connected": true, "error": Value::Null }));
                    }
                }
            }
        }
    }

    fn disconnect_with_error(&self, id: &u64, error: &str) {
        let dropped = {
            let mut state = self.state.lock().unwrap();
            match &state.connection {
                Some(active) if active.id == *id => {
                    let dropped = state.connection.take();
                    state.connection_broken = true;
                    dropped
                }
                _ => {
                    // 连接尚未注册（READY 竞态）→ 记录 pending，由 ensure_connection
                    // 在注册时决策，避免断开事件被永久丢失。
                    if state.connecting {
                        state.pending_disconnect = Some((*id, error.to_string()));
                    }
                    None
                }
            }
        };
        if dropped.is_some() {
            // 锁外 drop：reader 线程自身调用时（自然关闭）跳过 join，不会死锁。
            drop(dropped);
            let _ = self.publish_status(&json!({ "connected": false, "error": error }));
        }
    }

    fn publish_inner(&self, snapshot: Option<Value>) -> Value {
        // 无论连接是否就绪都先保存 latest snapshot（对齐 Electron 参考实现）：
        // 连接尝试失败后 refresh()/重连仍可发布最近一次快照。
        self.state.lock().unwrap().last_snapshot = snapshot.clone();

        let app_id = normalize_discord_application_id(&(self.get_application_id)());
        let enabled = (self.is_enabled)();
        let _ = self.publish_status(&json!({
            "enabled": enabled,
            "configured": !app_id.is_empty(),
            "applicationId": if app_id.is_empty() {
                Value::Null
            } else {
                Value::String(app_id.clone())
            },
        }));

        if !enabled || app_id.is_empty() {
            self.destroy();
            let _ = self.publish_status(&json!({
                "connected": false,
                "error": if enabled {
                    Value::String("Discord application identity is unavailable.".to_string())
                } else {
                    Value::Null
                },
            }));
            return self.get_status();
        }

        self.ensure_connection(&app_id);

        let Some(connection) = self.current_connection() else {
            return self.get_status();
        };

        // 连接就绪后选择 state 中最新快照发送：连接建立期间（connecting 窗口）
        // 到达的后续快照已写入 last_snapshot，若仍用本次 publish 的旧入参发送
        // 会丢快照。latest 由"谁最后写入 last_snapshot"决定，与连接线程无关。
        let snapshot = self.state.lock().unwrap().last_snapshot.clone();
        let activity = snapshot.as_ref().and_then(build_discord_activity);
        let activity_key = get_activity_key(activity.as_ref());
        let now = now_ms();
        let should_send = {
            let state = self.state.lock().unwrap();
            if activity.is_none() {
                state.last_activity_key != "empty"
            } else {
                activity_key != state.last_activity_key
                    || now.saturating_sub(state.last_update_at)
                        >= DISCORD_PRESENCE_UPDATE_INTERVAL_MS
            }
        };
        if !should_send {
            return self.get_status();
        }

        match connection.set_activity(activity.as_ref()) {
            Ok(()) => {
                let mut state = self.state.lock().unwrap();
                state.last_activity_key = if activity.is_none() {
                    "empty".to_string()
                } else {
                    activity_key
                };
                state.last_update_at = now;
                drop(state);
                let _ = self.publish_status(&json!({ "connected": true, "error": Value::Null }));
            }
            Err(error) => {
                let id = self
                    .state
                    .lock()
                    .unwrap()
                    .connection
                    .as_ref()
                    .map(|active| active.id);
                if let Some(id) = id {
                    self.disconnect_with_error(&id, &error);
                }
            }
        }
        self.get_status()
    }
}

// -- Tauri commands ------------------------------------------------------------

#[tauri::command]
// 返回 Discord Rich Presence 生命周期状态（enabled/configured/connected/error…）。
pub fn discord_presence_get_status(app: AppHandle) -> Result<Value, String> {
    Ok(app.state::<DiscordPresenceController>().get_status())
}

#[tauri::command]
// 渲染层发布播放快照 → 构建活动并同步到 Discord；返回最新状态。
// async command：publish 会做 named-pipe connect/handshake（最坏 5s 超时），
// 整个 controller publish 放进后台阻塞池，绝不阻塞 IPC 主线程。不能把
// State<'_, _> 借用移进 'static closure，故先从 AppHandle 取出 owned clone
// （DiscordPresenceController 基于 Arc core，克隆廉价），再 move 进
// spawn_blocking。返回 JSON/错误合同与同步版完全一致。
pub async fn discord_presence_publish_snapshot(
    app: AppHandle,
    snapshot: Value,
) -> Result<Value, String> {
    let controller = app.state::<DiscordPresenceController>().inner().clone();
    tauri::async_runtime::spawn_blocking(move || Ok(controller.publish_snapshot(snapshot)))
        .await
        .map_err(|error| format!("discord presence publish task failed: {error}"))?
}

/// 设置联动：DISCORD_RICH_PRESENCE_ENABLED 变更时重新评估并推送状态。
/// refresh() 会触发 connect/handshake（最坏 5s），不得同步阻塞 save_settings，
/// 因此 clone 出 controller 后转到后台线程执行。
pub fn refresh(app: &AppHandle) {
    let controller = app.state::<DiscordPresenceController>().inner().clone();
    std::thread::spawn(move || {
        let _ = controller.refresh();
    });
}

/// 应用退出时关闭 IPC 连接（best-effort）。
pub fn destroy(app: &AppHandle) {
    app.state::<DiscordPresenceController>().destroy();
}

// -- tests ---------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn playing_snapshot(overrides: &[(&str, Value)]) -> Value {
        let mut value = json!({
            "hasTrack": true,
            "title": "Song",
            "artist": "Artist",
            "playerState": "PLAYING",
            "currentTime": 30.0,
            "duration": 120.0,
            "updatedAt": 10_000.0,
            "coverUrl": "https://example.com/cover.jpg",
        });
        if let Some(object) = value.as_object_mut() {
            for (key, override_value) in overrides {
                object.insert(key.to_string(), override_value.clone());
            }
        }
        value
    }

    #[test]
    fn normalizes_discord_application_ids() {
        assert_eq!(DEFAULT_DISCORD_APPLICATION_ID, "1518508445483925645");
        assert_eq!(
            normalize_discord_application_id(" 123456789012345678 "),
            "123456789012345678"
        );
        assert_eq!(normalize_discord_application_id("not-a-snowflake"), "");
        assert_eq!(normalize_discord_application_id("123"), "");
    }

    #[test]
    fn normalizes_externally_reachable_cover_urls() {
        assert_eq!(
            normalize_discord_image_url(" https://example.com/cover.jpg "),
            "https://example.com/cover.jpg"
        );
        assert_eq!(
            normalize_discord_image_url("http://example.com/cover.jpg"),
            "https://example.com/cover.jpg"
        );
        assert_eq!(
            normalize_discord_image_url("blob:https://example.com/id"),
            ""
        );
        assert_eq!(
            normalize_discord_image_url("http://127.0.0.1:3000/cover.jpg"),
            ""
        );
        assert_eq!(normalize_discord_image_url("file:///tmp/cover.jpg"), "");
    }

    #[test]
    fn returns_null_when_there_is_no_active_track() {
        assert!(build_discord_activity(&json!({ "hasTrack": false })).is_none());
        assert!(build_discord_activity(&json!({ "hasTrack": true, "title": "" })).is_none());
    }

    #[test]
    fn builds_listening_activity_with_progress_timestamps_while_playing() {
        let activity = build_discord_activity(&playing_snapshot(&[])).unwrap();
        assert_eq!(activity["name"], "Folia");
        assert_eq!(activity["type"], 2);
        assert_eq!(activity["details"], "Song");
        assert_eq!(activity["state"], "Artist");
        assert_eq!(activity["largeImageKey"], "https://example.com/cover.jpg");
        assert_eq!(activity["largeImageText"], "Song");
        assert_eq!(activity["smallImageText"], "Playing");
        assert_eq!(activity["startTimestamp"].as_f64().unwrap(), 0.0);
        assert_eq!(activity["endTimestamp"].as_f64().unwrap(), 100_000.0);
        assert_eq!(activity["instance"], false);
    }

    #[test]
    fn marks_paused_playback_without_progress_timestamps() {
        let activity =
            build_discord_activity(&playing_snapshot(&[("playerState", json!("PAUSED"))])).unwrap();
        assert_eq!(activity["details"], "Song");
        assert_eq!(activity["state"], "Paused - Artist");
        assert_eq!(activity["smallImageText"], "Paused");
        assert!(activity.get("startTimestamp").is_none());
        assert!(activity.get("endTimestamp").is_none());
    }

    #[test]
    fn activity_state_uses_folia_default_artist_and_truncates() {
        let snapshot = playing_snapshot(&[
            ("artist", json!("   ")),
            ("title", json!(format!("{}{}", "t".repeat(200), "END"))),
        ]);
        let activity = build_discord_activity(&snapshot).unwrap();
        assert_eq!(activity["state"], "Folia");
        let details = activity["details"].as_str().unwrap();
        assert_eq!(details.len(), 128);
        assert!(!details.contains("END"));
    }

    #[test]
    fn activity_truncates_title_and_artist_to_128() {
        let long = "x".repeat(150);
        let snapshot = playing_snapshot(&[
            ("title", json!(long.clone())),
            ("artist", json!(long.clone())),
        ]);
        let activity = build_discord_activity(&snapshot).unwrap();
        assert_eq!(activity["details"].as_str().unwrap().len(), 128);
        assert_eq!(activity["state"].as_str().unwrap().len(), 128);
    }

    #[test]
    fn non_finite_duration_suppresses_timestamps() {
        let activity =
            build_discord_activity(&playing_snapshot(&[("duration", Value::Null)])).unwrap();
        assert!(activity.get("startTimestamp").is_none());
        assert!(activity.get("endTimestamp").is_none());
    }

    #[test]
    fn localhost_cover_urls_are_dropped() {
        let snapshot = playing_snapshot(&[("coverUrl", json!("http://127.0.0.1:3000/cover.jpg"))]);
        let activity = build_discord_activity(&snapshot).unwrap();
        assert!(activity.get("largeImageKey").is_none());
        assert_eq!(activity["largeImageText"], "Folia");
    }

    #[test]
    fn activity_key_changes_when_track_state_changes() {
        let playing = build_discord_activity(&playing_snapshot(&[])).unwrap();
        let paused =
            build_discord_activity(&playing_snapshot(&[("playerState", json!("PAUSED"))])).unwrap();
        let same = build_discord_activity(&playing_snapshot(&[])).unwrap();
        assert_eq!(
            get_activity_key(Some(&playing)),
            get_activity_key(Some(&same))
        );
        assert_ne!(
            get_activity_key(Some(&playing)),
            get_activity_key(Some(&paused))
        );
        assert_eq!(get_activity_key(None), "empty");
    }

    #[test]
    fn wire_format_matches_discord_rpc_schema() {
        let activity = build_discord_activity(&playing_snapshot(&[])).unwrap();
        let wire = discord_activity_wire_format(&activity);
        assert_eq!(wire["name"], "Folia");
        assert_eq!(wire["type"], 2);
        assert_eq!(wire["instance"], false);
        assert_eq!(wire["details"], "Song");
        assert_eq!(wire["state"], "Artist");
        assert!(wire.get("created_at").is_some());
        // camelCase → snake_case 嵌套变换
        assert_eq!(wire["timestamps"]["start"], 0.0);
        assert_eq!(wire["timestamps"]["end"], 100_000.0);
        assert_eq!(
            wire["assets"]["large_image"],
            "https://example.com/cover.jpg"
        );
        assert_eq!(wire["assets"]["large_text"], "Song");
        assert_eq!(wire["assets"]["small_text"], "Playing");
        // 原始 camelCase 键不应出现在线上格式
        assert!(wire.get("startTimestamp").is_none());
        assert!(wire.get("largeImageKey").is_none());
    }

    #[test]
    fn wire_format_omits_assets_when_cover_absent() {
        let activity =
            build_discord_activity(&playing_snapshot(&[("coverUrl", Value::Null)])).unwrap();
        let wire = discord_activity_wire_format(&activity);
        // 播放中且时长有限 → 时间戳仍在；无封面 → large_image 省略（small_text 仍在）
        assert!(wire.get("timestamps").is_some());
        assert!(wire.get("assets").is_some());
        assert!(wire["assets"].get("large_image").is_none());
        assert_eq!(wire["assets"]["small_text"], "Playing");
        assert_eq!(wire["state"], "Artist");
    }

    #[test]
    fn wire_format_omits_timestamps_when_paused() {
        let activity =
            build_discord_activity(&playing_snapshot(&[("playerState", json!("PAUSED"))])).unwrap();
        let wire = discord_activity_wire_format(&activity);
        assert!(wire.get("timestamps").is_none());
        assert_eq!(wire["state"], "Paused - Artist");
    }

    #[test]
    fn wire_format_drops_empty_details_and_state() {
        let wire = discord_activity_wire_format(&json!({
            "name": "Folia",
            "type": 2,
            "details": "",
            "state": "",
        }));
        assert!(wire.get("details").is_none());
        assert!(wire.get("state").is_none());
        assert_eq!(wire["name"], "Folia");
    }

    // -- controller with a fake transport ------------------------------------

    struct FakeConn {
        calls: Arc<Mutex<Vec<Option<Value>>>>,
    }

    impl RpcConnection for FakeConn {
        fn set_activity(&self, activity: Option<&Value>) -> Result<(), String> {
            self.calls.lock().unwrap().push(activity.cloned());
            Ok(())
        }
    }

    struct FakeTransport {
        fail_connect: std::sync::atomic::AtomicBool,
        /// connect() 返回 Ok 前立即触发的断开（模拟 READY 后、注册前的竞态断开）：
        /// `Some(Some(msg))` → on_disconnect(Some(msg))，`Some(None)` → on_disconnect(None)，
        /// `None` → 不触发。
        disconnect_before_return: Mutex<Option<Option<String>>>,
        /// 保存每次 connect 的 on_disconnect，测试可随时触发（模拟建立连接后的断开/ERROR）。
        saved_disconnects: Arc<Mutex<Vec<Arc<dyn Fn(Option<String>) + Send + Sync>>>>,
        connect_count: std::sync::atomic::AtomicUsize,
        calls: Arc<Mutex<Vec<Option<Value>>>>,
    }

    impl RpcTransport for FakeTransport {
        fn connect(
            &self,
            _app_id: &str,
            on_disconnect: Arc<dyn Fn(Option<String>) + Send + Sync>,
        ) -> Result<Arc<dyn RpcConnection + Send + Sync>, String> {
            self.connect_count
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.saved_disconnects
                .lock()
                .unwrap()
                .push(on_disconnect.clone());
            if self.fail_connect.load(std::sync::atomic::Ordering::SeqCst) {
                return Err("fake connect failed".to_string());
            }
            if let Some(reason) = self.disconnect_before_return.lock().unwrap().clone() {
                on_disconnect(reason);
            }
            Ok(Arc::new(FakeConn {
                calls: self.calls.clone(),
            }))
        }
    }

    fn fake_transport() -> (Arc<FakeTransport>, Arc<Mutex<Vec<Option<Value>>>>) {
        let calls: Arc<Mutex<Vec<Option<Value>>>> = Arc::new(Mutex::new(Vec::new()));
        (
            Arc::new(FakeTransport {
                fail_connect: std::sync::atomic::AtomicBool::new(false),
                disconnect_before_return: Mutex::new(None),
                saved_disconnects: Arc::new(Mutex::new(Vec::new())),
                connect_count: std::sync::atomic::AtomicUsize::new(0),
                calls: calls.clone(),
            }),
            calls,
        )
    }

    #[test]
    fn controller_disabled_status_is_honest_and_does_not_connect() {
        let (transport, calls) = fake_transport();
        let (controller, statuses) =
            DiscordPresenceController::for_test(transport, Arc::new(|| false));
        let status = controller.publish_snapshot(playing_snapshot(&[]));
        assert_eq!(status["enabled"], false);
        assert_eq!(status["connected"], false);
        assert_eq!(status["error"], Value::Null);
        assert_eq!(status["configured"], true);
        assert_eq!(calls.lock().unwrap().len(), 0);
        // 事件只推送给 enabled/configured/connected/error 变化的时刻
        assert!(!statuses.lock().unwrap().is_empty());
    }

    #[test]
    fn controller_connect_failure_is_an_honest_error() {
        let (transport, _) = fake_transport();
        transport
            .fail_connect
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let (controller, statuses) =
            DiscordPresenceController::for_test(transport, Arc::new(|| true));
        let status = controller.publish_snapshot(playing_snapshot(&[]));
        assert_eq!(status["enabled"], true);
        assert_eq!(status["connected"], false);
        assert_eq!(status["error"], "fake connect failed");
        let last = statuses.lock().unwrap().last().cloned().unwrap();
        assert_eq!(last["connected"], false);
        assert_eq!(last["error"], "fake connect failed");
    }

    #[test]
    fn controller_publishes_activity_and_throttles_within_interval() {
        let (transport, calls) = fake_transport();
        let (controller, _) = DiscordPresenceController::for_test(transport, Arc::new(|| true));

        let status = controller.publish_snapshot(playing_snapshot(&[]));
        assert_eq!(status["connected"], true);
        assert_eq!(calls.lock().unwrap().len(), 1);
        assert_eq!(
            calls.lock().unwrap()[0].as_ref().unwrap()["details"],
            "Song"
        );

        // 相同活动 + 未到 15s 节流 → 不再发送
        let status = controller.publish_snapshot(playing_snapshot(&[]));
        assert_eq!(status["connected"], true);
        assert_eq!(calls.lock().unwrap().len(), 1);

        // 活动变化（暂停）→ 立即发送
        controller.publish_snapshot(playing_snapshot(&[("playerState", json!("PAUSED"))]));
        assert_eq!(calls.lock().unwrap().len(), 2);
        assert_eq!(
            calls.lock().unwrap()[1].as_ref().unwrap()["state"],
            "Paused - Artist"
        );
    }

    #[test]
    fn controller_sends_clear_when_track_disappears() {
        let (transport, calls) = fake_transport();
        let (controller, _) = DiscordPresenceController::for_test(transport, Arc::new(|| true));
        controller.publish_snapshot(playing_snapshot(&[]));
        controller.publish_snapshot(json!({ "hasTrack": false }));
        let recorded = calls.lock().unwrap().clone();
        assert_eq!(recorded.len(), 2);
        assert!(recorded[1].is_none());
    }

    #[test]
    fn disabling_mid_session_destroys_connection_and_clears_error() {
        let (transport, calls) = fake_transport();
        let enabled = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let enabled_flag = enabled.clone();
        let (controller, statuses) = DiscordPresenceController::for_test(
            transport,
            Arc::new(move || enabled_flag.load(std::sync::atomic::Ordering::SeqCst)),
        );
        controller.publish_snapshot(playing_snapshot(&[]));
        assert_eq!(controller.get_status()["connected"], true);
        assert_eq!(calls.lock().unwrap().len(), 1);

        enabled.store(false, std::sync::atomic::Ordering::SeqCst);
        let status = controller.refresh();
        assert_eq!(status["enabled"], false);
        assert_eq!(status["connected"], false);
        assert_eq!(status["error"], Value::Null);
        let last = statuses.lock().unwrap().last().cloned().unwrap();
        assert_eq!(last["enabled"], false);
        assert_eq!(last["connected"], false);
    }

    #[test]
    fn snapshot_without_active_track_still_reports_connected() {
        let (transport, calls) = fake_transport();
        let (controller, _) = DiscordPresenceController::for_test(transport, Arc::new(|| true));
        let status = controller.publish_snapshot(json!({ "hasTrack": false }));
        assert_eq!(status["connected"], true);
        // 首次无曲目 → 清空活动
        assert_eq!(calls.lock().unwrap().len(), 1);
        assert!(calls.lock().unwrap()[0].is_none());
        // 连续无曲目 → 不再重复清空
        controller.publish_snapshot(json!({ "hasTrack": false }));
        assert_eq!(calls.lock().unwrap().len(), 1);
    }

    /// latest snapshot 在连接尝试前保存：连接失败后 refresh()/重连仍可发布。
    #[test]
    fn controller_saves_snapshot_before_connect_failure_and_refresh_reconnects() {
        let (transport, calls) = fake_transport();
        transport
            .fail_connect
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let (controller, _) =
            DiscordPresenceController::for_test(transport.clone(), Arc::new(|| true));

        let status = controller.publish_snapshot(playing_snapshot(&[]));
        assert_eq!(status["connected"], false);
        assert_eq!(status["error"], "fake connect failed");
        assert_eq!(calls.lock().unwrap().len(), 0);

        // 连接恢复后 refresh() 使用已保存的快照重新发布
        transport
            .fail_connect
            .store(false, std::sync::atomic::Ordering::SeqCst);
        let status = controller.refresh();
        assert_eq!(status["connected"], true);
        assert_eq!(status["error"], Value::Null);
        let recorded = calls.lock().unwrap().clone();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].as_ref().unwrap()["details"], "Song");
    }

    /// READY 后、注册前管道断开（pending generation 竞态）→ 不得误报 connected。
    #[test]
    fn controller_handles_disconnect_before_registration() {
        let (transport, _) = fake_transport();
        *transport.disconnect_before_return.lock().unwrap() = Some(None);
        let (controller, statuses) =
            DiscordPresenceController::for_test(transport, Arc::new(|| true));
        let status = controller.publish_snapshot(playing_snapshot(&[]));
        assert_eq!(status["connected"], false);
        assert_eq!(status["error"], "Discord disconnected.");
        let last = statuses.lock().unwrap().last().cloned().unwrap();
        assert_eq!(last["connected"], false);
        assert_eq!(last["error"], "Discord disconnected.");
    }

    /// READY 后、注册前收到 evt ERROR → 报告真实错误，不注册假连接。
    #[test]
    fn controller_handles_error_before_registration() {
        let (transport, _) = fake_transport();
        *transport.disconnect_before_return.lock().unwrap() =
            Some(Some("Invalid activity".to_string()));
        let (controller, statuses) =
            DiscordPresenceController::for_test(transport, Arc::new(|| true));
        let status = controller.publish_snapshot(playing_snapshot(&[]));
        assert_eq!(status["connected"], false);
        assert_eq!(status["error"], "Invalid activity");
        let last = statuses.lock().unwrap().last().cloned().unwrap();
        assert_eq!(last["error"], "Invalid activity");
    }

    /// 握手成功后收到 evt ERROR → 更新真实状态（connected:false + 错误），
    /// 随后 refresh() 重连并重新发布。
    #[test]
    fn controller_post_handshake_error_updates_real_status() {
        let (transport, calls) = fake_transport();
        let (controller, _) =
            DiscordPresenceController::for_test(transport.clone(), Arc::new(|| true));
        controller.publish_snapshot(playing_snapshot(&[]));
        assert_eq!(controller.get_status()["connected"], true);
        assert_eq!(calls.lock().unwrap().len(), 1);

        // 模拟 Discord 在握手后拒绝命令
        transport.saved_disconnects.lock().unwrap()[0](Some("Invalid activity".to_string()));
        let status = controller.get_status();
        assert_eq!(status["connected"], false);
        assert_eq!(status["error"], "Invalid activity");

        // 重连后（节流已重置）立即重发最近快照
        let status = controller.refresh();
        assert_eq!(status["connected"], true);
        assert_eq!(status["error"], Value::Null);
        assert_eq!(
            transport
                .connect_count
                .load(std::sync::atomic::Ordering::SeqCst),
            2
        );
        assert_eq!(calls.lock().unwrap().len(), 2);
        assert_eq!(
            calls.lock().unwrap()[1].as_ref().unwrap()["details"],
            "Song"
        );
    }

    // -- 回归：锁外 drop 旧连接 / 并发连接期间不丢最新快照 ----------------

    /// Drop 时回调 controller 的连接：真实 IpcConnection::Drop 会 cancel+join
    /// reader，而 reader 退出回调会重新取 controller 的 state 锁。此连接在
    /// Drop 中重取同一把锁并记录 try_lock 结果，用于证明 ensure_connection
    /// 释放 state 锁后才 drop —— 持锁 drop 会形成 AB-BA（drop 线程 join 等
    /// reader，reader 回调等锁），而不是可观测的 try_lock 失败/死锁。
    struct DropReentrantConn {
        on_drop: Arc<dyn Fn() + Send + Sync>,
    }

    impl RpcConnection for DropReentrantConn {
        fn set_activity(&self, _activity: Option<&Value>) -> Result<(), String> {
            Ok(())
        }
    }

    impl Drop for DropReentrantConn {
        fn drop(&mut self) {
            (self.on_drop)();
        }
    }

    /// 每次 connect 都返回 DropReentrantConn；Drop 回调探测 controller 锁。
    struct DropProbeTransport {
        drop_count: Arc<std::sync::atomic::AtomicUsize>,
        lock_probe_successes: Arc<std::sync::atomic::AtomicUsize>,
        lock_probe_failures: Arc<std::sync::atomic::AtomicUsize>,
        probe_target: Arc<Mutex<Option<Arc<ControllerCore>>>>,
    }

    impl RpcTransport for DropProbeTransport {
        fn connect(
            &self,
            _app_id: &str,
            _on_disconnect: Arc<dyn Fn(Option<String>) + Send + Sync>,
        ) -> Result<Arc<dyn RpcConnection + Send + Sync>, String> {
            let drop_count = self.drop_count.clone();
            let successes = self.lock_probe_successes.clone();
            let failures = self.lock_probe_failures.clone();
            let probe_target = self.probe_target.clone();
            Ok(Arc::new(DropReentrantConn {
                on_drop: Arc::new(move || {
                    drop_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let Some(target) = probe_target
                        .lock()
                        .unwrap()
                        .as_ref()
                        .map(|core| core.clone())
                    else {
                        return;
                    };
                    if target.state.try_lock().is_ok() {
                        successes.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    } else {
                        failures.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    }
                }),
            }))
        }
    }

    /// app_id 切换 → 旧连接在 state 锁释放后 drop。若仍持锁 drop，
    /// Drop 回调中的 try_lock 会失败（旧实现即如此），从而断言失败。
    #[test]
    fn app_id_switch_drops_old_connection_outside_state_lock() {
        let drop_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let lock_probe_successes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let lock_probe_failures = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let probe_target: Arc<Mutex<Option<Arc<ControllerCore>>>> = Arc::new(Mutex::new(None));
        let transport = Arc::new(DropProbeTransport {
            drop_count: drop_count.clone(),
            lock_probe_successes: lock_probe_successes.clone(),
            lock_probe_failures: lock_probe_failures.clone(),
            probe_target: probe_target.clone(),
        });

        let app_generation = Arc::new(std::sync::atomic::AtomicU64::new(1));
        let app_gen = app_generation.clone();
        let (controller, _) = DiscordPresenceController::for_test_with_app_id(
            transport,
            Arc::new(|| true),
            Arc::new(move || {
                if app_gen.load(std::sync::atomic::Ordering::SeqCst) == 1 {
                    "100000000000000001".to_string() // app A
                } else {
                    "200000000000000002".to_string() // app B
                }
            }),
        );
        *probe_target.lock().unwrap() = Some(controller.core.clone());

        // 连接 app A
        let status = controller.publish_snapshot(playing_snapshot(&[]));
        assert_eq!(status["connected"], true);
        assert_eq!(drop_count.load(std::sync::atomic::Ordering::SeqCst), 0);

        // 切换 app_id → 触发旧连接 drop；Drop 回调探测锁：锁必须未被持有
        app_generation.store(2, std::sync::atomic::Ordering::SeqCst);
        let status = controller.publish_snapshot(playing_snapshot(&[]));
        assert_eq!(status["connected"], true);
        assert_eq!(drop_count.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(
            lock_probe_failures.load(std::sync::atomic::Ordering::SeqCst),
            0
        );
        assert_eq!(
            lock_probe_successes.load(std::sync::atomic::Ordering::SeqCst),
            1
        );
    }

    /// connect() 阻塞直到测试放行，模拟真实的 5s connect 窗口。
    struct BlockingConnectTransport {
        /// connect() 进入后置位（测试据此确认"正在连接"）。
        started: Arc<std::sync::atomic::AtomicBool>,
        /// connect() 在此阻塞直到测试放行（Receiver 包进 Mutex 使 transport 满足 Sync）。
        release: Mutex<std::sync::mpsc::Receiver<()>>,
        calls: Arc<Mutex<Vec<Option<Value>>>>,
    }

    impl RpcTransport for BlockingConnectTransport {
        fn connect(
            &self,
            _app_id: &str,
            _on_disconnect: Arc<dyn Fn(Option<String>) + Send + Sync>,
        ) -> Result<Arc<dyn RpcConnection + Send + Sync>, String> {
            self.started
                .store(true, std::sync::atomic::Ordering::SeqCst);
            self.release
                .lock()
                .unwrap()
                .recv()
                .expect("test must release the blocking connect");
            Ok(Arc::new(FakeConn {
                calls: self.calls.clone(),
            }))
        }
    }

    /// 并发连接期间到达的后续快照不能丢：新连接就绪后必须发送 state 中
    /// 最新 last_snapshot，而不是首个 publish 传入的旧快照。
    #[test]
    fn latest_snapshot_during_connecting_is_not_lost() {
        let started = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let calls: Arc<Mutex<Vec<Option<Value>>>> = Arc::new(Mutex::new(Vec::new()));
        let transport = Arc::new(BlockingConnectTransport {
            started: started.clone(),
            release: Mutex::new(release_rx),
            calls: calls.clone(),
        });
        let (controller, _) = DiscordPresenceController::for_test(transport, Arc::new(|| true));

        // 线程 A：首次 publish 阻塞在 connect 内
        let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
        let first_controller = controller.clone();
        std::thread::spawn(move || {
            let _ = first_controller.publish_snapshot(playing_snapshot(&[("title", json!("Old"))]));
            let _ = done_tx.send(());
        });
        while !started.load(std::sync::atomic::Ordering::SeqCst) {
            std::thread::yield_now();
        }

        // 线程 B：连接期间发布更新的快照 → 存入 last_snapshot，但未发送
        let status = controller.publish_snapshot(playing_snapshot(&[("title", json!("New"))]));
        assert_eq!(status["connected"], false); // 仍在 connecting
        assert_eq!(calls.lock().unwrap().len(), 0);

        // 放行 connect → 线程 A 完成连接后必须发送最新快照（"New"）
        release_tx.send(()).unwrap();
        done_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("first publish did not finish within 5s");
        let recorded = calls.lock().unwrap().clone();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].as_ref().unwrap()["details"], "New");
    }

    /// 手动冒烟：`FOLIA_SMOKE=1 cargo test` 时真实探测本地 Discord IPC 管道。
    /// 无 Discord 时必须诚实降级（Err + 明确错误信息），不可假成功。
    #[test]
    #[cfg(windows)]
    fn smoke_discord_pipe_availability() {
        if std::env::var("FOLIA_SMOKE").as_deref() != Ok("1") {
            eprintln!("SMOKE(discord): skipped (set FOLIA_SMOKE=1 to run)");
            return;
        }
        match IpcConnection::open(DEFAULT_DISCORD_APPLICATION_ID, Arc::new(|_| {})) {
            Ok(_conn) => eprintln!("SMOKE(discord): connected to Discord IPC"),
            Err(error) => eprintln!("SMOKE(discord): honest degradation -> {error}"),
        }
    }
}

// -- Windows 真实命名管道 reader 测试 -----------------------------------------
// 用本地 CreateNamedPipeW 服务器模拟 Discord IPC，确定性验证 reader 的首帧
// 严格性、握手超时取消、销毁取消（不永久 detached）以及握手后 ERROR 上报。

#[cfg(all(test, windows))]
mod pipe_reader_tests {
    use super::*;
    use std::os::windows::io::FromRawHandle;
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::Storage::FileSystem::FILE_FLAGS_AND_ATTRIBUTES;
    use windows::Win32::System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, NAMED_PIPE_MODE, PIPE_UNLIMITED_INSTANCES,
    };

    static PIPE_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    fn wide(value: &str) -> Vec<u16> {
        value.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// 独立 byte-mode 双工命名管道服务器，模拟 Discord 的 IPC 服务端。
    struct TestPipeServer {
        handle: HANDLE,
        client_path: String,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    impl TestPipeServer {
        fn create() -> Self {
            let id = PIPE_COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let server_name = format!(r"\\.\pipe\folia-test-{}-{id}", std::process::id());
            let name_wide = wide(&server_name);
            let handle = unsafe {
                CreateNamedPipeW(
                    PCWSTR(name_wide.as_ptr()),
                    FILE_FLAGS_AND_ATTRIBUTES(0x3), // PIPE_ACCESS_DUPLEX
                    NAMED_PIPE_MODE(0), // PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT
                    PIPE_UNLIMITED_INSTANCES,
                    64 * 1024,
                    64 * 1024,
                    0,
                    None,
                )
            };
            assert!(!handle.is_invalid(), "CreateNamedPipeW failed");
            // 客户端连接路径与 server 名称使用同一管道（\\?\pipe\ 与 \\.\pipe\ 等价）。
            let client_path = format!(r"\\?\pipe\folia-test-{}-{id}", std::process::id());
            Self {
                handle,
                client_path,
                thread: None,
            }
        }

        fn client_path(&self) -> &str {
            &self.client_path
        }

        /// 后台线程接受客户端连接，随后读取并丢弃客户端的 HANDSHAKE 帧，
        /// 再执行 `after_connect(server_file)`。
        fn serve(&mut self, after_connect: impl FnOnce(std::fs::File) + Send + 'static) {
            // HANDLE 是 *mut c_void 包装，非 Send：按 usize 传递跨线程。
            let raw = self.handle.0 as usize;
            // 句柄所有权移交给服务线程的 File；struct 侧置空避免重复 CloseHandle。
            self.handle = HANDLE(std::ptr::null_mut());
            self.thread = Some(std::thread::spawn(move || {
                let handle = HANDLE(raw as *mut core::ffi::c_void);
                // 客户端可能先连接（ERROR_PIPE_CONNECTED）或等待服务端 accept，均视为成功。
                let _ = unsafe { ConnectNamedPipe(handle, None) };
                let mut file = unsafe { std::fs::File::from_raw_handle(handle.0) };
                // 等客户端写完 HANDSHAKE 再继续：避免服务端先关导致客户端写失败（os error 232）。
                {
                    use std::io::Read;
                    let mut header = [0u8; 8];
                    let _ = file.read_exact(&mut header);
                    let len =
                        u32::from_le_bytes([header[4], header[5], header[6], header[7]]) as usize;
                    let mut payload = vec![0u8; len];
                    let _ = file.read_exact(&mut payload);
                }
                after_connect(file);
            }));
        }

        fn join(&mut self) {
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
            // 句柄所有权已移交给 serve 线程的 File（无 serve 时兜底关闭）。
            if !self.handle.is_invalid() {
                let _ = unsafe { CloseHandle(self.handle) };
                self.handle = HANDLE(std::ptr::null_mut());
            }
        }
    }

    impl Drop for TestPipeServer {
        fn drop(&mut self) {
            self.join();
        }
    }

    fn write_frame_raw(file: &mut std::fs::File, op: u32, payload: &[u8]) -> std::io::Result<()> {
        use std::io::Write;
        file.write_all(&op.to_le_bytes())?;
        file.write_all(&(payload.len() as u32).to_le_bytes())?;
        file.write_all(payload)?;
        Ok(())
    }

    fn frame_payload(value: &Value) -> Vec<u8> {
        serde_json::to_vec(value).unwrap()
    }

    fn noop_disconnect() -> Arc<dyn Fn(Option<String>) + Send + Sync> {
        Arc::new(|_| {})
    }

    /// 首 OP_FRAME 必须严格 evt READY：意外事件被拒绝。
    #[test]
    fn first_frame_must_be_ready() {
        let mut server = TestPipeServer::create();
        server.serve(|mut file| {
            write_frame_raw(
                &mut file,
                OP_FRAME,
                &frame_payload(&json!({ "cmd": "DISPATCH", "evt": "ACTIVITY_JOIN" })),
            )
            .unwrap();
        });
        let result = IpcConnection::open_at(
            DEFAULT_DISCORD_APPLICATION_ID,
            server.client_path(),
            std::time::Duration::from_secs(3),
            noop_disconnect(),
        );
        match result {
            Err(error) => assert!(
                error.contains("unexpected first event"),
                "unexpected error: {error}"
            ),
            Ok(_) => panic!("non-READY first frame must be rejected"),
        }
        server.join();
    }

    /// 首帧为 ERROR → 握手失败并携带真实错误消息。
    #[test]
    fn first_frame_error_is_reported() {
        let mut server = TestPipeServer::create();
        server.serve(|mut file| {
            write_frame_raw(
                &mut file,
                OP_FRAME,
                &frame_payload(&json!({ "evt": "ERROR", "data": { "message": "bad app id" } })),
            )
            .unwrap();
        });
        let result = IpcConnection::open_at(
            DEFAULT_DISCORD_APPLICATION_ID,
            server.client_path(),
            std::time::Duration::from_secs(3),
            noop_disconnect(),
        );
        match result {
            Err(error) => assert!(
                error.contains("Discord handshake failed: bad app id"),
                "unexpected error: {error}"
            ),
            Ok(_) => panic!("ERROR first frame must fail the handshake"),
        }
        server.join();
    }

    /// 首帧畸形 JSON → 握手失败。
    #[test]
    fn first_frame_malformed_json_is_rejected() {
        let mut server = TestPipeServer::create();
        server.serve(|mut file| {
            write_frame_raw(&mut file, OP_FRAME, b"not-json").unwrap();
        });
        let result = IpcConnection::open_at(
            DEFAULT_DISCORD_APPLICATION_ID,
            server.client_path(),
            std::time::Duration::from_secs(3),
            noop_disconnect(),
        );
        match result {
            Err(error) => assert!(
                error.contains("malformed handshake response"),
                "unexpected error: {error}"
            ),
            Ok(_) => panic!("malformed first frame must fail the handshake"),
        }
        server.join();
    }

    /// 握手超时：reader 被取消并 join，open 返回明确的超时错误（不悬挂）。
    #[test]
    fn handshake_timeout_cancels_reader_and_errors() {
        let mut server = TestPipeServer::create();
        server.serve(|_file| {
            // 保持服务器打开但永不写入：客户端将阻塞等待首帧直至超时。
            std::thread::sleep(std::time::Duration::from_secs(2));
        });
        let result = IpcConnection::open_at(
            DEFAULT_DISCORD_APPLICATION_ID,
            server.client_path(),
            std::time::Duration::from_millis(300),
            noop_disconnect(),
        );
        match result {
            Err(error) => assert!(
                error.contains("Discord handshake timed out"),
                "unexpected error: {error}"
            ),
            Ok(_) => panic!("silent server must time out"),
        }
        server.join();
    }

    /// 销毁/禁用路径：reader 阻塞在静默管道上，drop 必须取消并 join，
    /// 且退出后触发 on_disconnect —— 不允许永久 detached。
    #[test]
    fn destroy_cancels_blocked_reader_and_fires_disconnect() {
        let (keep_tx, keep_rx) = std::sync::mpsc::channel::<()>();
        let mut server = TestPipeServer::create();
        server.serve(move |mut file| {
            write_frame_raw(
                &mut file,
                OP_FRAME,
                &frame_payload(&json!({ "evt": "READY" })),
            )
            .unwrap();
            // 保持服务器端打开：此后管道静默，reader 将阻塞在 read_exact。
            let _ = keep_rx.recv();
        });

        let (disconnect_tx, disconnect_rx) = std::sync::mpsc::channel::<Option<String>>();
        let on_disconnect: Arc<dyn Fn(Option<String>) + Send + Sync> = Arc::new(move |reason| {
            let _ = disconnect_tx.send(reason);
        });
        let conn = IpcConnection::open_at(
            DEFAULT_DISCORD_APPLICATION_ID,
            server.client_path(),
            std::time::Duration::from_secs(3),
            on_disconnect,
        )
        .expect("READY handshake should succeed");
        // 注意：不在 reader pending 时写管道（Windows 字节管道会阻塞），
        // 直接验证销毁路径对阻塞 reader 的取消。

        // 在独立线程 drop（内部 join reader）：若 reader 无法被打断，join 将永久阻塞。
        let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
        std::thread::spawn(move || {
            drop(conn);
            let _ = done_tx.send(());
        });
        done_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("reader was not cancelled within 2s after drop");

        // reader 退出时应触发 on_disconnect（管道被我们主动取消关闭 → None）。
        match disconnect_rx.recv_timeout(std::time::Duration::from_secs(1)) {
            Ok(reason) => assert_eq!(reason, None),
            Err(_) => panic!("reader did not fire on_disconnect after being cancelled"),
        }
        drop(keep_tx);
        server.join();
    }

    /// 握手成功后收到 evt ERROR → reader 断开并携带真实错误消息。
    #[test]
    fn post_handshake_error_disconnects_with_message() {
        let mut server = TestPipeServer::create();
        server.serve(|mut file| {
            write_frame_raw(
                &mut file,
                OP_FRAME,
                &frame_payload(&json!({ "evt": "READY" })),
            )
            .unwrap();
            write_frame_raw(
                &mut file,
                OP_FRAME,
                &frame_payload(
                    &json!({ "evt": "ERROR", "data": { "message": "Invalid activity" } }),
                ),
            )
            .unwrap();
            // 保持服务器打开，等待客户端处理完帧后退出。
            std::thread::sleep(std::time::Duration::from_secs(1));
        });

        let (disconnect_tx, disconnect_rx) = std::sync::mpsc::channel::<Option<String>>();
        let on_disconnect: Arc<dyn Fn(Option<String>) + Send + Sync> = Arc::new(move |reason| {
            let _ = disconnect_tx.send(reason);
        });
        let conn = IpcConnection::open_at(
            DEFAULT_DISCORD_APPLICATION_ID,
            server.client_path(),
            std::time::Duration::from_secs(3),
            on_disconnect,
        )
        .expect("READY handshake should succeed");

        // 帧按序送达：READY 后到达的 ERROR 走握手后分支 → on_disconnect(Some(msg))。
        match disconnect_rx.recv_timeout(std::time::Duration::from_secs(2)) {
            Ok(Some(message)) => assert_eq!(message, "Invalid activity"),
            other => panic!("expected post-handshake error disconnect, got {other:?}"),
        }
        drop(conn);
        server.join();
    }

    /// 服务端主动关闭管道（OP_CLOSE 或断线）→ reader 退出并触发 on_disconnect。
    #[test]
    fn server_close_fires_disconnect() {
        let mut server = TestPipeServer::create();
        server.serve(|mut file| {
            write_frame_raw(
                &mut file,
                OP_FRAME,
                &frame_payload(&json!({ "evt": "READY" })),
            )
            .unwrap();
            std::thread::sleep(std::time::Duration::from_millis(100));
            // file 在此 drop：服务器端句柄关闭 → 客户端读取失败。
        });

        let (disconnect_tx, disconnect_rx) = std::sync::mpsc::channel::<Option<String>>();
        let on_disconnect: Arc<dyn Fn(Option<String>) + Send + Sync> = Arc::new(move |reason| {
            let _ = disconnect_tx.send(reason);
        });
        let conn = IpcConnection::open_at(
            DEFAULT_DISCORD_APPLICATION_ID,
            server.client_path(),
            std::time::Duration::from_secs(3),
            on_disconnect,
        )
        .expect("READY handshake should succeed");

        match disconnect_rx.recv_timeout(std::time::Duration::from_secs(2)) {
            Ok(reason) => assert_eq!(reason, None, "pipe close carries no error message"),
            Err(_) => panic!("reader did not detect server-side close"),
        }
        drop(conn);
        server.join();
    }
}
