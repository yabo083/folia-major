# M4 规格：酷狗 API（kugouRequest 桥）

## 背景与结论（主代理已调研）

- 前端 `src/services/onlineMusic/kugouTransport.ts` 调用 `window.electron.kugouRequest(operation, params)`（见其 `requestKugou`），所有操作经此 IPC。
- Electron 侧 `electron/kugouApiBridge.cjs` 封装 npm 包 `kugoumusicapi`（Node 库），维护 lite 客户端设备身份与 cookie 会话（存 `KUGOU_API_SESSION_V1`）。
- 前端 `persistWebSession(response)` 会从响应读取 `cookie` / `data.token` / `data.userid` / `data.dfid` 持久化到本地 IndexedDB——因此响应形状必须与 kugoumusicapi 一致。
- 另一个关键路径：**酷狗匿名搜索** `requestKugouAnonymousSearch` 不走 kugouRequest，直接 `fetch(complexsearch.kugou.com)`（签名密钥已内嵌前端）。这在 WebView2 会撞 CORS → 由 **M5** 的 shim fetch-patch 解决，不在本模块范围。

## 契约

- `kugouRequest(operation, params)` → 返回该 operation 的响应 JSON（与 kugoumusicapi 输出一致）。
- `kugou_api_status` → `{ available: boolean, error: string|null }`。`available:false` 时前端整块功能禁用（可接受）。
- 会话/设备身份：设备 cookie（KUGOU_API_PLATFORM=lit0 `lite`、KUGOU_API_GUID、KUGOU_API_MID、KUGOU_API_DEV、KUGOU_API_MAC、KUGOU_API_WEBGL）+ `register_dev` 首启注册 + `KUGOU_API_SESSION_V1` 持久化（可存进 settings.json 或独立文件）。
- 设备验证重试：errcode 20028 或 "本次请求需要验证" 时需重新 register_dev 后重试一次（参考 kugouApiBridge.cjs `isDeviceVerificationRequired`）。

## 实现路线（三选一，子代理先评估 `folia-major/node_modules/kugoumusicapi` 体积与依赖再定）

1. **Rust 直连实现**（首选）：用 reqwest 实现 kugoumusicapi 核心操作（signing/headers 照抄）。若库规模可控（<2k 行逻辑），全量移植；否则只做 MVP 集合。
2. **Rust HTTP 中继**：不实现签名，把 kugoumusicapi 的请求逻辑理解后照抄请求构造（同路线 1，本质一样）。
3. 全部 deferred：`available:false`，酷狗功能在 Tauri 版暂不可用（M12 联调时明确提示）。

> 禁止引入 Node sidecar。

### MVP 操作集合（验收最低要求）
`register_dev`、`search`、`song_url`、`lyric`、`search_lyric`、`playlist_track_all`、`playlist_detail`、`album_detail`、`album_songs`、`artist_detail`、`artist_albums`、`personal_fm`、`everyday_recommend`。
> 完整操作列表见 kugouTransport.ts `KUGOU_OPERATIONS`（36 个）。登录链路（login_qr_*）、云盘、会员、歌单增删等可在 MVP 后补齐。

## 验收标准

1. `cargo test` 通过（请求签名/参数构造单测；可 mock 响应做序列化形状单测）。
2. `cargo check` 通过。
3. 冒烟（走代理）：`kugouRequest('search', { keyword:'周杰伦' })` 返回合法结果；`song_url` 能取到播放地址；`lyric` 能取到歌词（含 krm/krc 解密）。
4. 设备验证重试路径正确（可构造 errcode 20028 mock 验证）。
5. 前端 `npm run typecheck` 保持全绿（本模块不改前端）。

## 规格来源
- `folia-major/electron/kugouApiBridge.cjs`（会话/重试/设备 cookie）
- `folia-major/node_modules/kugoumusicapi/`（API 实现，参考）
- `folia-major/test/unit/electron/kugouApiBridge.test.ts`（行为契约）
- `src/services/onlineMusic/kugouTransport.ts`（操作列表与响应消费方式）
