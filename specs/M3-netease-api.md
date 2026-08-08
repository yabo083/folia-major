# M3 规格：网易云 API（Rust 本地 HTTP 服务器）

## 背景与结论（主代理已调研）

- 前端 `src/services/netease.ts` 通过 `window.electron.getNeteasePort()` 拿端口，然后直接 `fetch('http://localhost:PORT/...')`。
- 因此 M3 **必须是真实本地 HTTP 服务器**（不是纯 JS 客户端），监听 127.0.0.1 动态端口。
- 服务器本质是转发代理：接收前端的 REST 请求 → 用网易云加密协议（weapi/eapi/xeapi）包装 → 转发到 `music.163.com` 对应接口 → 解密/透传 JSON。
- `cookie` 由前端通过 URL query 参数传递（`?cookie=...`），服务器要把它作为请求网易云时的 Cookie 头。
- 加密算法已确认 Rust 可移植（详见下文算法段）。参考实现：`folia-major/node_modules/@neteasecloudmusicapienhanced/api/`（JS）。

## 契约（必须与前端 netease.ts 完全一致）

请求：GET + query 参数，`mode: cors` 由前端发起；响应：JSON 原样透传（`code` 字段透传）。前端会对 `code===301/401/403` 清理匿名 cookie，必须透传。

### CORS（必须，否则前端在 WebView2 内取不到数据）

- Tauri 页面源是 `tauri://localhost`，而服务器在 `http://localhost:PORT`，属跨源。
- 服务器必须对响应加 `Access-Control-Allow-Origin: *`，并处理 `OPTIONS` 预检（返回 204 + `Access-Control-Allow-Methods: GET,POST` + `Access-Control-Allow-Headers: *`）。参考 Electron 下 `serveNcmApi`（网易云库 server.js 自带 CORS 中间件）的行为。

### 端点清单（前端 netease.ts 实际调用，共 45 个）

| 端点 | 网易云目标接口 | 协议 |
|---|---|---|
| `/register/anonimous` | 匿名注册（增强版特有） | xeapi |
| `/login/qr/key` | /weapi/login/qrcode/unikey | weapi |
| `/login/qr/create?key&qrimg` | /weapi/login/qrcode/create | weapi |
| `/login/qr/check?key` | /weapi/login/qrcode/client/login | weapi |
| `/login/status` | /weapi/w/nuser/account/get | weapi |
| `/logout` | /weapi/logout | weapi |
| `/user/account` | /weapi/w/nuser/account/get | weapi |
| `/like?id&like` | /weapi/radio/like | weapi |
| `/likelist?uid` | /weapi/song/like/get | weapi |
| `/user/playlist?uid&limit&offset` | /weapi/user/playlist | weapi |
| `/user/cloud?limit&offset` | /weapi/v1/cloud/get | weapi |
| `/user/cloud/detail?id` | /weapi/v1/cloud/get/byids | weapi |
| `/cloud/lyric/get?uid&sid` | /weapi/cloud/lyric/get | weapi |
| `/playlist/detail?id` | /weapi/v6/playlist/detail | weapi |
| `/playlist/track/all?id&limit&offset` | /weapi/v6/playlist/detail + 手工切片 | weapi |
| `/playlist/tracks?op&pid&tracks` | /weapi/playlist/manipulate/tracks | weapi |
| `/playlist/subscribe?t&id` | /weapi/playlist/subscribe | weapi |
| `/playlist/detail/dynamic?id` | /weapi/playlist/detail | weapi |
| `/album?id` | /weapi/v1/album/{id} | weapi |
| `/album/sublist?limit&offset` | /weapi/album/sublist | weapi |
| `/album/sub?t&id` | /weapi/album/sub | weapi |
| `/album/detail/dynamic?id` | /weapi/album/detail/dynamic | weapi |
| `/artist/detail?id` | /weapi/artist/detail | weapi |
| `/artist/album?id&limit&offset` | /weapi/artist/albums/{id} | weapi |
| `/artist/top/song?id` | /weapi/v1/artist/songs | weapi |
| `/artist/songs?id&limit&offset&order` | /weapi/v1/artist/songs | weapi |
| `/song/detail?ids` | /weapi/v3/song/detail | weapi |
| `/song/url/v1?id&level&randomCNIP&https` | /weapi/song/enhance/player/url/v1 | weapi |
| `/song/chorus?id` | /weapi/song/chorus | weapi |
| `/song/copyright/rcmd?songid` | /weapi/song/copyright/config/rcmd | weapi |
| `/lyric/new?id` | /weapi/song/lyric?os=os | weapi |
| `/cloudsearch?keywords&limit&offset` | /weapi/cloudsearch/get/web | weapi |
| `/personal_fm` | /weapi/v1/radio/get | weapi |
| `/recommend/songs?afresh` | /weapi/v3/discovery/recommend/songs | weapi |
| `/recommend/songs/dislike?id` | /weapi/v3/discovery/recommend/songs/dislike | weapi |
| `/history/recommend/songs` | /weapi/discovery/recommend/songs/history/recent | weapi |
| `/history/recommend/songs/detail?date` | /weapi/discovery/recommend/songs/history/detail | weapi |
| `/personalized?limit` | /weapi/personalized/playlist | weapi |
| `/fm_trash?id` | /weapi/radio/trash/add | weapi |

> 具体目标 URL 与参数以 folia-major 参考库 `module/` 下对应文件为准，实现时必须逐一对照，禁止猜接口结构。

## 加密算法（参考库 util/crypto.js）

常量：
- `iv = '0102030405060708'`
- `presetKey = '0CoJUm6Qyw8W8jud'`
- `linuxapiKey = 'rFgB&h#%2?^eDg:Q'`
- `eapiKey = 'e82ckenh8dichen8'`
- `publicKey`：RSA 1024 公钥（见参考文件）
- weapi 随机 secretKey：16 位，来自 base62

Rust crate 映射：
- AES-128-CBC / AES-128-ECB PKCS7 → `aes` + `cbc`/`ecb` 模式 crate（或 `openssl`）
- RSA（forge `encrypt(msg, 'NONE')` = 原始 RSA，无填充，16 字节消息）→ `rsa` crate `RawEncryption` 或 `openssl` 手动
- MD5 → `md-5`
- 十六进制编解码 → `hex`
- X25519 → `x25519-dalek`

weapi：`params = AES-CBC(AES-CBC(text, presetKey, iv), secretKey, iv)`，`encSecKey = rawRSA(reverse(secretKey), publicKey)`。
eapi：`digest = md5('nobody'+url+'use'+text+'md5forencrypt')`，`data = url+'-36cd479b6b5-'+text+'-36cd479b6b5-'+digest`，`params = AES-ECB(data, eapiKey)`（hex，大写）。响应解密用 AES-ECB + eapiKey。
xeapi：X25519 密钥协商 + AES 相关（见参考库 util/client-sign.js、xeapiKey.js，M3b 处理）。

## 拆分

- **M3a**：axum 服务器骨架 + weapi/eapi 加密 + 前 24 个内容端点（search/song/lyric/playlist/album/artist/url/recommend 等无登录/低风险端点）。验收：cargo test 加密向量正确 + 服务器对 `/song/detail` 等真实请求（走代理）返回 code 200。
- **M3b**：xeapi 匿名注册（`/register/anonimous`，MUSIC_A cookie）+ 登录链路（qr key/create/check、login/status、logout、user/account）+ 用户端点。验收：匿名注册返回 cookie；QR 登录流程可用。

## 服务器实现要点

- 监听：启动时绑定 `127.0.0.1:0`（随机端口），成功后通过 Tauri 事件 `netease-api-status-changed` 推送 `{ status:'running', port, error:null, updatedAt }`，并让 `get_netease_port` 返回真实端口。
- 状态：`get_netease_api_status` 返回当前状态；启动失败推送 `{ status:'error', ... }`（前端 useElectronNeteaseApiStatus 会 toast）。
- 转发：reqwest client，对网易云接口发起 HTTPS 请求；UA 必须带网易云标志；`cookie` 参数原样作为 Cookie 头（参考库对 cookie 的处理：可能需手动加 `NMTID` 等，以参考库为准）。
- 安全：只监听 loopback；token 校验不是必需（仅 loopback），但不得开放到局域网。
- 并发：axum 默认即可；网易云接口有风控，参考库的重试/延迟逻辑可简化，但 `/register/anonimous` 与 xeapi key 刷新必须有缓存与失败降级（参考 `electron/neteaseApiStartup.cjs` 的重试策略）。

## 验证命令

- `cd src-tauri && cargo test`（加密向量单测必须过）
- `cd src-tauri && cargo check`
- 真实请求冒烟：用代理 HTTPS_PROXY=http://127.0.0.1:7890 起服务，`curl 'http://127.0.0.1:PORT/song/detail?ids=33894312'` 应返回 `{code:200, songs:[...]}`。
- 前端：`npm run typecheck` 保持全绿（本模块不改前端）。
