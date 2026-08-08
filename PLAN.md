# Folia-Tauri 二开计划

把 `folia-major`（Electron，NSIS 169MB）按 OpenCodeUI 的技术路线二开为 Tauri v2。
目标：Windows 安装包 < 40MB（M11 实测 8.537MiB），尽量通过 shim 复用前端，必要的 WebView2 交互改动保持最小。

## 架构决策（所有模块必须遵守）

1. **前端最小改动**：优先复用 `folia-major` 的 `src/`，桌面能力通过 `window.electron` shim（`public/electron-shim.js`）映射到 Tauri 命令。仅当 WebView2 安全模型无法由 shim 覆盖时改前端，并在 PLAN 记录原因；M9 远程视频导出的主窗口用户手势确认是当前唯一例外。
2. **命令契约**：Rust 命令用 snake_case（invoke 侧同名）；事件名与 Electron 完全一致（`update-status-changed` 等）；`window.electron` 的方法签名与 `folia-major/electron/preload.cjs` 完全一致。
3. **shim 兜底**：命令未实现时 shim 返回安全默认值（已有 fallback 表），应用始终保持可启动。
4. **状态持久化**：不用 electron-store，改用 Rust 直接读写 JSON（app_data_dir/folia/settings.json），键名与 electron-store 完全一致（含 `APP_LOCALE`、`CACHE_DIRECTORY` 等常量键）。
5. **内嵌 Node 服务**：网易云/KuGou API 优先"渲染层纯 JS 客户端 + Rust HTTP（tauri-plugin-http 无 CORS）"，禁止引入 Node sidecar（会摧毁体积目标）。
6. **安全边界**：所有外部输入（URL 代理目标、文件路径、命令参数）在 Rust 侧校验白名单/格式，同 Electron 原实现语义。
7. **测试**：Rust 单元测试用 cargo test；行为契约以 `folia-major/test/unit/electron/*.test.ts`（含 stageApi/discordPresence/kugouApiBridge/neteaseApiStartup/updateChannels/voiceInputPause/windowPlaybackHandoff）为规格来源，Rust 实现必须覆盖同等行为；前端 vitest 与 `npm run typecheck` 必须保持全绿。
8. **Rust 代码风格**：模块按领域拆文件（settings.rs / window.rs / netease.rs / cache.rs ...），lib.rs 只做注册；每个命令函数一行注释说明用途。

## 模块清单（依赖顺序）

| 模块 | 内容 | 验收标准 | 规格来源 |
|---|---|---|---|
| M0 ✅ | 脚手架：Tauri 骨架、shim、构建链路 | 安装包 7.1MB，应用可启动 | — |
| M1 ✅ | 设置与存储 | settings 读写落盘、locale、cache dir 真实实现 | `electron/main.cjs` store 用法、`src/components/modal/SettingsModal.tsx:398`、`src/hooks/useAppPreferences.ts` |
| M2 ✅ | 窗口与系统集成 | 窗口控制/置顶/点击穿透/透明/托盘/单实例真实实现 | `electron/main.cjs` 窗口段、`electron/windowPlaybackHandoff.cjs` |
| M3 ✅ | 网易云 API | 渲染层 JS 客户端或 Rust 实现，接口契约同 serveNcmApi | `@neteasecloudmusicapienhanced/api`（folia-major node_modules 参考）、`electron/neteaseApiStartup.cjs`、`test/unit/electron/neteaseApiStartup.test.ts` |
| M4 ✅ | 酷狗 API | kugouRequest 真实实现 | `electron/kugouApiBridge.cjs`、`test/unit/electron/kugouApiBridge.test.ts` |
| M5 ✅ | 歌词代理/网络层 | lyric_proxy_fetch、CORS 绕过、酷狗证书处理 | `electron/main.cjs` proxyLyricRequest、`vite.config.ts` devLyricProxyPlugin |
| M6 ✅ | 缓存层 | audio/cover cache 命令（含用量统计） | `electron/main.cjs` getAudioCachePaths 段 |
| M7 ✅ | OBS 源 + stage API | 本地 HTTP 服务器（Rust axum 或等价），事件双向 | `electron/stageApi.cjs`、`test/unit/stage/stageApi.test.ts` |
| M8 ✅ | thumbar/Discord RPC/语音输入暂停 | 三功能真实实现 | `electron/discordPresence.cjs`、`electron/voiceInputPause.cjs`、对应测试 |
| M9 ✅ | 视频导出 | getDisplayMedia 录制 + Rust 写文件 | `electron/main.cjs` video-export 段 |
| M10 ✅ | 自动更新 | tauri-plugin-updater + 构建期注入端点/公钥 + Windows release workflow | `electron/updateChannels.cjs`、`test/unit/electron/updateChannels.test.ts` |
| M11 ✅ | 打包体积优化 | release profile 已开 lto/strip/opt-level s；NSIS 实测 8.537MiB | — |
| M12 ◐ | 联调回归 | 全量 build/cargo test/vitest 与发布 exe 启动冒烟已通过；交互与 live updater 冒烟待人工完成 | — |

## 常用验证命令（在 folia-tauri 根目录）

- 前端类型：`npm run typecheck`
- 前端单测：`npm run test:unit`
- Rust 检查：`cd src-tauri && cargo check`（网络代理：HTTPS_PROXY=http://127.0.0.1:7890）
- Rust 测试：`cd src-tauri && cargo test`
- 完整打包：`npm run build:tauri:dir`（不产出安装包）/ `npm run build:tauri:app`
- 安装包路径：`src-tauri/target/release/bundle/nsis/Folia_0.6.12_x64-setup.exe`

## 体积基线（M0 实测）

- Rust exe：9.1MB（release，lto+strip+opt-level s）
- 前端 dist：12MB（含 workbox precache 63 项 / 6.9MB）
- NSIS 安装包：**7.1MB**

## M10 自动更新（已实现，live 验证待发布后补）

### 契约（与 Electron 完全一致，前端零改动）

- 命令：`get_update_status` / `updates_check` / `updates_mark_seen` /
  `updates_open_release_page` / `updates_download` / `updates_quit_and_install`
  （shim `public/electron-shim.js` 已按这些名字映射）。
- 事件：`update-status-changed`（emit 到 `main` 窗口）。
- 状态机：`disabled/idle/checking/available/latest/error/downloading/downloaded/unsupported`，
  `ElectronUpdateStatus` 字段形状逐字对齐 `folia-major`（含 `downloadProgress.percent/transferred/total`）。
- 通道语义（stable/beta/nightly）：`realeco→latest.json`（拒预发布）、`limo→beta.json`
  （稳定+beta，拒 alpha）、`cielo→alpha.json`（全放行）、`internal→不更新`；
  版本后缀推断与 `electron/updateChannels.cjs` 一致。rollover tag 发布页 URL 逻辑已镜像。

### 安全设计（本 fork 无正式发布仓库/签名密钥 → 构建期注入 + 运行时 fail closed）

- 端点映射 `FOLIA_UPDATE_ENDPOINTS`（JSON，channel→URL）、公钥 `FOLIA_UPDATE_PUBKEY`、
  release 页 `FOLIA_RELEASES_URL` 全部经 `option_env!` 编译期注入 `src-tauri/src/updater.rs`；
  任一缺失/非法 → `supported=false`，`updates_check` 返回 `idle`，**没有任何默认 URL**。
- 签名验证强制：公钥缺失时配置解析即失败、更新器不会被构造；插件 `Update::download`
  内部对下载内容做 `verify_signature`（解密公钥失败即下载失败）。
- 降级防护：比较器只接受严格大于当前版本的 manifest（semver）。
- 跨通道防护：每次检查只向**当前通道的单端点**查询（避免插件多端点"按序取第一个"串台）；
  比较器再按通道拒绝不该出现的预发布版本（realeco 拒 pre / limo 拒 alpha）。
- 并发/失效防护：顶层 check/download 经 `UpdaterState.operation`（tokio Mutex）串行化，
  manual/startup/settings/auto 触发不会交错；检查内自动下载、下载内补查均走无锁内部 helper
  避免重入死锁。禁用检查或切换 `UPDATE_CHANNEL` 时 `invalidate()` 递增 `generation`
  （`GenerationCounter`）并清空 available/progress/已下载包；已下载包按
  `DownloadBinding { channel, generation }` 绑定，`updates_quit_and_install` 不匹配即拒绝安装；
  进行中操作经 `mutate_if_current` 在 inner 锁内比对代次，旧操作（含下载进度回调）无法
  覆盖新状态；无更新检查成功时同步清空作废的旧下载包。
- `cargo:rerun-if-env-changed` 已加，改注入变量会触发重编译。

### 发布构建（`.github/workflows/release.yml` + `.github/release-config.json`）

- 手动触发，选择通道（realeco/limo/cielo）；`npm run build:tauri` → `npx tauri build
  --bundles nsis --config .github/release-config.json`（仅此 CI 合并配置开启
  `bundle.createUpdaterArtifacts`，本地 `build:tauri:dir` 不受影响、无需签名密钥）。
- 产出：签名 NSIS 安装包 + `.sig` + 通道 manifest（`scripts/generate-update-manifest.mjs`），
  **只 upload-artifact，不发布**（无 `gh release create`）。
- 可选 Windows 代码签名：提供 `FOLIA_WINDOWS_SIGNING_CERT_BASE64/PASSWORD` 时 signtool 签名
  安装包并**重签** `.sig`（顺序不可反）。

### 仓库 Settings 配置（缺失即 CI 失败）

| 类型 | 名称 | 说明 |
|---|---|---|
| Secret | `TAURI_SIGNING_PRIVATE_KEY` | 更新签名私钥（`tauri signer generate -w` 输出 base64），必填 |
| Secret | `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` | 私钥密码，必填（无密码则为空串） |
| Secret | `FOLIA_UPDATE_PUBKEY` | 与私钥配对的公钥 base64，必填 |
| Secret | `FOLIA_WINDOWS_SIGNING_CERT_BASE64` | 可选，PFX 证书 base64 |
| Secret | `FOLIA_WINDOWS_SIGNING_CERT_PASSWORD` | 可选，证书密码 |
| Variable | `FOLIA_UPDATE_ENDPOINTS` | 必填，`{"realeco":"https://…/latest.json","limo":"…/beta.json","cielo":"…/alpha.json"}` |
| Variable | `FOLIA_UPDATE_ASSET_BASE_URL` | 必填，安装包托管基址（manifest url 前缀） |
| Variable | `FOLIA_RELEASES_URL` | 必填，HTTPS release 页 URL |

### 本地验证（不依赖仓库配置）

- Rust：`cd src-tauri && cargo fmt && cargo check && cargo test`（updater 纯逻辑单测全覆盖）。
- TS：`npx vitest run -c vitest.config.ts test/unit/update/updateChannels.test.ts`。
- 未设置注入变量时：应用可正常启动，更新区显示"not supported"（fail closed 生效）。
- `src-tauri/tauri.conf.json` 保留空的 `plugins.updater` 对象（空公钥、空端点）仅用于满足插件配置反序列化；真实更新配置始终来自上述构建期注入变量，不配置时不会联网或安装更新。

## M11 打包体积（2026-08-07 实测）

- 构建命令：`npx cross-env CARGO_BUILD_JOBS=1 npx tauri build`。
- NSIS：`src-tauri/target/release/bundle/nsis/Folia_0.6.12_x64-setup.exe`，
  **8,951,778 bytes / 8.537MiB**，SHA-256
  `6A3F6697C1BFE8DD5B0208E79CFFFC954D1040AFBD597EDA96D48258CF2F4556`。
- 主程序：`src-tauri/target/release/folia-tauri.exe`，**13,274,624 bytes / 12.660MiB**。
- 发布 exe 在未注入 updater 配置时持续运行 8 秒且无启动 panic；更新功能保持 fail closed。
- 对比：原 Electron NSIS 约 169MB；当前安装包约为其 5.0%，且不携带 Chromium/Node/ffmpeg sidecar。

## 风险与已知缺口

- **M10 live 发布链路未验证**（本 fork 尚无 release 仓库/签名密钥/托管端点）：已实现的为
  本地可测部分（状态机、通道门禁、manifest 生成、CI 工作流、fail closed 行为）；
  `tauri build --config .github/release-config.json` 的真实签名产物、`latest/beta/alpha.json`
  经真实仓库 Serve 后 `updates_check` 到 `downloaded` 的端到端安装，需在配置好 Secrets/Variables
  后由 CI 跑通并手工冒烟（M12 联调项）。
- 可选代码签名未实测：signtool 路径探测依赖 Windows SDK 存在；未提供证书时跳过（SmartScreen 提示属预期）。
- Tauri updater 无 electron-updater 的 `channel` 字段，通道语义由"每通道独立 manifest 端点 +
  比较器门禁"实现；端点 URL 写死进二进制，更换托管位置需重新构建（与签名密钥轮换同频率）。

- M3 的 xeapi 匿名注册/密钥刷新链路最复杂，优先攻破 weapi/eapi 基础链路再上 xeapi。
- **M9 视频导出（WebView2）已知缺口**：WebView2 无 desktopCapturer，捕获改为 shim 层
  getDisplayMedia 适配（已实测 MediaRecorder/captureStream/getDisplayMedia 可用）：
  - getDisplayMedia 需要瞬时用户激活，因此在 `chooseVideoExportPath` 内**同步**启动（用户手势内），
    但会**先等**显示流成功解析再打开保存对话框（选择器拒绝/取消则不开保存框、原错误透传），
    两对话框不重叠；窗口尺寸以用户选择时刻为准，早于 `prepareVideoExportWindow` 的按 preset 缩放
    （最终录制分辨率可能不等于 preset）。
  - 系统选择器由用户决定，**无法静默强制选择 Folia**；Rust 仅在主窗口存在时返回哨兵 source id，
    用户选了其他窗口则录制的是该窗口（与 Electron desktopCapturer 语义不同）。
  - 远程控制事件先聚焦主窗口并显示持久操作提示；用户点击主窗口操作按钮后，
    在同一调用栈同步启动 getDisplayMedia，从而满足瞬时用户激活要求。实际选择器/写盘/恢复仍列入 M12 人工冒烟。
  - 原始 IPC 写文件依赖 Tauri 自定义协议（`application/octet-stream` 请求体）；若 WebView2 回退到
    postMessage IPC，raw body 会变 JSON 数组并被 Rust 如实拒绝（错误透传，不落盘）。
- M7 的远程控制窗口需 WebviewWindow 第二窗口 + 与主窗口的状态快照同步。
- 前端 importmap 引用了 aistudiocdn.com 的 CDN 包（index.html），构建时 rollup 会内联这些依赖，但需确认无运行时 CDN 依赖（M12 联调时验证离线可用性）。
- **WebView2 CORS**（主代理已确认，M3/M4/M5 必读）：Tauri 页面源为 `tauri://localhost`，跨源 fetch 会被 CORS 拦截。已确认的缓解：
  - netease 本地服务器必须自带 CORS 头 + OPTIONS 处理（见 specs/M3-netease-api.md）。
  - 酷狗匿名搜索 `requestKugouAnonymousSearch` 在前端直接 `fetch(complexsearch.kugou.com)`（不走 IPC），在 WebView2 会撞 CORS → 由 M5 在 shim 中 patch `window.fetch`，把目标 host 在 kugou.com/qq.com/amll 白名单内的跨源请求转发到 Rust HTTP relay 命令（shim 层改动，不动前端源码）。
  - kugoumusicapi 桥本身走 `kugouRequest` IPC，无 CORS 问题。
