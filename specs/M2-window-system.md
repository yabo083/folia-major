# M2 规格：窗口与系统集成

## 契约（命令清单，均来自 preload.cjs + vite-env.d.ts）

| 命令 | 语义 | Tauri 实现要点 |
|---|---|---|
| `window_minimize` | 最小化主窗口 | `Window::minimize()` |
| `window_toggle_maximize` | 最大化/还原 | `Window::toggle_maximize()`（v2 有）或 is_maximized 判断后 maximize/unmaximize |
| `window_toggle_fullscreen` | 全屏切换 | `Window::set_fullscreen(!is_fullscreen)` |
| `window_close` | 关闭主窗口 | `Window::close()`（触发应用退出逻辑见下） |
| `window_is_maximized` | 是否最大化 | `Window::is_maximized()` |
| `window_get_transparent_mode` | 读取透明模式 | 读 settings `TRANSPARENT_PLAYER_BACKGROUND` |
| `window_set_transparent_mode {enabled, handoff}` | 切换透明模式 + 重建窗口 | **最复杂，见下** |
| `window_playback_handoff_consume` | 消费播放状态交接 | Rust TTL store（见下） |
| `window_playback_handoff_submit {requestId, handoff}` | 提交播放状态交接 | Rust TTL store + 挂起请求解析 |
| `window_set_native_theme {themeSource}` | 设置原生主题 | 记录到 settings；Tauri/WebView2 无运行时 themeSource，M2 先持久化+no-op，M12 复核是否影响 UI |
| `window_get_click_through` | 读取点击穿透 | 返回状态 |
| `window_set_click_through {enabled}` | 点击穿透 | **Windows: SetWindowLongPtr WS_EX_TRANSPARENT（见下）** |
| `window_set_click_through_unlock_hover {active}` | 解锁悬停区域 | 配合穿透，只影响主进程侧状态，M2 可先持久化 no-op 或最小实现 |
| `window_get_always_on_top` | 读取置顶 | 状态 |
| `window_set_always_on_top {enabled}` | 置顶 | `Window::set_always_on_top(enabled)`，并持久化 `MAIN_WINDOW_ALWAYS_ON_TOP` |
| 单实例 | — | `tauri-plugin-single-instance`，二次实例触发 focus 主窗口 |
| 托盘 | 显示/隐藏/退出/遥控/穿透 | `tauri::tray`（M2 做基础：显示隐藏+退出；遥控入口在 M7） |

## 透明模式重建（难点）

- Electron 语义：切换 `TRANSPARENT_PLAYER_BACKGROUND` → 关闭主窗口 → 以 `transparent:true/false` 重建窗口 → 渲染进程重启 → 前端通过 handoff 恢复播放状态。
- Tauri 实现：
  1. 写入 settings `TRANSPARENT_PLAYER_BACKGROUND`。
  2. `rememberWindowPlaybackHandoff(handoff)`（若 requestId 挂起则走 resolvePending 逻辑）。
  3. 用 `tauri::WebviewWindowBuilder` 重建主窗口（`transparent` 在 builder 里设置），destroy 旧窗口。
  4. 重建后窗口 load 同一 frontendDist → shim 自动注入 → 前端读取 handoff 恢复。
- **handoff store**：把 `windowPlaybackHandoff.cjs` 的 TTL 语义（默认 15s、consume 一次即清、过期清空）移植为 Rust（Mutex<Option<(Value, Instant)>> + requestId 挂起表 `pendingWindowPlaybackHandoffRequests`）。单元测试覆盖 TTL/消费一次/过期。
- **注意**：重建窗口期间 `main-window-click-through-changed` 等事件状态要重置（同 Electron closed 处理）。

## 点击穿透 / 置顶 / 隐藏任务栏（Windows 原生）

- 穿透：取主窗口 HWND（Tauri v2 `window.hwnd()`，返回 raw HWND），`SetWindowLongPtrW(hwnd, GWL_EXSTYLE, ex | WS_EX_LAYERED | WS_EX_TRANSPARENT)`；关闭时清除 WS_EX_TRANSPARENT。需要 `windows` crate（仅 windows target）。
- 置顶：Tauri 原生 `set_always_on_top`。
- 隐藏任务栏（`HIDE_TASKBAR_ICON`）：Windows `WS_EX_TOOLWINDOW`（SetWindowLongPtr）或 `ITaskbarList::DeleteTab`。M2 用 WS_EX_TOOLWINDOW，注意与点击穿透的 ex-style 组合。

## 验收标准

1. `cargo test`：handoff store TTL/消费/挂起请求单测；穿透 ex-style 位运算单测。
2. `cargo check` + `cargo clippy`（如可用）通过。
3. `npm run typecheck` 全绿（本模块只动 shim fallback 结构，不动前端）。
4. 冒烟：`tauri dev` 下点最小化/最大化/全屏/关闭生效；置顶开关生效；点击穿透开启后鼠标穿透窗口。
5. 透明模式：切换后窗口重建、播放状态恢复（需要前端已接入手off逻辑——M12 联调确认）。
6. 托盘：右键菜单"显示/隐藏主窗口/退出"生效。

## 规格来源
- `folia-major/electron/main.cjs`：`createWindow`(2755+)、`recreateMainWindowWithTransparencyMode`、窗口 IPC(3231-3358)、托盘(702-772)、单实例(774-782)
- `folia-major/electron/windowPlaybackHandoff.cjs`（TTL store）
- `folia-major/test/unit/electron/windowPlaybackHandoff.test.ts`（行为契约）
- `folia-major/electron/preload.cjs`、`src/vite-env.d.ts`（契约形状）
