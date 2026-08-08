// src-tauri/src/window.rs
// M2 窗口与系统集成：窗口控制/置顶/点击穿透/透明模式重建/handoff 命令/托盘/单实例聚焦/窗口状态持久化。
// 行为对齐 electron/main.cjs 的窗口段与 windowPlaybackHandoff.cjs。

use serde_json::{json, Value};
use std::sync::Mutex;
use std::time::{Duration, Instant};
#[cfg(desktop)]
use tauri::menu::{Menu, MenuItem};
#[cfg(desktop)]
use tauri::tray::{TrayIconBuilder, TrayIconEvent};
use tauri::{
    AppHandle, Emitter, Manager, PhysicalPosition, PhysicalSize, WebviewUrl, WebviewWindowBuilder,
};

use crate::handoff::WindowPlaybackHandoffStore;
use crate::settings::SettingsStore;

pub const MAIN_WINDOW_LABEL: &str = "main";
const TRANSPARENT_PLAYER_BACKGROUND: &str = "TRANSPARENT_PLAYER_BACKGROUND";
const MAIN_WINDOW_ALWAYS_ON_TOP: &str = "MAIN_WINDOW_ALWAYS_ON_TOP";
const HIDE_TASKBAR_ICON: &str = "HIDE_TASKBAR_ICON";
const MINIMIZE_TO_TRAY: &str = "MINIMIZE_TO_TRAY";
const WINDOW_BOUNDS: &str = "WINDOW_BOUNDS";
const WINDOW_IS_MAXIMIZED: &str = "WINDOW_IS_MAXIMIZED";
const NATIVE_THEME_SOURCE: &str = "NATIVE_THEME_SOURCE";

const CLICK_THROUGH_HOTSPOT_RIGHT_INSET: i32 = 176;
const CLICK_THROUGH_HOTSPOT_WIDTH: i32 = 48;
const CLICK_THROUGH_HOTSPOT_HEIGHT: i32 = 40;
const CLICK_THROUGH_HOTSPOT_TOP_INSET: i32 = 4;
const CLICK_THROUGH_MONITOR_INTERVAL_MS: u64 = 50;
const WINDOW_STATE_SAVE_DEBOUNCE_MS: u64 = 300;

#[cfg(windows)]
pub(crate) fn disable_native_window_corners(window: &tauri::WebviewWindow) -> Result<(), String> {
    use std::ffi::c_void;
    use windows::Win32::Foundation::COLORREF;
    use windows::Win32::Graphics::Dwm::{
        DwmSetWindowAttribute, DWMWA_BORDER_COLOR, DWMWA_WINDOW_CORNER_PREFERENCE,
    };

    let hwnd = window
        .hwnd()
        .map_err(|error| format!("resolve window hwnd for transparent styling: {error}"))?;
    let corner_preference: u32 = 1; // DWMWCP_DONOTROUND
    let border_color = COLORREF(0);
    unsafe {
        DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            &corner_preference as *const u32 as *const c_void,
            std::mem::size_of_val(&corner_preference) as u32,
        )
        .map_err(|error| format!("disable native window corners: {error}"))?;
        DwmSetWindowAttribute(
            hwnd,
            DWMWA_BORDER_COLOR,
            &border_color as *const COLORREF as *const c_void,
            std::mem::size_of_val(&border_color) as u32,
        )
        .map_err(|error| format!("clear native window border: {error}"))?;
    }
    Ok(())
}

#[cfg(not(windows))]
pub(crate) fn disable_native_window_corners(_window: &tauri::WebviewWindow) -> Result<(), String> {
    Ok(())
}

#[cfg(windows)]
const WS_EX_LAYERED: isize = 0x0008_0000;
#[cfg(windows)]
const WS_EX_TRANSPARENT: isize = 0x0000_0020;
#[cfg(windows)]
const WS_EX_TOOLWINDOW: isize = 0x0000_0080;

// 主窗口侧状态（点击穿透、窗口状态保存节流）。
pub struct MainWindowState {
    inner: Mutex<ClickThroughState>,
}

#[derive(Default)]
struct ClickThroughState {
    enabled: bool,
    unlock_hover: bool,
    last_window_state_save: Option<Instant>,
}

impl MainWindowState {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(ClickThroughState::default()),
        }
    }

    // 当前点击穿透开关状态（M7 远程控制快照使用）。
    pub fn click_through_enabled(&self) -> bool {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .enabled
    }
}

#[cfg(windows)]
// 计算下一组窗口扩展样式：穿透开启且未解锁时并入 WS_EX_LAYERED|WS_EX_TRANSPARENT，
// 关闭/解锁时清除 TRANSPARENT（保留 layered）；隐藏任务栏时切换 WS_EX_TOOLWINDOW。
fn ex_style_toggle(
    base: isize,
    click_through: bool,
    unlock_hover: bool,
    hide_taskbar: bool,
) -> isize {
    let mut style = base;
    if click_through && !unlock_hover {
        style |= WS_EX_LAYERED | WS_EX_TRANSPARENT;
    } else {
        style &= !WS_EX_TRANSPARENT;
    }
    if hide_taskbar {
        style |= WS_EX_TOOLWINDOW;
    } else {
        style &= !WS_EX_TOOLWINDOW;
    }
    style
}

// 计算穿透解锁热点矩形（窗口局部坐标，镜像 MAIN_WINDOW_CLICK_THROUGH_UNLOCK_HOTSPOT）。
fn hotspot_bounds(window_width: i32, scale_factor: f64) -> (i32, i32, i32, i32) {
    let scaled = |value: i32| (f64::from(value) * scale_factor).round() as i32;
    let right = window_width - scaled(CLICK_THROUGH_HOTSPOT_RIGHT_INSET);
    let left = right - scaled(CLICK_THROUGH_HOTSPOT_WIDTH);
    let top = scaled(CLICK_THROUGH_HOTSPOT_TOP_INSET);
    let bottom = top + scaled(CLICK_THROUGH_HOTSPOT_HEIGHT);
    (left, top, right, bottom)
}

// 判断光标是否落在解锁热点内。
fn cursor_in_hotspot(x: i32, y: i32, left: i32, top: i32, right: i32, bottom: i32) -> bool {
    x >= left && x <= right && y >= top && y <= bottom
}

#[cfg(windows)]
// 通过 SetWindowLongPtrW 应用主窗口扩展样式（点击穿透/隐藏任务栏）。
fn apply_main_window_ex_style(
    window: &tauri::WebviewWindow,
    enabled: bool,
    unlock_hover: bool,
    hide_taskbar: bool,
) -> Result<(), String> {
    use windows::Win32::UI::WindowsAndMessaging::{
        GetWindowLongPtrW, SetWindowLongPtrW, SetWindowPos, GWL_EXSTYLE, SWP_FRAMECHANGED,
        SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOOWNERZORDER, SWP_NOSIZE,
    };

    let hwnd = window
        .hwnd()
        .map_err(|error| format!("resolve main window hwnd: {error}"))?;
    let current = unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) };
    let next = ex_style_toggle(current, enabled, unlock_hover, hide_taskbar);
    let result = unsafe { SetWindowLongPtrW(hwnd, GWL_EXSTYLE, next) };
    if result == 0 {
        return Err("set window ex-style returned 0".to_string());
    }
    // Force Windows to apply the extended-style change immediately. Without
    // a non-client refresh, WS_EX_TRANSPARENT can remain stale until resize.
    unsafe {
        SetWindowPos(
            hwnd,
            None,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOOWNERZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
        )
        .map_err(|error| format!("refresh main window ex-style: {error}"))?;
    }
    Ok(())
}

#[cfg(not(windows))]
fn apply_main_window_ex_style(
    _window: &tauri::WebviewWindow,
    _enabled: bool,
    _unlock_hover: bool,
    _hide_taskbar: bool,
) -> Result<(), String> {
    Ok(())
}

fn publish_click_through_state(app: &AppHandle) {
    let state = app.state::<MainWindowState>();
    let inner = state
        .inner
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let payload = json!({ "enabled": inner.enabled, "unlockHoverActive": inner.unlock_hover });
    drop(inner);
    let _ = app.emit("main-window-click-through-changed", payload);
}

#[cfg(desktop)]
fn start_click_through_monitor(app: AppHandle) {
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_millis(CLICK_THROUGH_MONITOR_INTERVAL_MS));
        tick_click_through_monitor(&app);
    });
}

#[cfg(windows)]
// 光标轮询：穿透开启时检测解锁热点并切换 WS_EX_TRANSPARENT（替代 Electron 的 forward:true 转发）。
fn tick_click_through_monitor(app: &AppHandle) {
    use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_LBUTTON};
    use windows::Win32::UI::WindowsAndMessaging::{GetCursorPos, GetWindowRect};

    let state = app.state::<MainWindowState>();
    if !state
        .inner
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .enabled
    {
        return;
    }
    let window = match app.get_webview_window(MAIN_WINDOW_LABEL) {
        Some(window) => window,
        None => return,
    };
    let hwnd = match window.hwnd() {
        Ok(hwnd) => hwnd,
        Err(_) => return,
    };
    let mut cursor = windows::Win32::Foundation::POINT::default();
    if unsafe { GetCursorPos(&mut cursor) }.is_err() {
        return;
    }
    let mut rect = windows::Win32::Foundation::RECT::default();
    if unsafe { GetWindowRect(hwnd, &mut rect) }.is_err() {
        return;
    }
    let window_width = rect.right - rect.left;
    let scale_factor = window.scale_factor().unwrap_or(1.0);
    let (left, top, right, bottom) = hotspot_bounds(window_width, scale_factor);
    let within_window = cursor.x >= rect.left
        && cursor.x <= rect.right
        && cursor.y >= rect.top
        && cursor.y <= rect.bottom;
    let within_hotspot = within_window
        && cursor_in_hotspot(
            cursor.x - rect.left,
            cursor.y - rect.top,
            left,
            top,
            right,
            bottom,
        );

    // WS_EX_TRANSPARENT prevents the WebView from receiving the click that should
    // unlock it. Handle the button press at the native layer while the pointer is
    // in the scaled hotspot, so the recovery action does not depend on DOM events.
    let left_button_down = unsafe { GetAsyncKeyState(VK_LBUTTON.0 as i32) } < 0;
    if within_hotspot && left_button_down {
        let _ = window_set_click_through(app.clone(), false);
        return;
    }

    let mut inner = state.inner.lock().unwrap_or_else(|p| p.into_inner());
    if inner.unlock_hover == within_hotspot {
        return;
    }
    inner.unlock_hover = within_hotspot;
    let unlock_hover = inner.unlock_hover;
    drop(inner);

    let hide_taskbar = app.state::<SettingsStore>().get_bool(HIDE_TASKBAR_ICON);
    let _ = apply_main_window_ex_style(&window, true, unlock_hover, hide_taskbar);
    publish_click_through_state(app);
}

#[cfg(not(windows))]
fn tick_click_through_monitor(_app: &AppHandle) {}

fn save_window_state(app: &AppHandle, window: &tauri::WebviewWindow) {
    let is_maximized = window.is_maximized().unwrap_or(false);
    let settings = app.state::<SettingsStore>();
    let _ = settings.set(WINDOW_IS_MAXIMIZED.to_string(), Value::Bool(is_maximized));
    if !is_maximized {
        if let (Ok(position), Ok(size)) = (window.outer_position(), window.outer_size()) {
            let _ = settings.set(
                WINDOW_BOUNDS.to_string(),
                json!({ "x": position.x, "y": position.y, "width": size.width, "height": size.height }),
            );
        }
    }
}

#[cfg(desktop)]
fn restore_window_state(app: &AppHandle) {
    let Some(window) = app.get_webview_window(MAIN_WINDOW_LABEL) else {
        return;
    };
    let settings = app.state::<SettingsStore>();
    if let Some(Value::Object(map)) = settings.get(WINDOW_BOUNDS) {
        let x = map
            .get("x")
            .and_then(Value::as_i64)
            .and_then(|v| i32::try_from(v).ok());
        let y = map
            .get("y")
            .and_then(Value::as_i64)
            .and_then(|v| i32::try_from(v).ok());
        let width = map
            .get("width")
            .and_then(Value::as_u64)
            .and_then(|v| u32::try_from(v).ok());
        let height = map
            .get("height")
            .and_then(Value::as_u64)
            .and_then(|v| u32::try_from(v).ok());
        if let (Some(x), Some(y), Some(width), Some(height)) = (x, y, width, height) {
            let _ = window.set_position(PhysicalPosition::new(x, y));
            let _ = window.set_size(PhysicalSize::new(width, height));
        }
    }
    if settings.get_bool(WINDOW_IS_MAXIMIZED) {
        let _ = window.maximize();
    }
}

#[cfg(desktop)]
fn setup_window_state_persistence(app: &AppHandle) {
    let Some(window) = app.get_webview_window(MAIN_WINDOW_LABEL) else {
        return;
    };
    let handle = app.clone();
    window.on_window_event(move |event| {
        use tauri::WindowEvent;
        let should_save = matches!(
            event,
            WindowEvent::Moved(_) | WindowEvent::Resized(_) | WindowEvent::CloseRequested { .. }
        );
        if !should_save {
            return;
        }
        let state = handle.state::<MainWindowState>();
        let mut inner = state.inner.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(last) = inner.last_window_state_save {
            if last.elapsed() < Duration::from_millis(WINDOW_STATE_SAVE_DEBOUNCE_MS) {
                return;
            }
        }
        inner.last_window_state_save = Some(Instant::now());
        drop(inner);
        if let Some(window) = handle.get_webview_window(MAIN_WINDOW_LABEL) {
            save_window_state(&handle, &window);
        }
    });
}

// 启动/透明重建后应用 always-on-top 与扩展样式。
#[cfg(desktop)]
fn apply_startup_styles(app: &AppHandle) -> Result<(), String> {
    let settings = app.state::<SettingsStore>();
    let always_on_top = settings.get_bool(MAIN_WINDOW_ALWAYS_ON_TOP);
    let hide_taskbar = settings.get_bool(HIDE_TASKBAR_ICON);
    let Some(window) = app.get_webview_window(MAIN_WINDOW_LABEL) else {
        return Ok(());
    };
    window
        .set_always_on_top(always_on_top)
        .map_err(|error| format!("set always-on-top: {error}"))?;
    let state = app.state::<MainWindowState>();
    let (enabled, unlock_hover) = {
        let inner = state.inner.lock().unwrap_or_else(|p| p.into_inner());
        (inner.enabled, inner.unlock_hover)
    };
    apply_main_window_ex_style(&window, enabled, unlock_hover, hide_taskbar)
}

#[cfg(desktop)]
fn is_main_window_visible(app: &AppHandle) -> bool {
    app.get_webview_window(MAIN_WINDOW_LABEL)
        .map(|window| {
            window.is_visible().unwrap_or(false) && !window.is_minimized().unwrap_or(false)
        })
        .unwrap_or(false)
}

#[cfg(desktop)]
fn hide_main_window(app: &AppHandle) -> bool {
    let Some(window) = app.get_webview_window(MAIN_WINDOW_LABEL) else {
        return false;
    };
    if window.is_minimized().unwrap_or(false) {
        let _ = window.unminimize();
    }
    window.hide().is_ok()
}

#[cfg(desktop)]
pub fn focus_main_window(app: &AppHandle) {
    let Some(window) = app.get_webview_window(MAIN_WINDOW_LABEL) else {
        return;
    };
    let _ = window.unminimize();
    let _ = window.show();
    let _ = window.set_focus();
}

#[cfg(desktop)]
fn toggle_main_window_visibility(app: &AppHandle) {
    if is_main_window_visible(app) {
        hide_main_window(app);
    } else {
        focus_main_window(app);
    }
}

// 创建系统托盘：左键切换显隐，右键菜单显示/隐藏 + 退出。
#[cfg(desktop)]
pub fn create_tray(app: &AppHandle) -> Result<(), String> {
    let show_hide = MenuItem::with_id(
        app,
        "toggle-visibility",
        "Show/Hide Window",
        true,
        None::<&str>,
    )
    .map_err(|error| format!("build tray menu item: {error}"))?;
    let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)
        .map_err(|error| format!("build tray menu item: {error}"))?;
    let disable_click_through = MenuItem::with_id(
        app,
        "disable-click-through",
        "Disable Click-through",
        true,
        None::<&str>,
    )
    .map_err(|error| format!("build tray click-through menu item: {error}"))?;
    let menu = Menu::with_items(app, &[&show_hide, &disable_click_through, &quit])
        .map_err(|error| format!("build tray menu: {error}"))?;
    let icon = app
        .default_window_icon()
        .cloned()
        .ok_or_else(|| "no default window icon available".to_string())?;
    let tray = TrayIconBuilder::new()
        .icon(icon)
        .menu(&menu)
        .show_menu_on_left_click(false)
        .tooltip("Folia")
        .on_menu_event(|app, event| match event.id().as_ref() {
            "toggle-visibility" => toggle_main_window_visibility(app),
            "disable-click-through" => {
                let _ = window_set_click_through(app.clone(), false);
                focus_main_window(app);
            }
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if matches!(event, TrayIconEvent::Click { .. }) {
                toggle_main_window_visibility(tray.app_handle());
            }
        });
    tray.build(app)
        .map_err(|error| format!("create tray: {error}"))?;
    Ok(())
}

// M2 启动装配：恢复窗口状态、应用启动样式、挂载状态持久化、启动穿透光标轮询、创建托盘。
#[cfg(desktop)]
pub fn setup(app: &mut tauri::App) {
    let handle = app.handle().clone();
    restore_window_state(&handle);
    let _ = apply_startup_styles(&handle);
    setup_window_state_persistence(&handle);
    start_click_through_monitor(handle.clone());
    let _ = create_tray(&handle);
}

#[tauri::command]
// 显示/聚焦主窗口（恢复最小化 + show + set_focus）。
// 远程发起视频导出时，主窗口需要可见并被带到前台，用户才能看到确认 toast 并点击。
#[cfg(desktop)]
pub fn window_focus_main(app: AppHandle) -> Result<bool, String> {
    focus_main_window(&app);
    Ok(app.get_webview_window(MAIN_WINDOW_LABEL).is_some())
}

#[tauri::command]
// 最小化主窗口；MINIMIZE_TO_TRAY 开启时改为隐藏到托盘。
#[cfg(desktop)]
pub fn window_minimize(app: AppHandle) -> Result<bool, String> {
    let Some(window) = app.get_webview_window(MAIN_WINDOW_LABEL) else {
        return Ok(false);
    };
    if app.state::<SettingsStore>().get_bool(MINIMIZE_TO_TRAY) {
        if window.is_minimized().unwrap_or(false) {
            let _ = window.unminimize();
        }
        window
            .hide()
            .map_err(|error| format!("hide main window: {error}"))?;
        return Ok(true);
    }
    window
        .minimize()
        .map_err(|error| format!("minimize main window: {error}"))?;
    Ok(true)
}

#[tauri::command]
// 最大化/还原主窗口，返回切换后的最大化状态。
#[cfg(desktop)]
pub fn window_toggle_maximize(app: AppHandle) -> Result<bool, String> {
    let Some(window) = app.get_webview_window(MAIN_WINDOW_LABEL) else {
        return Ok(false);
    };
    if window.is_maximized().unwrap_or(false) {
        window
            .unmaximize()
            .map_err(|error| format!("unmaximize main window: {error}"))?;
        Ok(false)
    } else {
        window
            .maximize()
            .map_err(|error| format!("maximize main window: {error}"))?;
        Ok(true)
    }
}

#[tauri::command]
// 全屏/退出全屏切换，返回切换后的状态。
#[cfg(desktop)]
pub fn window_toggle_fullscreen(app: AppHandle) -> Result<bool, String> {
    let Some(window) = app.get_webview_window(MAIN_WINDOW_LABEL) else {
        return Ok(false);
    };
    let next = !window.is_fullscreen().unwrap_or(false);
    window
        .set_fullscreen(next)
        .map_err(|error| format!("set fullscreen: {error}"))?;
    Ok(next)
}

#[tauri::command]
// 关闭主窗口并退出应用（镜像 Electron window-all-closed 退出语义）。
pub fn window_close(app: AppHandle) -> Result<bool, String> {
    let Some(window) = app.get_webview_window(MAIN_WINDOW_LABEL) else {
        return Ok(false);
    };
    save_window_state(&app, &window);
    app.state::<WindowPlaybackHandoffStore>().clear_all();
    let _ = window.close();
    app.exit(0);
    Ok(true)
}

#[tauri::command]
// 返回主窗口是否处于最大化状态。
pub fn window_is_maximized(app: AppHandle) -> Result<bool, String> {
    Ok(app
        .get_webview_window(MAIN_WINDOW_LABEL)
        .map(|window| window.is_maximized().unwrap_or(false))
        .unwrap_or(false))
}

#[tauri::command]
// 读取透明播放器背景开关（持久化设置）。
pub fn window_get_transparent_mode(app: AppHandle) -> Result<bool, String> {
    Ok(app
        .state::<SettingsStore>()
        .get_bool(TRANSPARENT_PLAYER_BACKGROUND))
}

#[tauri::command]
// 切换透明播放器背景：写设置、暂存 handoff、重置穿透状态并重建主窗口。
#[cfg(desktop)]
pub fn window_set_transparent_mode(
    app: AppHandle,
    enabled: bool,
    handoff: Option<Value>,
) -> Result<bool, String> {
    app.state::<SettingsStore>().set(
        TRANSPARENT_PLAYER_BACKGROUND.to_string(),
        Value::Bool(enabled),
    )?;
    if let Some(handoff) = handoff.as_ref() {
        if handoff.is_object() {
            app.state::<WindowPlaybackHandoffStore>()
                .save(handoff.clone());
        }
    }
    {
        let state = app.state::<MainWindowState>();
        let mut inner = state.inner.lock().unwrap_or_else(|p| p.into_inner());
        inner.enabled = false;
        inner.unlock_hover = false;
    }

    let (bounds, was_maximized) = {
        let window = app.get_webview_window(MAIN_WINDOW_LABEL);
        let bounds =
            window
                .as_ref()
                .and_then(|w| match (w.outer_position().ok(), w.outer_size().ok()) {
                    (Some(position), Some(size)) => {
                        Some((position.x, position.y, size.width, size.height))
                    }
                    _ => None,
                });
        let was_maximized = window
            .as_ref()
            .map(|w| w.is_maximized().unwrap_or(false))
            .unwrap_or(false);
        (bounds, was_maximized)
    };

    if let Some(window) = app.get_webview_window(MAIN_WINDOW_LABEL) {
        let _ = window.destroy();
    }

    let window = WebviewWindowBuilder::new(
        &app,
        MAIN_WINDOW_LABEL,
        WebviewUrl::App("index.html".into()),
    )
    .title("Folia")
    .decorations(false)
    .transparent(enabled)
    .shadow(!enabled)
    .background_color(if enabled {
        tauri::window::Color(0, 0, 0, 0)
    } else {
        tauri::window::Color(9, 9, 11, 255)
    })
    .resizable(true)
    .center()
    .build()
    .map_err(|error| format!("rebuild main window: {error}"))?;
    if enabled {
        disable_native_window_corners(&window)?;
    }
    if let Some((x, y, width, height)) = bounds {
        let _ = window.set_position(PhysicalPosition::new(x, y));
        let _ = window.set_size(PhysicalSize::new(width, height));
    }
    if was_maximized {
        let _ = window.maximize();
    }
    let _ = apply_startup_styles(&app);
    setup_window_state_persistence(&app);
    Ok(true)
}

#[tauri::command]
// 消费一次播放状态交接（TTL 内有效，消费后即清）。
pub fn window_playback_handoff_consume(app: AppHandle) -> Result<Option<Value>, String> {
    Ok(app.state::<WindowPlaybackHandoffStore>().consume())
}

#[tauri::command]
// 提交播放状态交接；带 requestId 时先暂存再解析挂起请求，否则仅暂存。
pub fn window_playback_handoff_submit(
    app: AppHandle,
    request_id: Option<String>,
    handoff: Option<Value>,
) -> Result<bool, String> {
    let store = app.state::<WindowPlaybackHandoffStore>();
    let remembered = match handoff.as_ref() {
        Some(value) if value.is_object() => store.save(value.clone()),
        _ => false,
    };
    match request_id {
        Some(id) if !id.trim().is_empty() => Ok(store.resolve(&id, handoff)),
        _ => Ok(remembered),
    }
}

#[tauri::command]
// 记录原生主题来源到设置；WebView2 无运行时 themeSource，M2 阶段持久化 no-op（M12 复核 UI）。
pub fn window_set_native_theme(app: AppHandle, theme_source: String) -> Result<(), String> {
    app.state::<SettingsStore>()
        .set(NATIVE_THEME_SOURCE.to_string(), Value::String(theme_source))?;
    Ok(())
}

#[tauri::command]
// 返回主窗口点击穿透是否开启。
pub fn window_get_click_through(app: AppHandle) -> Result<bool, String> {
    let state = app.state::<MainWindowState>();
    let enabled = state
        .inner
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .enabled;
    Ok(enabled)
}

#[tauri::command]
// 开启/关闭主窗口点击穿透并发布状态变更事件。
pub fn window_set_click_through(app: AppHandle, enabled: bool) -> Result<bool, String> {
    let state = app.state::<MainWindowState>();
    let hide_taskbar;
    {
        let mut inner = state.inner.lock().unwrap_or_else(|p| p.into_inner());
        inner.enabled = enabled;
        if !enabled {
            inner.unlock_hover = false;
        }
        hide_taskbar = app.state::<SettingsStore>().get_bool(HIDE_TASKBAR_ICON);
    }
    if let Some(window) = app.get_webview_window(MAIN_WINDOW_LABEL) {
        let unlock_hover = state
            .inner
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .unlock_hover;
        apply_main_window_ex_style(&window, enabled, unlock_hover, hide_taskbar)?;
    }
    publish_click_through_state(&app);
    Ok(enabled)
}

#[tauri::command]
// 设置穿透解锁悬停状态（仅穿透开启时生效）。
pub fn window_set_click_through_unlock_hover(app: AppHandle, active: bool) -> Result<bool, String> {
    let state = app.state::<MainWindowState>();
    let next = {
        let mut inner = state.inner.lock().unwrap_or_else(|p| p.into_inner());
        inner.unlock_hover = active && inner.enabled;
        inner.unlock_hover
    };
    if let Some(window) = app.get_webview_window(MAIN_WINDOW_LABEL) {
        let (enabled, unlock_hover) = {
            let inner = state.inner.lock().unwrap_or_else(|p| p.into_inner());
            (inner.enabled, inner.unlock_hover)
        };
        let hide_taskbar = app.state::<SettingsStore>().get_bool(HIDE_TASKBAR_ICON);
        apply_main_window_ex_style(&window, enabled, unlock_hover, hide_taskbar)?;
    }
    publish_click_through_state(&app);
    Ok(next)
}

#[tauri::command]
// 返回主窗口是否置顶。
pub fn window_get_always_on_top(app: AppHandle) -> Result<bool, String> {
    if let Some(window) = app.get_webview_window(MAIN_WINDOW_LABEL) {
        if let Ok(value) = window.is_always_on_top() {
            return Ok(value);
        }
    }
    Ok(app
        .state::<SettingsStore>()
        .get_bool(MAIN_WINDOW_ALWAYS_ON_TOP))
}

#[tauri::command]
// 设置主窗口置顶并持久化设置。
#[cfg(desktop)]
pub fn window_set_always_on_top(app: AppHandle, enabled: bool) -> Result<bool, String> {
    app.state::<SettingsStore>()
        .set(MAIN_WINDOW_ALWAYS_ON_TOP.to_string(), Value::Bool(enabled))?;
    if let Some(window) = app.get_webview_window(MAIN_WINDOW_LABEL) {
        window
            .set_always_on_top(enabled)
            .map_err(|error| format!("set always-on-top: {error}"))?;
    }
    Ok(enabled)
}

#[cfg(windows)]
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn click_through_adds_transparent_and_layered() {
        let style = ex_style_toggle(0, true, false, false);
        assert_eq!(style & WS_EX_TRANSPARENT, WS_EX_TRANSPARENT);
        assert_eq!(style & WS_EX_LAYERED, WS_EX_LAYERED);
    }

    #[test]
    fn unlock_hover_clears_transparent_but_keeps_layered() {
        let enabled = ex_style_toggle(0, true, false, false);
        let style = ex_style_toggle(enabled, true, true, false);
        assert_eq!(style & WS_EX_TRANSPARENT, 0);
        assert_eq!(style & WS_EX_LAYERED, WS_EX_LAYERED);
    }

    #[test]
    fn disabling_click_through_clears_transparent() {
        let base = WS_EX_LAYERED | WS_EX_TRANSPARENT;
        let style = ex_style_toggle(base, false, false, false);
        assert_eq!(style & WS_EX_TRANSPARENT, 0);
        assert_eq!(style & WS_EX_LAYERED, WS_EX_LAYERED);
    }

    #[test]
    fn hide_taskbar_adds_toolwindow_and_can_be_removed() {
        let style = ex_style_toggle(0, false, false, true);
        assert_eq!(style & WS_EX_TOOLWINDOW, WS_EX_TOOLWINDOW);
        let cleared = ex_style_toggle(style, false, false, false);
        assert_eq!(cleared & WS_EX_TOOLWINDOW, 0);
    }

    #[test]
    fn hotspot_bounds_match_electron_constants_at_100_percent() {
        let (left, top, right, bottom) = hotspot_bounds(1200, 1.0);
        assert_eq!((left, top, right, bottom), (976, 4, 1024, 44));
    }

    #[test]
    fn hotspot_bounds_scale_css_pixels_for_high_dpi_windows() {
        let (left, top, right, bottom) = hotspot_bounds(1500, 1.25);
        assert_eq!((left, top, right, bottom), (1220, 5, 1280, 55));
    }

    #[test]
    fn cursor_in_hotspot_checks_bounds() {
        assert!(cursor_in_hotspot(1000, 20, 976, 4, 1024, 44));
        assert!(!cursor_in_hotspot(960, 20, 976, 4, 1024, 44));
        assert!(!cursor_in_hotspot(1000, 50, 976, 4, 1024, 44));
    }
}
