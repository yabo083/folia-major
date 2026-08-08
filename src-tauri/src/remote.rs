//! M7 remote control: the `Folia Remote` companion WebviewWindow and the
//! snapshot / command event bridge between it and the main window.
//!
//! Behavior mirrors `folia-major/electron/main.cjs` remote-control section.
//! The remote window loads the same frontend with `?remote=1`; the renderer
//! pulls snapshots via `remote-control-get-snapshot`, receives live updates on
//! `remote-control-snapshot`, and sends commands via `remote-control-send-command`
//! (forwarded to the main window on `remote-control-command`).

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use serde_json::{json, Value};
use tauri::{
    window::Color, AppHandle, Emitter, Manager, PhysicalSize, WebviewUrl, WebviewWindowBuilder,
};

use crate::handoff::WindowPlaybackHandoffStore;
use crate::settings::SettingsStore;
use crate::window::MainWindowState;

pub const REMOTE_WINDOW_LABEL: &str = "remote";
const REMOTE_WINDOW_TITLE: &str = "Folia Remote";
const WINDOW_PLAYBACK_HANDOFF_REQUEST_TIMEOUT_MS: u128 = 800;

/// Latest remote-control snapshot (managed state).
pub struct RemoteControlState {
    latest_snapshot: Mutex<Option<Value>>,
}

impl RemoteControlState {
    pub fn new() -> Self {
        Self {
            latest_snapshot: Mutex::new(None),
        }
    }
}

impl Default for RemoteControlState {
    fn default() -> Self {
        Self::new()
    }
}

fn remote_window(app: &AppHandle) -> Option<tauri::WebviewWindow> {
    app.get_webview_window(REMOTE_WINDOW_LABEL)
}

fn remote_url(app: &AppHandle) -> WebviewUrl {
    if cfg!(debug_assertions) {
        let dev = app
            .config()
            .build
            .dev_url
            .clone()
            .unwrap_or_else(|| url::Url::parse("http://localhost:3000").unwrap());
        let mut url = dev;
        url.query_pairs_mut().append_pair("remote", "1");
        WebviewUrl::External(url)
    } else {
        WebviewUrl::App(PathBuf::from("index.html?remote=1"))
    }
}

fn apply_always_on_top(app: &AppHandle) {
    if let Some(window) = remote_window(app) {
        let _ = window.set_always_on_top(
            app.state::<SettingsStore>()
                .get_bool(crate::settings::REMOTE_CONTROL_ALWAYS_ON_TOP),
        );
    }
}

fn apply_skip_taskbar(app: &AppHandle) {
    if let Some(window) = remote_window(app) {
        let _ = window.set_skip_taskbar(
            app.state::<SettingsStore>()
                .get_bool(crate::settings::REMOTE_CONTROL_SKIP_TASKBAR),
        );
    }
}

/// Apply always-on-top / skip-taskbar settings to the remote window (hooked
/// into `save_settings` so a settings change takes effect immediately).
pub fn apply_remote_window_settings(app: &AppHandle) {
    apply_always_on_top(app);
    apply_skip_taskbar(app);
}

/// Broadcast `playback-sync-bridge-status-changed` to the main window
/// (remote-control open state + Discord presence toggle state).
pub fn broadcast_playback_sync_bridge_status(app: &AppHandle) {
    let payload = json!({
        "remoteControlOpen": remote_window(app).is_some(),
        "discordPresenceEnabled": app
            .state::<SettingsStore>()
            .get_bool(crate::settings::DISCORD_RICH_PRESENCE_ENABLED),
    });
    let _ = app.emit_to("main", "playback-sync-bridge-status-changed", payload);
}

fn create_remote_window(app: &AppHandle) -> Result<(), String> {
    if let Some(window) = remote_window(app) {
        let _ = window.set_title(REMOTE_WINDOW_TITLE);
        apply_always_on_top(app);
        let _ = window.show();
        let _ = window.set_focus();
        broadcast_playback_sync_bridge_status(app);
        return Ok(());
    }

    let settings = app.state::<SettingsStore>();
    let always_on_top = settings.get_bool(crate::settings::REMOTE_CONTROL_ALWAYS_ON_TOP);
    let skip_taskbar = settings.get_bool(crate::settings::REMOTE_CONTROL_SKIP_TASKBAR);
    // Mirror the Electron remote window (`transparent: true, hasShadow: false,
    // backgroundColor: '#00000000'`). `shadow(false)` keeps tao from drawing an
    // undecorated shadow with hit-test insets around a fully transparent window on
    // Windows (which can leave the card visually offset and steal hit-testing);
    // the explicit alpha-0 background avoids a white flash before first paint.
    let window = WebviewWindowBuilder::new(app, REMOTE_WINDOW_LABEL, remote_url(app))
        .title(REMOTE_WINDOW_TITLE)
        .inner_size(450.0, 230.0)
        .min_inner_size(450.0, 230.0)
        .max_inner_size(450.0, 230.0)
        .resizable(false)
        .decorations(false)
        .transparent(true)
        .shadow(false)
        .background_color(Color(0, 0, 0, 0))
        .always_on_top(always_on_top)
        .skip_taskbar(skip_taskbar)
        .build()
        .map_err(|error| format!("create remote control window: {error}"))?;
    crate::window::disable_native_window_corners(&window)?;
    // The remote window must always receive pointer input; explicitly clear any
    // residual click-through state (Windows: WS_EX_TRANSPARENT) so the controls
    // are never silently ignored.
    if let Err(error) = window.set_ignore_cursor_events(false) {
        let _ = window.close();
        return Err(format!(
            "enable pointer input for remote control window: {error}"
        ));
    }
    let handle = app.clone();
    window.on_window_event(move |event| {
        if matches!(event, tauri::WindowEvent::Destroyed) {
            broadcast_playback_sync_bridge_status(&handle);
        }
    });
    let _ = window.show();
    let _ = window.set_focus();
    broadcast_playback_sync_bridge_status(app);
    Ok(())
}

// -- snapshot bridge ---------------------------------------------------------

/// Merge an incoming snapshot like `remote-control-publish-snapshot`:
/// preserve the previous lyrics when the payload omits them, and stamp the
/// main-window click-through / always-on-top flags.
fn merge_remote_snapshot(
    current: Option<&Value>,
    incoming: Value,
    click_through_enabled: bool,
    main_window_always_on_top: bool,
) -> Value {
    let has_lyrics = incoming.get("lyrics").is_some();
    let mut merged = incoming;
    if !has_lyrics {
        if let Some(lyrics) = current
            .and_then(|c| c.get("lyrics"))
            .cloned()
            .filter(|v| !v.is_null())
        {
            merged["lyrics"] = lyrics;
        }
    }
    merged["mainWindowClickThroughEnabled"] = json!(click_through_enabled);
    merged["mainWindowAlwaysOnTop"] = json!(main_window_always_on_top);
    merged
}

fn sanitize_video_export_size(width: f64, height: f64) -> Option<(u32, u32)> {
    let width = width.round();
    let height = height.round();
    if !width.is_finite() || !height.is_finite() || width < 320.0 || height < 320.0 {
        return None;
    }
    Some((width.min(3840.0) as u32, height.min(3840.0) as u32))
}

// ---------------------------------------------------------------------------
// Tauri commands (snake_case, shim-invoked)
// ---------------------------------------------------------------------------

#[tauri::command]
// 打开（或聚焦）远程控制窗口并广播 playback-sync-bridge 状态。
pub async fn remote_control_open(app: AppHandle) -> Result<bool, String> {
    create_remote_window(&app)?;
    Ok(true)
}

#[tauri::command]
// 切换远程控制窗口的开关状态，返回开关后的打开状态。
pub async fn remote_control_toggle(app: AppHandle) -> Result<bool, String> {
    if remote_window(&app).is_some() {
        if let Some(window) = remote_window(&app) {
            let _ = window.close();
        }
        Ok(false)
    } else {
        create_remote_window(&app)?;
        Ok(true)
    }
}

#[tauri::command]
// 关闭远程控制窗口，返回是否确实关闭了窗口。
pub fn remote_control_close(app: AppHandle) -> Result<bool, String> {
    if let Some(window) = remote_window(&app) {
        let _ = window.close();
        Ok(true)
    } else {
        Ok(false)
    }
}

#[tauri::command]
// 读取远程控制窗口的 always-on-top 设置。
pub fn remote_control_get_always_on_top(app: AppHandle) -> Result<bool, String> {
    Ok(app
        .state::<SettingsStore>()
        .get_bool(crate::settings::REMOTE_CONTROL_ALWAYS_ON_TOP))
}

#[tauri::command]
// 持久化并应用远程控制窗口的 always-on-top 设置。
pub fn remote_control_set_always_on_top(
    app: AppHandle,
    always_on_top: bool,
) -> Result<bool, String> {
    app.state::<SettingsStore>().set(
        crate::settings::REMOTE_CONTROL_ALWAYS_ON_TOP.to_string(),
        json!(always_on_top),
    )?;
    apply_always_on_top(&app);
    Ok(always_on_top)
}

#[tauri::command]
// 主窗口发布远程控制快照：合并 lyrics 保留逻辑并转发给远程窗口。
pub fn remote_control_publish_snapshot(
    app: AppHandle,
    snapshot: Value,
    state: tauri::State<'_, RemoteControlState>,
) -> Result<bool, String> {
    let current = state.latest_snapshot.lock().unwrap().clone();
    if snapshot.is_null() {
        *state.latest_snapshot.lock().unwrap() = None;
        return Ok(true);
    }
    let click_through = app.state::<MainWindowState>().click_through_enabled();
    let main_window_always_on_top = app
        .state::<SettingsStore>()
        .get_bool(crate::settings::MAIN_WINDOW_ALWAYS_ON_TOP);
    let merged = merge_remote_snapshot(
        current.as_ref(),
        snapshot,
        click_through,
        main_window_always_on_top,
    );
    *state.latest_snapshot.lock().unwrap() = Some(merged.clone());
    let _ = app.emit_to(REMOTE_WINDOW_LABEL, "remote-control-snapshot", merged);
    Ok(true)
}

#[tauri::command]
// 读取最近一次远程控制快照（供远程窗口挂载时拉取）。
pub fn remote_control_get_snapshot(
    state: tauri::State<'_, RemoteControlState>,
) -> Result<Option<Value>, String> {
    Ok(state.latest_snapshot.lock().unwrap().clone())
}

#[tauri::command]
// 远程窗口发送控制命令：处理窗口级命令，其余转发给主窗口渲染进程。
pub async fn remote_control_send_command(app: AppHandle, command: Value) -> Result<bool, String> {
    let command_type = command.get("type").and_then(Value::as_str).unwrap_or("");
    match command_type {
        "set-main-window-click-through" => {
            let enabled = command
                .get("enabled")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            crate::window::window_set_click_through(app, enabled)
        }
        "set-main-window-always-on-top" => {
            let enabled = command
                .get("enabled")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            crate::window::window_set_always_on_top(app, enabled)
        }
        "set-transparent-mode-enabled" | "disable-transparent-mode" => {
            let enabled = command_type == "set-transparent-mode-enabled"
                && command
                    .get("enabled")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
            set_transparent_mode_from_remote(app, enabled).await
        }
        "resize-main-window" => resize_main_window(&app, &command),
        _ => {
            let Some(_main) = app.get_webview_window(crate::window::MAIN_WINDOW_LABEL) else {
                eprintln!(
                    "[remote] cannot deliver command `{command_type}`: main window is not open"
                );
                return Ok(false);
            };
            let _ = app.emit_to("main", "remote-control-command", command);
            Ok(true)
        }
    }
}

#[tauri::command]
// 返回 playback-sync-bridge 状态（远程窗口是否打开 + Discord 开关）。
pub fn playback_sync_bridge_get_status(app: AppHandle) -> Result<Value, String> {
    Ok(json!({
        "remoteControlOpen": remote_window(&app).is_some(),
        "discordPresenceEnabled": app
            .state::<SettingsStore>()
            .get_bool(crate::settings::DISCORD_RICH_PRESENCE_ENABLED),
    }))
}

// -- helpers -----------------------------------------------------------------

async fn set_transparent_mode_from_remote(app: AppHandle, enabled: bool) -> Result<bool, String> {
    // Request a playback handoff from the main window renderer (mirrors
    // `setMainWindowTransparentModeFromRemote`), then rebuild the main window.
    let store = app.state::<WindowPlaybackHandoffStore>();
    let request_id = format!(
        "remote-handoff-{}-{}",
        now_ms(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0)
    );
    let rx = store.register(
        request_id.clone(),
        WINDOW_PLAYBACK_HANDOFF_REQUEST_TIMEOUT_MS,
    );
    let _ = app.emit_to(
        "main",
        "window-playback-handoff-requested",
        json!({ "requestId": request_id }),
    );
    let recv = tokio::task::spawn_blocking(move || rx.recv());
    let handoff = tokio::time::timeout(
        Duration::from_millis(WINDOW_PLAYBACK_HANDOFF_REQUEST_TIMEOUT_MS as u64 + 50),
        recv,
    )
    .await
    .ok()
    .and_then(|join| join.ok())
    .and_then(|result| result.ok())
    .filter(|v| !v.is_null());
    let _ = app.state::<WindowPlaybackHandoffStore>().clear_expired();
    crate::window::window_set_transparent_mode(app, enabled, handoff)
}

fn resize_main_window(app: &AppHandle, command: &Value) -> Result<bool, String> {
    let Some(main) = app.get_webview_window(crate::window::MAIN_WINDOW_LABEL) else {
        return Ok(false);
    };
    let width = command.get("width").and_then(Value::as_f64);
    let height = command.get("height").and_then(Value::as_f64);
    let (Some(width), Some(height)) = (width, height) else {
        return Ok(false);
    };
    let Some((width, height)) = sanitize_video_export_size(width, height) else {
        return Ok(false);
    };
    if main.is_fullscreen().unwrap_or(false) {
        let _ = main.set_fullscreen(false);
    }
    if main.is_maximized().unwrap_or(false) {
        let _ = main.unmaximize();
    }
    let _ = main.set_size(PhysicalSize::new(width, height));
    let _ = main.center();
    let _ = main.set_focus();
    Ok(true)
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_snapshot_preserves_lyrics_from_previous() {
        let current = json!({
            "title": "Old",
            "lyrics": { "lines": [{ "text": "hello" }] },
        });
        let incoming = json!({
            "title": "New",
            "currentTime": 5,
        });
        let merged = merge_remote_snapshot(Some(&current), incoming, true, false);
        assert_eq!(merged["title"], "New");
        assert_eq!(merged["lyrics"]["lines"][0]["text"], "hello");
        assert_eq!(merged["mainWindowClickThroughEnabled"], true);
        assert_eq!(merged["mainWindowAlwaysOnTop"], false);
    }

    #[test]
    fn merge_snapshot_uses_incoming_lyrics_when_present() {
        let current = json!({ "lyrics": { "lines": [] } });
        let incoming = json!({ "lyrics": { "lines": [{ "text": "new" }] } });
        let merged = merge_remote_snapshot(Some(&current), incoming, false, true);
        assert_eq!(merged["lyrics"]["lines"][0]["text"], "new");
        assert_eq!(merged["mainWindowClickThroughEnabled"], false);
        assert_eq!(merged["mainWindowAlwaysOnTop"], true);
    }

    #[test]
    fn merge_snapshot_without_previous_keeps_no_lyrics() {
        let incoming = json!({ "title": "T" });
        let merged = merge_remote_snapshot(None, incoming, false, false);
        assert!(merged.get("lyrics").is_none());
    }

    #[test]
    fn sanitize_video_export_size_clamps_and_rejects() {
        assert_eq!(
            sanitize_video_export_size(1920.0, 1080.0),
            Some((1920, 1080))
        );
        assert_eq!(
            sanitize_video_export_size(5000.0, 5000.0),
            Some((3840, 3840))
        );
        assert_eq!(sanitize_video_export_size(100.0, 100.0), None);
        assert_eq!(
            sanitize_video_export_size(1920.4, 1080.6),
            Some((1920, 1081))
        );
        assert_eq!(sanitize_video_export_size(f64::NAN, 1080.0), None);
    }
}
