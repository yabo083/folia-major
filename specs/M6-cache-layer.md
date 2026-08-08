# M6 规格：缓存层（audio/cover cache）

## 契约（与 Electron 完全一致）

路径规则（`electron/main.cjs` getAudioCachePaths/getCoverCachePaths）：
- 根目录：settings `CACHE_DIRECTORY`（未设置时 `app_data_dir/media-cache`）——**与 M1 的 get_cache_directory 联动**。
- audio：`<root>/audio/<sha256(cacheKey)>.bin` + 同名 `.json`（meta：`{ mimeType }`）。
- cover：`<root>/cover/<sha256(cacheKey)>.bin` + `.json`。

命令与返回形状：
| 命令 | 返回 |
|---|---|
| `get_audio_cache {cacheKey}` | `{ found:bool, data?:ArrayBuffer, mimeType?:string }`（默认 mimeType `audio/mpeg`） |
| `has_audio_cache {cacheKey}` | bool |
| `save_audio_cache {cacheKey, data, mimeType}` | bool（data 为 Uint8Array，写入 .bin；meta 写 .json） |
| `get_audio_cache_usage` | 字节数（音频目录 .bin 大小总和，见 getAudioCacheUsageBytes） |
| `get_audio_cache_stats` | `{ size:number, count:number }`（仅 .bin 文件） |
| `clear_audio_cache` | bool（递归删除 audio 目录） |
| `get_cover_cache {cacheKey}` | `{ found, data, mimeType }`（cover 默认 mimeType 由 meta 决定，参考 readCoverCacheEntry） |
| `save_cover_cache {cacheKey, data, mimeType}` | bool |
| `remove_cover_cache {cacheKey}` | bool（删 .bin+.json） |
| `get_cover_cache_usage` | 字节数 |
| `clear_cover_cache` | bool |

> data 在 IPC 侧是 Uint8Array/ArrayBuffer。Tauri invoke 命令参数传 `Vec<u8>`（JSON 数组），返回时用 `tauri::ipc::Response` 或 base64？注意：shim 用 invoke 传 JSON，二进制数组 会膨胀。**实现必须保持与 Electron 相同的数据传递**：Electron 传 Buffer（IPC 序列化为 typed array）。Tauri 侧用 `Vec<u8>` serde 即可（invoke 支持数字数组/ArrayBuffer），M6 实现后 M12 联调实测前端的 data 是否被正确识别为 Uint8Array。若 JSON 数组不可行，可改为 base64 字符串并同步调整 shim（记录到 PLAN）。

## 验收标准

1. `cargo test`：路径生成（sha256 文件名）、读写往返、meta mimeType 默认/覆盖、stats 计数与大小、clear 删除。
2. `cargo check` 通过；`npm run typecheck` 全绿。
3. 冒烟（M12 联调）：播放在线歌曲后 audio 缓存落盘；切换歌曲命中缓存；封面缓存读写正常；设置自定义缓存目录后路径生效。

## 规格来源
- `electron/main.cjs`：`getAudioCachePaths`(578-587)、`readAudioCacheEntry`(1924)、`getAudioCacheStats`(2000)、`readCoverCacheEntry`(2042)、`getAudioCacheUsageBytes`(1978)、`clearAudioCacheDirectory`、对应 IPC(3170-3216)
- `src/services/audioCache.ts`、`src/services/coverCache.ts`（前端消费方式）
- `src/vite-env.d.ts` `ElectronAudioCacheEntry`(23-27)
