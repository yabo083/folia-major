# M5 规格：歌词代理 / 网络层（lyric_proxy_fetch + CORS 中继）

## 背景与结论（主代理已调研）

- Electron 里两类网络转发：
  1. IPC `lyric_proxy_fetch(url, init)` → main 进程 `proxyLyricRequest`（host 白名单 + 转发，404→204 特例）→ `fetchLyricProxy`。
  2. `session.webRequest.onHeadersReceived` 给 qq.com/kugou.com/amll 域名响应注入 `Access-Control-Allow-Origin: *`（见 main.cjs `setupCorsBypassHandlers`）。
- WebView2 无 webRequest 注入响应头能力，且 `tauri://localhost` 页面跨源 fetch 会被 CORS 拦截。因此 Tauri 需要：

### A. `lyric_proxy_fetch` Rust 实现（替代 Electron IPC）
- 命令：`lyric_proxy_fetch { url, init }` → `{ ok, status, statusText, headers, bodyText }`（与 Electron 返回形状一致，见 main.cjs `proxyLyricRequest`）。
- Host 白名单（必须一致）：`qq.com`、`*.qq.com`、`y.gtimg.cn`、`kugou.com`、`*.kugou.com`、`kgimg.com`、`*.kgimg.com`、`amll-ttml-db.stevexmh.net`。白名单外直接 403。
- 转发时剔除 host/connection/content-length/origin/referer 头（同 Electron）。
- 特例：amll 域 404 → 204。
- reqwest 客户端：走系统代理（可用 `tauri-plugin-http` 的 reqwest client，或自带 reqwest + 系统代理检测）。

### B. shim 层 `window.fetch` patch（解决 WebView2 CORS）
- 问题：`requestKugouAnonymousSearch` 直接 `fetch(complexsearch.kugou.com)`、封面/媒体等偶发跨源请求在 WebView2 会被 CORS 拦截，且前端源码不可改。
- 方案：在 `public/electron-shim.js` 里，仅当 Tauri 运行时，对目标 host 在白名单内（kugou.com/*.kugou.com/qq.com/*.qq.com/y.gtimg.cn/kgimg.com/amll-ttml-db.stevexmh.net）的 `fetch` 调用，改写为走 `lyric_proxy_fetch` 命令；非白名单原样放行。
- 注意：
  - 保持 `fetch` 签名与返回的 `Response` 兼容（`ok/status/statusText/headers/json()/arrayBuffer()/text()` 至少这些）。
  - 必须异步 patch（在 app 模块脚本运行前完成注入——shim 已是 head 同步脚本，直接 `window.fetch = patchedFetch`）。
  - 网易云 localhost 请求不要拦截（走 M3 服务器的 CORS 头）。
  - 不做黑名单绕过，仅白名单中继。

## 验收标准

1. `cargo test`：`lyric_proxy_fetch` 白名单/非白名单、404→204、头剔除逻辑单测。
2. `cargo check` 通过。
3. 冒烟（走代理）：`lyric_proxy_fetch('https://www.kugou.com/...')` 返回 200 + 数据；非白名单域返回 403。
4. 前端 `npm run typecheck` 全绿；`window.fetch` patch 不破坏 vitest（patch 仅在 Tauri 运行时生效，jsdom 环境无 `window.__TAURI__`）。
5. M12 联调时验证：酷狗匿名搜索在 WebView2 内可正常请求（CORS 已绕开）。

## 规格来源
- `folia-major/electron/main.cjs` `proxyLyricRequest`/`isAllowedLyricProxyHost`/`setupCorsBypassHandlers`
- `folia-major/vite.config.ts` `devLyricProxyPlugin`（dev 行为参考）
- `src/services/onlineMusic/kugouTransport.ts` `requestKugouAnonymousSearch`
