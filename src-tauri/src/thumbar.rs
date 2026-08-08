//! M8 Windows taskbar thumbnail toolbar ("thumbar") buttons.
//!
//! Mirrors `folia-major/electron/main.cjs` `updateWindowThumbarButtons` /
//! `thumbar-update-buttons` + `thumbar-action`: previous / play-pause / next
//! buttons rendered by the Windows taskbar thumbnail, with clicks forwarded to
//! the renderer as `thumbar-action` events.
//!
//! Real implementation (Windows only): `ITaskbarList3::ThumbBarAddButtons` /
//! `ThumbBarUpdateButtons` on the main window HWND, glyph icons generated with
//! GDI, and a comctl32 window subclass that translates `WM_COMMAND` +
//! `THBN_CLICKED` into the renderer event. On non-Windows the command degrades
//! safely (returns `false`, no buttons) exactly like Electron's
//! `isWindowsThumbarSupported` guard.

use serde_json::Value;
use std::sync::{Mutex, OnceLock};
#[cfg(windows)]
use tauri::Emitter;
use tauri::{AppHandle, Manager};

#[cfg(windows)]
use crate::window::MAIN_WINDOW_LABEL;

// Button ids must match the order Electron builds its thumbar buttons.
// Windows-only: consumed by the `#[cfg(windows)] mod native` implementation.
#[cfg(windows)]
pub const THUMBAR_PREVIOUS: u32 = 1;
#[cfg(windows)]
pub const THUMBAR_PLAY_PAUSE: u32 = 2;
#[cfg(windows)]
pub const THUMBAR_NEXT: u32 = 3;

#[cfg(windows)]
const THUMBAR_ICON_SIZE: i32 = 16;
#[cfg(windows)]
const THUMBAR_SUBCLASS_ID: usize = 0x464f4c41; // 'FOLA'

/// App handle used by the window subclass proc to forward clicks; set once in
/// `setup`. A single main window exists for the app lifetime.
static MAIN_APP: OnceLock<AppHandle> = OnceLock::new();

/// Tracks which HWND currently has the subclass + toolbar installed so a window
/// rebuild (transparent-mode recreation) re-installs both lazily.
pub struct ThumbarState {
    inner: Mutex<ThumbarInner>,
}

/// 每个 HWND 独立跟踪工具条安装状态：`toolbar_hwnd` 记录最后一次成功
/// `ThumbBarAddButtons` 的 HWND，保证同一 HWND 只 Add 一次，后续一律 Update。
/// 字段仅 Windows 原生实现读写；非 Windows 桌面目标上结构体为空壳。
struct ThumbarInner {
    #[cfg(windows)]
    subclassed_hwnd: Option<isize>,
    #[cfg(windows)]
    toolbar_hwnd: Option<isize>,
}

impl ThumbarState {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(ThumbarInner {
                #[cfg(windows)]
                subclassed_hwnd: None,
                #[cfg(windows)]
                toolbar_hwnd: None,
            }),
        }
    }
}

impl Default for ThumbarState {
    fn default() -> Self {
        Self::new()
    }
}

/// Validated taskbar-control state (mirrors `thumbar-update-buttons` coercion).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TaskbarState {
    pub has_active_track: bool,
    pub can_go_previous: bool,
    pub can_go_next: bool,
    pub is_playing: bool,
}

impl Default for TaskbarState {
    fn default() -> Self {
        Self {
            has_active_track: false,
            can_go_previous: false,
            can_go_next: false,
            is_playing: false,
        }
    }
}

// 外部输入校验：仅接受布尔字段，缺失/非布尔按 false 处理（同 Electron Boolean()）。
pub fn parse_taskbar_state(state: &Value) -> TaskbarState {
    let as_bool = |key: &str| state.get(key).and_then(Value::as_bool).unwrap_or(false);
    TaskbarState {
        has_active_track: as_bool("hasActiveTrack"),
        can_go_previous: as_bool("canGoPrevious"),
        can_go_next: as_bool("canGoNext"),
        is_playing: as_bool("isPlaying"),
    }
}

/// 纯状态机输出：给定某 HWND 的任务栏安装状态与播放状态，决定下一步操作。
/// 纯逻辑、无平台依赖，供 Windows 实现调用并被单元测试直接验证。
/// 仅在 Windows 生产代码与测试中引用；其他桌面目标下不编译。
#[cfg(any(windows, test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToolbarAction {
    /// 该 HWND 从未 Add 过且无曲目：不调用任何任务栏 API（no-op）。
    NoOp,
    /// 该 HWND 从未 Add 过且有曲目：ThumbBarAddButtons。
    Add,
    /// 该 HWND 已 Add 过且有曲目：ThumbBarUpdateButtons。
    Update,
    /// 该 HWND 已 Add 过但无曲目：用 3 个相同 ID + THBF_HIDDEN 的按钮
    /// ThumbBarUpdateButtons 隐藏，并保留已安装标记（added=true）。
    Hide,
}

/// 纯决策：`toolbar_hwnd == Some(hwnd)` 表示该 HWND 已安装工具条。
#[cfg(any(windows, test))]
fn decide_toolbar_action(
    toolbar_hwnd: Option<isize>,
    hwnd: isize,
    has_track: bool,
) -> ToolbarAction {
    if toolbar_hwnd == Some(hwnd) {
        if has_track {
            ToolbarAction::Update
        } else {
            ToolbarAction::Hide
        }
    } else if has_track {
        ToolbarAction::Add
    } else {
        ToolbarAction::NoOp
    }
}

/// 纯状态转移：任务栏 API 调用成功后更新 `toolbar_hwnd`。只有 Add 写入新 HWND；
/// Update / Hide 保留原标记（隐藏后曲目恢复时仍走 Update 而非重复 Add）。
#[cfg(any(windows, test))]
fn mark_after_success(
    toolbar_hwnd: Option<isize>,
    hwnd: isize,
    action: ToolbarAction,
) -> Option<isize> {
    match action {
        ToolbarAction::Add => Some(hwnd),
        ToolbarAction::NoOp | ToolbarAction::Update | ToolbarAction::Hide => toolbar_hwnd,
    }
}

/// 锁内临界区的单步纯逻辑：串行执行 decide →（成功时）mark。
/// `succeeded == false`（任务栏 API 失败）时状态不变，同一 HWND 下次重试仍走
/// Add，避免“Add 失败却已标记安装”导致永远走 Update 的错误。
/// Windows 实现须在持有 `tracked` 锁期间调用本函数并提交返回的新状态，
/// 使并发下“同一 HWND 只 Add 一次”的不变量成立（详见 `apply_buttons`）。
#[cfg(any(windows, test))]
fn step_serialized(
    installed: Option<isize>,
    hwnd: isize,
    has_track: bool,
    succeeded: bool,
) -> (ToolbarAction, Option<isize>) {
    let action = decide_toolbar_action(installed, hwnd, has_track);
    let state = if succeeded {
        mark_after_success(installed, hwnd, action)
    } else {
        installed
    };
    (action, state)
}

/// Windows-only: TRUE (non-zero) when the taskbar exposes a thumbnail toolbar
/// for the given window; mirrors Electron `isWindowsThumbarSupported`.
fn thumbar_supported() -> bool {
    cfg!(windows)
}

// -- Windows native implementation ------------------------------------------

#[cfg(windows)]
mod native {
    use super::*;
    use windows::core::BOOL;
    use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
    use windows::Win32::Graphics::Gdi::{
        CreateDIBSection, DeleteObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS,
        HGDIOBJ,
    };
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_ALL, COINIT_APARTMENTTHREADED,
    };
    use windows::Win32::UI::Shell::{
        ITaskbarList3, THBF_DISABLED, THBF_ENABLED, THBF_HIDDEN, THBN_CLICKED, THB_FLAGS, THB_ICON,
        THB_TOOLTIP, THUMBBUTTON, THUMBBUTTONFLAGS, THUMBBUTTONMASK,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateIconIndirect, DestroyIcon, HICON, ICONINFO, WM_COMMAND,
    };

    pub const CLSID_TASKBAR_LIST: windows::core::GUID =
        windows::core::GUID::from_u128(0x56fdf344_fd6d_11d0_958a_006097c9a090);

    type SubclassProc =
        unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM, usize, usize) -> LRESULT;

    #[link(name = "comctl32")]
    unsafe extern "system" {
        fn SetWindowSubclass(
            hwnd: HWND,
            pfn_subclass: SubclassProc,
            id_subclass: usize,
            dw_ref_data: usize,
        ) -> BOOL;
        fn DefSubclassProc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT;
    }

    /// Glyphs drawn into the 16x16 icon (aligned with the build/thumbar PNGs
    /// the Electron version loads).
    #[derive(Clone, Copy)]
    enum Glyph {
        Previous,
        Play,
        Pause,
        Next,
    }

    fn in_triangle(x: f64, y: f64, ax: f64, ay: f64, bx: f64, by: f64, cx: f64, cy: f64) -> bool {
        let d1 = (x - bx) * (ay - by) - (ax - bx) * (y - by);
        let d2 = (x - cx) * (by - cy) - (bx - cx) * (y - cy);
        let d3 = (x - ax) * (cy - ay) - (cx - ax) * (y - ay);
        let has_neg = d1 < 0.0 || d2 < 0.0 || d3 < 0.0;
        let has_pos = d1 > 0.0 || d2 > 0.0 || d3 > 0.0;
        !(has_neg && has_pos)
    }

    fn glyph_opaque(glyph: Glyph, x: i32, y: i32) -> bool {
        let (xf, yf) = (x as f64 + 0.5, y as f64 + 0.5);
        match glyph {
            Glyph::Play => in_triangle(xf, yf, 5.0, 4.0, 5.0, 12.0, 12.0, 8.0),
            Glyph::Pause => (x == 4 || x == 5 || x == 10 || x == 11) && (4..=11).contains(&y),
            Glyph::Previous => {
                in_triangle(xf, yf, 5.0, 8.0, 10.0, 4.0, 10.0, 12.0)
                    || ((x == 11 || x == 12) && (4..=11).contains(&y))
            }
            Glyph::Next => {
                in_triangle(xf, yf, 11.0, 8.0, 6.0, 4.0, 6.0, 12.0)
                    || ((x == 3 || x == 4) && (4..=11).contains(&y))
            }
        }
    }

    // 用 GDI 画一个 16x16 白字形 HICON（32bpp DIB + CreateIconIndirect，带 alpha）。
    // hbm（DIB 位图句柄）无论 CreateIconIndirect 成败都必须 DeleteObject：
    // 成功后图标已复制位图内容；失败时更不能跳过清理造成句柄泄漏。
    fn create_glyph_icon(glyph: Glyph) -> windows::core::Result<HICON> {
        let width = THUMBAR_ICON_SIZE;
        let height = THUMBAR_ICON_SIZE;
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width,
                biHeight: -height, // top-down
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0 as u32,
                ..Default::default()
            },
            bmiColors: [Default::default(); 1],
        };
        let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
        let hbm = unsafe { CreateDIBSection(None, &bmi, DIB_RGB_COLORS, &mut bits, None, 0)? };
        let pixel_count = (width * height) as usize;
        let pixels = unsafe { std::slice::from_raw_parts_mut(bits as *mut u32, pixel_count) };
        for y in 0..height {
            for x in 0..width {
                let opaque = glyph_opaque(glyph, x, y);
                let (r, g, b, a) = if opaque {
                    (255u32, 255u32, 255u32, 255u32)
                } else {
                    (0u32, 0u32, 0u32, 0u32)
                };
                pixels[(y * width + x) as usize] = (a << 24) | (b << 16) | (g << 8) | r;
            }
        }
        let icon_info = ICONINFO {
            fIcon: BOOL(1),
            xHotspot: 0,
            yHotspot: 0,
            hbmMask: hbm,
            hbmColor: hbm,
        };
        let hicon = unsafe { CreateIconIndirect(&icon_info) };
        // 先释放 hbm 再返回原 Result：`hicon` 为 Err 时也不能跳过 DeleteObject。
        unsafe {
            let _ = DeleteObject(HGDIOBJ(hbm.0));
        }
        hicon
    }

    fn set_tooltip(slot: &mut [u16; 260], tip: &str) {
        let mut encoded = tip.encode_utf16();
        for target in slot.iter_mut() {
            *target = encoded.next().unwrap_or(0);
        }
    }

    fn button_flags(enabled: bool) -> THUMBBUTTONFLAGS {
        if enabled {
            THBF_ENABLED
        } else {
            THBF_DISABLED
        }
    }

    /// 构建与 Electron 相同的三个按钮（顺序 previous / play-pause / next）。
    /// 中途创建图标失败时，销毁已成功创建的 HICON（CreateIconIndirect 产物
    /// 必须用 DestroyIcon 释放），避免泄漏。
    fn build_buttons(state: TaskbarState) -> windows::core::Result<[THUMBBUTTON; 3]> {
        let mask = THUMBBUTTONMASK(THB_ICON.0 | THB_TOOLTIP.0 | THB_FLAGS.0);
        let mut buttons = [
            THUMBBUTTON {
                dwMask: mask,
                iId: THUMBAR_PREVIOUS,
                iBitmap: 0,
                hIcon: HICON(std::ptr::null_mut()),
                szTip: [0; 260],
                dwFlags: button_flags(state.can_go_previous),
            },
            THUMBBUTTON {
                dwMask: mask,
                iId: THUMBAR_PLAY_PAUSE,
                iBitmap: 0,
                hIcon: HICON(std::ptr::null_mut()),
                szTip: [0; 260],
                dwFlags: THBF_ENABLED,
            },
            THUMBBUTTON {
                dwMask: mask,
                iId: THUMBAR_NEXT,
                iBitmap: 0,
                hIcon: HICON(std::ptr::null_mut()),
                szTip: [0; 260],
                dwFlags: button_flags(state.can_go_next),
            },
        ];

        let play_pause_glyph = if state.is_playing {
            Glyph::Pause
        } else {
            Glyph::Play
        };
        let glyphs = [Glyph::Previous, play_pause_glyph, Glyph::Next];
        for (index, glyph) in glyphs.into_iter().enumerate() {
            match create_glyph_icon(glyph) {
                Ok(icon) => buttons[index].hIcon = icon,
                Err(error) => {
                    // 清理此前已创建成功的图标后返回错误。
                    for button in buttons.iter().take(index) {
                        unsafe {
                            let _ = DestroyIcon(button.hIcon);
                        }
                    }
                    return Err(error);
                }
            }
        }

        set_tooltip(&mut buttons[0].szTip, "Previous Track");
        set_tooltip(
            &mut buttons[1].szTip,
            if state.is_playing { "Pause" } else { "Play" },
        );
        set_tooltip(&mut buttons[2].szTip, "Next Track");

        Ok(buttons)
    }

    /// Install the comctl32 subclass that routes `THBN_CLICKED` → renderer.
    /// 窗口重建（透明模式重建）后 HWND 变化：重挂 subclass 并重置工具条标记，
    /// 让下一次推送走 ThumbBarAddButtons 而非 UpdateButtons。
    /// SetWindowSubclass 失败时返回 Err 且不标记成功，调用方可重试。
    pub fn install_subclass(hwnd: HWND, tracked: &Mutex<ThumbarInner>) -> Result<(), String> {
        let mut inner = tracked.lock().unwrap_or_else(|p| p.into_inner());
        if inner.subclassed_hwnd == Some(hwnd.0 as isize) {
            return Ok(());
        }
        // SetWindowSubclass is documented as thread-safe; replace any stale entry.
        let installed = unsafe { SetWindowSubclass(hwnd, subclass_proc, THUMBAR_SUBCLASS_ID, 0) };
        if !installed.as_bool() {
            return Err("SetWindowSubclass failed".to_string());
        }
        inner.subclassed_hwnd = Some(hwnd.0 as isize);
        // 新窗口上工具条从未 Add 过。
        inner.toolbar_hwnd = None;
        Ok(())
    }

    unsafe extern "system" fn subclass_proc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
        _subclass_id: usize,
        _ref_data: usize,
    ) -> LRESULT {
        if msg == WM_COMMAND {
            let notification = ((wparam.0 >> 16) & 0xffff) as u32;
            if notification == THBN_CLICKED {
                let button_id = (wparam.0 & 0xffff) as u32;
                let action = match button_id {
                    THUMBAR_PREVIOUS => Some("previous"),
                    THUMBAR_PLAY_PAUSE => Some("play-pause"),
                    THUMBAR_NEXT => Some("next"),
                    _ => None,
                };
                if let Some(action) = action {
                    if let Some(app) = MAIN_APP.get() {
                        let _ = app.emit_to("main", "thumbar-action", action);
                    }
                }
            }
        }
        DefSubclassProc(hwnd, msg, wparam, lparam)
    }

    /// 无曲目时隐藏已安装的按钮：3 个相同 ID + THBF_HIDDEN，不创建图标。
    fn hidden_buttons() -> [THUMBBUTTON; 3] {
        let mask = THUMBBUTTONMASK(THB_FLAGS.0);
        let flags = THUMBBUTTONFLAGS(THBF_HIDDEN.0);
        let hidden = || THUMBBUTTON {
            dwMask: mask,
            iId: 0,
            iBitmap: 0,
            hIcon: HICON(std::ptr::null_mut()),
            szTip: [0; 260],
            dwFlags: flags,
        };
        let mut buttons = [hidden(), hidden(), hidden()];
        buttons[0].iId = THUMBAR_PREVIOUS;
        buttons[1].iId = THUMBAR_PLAY_PAUSE;
        buttons[2].iId = THUMBAR_NEXT;
        buttons
    }

    /// 真实应用 thumbar 按钮：由纯状态机 `decide_toolbar_action` 决定
    /// NoOp / Add / Update / Hide，失败返回 Err（可重试）。
    pub fn apply_buttons(
        app: &AppHandle,
        state: TaskbarState,
        tracked: &Mutex<ThumbarInner>,
    ) -> Result<bool, String> {
        let Some(window) = app.get_webview_window(MAIN_WINDOW_LABEL) else {
            return Ok(false);
        };
        let hwnd = window
            .hwnd()
            .map_err(|error| format!("resolve main window hwnd: {error}"))?;
        install_subclass(hwnd, tracked)?;

        // 每个调用线程初始化 COM（幂等；CoUninitialize 仅在我们真正初始化后调用）。
        let com = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        let co_initialized = com.is_ok();
        let result = (|| -> Result<bool, String> {
            let taskbar: ITaskbarList3 =
                unsafe { CoCreateInstance(&CLSID_TASKBAR_LIST, None, CLSCTX_ALL) }
                    .map_err(|error| format!("create taskbar list: {error}"))?;
            unsafe { taskbar.HrInit() }
                .map_err(|error| format!("initialize taskbar list: {error}"))?;

            let hwnd_isize = hwnd.0 as isize;
            // 串行化 per-HWND 临界区：decide → Add/Update/Hide → mark 全部在持有
            // `tracked` 锁期间完成。若不持锁，两个并发调用可能都读到
            // toolbar_hwnd=None，随后同时 ThumbBarAddButtons 破坏“只 Add 一次”。
            // 临界区内只做纯计算与任务栏 API 调用，不再二次加锁（避免锁重入）。
            let mut inner = tracked.lock().unwrap_or_else(|p| p.into_inner());
            let (action, marked_state) =
                step_serialized(inner.toolbar_hwnd, hwnd_isize, state.has_active_track, true);
            let outcome = match action {
                // 未 Add 且无曲目：无需调用任何任务栏 API。
                ToolbarAction::NoOp => Ok(true),
                ToolbarAction::Add | ToolbarAction::Update => {
                    let buttons = build_buttons(state)
                        .map_err(|error| format!("build thumbar buttons: {error}"))?;
                    let result = unsafe {
                        match action {
                            ToolbarAction::Add => taskbar.ThumbBarAddButtons(hwnd, &buttons),
                            _ => taskbar.ThumbBarUpdateButtons(hwnd, &buttons),
                        }
                    };
                    // HICON 由 CreateIconIndirect 创建，必须用 DestroyIcon 释放。
                    for button in &buttons {
                        unsafe {
                            let _ = DestroyIcon(button.hIcon);
                        }
                    }
                    if result.is_ok() {
                        Ok(true)
                    } else {
                        Err(format!("set thumbar buttons: {result:?}"))
                    }
                }
                // 已 Add 但无曲目：用 3 个 THBF_HIDDEN 按钮 Update 隐藏，保留 added=true。
                ToolbarAction::Hide => {
                    let hidden = hidden_buttons();
                    let result = unsafe { taskbar.ThumbBarUpdateButtons(hwnd, &hidden) };
                    if result.is_ok() {
                        Ok(true)
                    } else {
                        Err(format!("hide thumbar buttons: {result:?}"))
                    }
                }
            };
            if outcome.is_ok() {
                // 仅成功后提交状态：Add 失败保持未安装，重试仍走 Add。
                inner.toolbar_hwnd = marked_state;
            }
            outcome
        })();
        if co_initialized {
            unsafe { CoUninitialize() };
        }
        result
    }
}

#[cfg(not(windows))]
mod native {
    use super::*;

    // setup() 只在 Windows 上调用 native::install_subclass（见 setup 的
    // `#[cfg(windows)]` 块），非 Windows 无需 stub。

    pub fn apply_buttons(
        _app: &AppHandle,
        _state: TaskbarState,
        _tracked: &Mutex<ThumbarInner>,
    ) -> Result<bool, String> {
        Ok(false)
    }
}

// -- Tauri integration --------------------------------------------------------

/// 启动装配：记录 AppHandle（供 subclass 转发事件）并给主窗口挂上 subclass。
pub fn setup(app: &AppHandle) {
    let _ = MAIN_APP.set(app.clone());
    #[cfg(windows)]
    {
        let Some(window) = app.get_webview_window(MAIN_WINDOW_LABEL) else {
            return;
        };
        if let Ok(hwnd) = window.hwnd() {
            let state = app.state::<ThumbarState>();
            if let Err(error) = native::install_subclass(hwnd, &state.inner) {
                eprintln!("thumbar: install subclass failed: {error}");
            }
        }
    }
}

#[tauri::command]
// 渲染层推送播放状态 → 更新 Windows 任务栏缩略图按钮；非 Windows 安全返回 false。
pub fn thumbar_update_buttons(app: AppHandle, state: Value) -> Result<bool, String> {
    if !thumbar_supported() {
        return Ok(false);
    }
    let parsed = parse_taskbar_state(&state);
    let tracked = &app.state::<ThumbarState>().inner;
    native::apply_buttons(&app, parsed, tracked)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_taskbar_state_coerces_booleans_like_electron() {
        let parsed = parse_taskbar_state(&json!({
            "hasActiveTrack": true,
            "canGoPrevious": false,
            "canGoNext": true,
            "isPlaying": true,
        }));
        assert_eq!(
            parsed,
            TaskbarState {
                has_active_track: true,
                can_go_previous: false,
                can_go_next: true,
                is_playing: true,
            }
        );
    }

    #[test]
    fn parse_taskbar_state_treats_missing_and_foreign_fields_as_false() {
        let parsed = parse_taskbar_state(&json!({
            "hasActiveTrack": "yes",
            "canGoPrevious": null,
            "canGoNext": 1,
            "isPlaying": { "nested": true },
        }));
        assert_eq!(parsed, TaskbarState::default());
    }

    #[test]
    fn parse_taskbar_state_ignores_unknown_keys() {
        let parsed = parse_taskbar_state(&json!({
            "hasActiveTrack": true,
            "isPlaying": true,
            "evil": "payload",
        }));
        assert!(parsed.has_active_track);
        assert!(parsed.is_playing);
        assert!(!parsed.can_go_previous);
        assert!(!parsed.can_go_next);
    }

    #[test]
    fn thumbar_supported_is_windows_only() {
        assert_eq!(thumbar_supported(), cfg!(windows));
    }

    // -- 纯状态机：Add/Update/Hide/NoOp 决策与状态转移（无平台依赖） --

    #[test]
    fn decide_never_added_and_no_track_is_noop() {
        assert_eq!(decide_toolbar_action(None, 100, false), ToolbarAction::NoOp);
    }

    #[test]
    fn decide_never_added_with_track_adds() {
        assert_eq!(decide_toolbar_action(None, 100, true), ToolbarAction::Add);
    }

    #[test]
    fn decide_added_with_track_updates() {
        assert_eq!(
            decide_toolbar_action(Some(100), 100, true),
            ToolbarAction::Update
        );
    }

    #[test]
    fn decide_added_without_track_hides() {
        assert_eq!(
            decide_toolbar_action(Some(100), 100, false),
            ToolbarAction::Hide
        );
    }

    #[test]
    fn decide_is_per_hwnd_not_a_global_flag() {
        // 窗口重建后 HWND 变化：旧的“已 Add”状态对新 HWND 无效。
        assert_eq!(
            decide_toolbar_action(Some(99), 100, true),
            ToolbarAction::Add
        );
        assert_eq!(
            decide_toolbar_action(Some(99), 100, false),
            ToolbarAction::NoOp
        );
    }

    #[test]
    fn mark_after_success_records_only_add() {
        assert_eq!(mark_after_success(None, 100, ToolbarAction::Add), Some(100));
        assert_eq!(
            mark_after_success(Some(100), 100, ToolbarAction::Update),
            Some(100)
        );
        assert_eq!(
            mark_after_success(Some(100), 100, ToolbarAction::Hide),
            Some(100)
        );
        assert_eq!(mark_after_success(None, 100, ToolbarAction::NoOp), None);
    }

    #[test]
    fn hide_keeps_added_so_track_resume_updates_same_hwnd() {
        let hwnd = 42;
        // 无曲目：Hide，且状态保留 Some(hwnd)（added=true）。
        assert_eq!(
            decide_toolbar_action(Some(hwnd), hwnd, false),
            ToolbarAction::Hide
        );
        let after_hide = mark_after_success(Some(hwnd), hwnd, ToolbarAction::Hide);
        assert_eq!(after_hide, Some(hwnd));
        // 曲目恢复：同一 HWND 走 Update 而非再次 Add。
        assert_eq!(
            decide_toolbar_action(after_hide, hwnd, true),
            ToolbarAction::Update
        );
    }

    #[test]
    fn state_round_trip_noop_then_add_then_update_then_hide() {
        let hwnd = 7;
        let mut installed: Option<isize> = None;
        // 无曲目、未安装 → NoOp，状态不变。
        let a = decide_toolbar_action(installed, hwnd, false);
        assert_eq!(a, ToolbarAction::NoOp);
        installed = mark_after_success(installed, hwnd, a);
        assert_eq!(installed, None);
        // 有曲目、未安装 → Add，成功后记录。
        let a = decide_toolbar_action(installed, hwnd, true);
        assert_eq!(a, ToolbarAction::Add);
        installed = mark_after_success(installed, hwnd, a);
        assert_eq!(installed, Some(hwnd));
        // 有曲目、已安装 → Update。
        assert_eq!(
            decide_toolbar_action(installed, hwnd, true),
            ToolbarAction::Update
        );
        // 无曲目、已安装 → Hide，仍保留。
        let a = decide_toolbar_action(installed, hwnd, false);
        assert_eq!(a, ToolbarAction::Hide);
        installed = mark_after_success(installed, hwnd, a);
        assert_eq!(installed, Some(hwnd));
        // 曲目恢复 → Update（不重复 Add）。
        assert_eq!(
            decide_toolbar_action(installed, hwnd, true),
            ToolbarAction::Update
        );
    }

    #[test]
    fn step_serialized_marks_only_on_success_and_retries_add_on_failure() {
        let hwnd = 100;
        // 未安装、失败：状态不变，重试仍走 Add。
        let (action, state) = step_serialized(None, hwnd, true, false);
        assert_eq!(action, ToolbarAction::Add);
        assert_eq!(state, None);
        // 未安装、成功：Add 并记录已安装。
        let (action, state) = step_serialized(None, hwnd, true, true);
        assert_eq!(action, ToolbarAction::Add);
        assert_eq!(state, Some(hwnd));
        // 已安装、成功：Update 且标记保留。
        let (action, state) = step_serialized(state, hwnd, true, true);
        assert_eq!(action, ToolbarAction::Update);
        assert_eq!(state, Some(hwnd));
        // 已安装、失败：状态不变，重试仍 Update（不会误标记导致问题）。
        let (action, state) = step_serialized(state, hwnd, true, false);
        assert_eq!(action, ToolbarAction::Update);
        assert_eq!(state, Some(hwnd));
    }

    #[test]
    fn serialized_critical_section_adds_at_most_once_under_concurrency() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let hwnd = 200;
        // 与真实代码相同的结构：一个 Mutex 保护 per-HWND 状态，临界区内
        // step_serialized 决定动作并计算提交状态。若无锁串行化，两个并发调用
        // 都会读到 None 并各自决定 Add。
        let installed = Mutex::new(None::<isize>);
        let add_count = AtomicUsize::new(0);
        std::thread::scope(|scope| {
            for _ in 0..8 {
                scope.spawn(|| {
                    for _ in 0..50 {
                        let mut inner = installed.lock().unwrap_or_else(|p| p.into_inner());
                        let (action, next) = step_serialized(*inner, hwnd, true, true);
                        if action == ToolbarAction::Add {
                            add_count.fetch_add(1, Ordering::SeqCst);
                        }
                        *inner = next;
                    }
                });
            }
        });
        // Add-once 不变量：并发下同一 HWND 恰好 Add 一次，此后均为 Update。
        assert_eq!(add_count.load(Ordering::SeqCst), 1);
    }
}
